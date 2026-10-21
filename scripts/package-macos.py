#!/usr/bin/env python3
"""Build an experimental arm64 .app from local dependencies; never downloads.

All subprocesses use argv lists, fail closed and never request TCC or open GUI.
Run test_package_macos.py on any OS; packaging requires Apple command line tools.
"""
import argparse
import hashlib
import json
import os
from pathlib import Path
import plistlib
import re
import shutil
import subprocess
import sys
import tomllib
import tarfile
import zipfile
from cairo_reference import read_reference, require_reference, verify_build_report

ROOT = Path(__file__).resolve().parents[1]
APPLE = ("/System/Library/", "/usr/lib/")
MACH_MAGIC = {b"\xcf\xfa\xed\xfe", b"\xfe\xed\xfa\xcf", b"\xca\xfe\xba\xbe", b"\xbe\xba\xfe\xca"}
# Audited runtime formula families; a new transitive formula needs review instead
# of inheriting a licensing claim. Formula-level GPL gettext contains LGPL libintl.
APPROVED_FORMULAS = {"brotli", "cairo", "fontconfig", "freetype", "fribidi", "gdk-pixbuf",
    "gettext", "glib", "graphene", "graphite2", "gtk4", "harfbuzz", "jpeg-turbo", "libadwaita",
    "libdatrie", "libepoxy", "libpng", "librsvg", "libthai", "libtiff", "libx11", "libxau",
    "libxcb", "libxdmcp", "libxext", "libxrender", "pango", "pcre2", "pixman", "xz", "zstd"}
# These newly linked families are audited only at the listed source version and
# resolved library filename, not as blanket permission to redistribute tools.
AUDITED_RUNTIME_COMPONENTS = {
    "appstream": ("1.2.0", "libappstream.5.dylib"),
    "libfyaml": ("0.9.6", "libfyaml.0.9.6.dylib"),
    "libxmlb": ("0.3.29", "libxmlb.2.dylib"),
}
LICENSE_RE = re.compile(r"^(copying|licen[sc]e|copyright|notice|authors)([._-].*)?$", re.I)


def run(*args, cwd=None):
    return subprocess.check_output([str(a) for a in args], text=True, cwd=cwd).strip()


def sha256(path):
    with path.open("rb") as stream:
        return hashlib.file_digest(stream, "sha256").hexdigest()


def macho(path):
    if not path.is_file():
        return False
    with path.open("rb") as stream:
        return stream.read(4) in MACH_MAGIC


def loads(output):
    return [line.strip().split(" (compatibility version", 1)[0]
            for line in output.splitlines()[1:] if line.strip()]


def load_commands(output):
    rpaths, minimums = [], []
    command = None
    for line in output.splitlines():
        words = line.split()
        if words[:1] == ["cmd"]:
            command = words[1]
        elif command == "LC_RPATH" and words[:1] == ["path"]:
            rpaths.append(line.strip()[5:].rsplit(" (offset ", 1)[0])
        elif command == "LC_BUILD_VERSION" and words[:1] == ["minos"]:
            minimums.append(words[1])
        elif command == "LC_VERSION_MIN_MACOSX" and words[:1] == ["version"]:
            minimums.append(words[1])
    if not minimums:
        raise ValueError("Mach-O has no macOS deployment target")
    return rpaths, max(minimums, key=version_tuple)


def version_tuple(value):
    return tuple(int(p) for p in value.split("."))


def safe_tree(root):
    for path in root.rglob("*"):
        if path.is_symlink() and not path.resolve().is_relative_to(root.resolve()):
            raise ValueError(f"Escaping bundle symlink: {path}")


