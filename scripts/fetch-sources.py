#!/usr/bin/env python3
"""Fetch exact HTTPS source archives from a packager source-lock; verify SHA first.

Does not install/execute downloads or extract archive paths. Network requires
--download. The resulting tarball belongs beside the matching app release.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import re
import shutil
import secrets
import stat
import tarfile
import urllib.request

MAX_BYTES = 512 * 1024 * 1024  # Per archive, fail rather than unlimited download.


def filename(entry):
    if not re.fullmatch(r"[0-9a-f]{64}", entry["sha256"]):
        raise ValueError("Invalid SHA-256")
    for key in ("name", "version"):
        if not re.fullmatch(r"[A-Za-z0-9_.+@-]+", entry[key]):
            raise ValueError(f"Unsafe source {key}")
    if not entry["url"].startswith("https://"):
        raise ValueError("Source must use HTTPS")
    return entry["sha256"] + "-" + entry["name"] + "-" + entry["version"] + ".source"


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def verified_existing(directory, name, expected):
    """Read the existing inode, never follow a target symlink (even dangling)."""
    try:
        fd = os.open(name, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK, dir_fd=directory)
    except FileNotFoundError:
        return False
    except OSError as error:
        raise ValueError(f"Unsafe existing source: {name}") from error
    with os.fdopen(fd, "rb") as stream:
        if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
            raise ValueError(f"Existing source is not a regular file: {name}")
        if hashlib.file_digest(stream, "sha256").hexdigest() != expected:
            raise ValueError(f"Existing source hash mismatch: {name}")
    return True


def publish_stream(directory, name, source, expected, replace_snapshot=False):
    """Write privately, hash the written bytes, publish exclusively without overwrite."""
    temporary = ".gliff-source-" + secrets.token_hex(16)
    fd = os.open(temporary, os.O_WRONLY | os.O_CREAT | os.O_EXCL | os.O_NOFOLLOW,
                 0o600, dir_fd=directory)
    try:
        checksum, size = hashlib.sha256(), 0
        with os.fdopen(fd, "wb") as dest:
            while chunk := source.read(1024 * 1024):
                size += len(chunk)
                if size > MAX_BYTES:
                    raise ValueError(f"Source exceeds {MAX_BYTES} bytes")
                checksum.update(chunk)
                dest.write(chunk)
            dest.flush()
            os.fsync(dest.fileno())
        if checksum.hexdigest() != expected:
            raise ValueError(f"Source SHA mismatch: {name}")
        # link() is an exclusive atomic publication: EEXIST on any target inode,
        # including a dangling symlink installed after our initial check.
        if replace_snapshot:
            # Rename replaces the directory entry, never writes through a link.
            os.replace(temporary, name, src_dir_fd=directory, dst_dir_fd=directory)
            temporary = None
        else:
            os.link(temporary, name, src_dir_fd=directory, dst_dir_fd=directory, follow_symlinks=False)
    finally:
        if temporary is not None:
            os.unlink(temporary, dir_fd=directory)


def fetch(lock, output, download):
    entries = json.loads(lock.read_text())["sources"]
    output.mkdir(parents=True, exist_ok=True, mode=0o700)
    directory = os.open(output, os.O_RDONLY | os.O_DIRECTORY | os.O_NOFOLLOW)
    try:
        info = os.fstat(directory)
        if info.st_uid != os.getuid() or info.st_mode & 0o022:
            raise ValueError("Source output directory must be owned by this user and not group/world writable")
        for entry in entries:
            name = filename(entry)
            target = output / name
            # Explicit early rejection also covers Path.exists()==False for
            # dangling links. The fd-relative nofollow/exclusive operations
            # below retain protection if a target appears after this check.
            if target.is_symlink():
                raise ValueError(f"Unsafe existing source symlink: {target}")
            if verified_existing(directory, name, entry["sha256"]):
                continue
            if entry.get("kind") == "cargo":
                cargo_home = Path(os.environ.get("CARGO_HOME", Path.home() / ".cargo"))
                cached = list((cargo_home / "registry/cache").glob("*/" + entry["name"] + "-" + entry["version"] + ".crate"))
                match = next((p for p in cached if p.is_file() and digest(p) == entry["sha256"]), None)
                if match:
                    with match.open("rb") as source:
                        publish_stream(directory, name, source, entry["sha256"])
                    continue
            if not download:
                raise ValueError(f"Missing {name}; --download explicitly enables network")
            request = urllib.request.Request(entry["url"], headers={"User-Agent": "gliff-macos-source-archiver/1"})
            with urllib.request.urlopen(request, timeout=120) as source:
                if not source.geturl().startswith("https://"):
                    raise ValueError("Source redirected to insecure transport")
                publish_stream(directory, name, source, entry["sha256"])
        # The bootstrap lock is intentionally replaced by the complete lock.
        # Atomic rename changes only this directory entry, not a symlink target.
        if (output / "source-lock.json").is_symlink():
            raise ValueError("Unsafe source-lock snapshot symlink")
        import io
        data = lock.read_bytes()
        publish_stream(directory, "source-lock.json", io.BytesIO(data),
                       hashlib.sha256(data).hexdigest(), replace_snapshot=True)

    finally:
        os.close(directory)
    return sum((output / filename(entry)).stat().st_size for entry in entries)


def archive_sources(source_dir, archive, recipes):
    # No archive-controlled names; tar contains verified files and local recipes.
    entries = json.loads((source_dir / "source-lock.json").read_text())["sources"]
    with tarfile.open(archive, "w:gz") as out:
        out.add(source_dir / "source-lock.json", arcname="third-party-sources/source-lock.json")
        seen = set()
        for entry in entries:
            name = filename(entry)
            if name in seen:
                continue
            seen.add(name)
            target = source_dir / name
            if target.is_symlink() or digest(target) != entry["sha256"]:
                raise ValueError(f"Source changed before archiving: {target}")
            out.add(target, arcname="third-party-sources/archives/" + name)
        if not recipes.is_dir():
            raise ValueError("Bundle license/recipe directory is required")
        for path in recipes.rglob("*"):
            if path.is_symlink():
                raise ValueError(f"Unexpected recipe symlink: {path}")
        out.add(recipes, arcname="third-party-sources/licenses-and-recipes")
    checksum = digest(archive)
    with (archive.parent / "SHA256SUMS").open("a") as checksums:
        checksums.write(checksum + "  " + archive.name + "\n")


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("lock", type=Path)
    parser.add_argument("--output", required=True, type=Path)
    parser.add_argument("--download", action="store_true")
    parser.add_argument("--archive", type=Path)
    parser.add_argument("--recipes", type=Path)
    args = parser.parse_args()
    size = fetch(args.lock, args.output, args.download)
    if args.archive:
        if not args.recipes:
            parser.error("--archive requires --recipes from the matching bundle")
        archive_sources(args.output, args.archive, args.recipes)
    print(f"Verified source archives: {size / 1024 / 1024:.1f} MiB")


if __name__ == "__main__":
    main()
