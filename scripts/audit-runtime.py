#!/usr/bin/env python3
"""Read the complete known Mach-O closure; reject unknowns without producing an app."""
import argparse
import hashlib
import importlib.util
import json
import os
from pathlib import Path
import stat
import subprocess
import sys

spec = importlib.util.spec_from_file_location("packager_audit", Path(__file__).with_name("package-macos.py"))
pack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pack)
MAX_NODES, MAX_EDGES, MAX_EVIDENCE = 2048, 16384, 1024 * 1024
MAX_TOTAL_EVIDENCE = 16 * 1024 * 1024


def evidence(keg):
    """Read only bounded regular metadata inside this resolved installed keg."""
    result = {"keg": str(keg), "formula": keg.parent.name, "keg_version": keg.name}
    for key, path in (("recipe", keg / ".brew" / (keg.parent.name + ".rb")),
                      ("receipt", keg / "INSTALL_RECEIPT.json")):
        if path.is_symlink() or not path.resolve().is_relative_to(keg):
            raise ValueError(f"Escaping/symlink evidence: {path}")
        with os.fdopen(os.open(path, os.O_RDONLY | os.O_NOFOLLOW | os.O_NONBLOCK), "rb") as stream:
            if not stat.S_ISREG(os.fstat(stream.fileno()).st_mode):
                raise ValueError(f"Nonregular evidence: {path}")
            data = stream.read(MAX_EVIDENCE + 1)
        if len(data) > MAX_EVIDENCE:
            raise ValueError(f"Oversize evidence: {path}")
        result[key] = {"path": str(path), "sha256": hashlib.sha256(data).hexdigest(), "text": data.decode()}
    receipt = json.loads(result["receipt"]["text"])
    if not isinstance(receipt, dict):
        raise ValueError(f"Invalid receipt object: {keg}")
    source = receipt.get("source", {})
    if not isinstance(source, dict):
        raise ValueError(f"Invalid receipt source object: {keg}")
    versions = source.get("versions", {})
    if not isinstance(versions, dict):
        raise ValueError(f"Invalid receipt versions object: {keg}")
    result["source_version"] = versions.get("stable")
    return result


def audit(binary, prefix, cairo_prefix):
    prefix, binary, cairo_prefix = prefix.resolve(), binary.resolve(), cairo_prefix.resolve()
    result = {"diagnostic_only": True, "distributable": False, "nodes": [], "edges": [],
              "kegs": [], "rejections": [], "errors": []}
    reference = pack.read_reference(prefix)
    report = json.loads((cairo_prefix / "cairo-build.json").read_text())
    pack.verify_build_report(report, reference)
    result["cairo_reference"] = reference
    pending = [binary, *(p for p, _ in pack.runtime_binary_seeds(prefix))]
    seen, kegs = set(), set()
    while pending:
        source = pending.pop(0).resolve()
        original = source
        try:
            if source.name in reference["library_sha256"]:
                original = Path(reference["keg_path"]) / "lib" / source.name
                source = (cairo_prefix / "lib" / source.name).resolve()
                if pack.sha256(source) != report["libraries"][source.name]["sha256"]:
                    raise ValueError("Cairo override changed after ABI audit")
            if source in seen:
                continue
            if len(seen) >= MAX_NODES:
                raise ValueError("Diagnostic node limit reached; graph is incomplete")
            seen.add(source)
            if original != binary:
                try:
                    pack.runtime_keg(original, prefix)
                except ValueError as error:
                    rejection = {"path": str(source), "installed_path": str(original), "reason": str(error)}
                    if original.is_relative_to(prefix / "Cellar"):
                        relative = original.relative_to(prefix / "Cellar")
                        if len(relative.parts) >= 2:
                            rejection.update(formula=relative.parts[0], keg_version=relative.parts[1])
                    result["rejections"].append(rejection)
                if not original.is_relative_to(prefix / "Cellar"):
                    raise ValueError("Dependency outside Homebrew Cellar is not inspected")
                if original.is_relative_to(prefix / "Cellar"):
                    relative = original.relative_to(prefix / "Cellar")
                    if len(relative.parts) >= 3:
                        kegs.add(prefix / "Cellar" / relative.parts[0] / relative.parts[1])
            if any(token in source.name.lower() for token in ("lzo", "glycin")):
                result["rejections"].append({"path": str(source), "reason": "Forbidden/unsupported LZO or Glycin runtime"})
            rpaths, minimum = pack.load_commands(pack.run("otool", "-l", source))
            result["nodes"].append({"path": str(source), "installed_path": str(original), "sha256": pack.sha256(source),
                                    "architecture": pack.run("lipo", "-archs", source), "minimum_macos": minimum})
            for name in pack.loads(pack.run("otool", "-L", source)):
                if len(result["edges"]) >= MAX_EDGES:
                    raise ValueError("Diagnostic edge limit reached; graph is incomplete")
                edge = {"parent": str(source), "import": name}
                try:
                    dependency = pack.resolve_load(name, source, binary, rpaths)
                    if dependency is None:
                        continue
                    edge["installed_path"] = str(dependency)
                    effective = cairo_prefix / "lib" / dependency.name if dependency.name in reference["library_sha256"] else dependency
                    edge["resolved_path"] = str(effective.resolve())
                    if effective.resolve() == source:
                        continue  # LC_ID_DYLIB is not a dependency edge.
                    pending.append(dependency)
                except (ValueError, OSError) as error:
                    edge["error"] = str(error)
                    result["errors"].append(edge)
                result["edges"].append(edge)
        except (ValueError, OSError, subprocess.CalledProcessError) as error:
            result["errors"].append({"path": str(source), "error": str(error)})
            if len(seen) >= MAX_NODES or len(result["edges"]) >= MAX_EDGES:
                break
    total_evidence = 0
    for keg in sorted(kegs):
        try:
            item = evidence(keg)
            total_evidence += sum(len(item[key]["text"].encode()) for key in ("recipe", "receipt"))
            if total_evidence > MAX_TOTAL_EVIDENCE:
                raise ValueError("Diagnostic total evidence limit reached")
            result["kegs"].append(item)
        except (ValueError, OSError, UnicodeError) as error:
            result["errors"].append({"keg": str(keg), "error": str(error)})
    for item in result["rejections"]:
        item["incoming_edges"] = [edge for edge in result["edges"] if edge.get("resolved_path") == item["path"]]
    try:
        pack.require_reference(reference, prefix)
    except (ValueError, OSError) as error:
        result["errors"].append({"error": str(error)})
    result["complete"] = not result["errors"]
    return result


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, required=True, help="Fresh diagnostic directory (JSON only)")
    parser.add_argument("--brew-prefix", type=Path, default=Path("/opt/homebrew"))
    parser.add_argument("--cairo-prefix", type=Path, required=True)
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("The diagnostic requires macOS; synthetic tests are portable")
    if args.output.exists() or args.output.is_symlink():
        parser.error("Choose a fresh diagnostic directory")
    args.output.mkdir(parents=True)
    try:
        result = audit(pack.ROOT / "target/release/gliff", args.brew_prefix, args.cairo_prefix)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        result = {"diagnostic_only": True, "distributable": False, "complete": False, "errors": [str(error)]}
    with (args.output / "runtime-closure.json").open("x") as stream:
        json.dump(result, stream, indent=2)
        stream.write("\n")
    print(f"Diagnostic report: {args.output / 'runtime-closure.json'}")
    if result.get("rejections") or result["errors"]:
        parser.exit(1, "Runtime diagnostic rejected: inspect all rejections/errors; no app was produced\n")


if __name__ == "__main__":
    main()
