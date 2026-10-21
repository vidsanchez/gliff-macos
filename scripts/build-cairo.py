#!/usr/bin/env python3
"""Build exact Cairo 1.18.6 with script ABI intact and LZO disabled.

Offline: requires a verified source archive plus Meson/Ninja and existing build
libraries. Never installs into Homebrew. Run before package-macos.py.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import subprocess
import tarfile

VERSION = "1.18.6"
SOURCE_SHA256 = "1c767308174337a74694da0f3ec069c271452163a1ef4540964c50c301f157d4"
LIBRARIES = ("libcairo.2.dylib", "libcairo-gobject.2.dylib", "libcairo-script-interpreter.2.dylib")
OPTIONS = ["--default-library=shared", "--buildtype=release", "-Dlzo=disabled", "-Dtests=disabled",
           "-Dfontconfig=enabled", "-Dfreetype=enabled", "-Dpng=enabled", "-Dglib=enabled",
           "-Dxcb=enabled", "-Dxlib=enabled", "-Dzlib=enabled", "-Dquartz=enabled"]


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def extract_source(archive, destination):
    if digest(archive) != SOURCE_SHA256:
        raise ValueError("Cairo source SHA-256 mismatch")
    destination.mkdir(parents=True)
    with tarfile.open(archive) as source:
        total = 0
        members = source.getmembers()
        for member in members:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts or not relative.parts or relative.parts[0] != "cairo-" + VERSION:
                raise ValueError("Unsafe source archive path")
            if not member.isfile() and not member.isdir():
                raise ValueError("Source archive links/special entries are not supported")
            total += member.size
            if member.size > 128 * 1024 * 1024 or total > 1024 * 1024 * 1024:
                raise ValueError("Cairo source extraction size limit exceeded")
        source.extractall(destination, members=members, filter="data")
    return destination / ("cairo-" + VERSION)


def output(*args):
    return subprocess.check_output([str(a) for a in args], text=True)


def exported_symbols(path):
    return {line.split()[-1] for line in output("nm", "-gU", path).splitlines() if line.split()}


def audit(prefix, reference):
    result = {}
    for name in LIBRARIES:
        path = prefix / "lib" / name
        if output("lipo", "-archs", path).strip() != "arm64":
            raise ValueError("Cairo override must be arm64")
        dependencies = output("otool", "-L", path)
        if "liblzo" in dependencies.lower():
            raise ValueError(f"LZO survived Cairo rebuild: {name}")
        original = exported_symbols(reference / "lib" / name)
        rebuilt = exported_symbols(path)
        missing = original - rebuilt
        if missing:
            raise ValueError(f"Cairo ABI lost exported symbols: {name}: {sorted(missing)}")
        result[name] = {"original_symbols": len(original), "rebuilt_symbols": len(rebuilt),
                        "missing_symbols": [], "sha256": digest(path), "dependencies": dependencies.splitlines()[1:]}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path)
    parser.add_argument("--output", type=Path, required=True)
    parser.add_argument("--audit-only", action="store_true", help="Audit a completed build without recompiling")
    parser.add_argument("--brew-prefix", type=Path, default=Path("/opt/homebrew"))
    args = parser.parse_args()
    root = args.output.resolve()
    if (args.brew_prefix / "opt/cairo").resolve().name != VERSION:
        parser.error("This audited recipe requires installed Cairo 1.18.6 as ABI/source reference; review another version explicitly")
    stage, build = root / "prefix", root / "build"
    if args.audit_only:
        if digest(args.archive) != SOURCE_SHA256:
            raise ValueError("Cairo source SHA-256 mismatch")
        options = json.loads((build / "meson-info/intro-buildoptions.json").read_text())
        actual = {o["name"]: o["value"] for o in options}
        for option in OPTIONS:
            if option.startswith("-D"):
                key, value = option[2:].split("=", 1)
                if actual.get(key) != value:
                    raise ValueError(f"Cairo build option mismatch: {key}")
    else:
        if root.exists():
            parser.error("Choose a new output directory; never overwrite a build")
        source = extract_source(args.archive, root / "source")
        subprocess.check_call(["meson", "setup", str(build), str(source), "--prefix=" + str(stage), "--libdir=lib", *OPTIONS])
        subprocess.check_call(["meson", "compile", "-C", str(build)])
        subprocess.check_call(["meson", "install", "-C", str(build), "--no-rebuild"])
    report = {"version": VERSION, "source_sha256": SOURCE_SHA256, "options": OPTIONS,
              "script_interpreter_preserved": True, "lzo": "disabled",
              "libraries": audit(stage, args.brew_prefix / "opt/cairo")}
    (stage / "cairo-build.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Cairo override verified: {stage}")


if __name__ == "__main__":
    main()