def resolve_load(name, binary, executable, rpaths):
    if name.startswith(APPLE):
        return None
    def expand(value):
        return Path(value.replace("@loader_path", str(binary.parent))
                    .replace("@executable_path", str(executable.parent)))
    if name.startswith("@rpath/"):
        candidates = [expand(p) / name[7:] for p in rpaths]
    elif name.startswith(("@loader_path/", "@executable_path/")):
        candidates = [expand(name)]
    elif name.startswith("/"):
        candidates = [Path(name)]
    else:
        raise ValueError(f"Unresolved install name {name} in {binary}")
    for candidate in candidates:
        if candidate.is_file():
            return candidate.resolve()
    raise ValueError(f"Missing dependency {name} in {binary}")


def source_filename(entry):
    # Hash-prefixed deterministic names; URL paths cannot escape the output dir.
    return entry["sha256"] + "-" + entry["name"] + "-" + entry["version"] + ".source"


def recipe_sources(recipe, name, version):
    text = recipe.read_text()
    # livecheck URLs describe release listings, not source archives.
    text = re.sub(r"^  head do\n.*?^  end\n", "", text, flags=re.M | re.S)
    text = re.sub(r"^  livecheck do\n.*?^  end\n", "", text, flags=re.M | re.S)
    text = re.sub(r"^  test do\n.*?^  end\n", "", text, flags=re.M | re.S)
    entries = []
    # Installed formula recipes retain stable URL+sha as well as patch/resource
    # stanzas. Refuse interpolation or an unpinned remote rather than guessing.
    urls = list(re.finditer(r'^\s*url "([^"\n]+)"([^\n]*)', text, re.M))
    for i, match in enumerate(urls):
        url = match[1]
        end = urls[i + 1].start() if i + 1 < len(urls) else len(text)
        digest = re.search(r'^\s*sha256 "([0-9a-f]{64})"', text[match.end():end], re.M)
        if not url.startswith("https://") or "#{" in url or not digest:
            raise ValueError(f"Source URL requires manual audit (not a pinned HTTPS archive): {recipe}: {url}")
        entries.append({"kind": "homebrew", "name": name, "version": version,
                        "url": url, "sha256": digest[1], "recipe_sha256": sha256(recipe)})
    if not entries:
        raise ValueError(f"No pinned source archives in {recipe}")
    return entries


def copy_archive_licenses(archive, destination):
    # Only read bounded regular license files, never extract archive paths or links.
    if tarfile.is_tarfile(archive):
        with tarfile.open(archive) as source:
            for member in source:
                if member.isfile() and LICENSE_RE.match(Path(member.name).name):
                    if member.size > 5 * 1024 * 1024:
                        raise ValueError(f"Oversize license in {archive}")
                    stream = source.extractfile(member)
                    data = stream.read(5 * 1024 * 1024 + 1)
                    target = destination / (Path(member.name).name + "-" + hashlib.sha256(member.name.encode()).hexdigest()[:12])
                    target.write_bytes(data)
    elif zipfile.is_zipfile(archive):
        with zipfile.ZipFile(archive) as source:
            for member in source.infolist():
                if not member.is_dir() and LICENSE_RE.match(Path(member.filename).name):
                    if member.file_size > 5 * 1024 * 1024:
                        raise ValueError(f"Oversize license in {archive}")
                    target = destination / (Path(member.filename).name + "-" + hashlib.sha256(member.filename.encode()).hexdigest()[:12])
                    target.write_bytes(source.read(member))


def require_crate_license(package, candidates):
    """No missing-license exceptions: each distributed crate needs actual text."""
    if not any(re.match(r"^(copying|licen[sc]e|copyright)", path.name, re.I) for path in candidates):
        raise ValueError(f"Missing license text: {package['name']} {package['version']}")


