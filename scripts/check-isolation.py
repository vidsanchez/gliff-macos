"""Static workspace-boundary checks; not a compiler or dependency resolver."""
from pathlib import Path
import json
import subprocess
import tomllib

root = Path(__file__).resolve().parents[1]
expected = {"gliff", "gliff-proto", "gliff-transport", "gliff-sw"}
# Repository metadata and generated artifacts are not source. Do not traverse
# them: a standalone .git or a dylib symlink in an ignored bundle is legitimate.
import os
ignored = {".git", ".codegraph", "target", "dist", "__pycache__"}
for base, dirs, files in os.walk(root, followlinks=False):
    dirs[:] = [name for name in dirs if name not in ignored]
    for name in [*dirs, *files]:
        path = Path(base) / name
        if path.is_symlink():
            raise SystemExit(f"Unexpected source symlink: {path}")
metadata = json.loads(subprocess.check_output(
    ["cargo", "metadata", "--offline", "--no-deps", "--format-version", "1"],
    cwd=root, text=True,
))
assert Path(metadata["workspace_root"]).resolve() == root
assert {p["name"] for p in metadata["packages"]} == expected
assert len(metadata["workspace_members"]) == len(expected)
for package in metadata["packages"]:
    assert Path(package["manifest_path"]).resolve().is_relative_to(root)
    for dependency in package["dependencies"]:
        path = dependency.get("path")
        if path:
            assert Path(path).resolve().is_relative_to(root), dependency
for manifest in [root / "Cargo.toml", *root.glob("crates/*/Cargo.toml")]:
    data = tomllib.loads(manifest.read_text())
    sections = [data.get("dependencies", {}), data.get("dev-dependencies", {}),
                data.get("build-dependencies", {}),
                data.get("workspace", {}).get("dependencies", {})]
    for section in sections:
        for name, spec in section.items():
            assert not name.startswith(("hypr-", "wayland-", "gliff-va", "gliff-vk")), name
            assert name not in {"gbm", "drm", "drm-fourcc", "xkbcommon", "dbus"}, name
            if isinstance(spec, dict) and "path" in spec:
                assert (manifest.parent / spec["path"]).resolve().is_relative_to(root)
for excluded in ["pkgbuild"]:
    assert not (root / excluded).exists(), excluded
print("OK: independent workspace; 4 crates; all local dependencies remain inside this checkout")
