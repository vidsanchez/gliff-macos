#!/usr/bin/env python3
"""Check declared MSRV against locked arm64 target dependency metadata.

This static check does not prove compiler compatibility; CI separately runs
cargo +1.92.0 check. Does not install a toolchain or download dependencies.
"""
import json
from pathlib import Path
import subprocess
import tomllib

ROOT = Path(__file__).resolve().parents[1]


def version(value):
    return tuple((list(map(int, value.split("."))) + [0, 0])[:3])


def check(metadata, declared):
    workspace = set(metadata["workspace_members"])
    resolved = {node["id"] for node in metadata["resolve"]["nodes"]}
    dependencies = [p for p in metadata["packages"] if p["id"] in resolved and p["id"] not in workspace and p.get("rust_version")]
    maximum = max((p["rust_version"] for p in dependencies), key=version)
    if version(declared) < version(maximum):
        raise ValueError(f"Declared Rust {declared} is below locked dependency requirement {maximum}")
    return maximum


def main():
    declared = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["rust-version"]
    metadata = json.loads(subprocess.check_output(["cargo", "metadata", "--offline", "--locked",
        "--filter-platform", "aarch64-apple-darwin", "--format-version", "1"], cwd=ROOT, text=True))
    print(f"Declared Rust {declared}; locked target dependency metadata maximum {check(metadata, declared)}")


if __name__ == "__main__":
    main()