class Bundle:
    def __init__(self, app, prefix, source_dir=None, cairo_prefix=None):
        self.app = app
        self.prefix = prefix.resolve()
        self.contents = app / "Contents"
        self.resources = self.contents / "Resources"
        self.frameworks = self.contents / "Frameworks"
        self.executable = self.contents / "MacOS/gliff-bin"
        self.mapping = {}
        self.kegs = set()
        self.source_dir = source_dir
        self.cairo_prefix = cairo_prefix.resolve() if cairo_prefix else None
        if not self.cairo_prefix:
            raise ValueError("A verified --cairo-prefix built without LZO is mandatory")
        self.cairo_reference = read_reference(self.prefix)
        self.cairo_report = json.loads((self.cairo_prefix / "cairo-build.json").read_text())
        verify_build_report(self.cairo_report, self.cairo_reference)

    def remember_keg(self, source):
        source = source.resolve()
        cellar = self.prefix / "Cellar"
        if not source.is_relative_to(cellar):
            raise ValueError(f"Non-Apple dependency outside Homebrew Cellar: {source}")
        relative = source.relative_to(cellar)
        if len(relative.parts) < 3:
            raise ValueError(f"Not a formula file: {source}")
        if relative.parts[0] == "gettext" and source.name != "libintl.8.dylib":
            raise ValueError("Only LGPL libintl is audited from gettext; another component needs review")
        if relative.parts[0] == "xz" and not source.name.startswith("liblzma."):
            raise ValueError("Only liblzma is audited from xz; another component needs review")
        family, keg_version = relative.parts[:2]
        if family in AUDITED_RUNTIME_COMPONENTS:
            version, filename = AUDITED_RUNTIME_COMPONENTS[family]
            if (not re.fullmatch(re.escape(version) + r"(?:_[1-9]\d*)?", keg_version)
                    or relative.parts[2:] != ("lib", filename)):
                raise ValueError(f"Unaudited runtime component: {relative.as_posix()}; "
                                 f"only source {version}, lib/{filename} is audited")
        elif family not in APPROVED_FORMULAS:
            raise ValueError(f"Unreviewed runtime formula/license: {family}")
        self.kegs.add(cellar / relative.parts[0] / relative.parts[1])

    def copy_binary(self, source, destination=None):
        source = source.resolve()
        cairo_names = {"libcairo.2.dylib", "libcairo-gobject.2.dylib", "libcairo-script-interpreter.2.dylib"}
        if source.name in cairo_names:
            source = (self.cairo_prefix / "lib" / source.name).resolve()
            expected = self.cairo_report["libraries"][source.name]["sha256"]
            if sha256(source) != expected:
                raise ValueError("Cairo override changed after ABI audit")
        if source in self.mapping:
            return self.mapping[source]
        destination = destination or self.frameworks / source.name
        if destination.exists():
            raise ValueError(f"Binary basename collision: {destination}")
        if source != (ROOT / "target/release/gliff").resolve():
            if source.is_relative_to(self.cairo_prefix):
                self.remember_keg(self.prefix / "opt/cairo/lib" / source.name)
            else:
                self.remember_keg(source)
        destination.parent.mkdir(parents=True, exist_ok=True)
        shutil.copy2(source, destination)
        destination.chmod(destination.stat().st_mode | 0o200)
        self.mapping[source] = destination
        return destination

    def close(self):
        done = set()
        while todo := set(self.mapping) - done:
            for source in sorted(todo):
                target = self.mapping[source]
                rpaths, _ = load_commands(run("otool", "-l", source))
                for name in loads(run("otool", "-L", source)):
                    dependency = resolve_load(name, source, ROOT / "target/release/gliff", rpaths)
                    if dependency is None:
                        continue
                    if dependency == source:  # LC_ID_DYLIB is printed by otool -L.
                        run("install_name_tool", "-id", "@rpath/" + target.name, target)
                        continue
                    copied = self.copy_binary(dependency)
                    relative = os.path.relpath(copied, target.parent)
                    run("install_name_tool", "-change", name, "@loader_path/" + relative, target)
                # Homebrew rpaths must not survive even if all imports relocated.
                for value in rpaths:
                    run("install_name_tool", "-delete_rpath", value, target)
                done.add(source)
        if any("lzo" in path.name.lower() for path in self.mapping):
            raise ValueError("GPL LZO is forbidden in the runtime closure; rebuild Cairo without LZO")
        if any("glycin" in path.name.lower() for path in self.mapping):
            raise ValueError("Glycin runtime found: loader sandbox/config closure is not supported; audit before packaging")

    def copy_data(self, source, destination, required=True):
        if not source.exists():
            if required:
                raise ValueError(f"Missing runtime resource: {source}")
            return
        self.remember_keg(source)
        destination.parent.mkdir(parents=True, exist_ok=True)
        if source.is_dir():
            shutil.copytree(source, destination, dirs_exist_ok=True, symlinks=False)
        else:
            shutil.copy2(source, destination)

    def resources_copy(self):
        # Copy dynamic modules separately so close() audits their own dylib graph.
        for pattern in ("lib/gdk-pixbuf-2.0/*/loaders/*.so", "lib/gdk-pixbuf-2.0/*/loaders/*.dylib",
                        "lib/gio/modules/*.so", "lib/gio/modules/*.dylib",
                        "lib/gtk-4.0/*/printbackends/*.so", "lib/gtk-4.0/*/immodules/*.so"):
            for source in sorted(self.prefix.glob(pattern)):
                self.copy_binary(source, self.resources / source.relative_to(self.prefix))
        self.copy_binary(self.prefix / "bin/gdk-pixbuf-query-loaders",
                         self.contents / "MacOS/gdk-pixbuf-query-loaders")
        schemas = self.resources / "share/glib-2.0/schemas"
        for formula in ("gtk4", "libadwaita"):
            source = self.prefix / "opt" / formula / "share/glib-2.0/schemas"
            self.copy_data(source, schemas, required=formula == "gtk4")
        run(self.prefix / "bin/glib-compile-schemas", schemas)
        # Original minimal metadata, NOT the GPL-licensed hicolor index.theme.
        # No third-party icon theme is copied. GTK supplies built-in symbols.
        theme = self.resources / "share/icons/hicolor"
        (theme / "1024x1024/apps").mkdir(parents=True)
        (theme / "index.theme").write_text("""[Icon Theme]
Name=Gliff fallback
Comment=Original Gliff application artwork
Hidden=true
Directories=1024x1024/apps

[1024x1024/apps]
Size=1024
Context=Applications
Type=Fixed
""")
        shutil.copy2(ROOT / "resources/gliff.png", theme / "1024x1024/apps/gliff.png")
        fonts = self.resources / "etc/fonts"
        fonts.mkdir(parents=True)
        (fonts / "fonts.conf").write_text('''<?xml version="1.0"?>
<!DOCTYPE fontconfig SYSTEM "urn:fontconfig:fonts.dtd">
<fontconfig>
<dir>/System/Library/Fonts</dir><dir>/Library/Fonts</dir><dir prefix="xdg">fonts</dir>
<cachedir prefix="xdg">fontconfig</cachedir>
<alias><family>sans-serif</family><prefer><family>Helvetica</family></prefer></alias>
<alias><family>serif</family><prefer><family>Times</family></prefer></alias>
<alias><family>monospace</family><prefer><family>Menlo</family></prefer></alias>
</fontconfig>
''')
        (self.resources / "lib/gio/modules").mkdir(parents=True, exist_ok=True)
        (self.resources / "lib/gtk-4.0").mkdir(parents=True, exist_ok=True)
        self.close()

    def source_plan(self):
        require_reference(self.cairo_reference, self.prefix)
        entries = []
        for keg in sorted(self.kegs):
            recipe = keg / ".brew" / (keg.parent.name + ".rb")
            if keg.parent.name == "cairo":
                if keg.name != self.cairo_reference["keg_version"]:
                    raise ValueError("Cairo source inventory differs from the rebuilt reference")
                entries.append(self.cairo_reference["source"])
            else:
                entries.extend(recipe_sources(recipe, keg.parent.name, keg.name))
        lock = tomllib.loads((ROOT / "Cargo.lock").read_text())
        for package in lock["package"]:
            if package.get("source", "").startswith("registry+"):
                entries.append({"name": package["name"], "version": package["version"], "kind": "cargo",
                    "url": f"https://static.crates.io/crates/{package['name']}/{package['name']}-{package['version']}.crate",
                    "sha256": package["checksum"]})
        return {"format": 1, "cairo_reference": self.cairo_reference, "sources": entries}

    def licenses(self):
        dest = self.resources / "licenses"
        dest.mkdir()
        inventory = {"homebrew": [], "cargo": []}
        sources = self.source_plan()["sources"]
        for keg in sorted(self.kegs):
            formula = keg.parent.name
            directory = dest / "homebrew" / formula
            directory.mkdir(parents=True)
            candidates = [p for p in keg.rglob("*") if p.is_file() and LICENSE_RE.match(p.name)
                          and not any(x in p.parts for x in ("include", "lib", "bin"))]
            # Source trees may have per-component licenses absent from the
            # bottle root (notably GLib and gettext's LGPL libintl).
            if self.source_dir:
                entries = [entry for entry in sources if entry["kind"] == "homebrew" and entry["name"] == formula]
                for entry in entries:
                    archive = self.source_dir / source_filename(entry)
                    if not archive.is_file() or sha256(archive) != entry["sha256"]:
                        raise ValueError(f"Missing/hash-mismatched source archive: {archive}")
                    copy_archive_licenses(archive, directory)
            has_bottle_text = any(re.match(r"^(copying|licen[sc]e|copyright)", p.name, re.I) for p in candidates)
            has_source_text = any(p.is_file() and re.match(r"^(copying|licen[sc]e|copyright)", p.name, re.I)
                                  for p in directory.rglob("*"))
            if not (has_bottle_text or has_source_text):
                raise ValueError(f"Missing license text for {keg}; fetch its exact source archives")
            for source in candidates:
                relative = source.relative_to(keg)
                target = directory / relative
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(source, target)
            recipe = keg / ".brew" / (formula + ".rb")
            if not recipe.exists():
                raise ValueError(f"Missing source recipe: {keg}")
            shutil.copy2(recipe, directory)
            receipt = keg / "INSTALL_RECEIPT.json"
            shutil.copy2(receipt, directory)
            sbom = keg / "sbom.spdx.json"
            if sbom.exists():
                shutil.copy2(sbom, directory)
            source_urls = re.findall(r'^\s*url "([^"]+)"', recipe.read_text(), re.M)
            if not source_urls:
                raise ValueError(f"Missing corresponding source URL: {recipe}")
            inventory["homebrew"].append({"name": formula, "version": keg.name,
                "source_urls": source_urls, "recipe_sha256": sha256(recipe),
                "source_archive_distribution_gate": "pending: retain and publish corresponding source archives"})
            if formula == "cairo":
                inventory["homebrew"][-1].update({"source_version": self.cairo_reference["version"],
                    "source_urls": [self.cairo_reference["source"]["url"]],
                    "source_sha256": self.cairo_reference["source"]["sha256"]})
        metadata = json.loads(run("cargo", "metadata", "--offline", "--locked", "--filter-platform", "aarch64-apple-darwin", "--format-version", "1", cwd=ROOT))
        for package in metadata["packages"]:
            source = Path(package["manifest_path"]).parent
            if source.is_relative_to(ROOT):
                continue
            directory = dest / "cargo" / (package["name"] + "-" + package["version"])
            candidates = [p for p in source.rglob("*") if p.is_file() and LICENSE_RE.match(p.name)]
            directory.mkdir(parents=True)
            require_crate_license(package, candidates)
            for path in candidates:
                target = directory / path.relative_to(source)
                target.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy2(path, target)
            inventory["cargo"].append({"name": package["name"], "version": package["version"],
                "license": package.get("license"), "repository": package.get("repository")})
        for name in ("build-cairo.py", "cairo_reference.py", "cairo-source.json"):
            shutil.copy2(ROOT / "scripts" / name, dest / name)
        shutil.copy2(self.cairo_prefix / "cairo-build.json", dest / "cairo-build.json")
        for name in ("LICENSE", "NOTICE", "ATTRIBUTION.md", "Cargo.lock"):
            shutil.copy2(ROOT / name, dest)
        require_reference(self.cairo_reference, self.prefix)
        (dest / "third-party.json").write_text(json.dumps(inventory, indent=2) + "\n")
        return inventory


