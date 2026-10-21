#!/usr/bin/env python3
"""Build the exact installed Cairo source with script ABI intact and LZO disabled.

Offline: requires a verified source archive plus Meson/Ninja and existing build
libraries. Never installs into Homebrew. Run before package-macos.py.
"""
import argparse
import hashlib
import json
from pathlib import Path
import subprocess
import tarfile

from cairo_reference import LIBRARIES, read_reference, require_reference, source_lock, source_filename, verify_build_report
OPTIONS = ["--default-library=shared", "--buildtype=release", "-Dlzo=disabled", "-Dtests=disabled",
           "-Dfontconfig=enabled", "-Dfreetype=enabled", "-Dpng=enabled", "-Dglib=enabled",
           "-Dxcb=enabled", "-Dxlib=enabled", "-Dzlib=enabled", "-Dquartz=enabled"]


def digest(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def extract_source(archive, destination, reference):
    version = reference["version"]
    if digest(archive) != reference["source"]["sha256"]:
        raise ValueError("Cairo source SHA-256 mismatch")
    destination.mkdir(parents=True)
    with tarfile.open(archive) as source:
        total = 0
        members = source.getmembers()
        for member in members:
            relative = Path(member.name)
            if relative.is_absolute() or ".." in relative.parts or not relative.parts or relative.parts[0] != "cairo-" + version:
                raise ValueError("Unsafe source archive path")
            if not member.isfile() and not member.isdir():
                raise ValueError("Source archive links/special entries are not supported")
            total += member.size
            if member.size > 128 * 1024 * 1024 or total > 1024 * 1024 * 1024:
                raise ValueError("Cairo source extraction size limit exceeded")
        source.extractall(destination, members=members, filter="data")
    return destination / ("cairo-" + version)


def output(*args):
    return subprocess.check_output([str(a) for a in args], text=True)


def exported_symbols(path):
    return {line.split()[-1] for line in output("nm", "-gU", path).splitlines() if line.split()}


def audit(prefix, reference):
    result = {}
    for name in LIBRARIES:
        path = prefix / "lib" / name
        if any(output("lipo", "-archs", item).strip() != "arm64" for item in (path, reference / "lib" / name)):
            raise ValueError("Cairo override must be arm64")
        dependencies = output("otool", "-L", path)
        if "liblzo" in dependencies.lower():
            raise ValueError(f"LZO survived Cairo rebuild: {name}")
        original = exported_symbols(reference / "lib" / name)
        rebuilt = exported_symbols(path)
        missing = original - rebuilt
        if not original:
            raise ValueError(f"Cairo reference has no exported symbols: {name}")
        if missing:
            raise ValueError(f"Cairo ABI lost exported symbols: {name}: {sorted(missing)}")
        result[name] = {"original_symbols": len(original), "rebuilt_symbols": len(rebuilt),
                        "missing_symbols": [], "architecture": "arm64", "sha256": digest(path), "reference_sha256": digest(reference / "lib" / name), "dependencies": dependencies.splitlines()[1:]}
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("archive", type=Path, nargs="?")
    parser.add_argument("--source-dir", type=Path, help="Directory containing the exact archive named by the reference lock")
    parser.add_argument("--reference-lock", type=Path, help="Previously planned installed-reference snapshot")
    parser.add_argument("--plan-source", type=Path, help="Write the selected installed-reference lock; no network/build")
    parser.add_argument("--output", type=Path)
    parser.add_argument("--audit-only", action="store_true", help="Audit a completed build without recompiling")
    parser.add_argument("--brew-prefix", type=Path, default=Path("/opt/homebrew"))
    args = parser.parse_args()
    reference = read_reference(args.brew_prefix)
    if args.plan_source:
        args.plan_source.parent.mkdir(parents=True, exist_ok=True)
        with args.plan_source.open("x") as stream:
            stream.write(json.dumps(source_lock(reference), indent=2) + "\n")
        print(json.dumps(reference, indent=2))
        return
    if args.reference_lock:
        locked = json.loads(args.reference_lock.read_text())
        if locked != source_lock(reference):
            raise ValueError("Cairo reference lock differs from the installed reference")
    if not args.output:
        parser.error("--output is required for build/audit")
    archive = args.archive or (args.source_dir / source_filename(reference) if args.source_dir else None)
    if not archive:
        parser.error("provide --source-dir or an archive")
    root = args.output.resolve()
    stage, build = root / "prefix", root / "build"
    if args.audit_only:
        verify_build_report(json.loads((stage / "cairo-build.json").read_text()), reference)
        if digest(archive) != reference["source"]["sha256"]:
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
        source = extract_source(archive, root / "source", reference)
        subprocess.check_call(["meson", "setup", str(build), str(source), "--prefix=" + str(stage), "--libdir=lib", *OPTIONS])
        subprocess.check_call(["meson", "compile", "-C", str(build)])
        subprocess.check_call(["meson", "install", "-C", str(build), "--no-rebuild"])
    require_reference(reference, args.brew_prefix)
    report = {"version": reference["version"], "source_sha256": reference["source"]["sha256"], "reference": reference, "options": OPTIONS,
              "script_interpreter_preserved": True, "lzo": "disabled",
              "libraries": audit(stage, args.brew_prefix / "opt/cairo")}
    verify_build_report(report, reference)
    (stage / "cairo-build.json").write_text(json.dumps(report, indent=2) + "\n")
    print(f"Cairo override verified: {stage}")


if __name__ == "__main__":
    main()