def validate(app):
    safe_tree(app)
    executable = app / "Contents/MacOS/gliff-bin"
    report = []
    for path in sorted(app.rglob("*")):
        if not macho(path):
            continue
        if "lzo" in path.name.lower() or "glycin" in path.name.lower():
            raise ValueError(f"Unsupported/GPL runtime library: {path}")
        arches = run("lipo", "-archs", path).split()
        if arches != ["arm64"]:
            raise ValueError(f"Expected arm64 only, got {arches}: {path}")
        rpaths, minimum = load_commands(run("otool", "-l", path))
        if any(not p.startswith(("@loader_path/", "@executable_path/")) for p in rpaths):
            raise ValueError(f"Non-relative rpath: {path}: {rpaths}")
        for name in loads(run("otool", "-L", path)):
            if name.startswith(APPLE):
                continue
            if name.startswith("/"):
                raise ValueError(f"External absolute install name: {path}: {name}")
            # Library IDs are @rpath/basename; imports must actually resolve.
            if name == "@rpath/" + path.name:
                continue
            target = resolve_load(name, path, executable, rpaths)
            if target is not None and not target.is_relative_to(app.resolve()):
                raise ValueError(f"Dependency escapes bundle: {name}")
        report.append({"path": str(path.relative_to(app)), "architecture": "arm64", "minimum_macos": minimum})
    if not report:
        raise ValueError("Bundle has no Mach-O binaries")
    required = ("Contents/Info.plist", "Contents/MacOS/Gliff", "Contents/MacOS/gliff-bin",
                "Contents/MacOS/gdk-pixbuf-query-loaders", "Contents/Resources/Gliff.icns",
                "Contents/Resources/share/glib-2.0/schemas/gschemas.compiled",
                "Contents/Resources/share/icons/hicolor/index.theme", "Contents/Resources/etc/fonts/fonts.conf",
                "Contents/Resources/licenses/third-party.json")
    for relative in required:
        if not (app / relative).is_file():
            raise ValueError(f"Incomplete resource closure: {relative}")
    return {"architecture": "arm64", "minimum_macos": max((r["minimum_macos"] for r in report), key=version_tuple),
            "mach_o": report, "clean_machine_qa": "pending", "corresponding_sources_gate": "pending",
            "signing": "ad-hoc; no Developer ID; not notarized", "experimental": True}


def icon(resources):
    iconset = resources / "Gliff.iconset"
    iconset.mkdir()
    for size in (16, 32, 128, 256, 512):
        for factor in (1, 2):
            suffix = "@2x" if factor == 2 else ""
            target = iconset / f"icon_{size}x{size}{suffix}.png"
            run("sips", "-z", size * factor, size * factor, ROOT / "resources/gliff.png", "--out", target)
    run("iconutil", "-c", "icns", iconset, "-o", resources / "Gliff.icns")
    shutil.rmtree(iconset)


def package(output, prefix, source_dir=None, plan_only=False, cairo_prefix=None):
    version = tomllib.loads((ROOT / "Cargo.toml").read_text())["workspace"]["package"]["version"]
    if output.exists():
        raise ValueError(f"Output already exists; choose a fresh directory: {output}")
    output.mkdir(parents=True)
    app = output / "Gliff.app"
    bundle = Bundle(app, prefix, source_dir, cairo_prefix)
    bundle.copy_binary(ROOT / "target/release/gliff", bundle.executable)
    bundle.resources_copy()
    source_lock = bundle.source_plan()
    (output / "source-lock.json").write_text(json.dumps(source_lock, indent=2) + "\n")
    if plan_only:
        print(f"Source plan: {output / 'source-lock.json'} ({len(source_lock['sources'])} archives; network not used)")
        return
    inventory = bundle.licenses()
    run("clang", "-arch", "arm64", "-mmacosx-version-min=11.0", "-Wall", "-Wextra", "-Werror",
        '-DGLIFF_VERSION="' + version + '"', ROOT / "scripts/launcher.c", "-o", bundle.contents / "MacOS/Gliff")
    icon(bundle.resources)
    # Compute deployment targets before creating plist; this includes helpers,
    # loaders and every transitive dylib, not only the Rust executable.
    minimum = max((load_commands(run("otool", "-l", p))[1] for p in app.rglob("*") if macho(p)), key=version_tuple)
    info = {"CFBundleIdentifier": "io.github.vidsanchez.gliff-macos", "CFBundleName": "Gliff",
            "CFBundleDisplayName": "Gliff", "CFBundleExecutable": "Gliff", "CFBundlePackageType": "APPL",
            "CFBundleIconFile": "Gliff.icns", "CFBundleShortVersionString": version.split("-")[0],
            "CFBundleVersion": "1", "GliffReleaseVersion": version, "LSMinimumSystemVersion": minimum,
            "NSHighResolutionCapable": True}
    (bundle.contents / "Info.plist").write_bytes(plistlib.dumps(info))
    report = validate(app)
    report.update({"version": version, "homebrew": inventory["homebrew"],
                   "cairo_override": bundle.cairo_report, "rustc": run("rustc", "--version")})
    (bundle.resources / "build-report.json").write_text(json.dumps(report, indent=2) + "\n")
    for path in sorted((p for p in app.rglob("*") if macho(p)), key=lambda p: len(p.parts), reverse=True):
        run("codesign", "--force", "--sign", "-", "--timestamp=none", path)
    run("codesign", "--force", "--sign", "-", "--timestamp=none", app)
    run("codesign", "--verify", "--deep", "--strict", app)
    archive = output / f"Gliff-{version}-arm64.zip"
    run("ditto", "-c", "-k", "--sequesterRsrc", "--keepParent", app, archive)
    report["archive_sha256"] = sha256(archive)
    report["tag"] = "v" + version
    report["archive_url"] = f"https://github.com/vidsanchez/gliff-macos/releases/download/v{version}/{archive.name}"
    (output / "build-report.json").write_text(json.dumps(report, indent=2) + "\n")
    (output / "SHA256SUMS").write_text(f"{sha256(archive)}  {archive.name}\n")
    print(json.dumps({"archive": str(archive), "minimum_macos": minimum,
                      "clean_machine_qa": "pending", "corresponding_sources_gate": "pending"}, indent=2))


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--output", type=Path, default=ROOT / "dist")
    parser.add_argument("--brew-prefix", type=Path, default=Path("/opt/homebrew"))
    parser.add_argument("--cairo-prefix", type=Path, help="Prefix emitted by build-cairo.py (mandatory; avoids GPL LZO)")
    parser.add_argument("--source-dir", type=Path, help="Exact archives fetched from source-lock.json (for absent bottle license texts)")
    parser.add_argument("--plan-sources", action="store_true", help="Assemble dependency graph and write exact source lock; no downloads, signing or ZIP")
    parser.add_argument("--validate", type=Path, help="Audit an already assembled app, without launching it")
    args = parser.parse_args()
    if sys.platform != "darwin":
        parser.error("Packaging/validation requires macOS (fixture tests are portable)")
    try:
        if args.validate:
            print(json.dumps(validate(args.validate.resolve()), indent=2))
        else:
            package(args.output.resolve(), args.brew_prefix, args.source_dir, args.plan_sources, args.cairo_prefix)
    except (ValueError, OSError, subprocess.CalledProcessError) as error:
        parser.exit(1, f"Packaging failed closed: {error}\n")


if __name__ == "__main__":
    main()
