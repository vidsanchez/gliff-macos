"""Portable synthetic packaging tests; no GUI, real clipboard, network or TCC."""
import importlib.util
import io
import json
from pathlib import Path
import subprocess
import tarfile
import tempfile
import unittest
from unittest.mock import patch


def module(name, filename):
    spec = importlib.util.spec_from_file_location(name, Path(__file__).with_name(filename))
    result = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(result)
    return result


pack = module("package_macos", "package-macos.py")
fetch = module("fetch_sources", "fetch-sources.py")
cairo = module("build_cairo", "build-cairo.py")
COMMANDS = '''Load command 0
          cmd LC_BUILD_VERSION
        minos 15.2
Load command 1
          cmd LC_RPATH
         path @loader_path/../Frameworks (offset 12)
'''


class PackagerTests(unittest.TestCase):
    def test_loads_preserve_spaces_and_ignore_header(self):
        self.assertEqual(pack.loads("/app with space/gliff:\n\t@loader_path/lib a.dylib (compatibility version 1.0.0, current version 2.0.0)\n"), ["@loader_path/lib a.dylib"])

    def test_all_minimum_versions_and_rpaths(self):
        paths, minimum = pack.load_commands(COMMANDS + "cmd LC_VERSION_MIN_MACOSX\nversion 27.0\n")
        self.assertEqual(paths, ["@loader_path/../Frameworks"])
        self.assertEqual(minimum, "27.0")
        self.assertGreater(pack.version_tuple("15.10"), pack.version_tuple("15.9"))

    def test_missing_minimum_fails(self):
        with self.assertRaisesRegex(ValueError, "deployment target"):
            pack.load_commands("cmd LC_BUILD_VERSION\nsdk 27.0")

    def test_relative_dependency_and_spaces(self):
        with tempfile.TemporaryDirectory(prefix="gliff fixtures ") as tmp:
            root = Path(tmp)
            binary = root / "Mac OS/gliff"
            binary.parent.mkdir()
            target = root / "Frameworks/lib a.dylib"
            target.parent.mkdir()
            target.touch()
            self.assertEqual(pack.resolve_load("@loader_path/../Frameworks/lib a.dylib", binary, binary, []), target.resolve())
            self.assertEqual(pack.resolve_load("@rpath/lib a.dylib", binary, binary, ["@loader_path/../Frameworks"]), target.resolve())
            with self.assertRaisesRegex(ValueError, "Missing dependency"):
                pack.resolve_load("@rpath/missing.dylib", binary, binary, [])

    def test_apple_libraries_are_system(self):
        self.assertIsNone(pack.resolve_load("/usr/lib/libSystem.B.dylib", Path("a"), Path("a"), []))
        with self.assertRaisesRegex(ValueError, "Unresolved install"):
            pack.resolve_load("relative/libbad.dylib", Path("a"), Path("a"), [])

    def test_symlink_escape_rejected_and_internal_allowed(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp) / "app"
            root.mkdir()
            (root / "file").touch()
            (root / "link").symlink_to("file")
            pack.safe_tree(root)
            (root / "escape").symlink_to("..")
            with self.assertRaisesRegex(ValueError, "Escaping bundle"):
                pack.safe_tree(root)

    def validate_fixture(self, library, architecture="arm64", min_library="27.0"):
        with tempfile.TemporaryDirectory(prefix="moved app ") as tmp:
            root = Path(tmp) / "Gliff.app"
            relative = ["Contents/MacOS/gliff-bin", "Contents/MacOS/Gliff", "Contents/MacOS/gdk-pixbuf-query-loaders",
                        "Contents/Frameworks/libtest.dylib", "Contents/Info.plist", "Contents/Resources/Gliff.icns",
                        "Contents/Resources/share/glib-2.0/schemas/gschemas.compiled",
                        "Contents/Resources/share/icons/hicolor/index.theme", "Contents/Resources/etc/fonts/fonts.conf",
                        "Contents/Resources/licenses/third-party.json"]
            for path in relative:
                target = root / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.write_bytes(b"\xcf\xfa\xed\xfe" if "MacOS" in path or "Frameworks" in path else b"fixture")
            def fake_run(command, flag, path):
                if command == "lipo":
                    return architecture
                if flag == "-l":
                    return COMMANDS.replace("15.2", min_library if path.name == "libtest.dylib" else "11.0")
                if flag == "-L":
                    return str(path) + ":\n\t" + (library if path.name == "gliff-bin" else "/usr/lib/libSystem.B.dylib") + " (compatibility version 1.0.0, current version 1.0.0)"
                raise AssertionError((command, flag, path))
            with patch.object(pack, "run", side_effect=fake_run):
                return pack.validate(root)

    def test_validator_checks_transitive_minimum(self):
        report = self.validate_fixture("@loader_path/../Frameworks/libtest.dylib")
        self.assertEqual(report["minimum_macos"], "27.0")
        self.assertEqual(report["clean_machine_qa"], "pending")

    def test_validator_rejects_external_absolute(self):
        with self.assertRaisesRegex(ValueError, "External absolute"):
            self.validate_fixture("/opt/homebrew/opt/gtk4/lib/libgtk.dylib")

    def test_validator_rejects_intel_or_fat(self):
        for arch in ("x86_64", "arm64 x86_64"):
            with self.assertRaisesRegex(ValueError, "arm64 only"):
                self.validate_fixture("/usr/lib/libSystem.B.dylib", architecture=arch)

    def test_commands_use_argv_and_fail_fast(self):
        with patch.object(subprocess, "check_output", side_effect=subprocess.CalledProcessError(1, ["tool"])) as call:
            with self.assertRaises(subprocess.CalledProcessError):
                pack.run("tool", Path("space here"), "$(touch nope)")
            self.assertEqual(call.call_args.args[0], ["tool", "space here", "$(touch nope)"])

    def test_source_recipe_includes_patches_and_rejects_unpinned(self):
        with tempfile.TemporaryDirectory() as tmp:
            recipe = Path(tmp) / "formula.rb"
            recipe.write_text('url "https://example.org/lib.tar.xz"\nsha256 "' + "a" * 64 + '"\npatch do\nurl "https://example.org/fix.patch"\nsha256 "' + "b" * 64 + '"\nend\n')
            entries = pack.recipe_sources(recipe, "lib", "1.2")
            self.assertEqual(len(entries), 2)
            recipe.write_text('url "https://example.org/#{version}.tar.xz"\nsha256 "' + "a" * 64 + '"')
            with self.assertRaisesRegex(ValueError, "manual audit"):
                pack.recipe_sources(recipe, "lib", "1.2")

    def test_archive_licenses_never_extract_paths_or_links(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            archive = root / "archive.tar"
            with tarfile.open(archive, "w") as out:
                info = tarfile.TarInfo("../../LICENSE")
                data = b"exact source license"
                info.size = len(data)
                out.addfile(info, io.BytesIO(data))
                link = tarfile.TarInfo("COPYING")
                link.type = tarfile.SYMTYPE
                link.linkname = "/etc/passwd"
                out.addfile(link)
            dest = root / "licenses"
            dest.mkdir()
            pack.copy_archive_licenses(archive, dest)
            self.assertEqual(len(list(dest.iterdir())), 1)
            self.assertEqual(next(dest.iterdir()).read_bytes(), data)

    def test_source_fetch_offline_hash_validation(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            entry = {"sha256": "a" * 64, "name": "lib", "version": "1.0", "url": "https://example.org/a"}
            lock = root / "lock.json"
            lock.write_text(json.dumps({"sources": [entry]}))
            with patch("urllib.request.urlopen", side_effect=AssertionError("no network")):
                with self.assertRaisesRegex(ValueError, "--download"):
                    fetch.fetch(lock, root / "sources", False)
                (root / "sources" / fetch.filename(entry)).write_text("mismatch")
                with self.assertRaisesRegex(ValueError, "hash mismatch"):
                    fetch.fetch(lock, root / "sources", False)

    def test_cached_source_rejects_dangling_target_symlink(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo_home = root / "cargo"
            cache = cargo_home / "registry/cache/test"
            cache.mkdir(parents=True)
            data = b"synthetic cached crate"
            (cache / "testcrate-1.0.crate").write_bytes(data)
            entry = {"kind": "cargo", "name": "testcrate", "version": "1.0", "url": "https://example.org/crate", "sha256": fetch.hashlib.sha256(data).hexdigest()}
            lock = root / "lock.json"
            lock.write_text(json.dumps({"sources": [entry]}))
            output = root / "sources"
            output.mkdir()
            outside = root / "must-not-exist"
            (output / fetch.filename(entry)).symlink_to(outside)
            with patch.dict("os.environ", {"CARGO_HOME": str(cargo_home)}):
                with self.assertRaisesRegex(ValueError, "symlink"):
                    fetch.fetch(lock, output, False)
            self.assertFalse(outside.exists())
            self.assertTrue((output / fetch.filename(entry)).is_symlink())
            self.assertFalse(any(output.glob(".gliff-source-*")))

    def test_source_publication_is_exclusive_and_digest_failure_cleans_up(self):
        import os
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            directory = os.open(root, os.O_RDONLY | os.O_DIRECTORY)
            data = b"verified source"
            expected = fetch.hashlib.sha256(data).hexdigest()
            try:
                with self.assertRaisesRegex(ValueError, "SHA mismatch"):
                    fetch.publish_stream(directory, "target", io.BytesIO(b"bad"), expected)
                self.assertEqual(list(root.iterdir()), [])
                class Race(io.BytesIO):
                    def read(self, size=-1):
                        if not (root / "target").is_symlink():
                            (root / "target").symlink_to(root / "outside")
                        return super().read(size)
                with self.assertRaises(FileExistsError):
                    fetch.publish_stream(directory, "target", Race(data), expected)
                self.assertFalse((root / "outside").exists())
                self.assertFalse(any(root.glob(".gliff-source-*")))
            finally:
                os.close(directory)

    def test_cached_source_success_and_snapshot_replacement(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            cargo = root / "cargo/registry/cache/test"
            cargo.mkdir(parents=True)
            data = b"cached"
            (cargo / "test-1.crate").write_bytes(data)
            entry = {"kind": "cargo", "name": "test", "version": "1", "url": "https://example.org/source", "sha256": fetch.hashlib.sha256(data).hexdigest()}
            lock = root / "lock.json"
            lock.write_text(json.dumps({"sources": [entry]}))
            output = root / "sources"
            with patch.dict("os.environ", {"CARGO_HOME": str(root / "cargo")}):
                self.assertEqual(fetch.fetch(lock, output, False), len(data))
                lock.write_text(json.dumps({"format": 1, "sources": [entry]}))
                self.assertEqual(fetch.fetch(lock, output, False), len(data))
            self.assertEqual((output / fetch.filename(entry)).read_bytes(), data)
            self.assertEqual((output / "source-lock.json").read_bytes(), lock.read_bytes())
            self.assertFalse(any(output.glob(".gliff-source-*")))

    def test_source_filename_rejects_traversal(self):
        with self.assertRaisesRegex(ValueError, "Unsafe"):
            fetch.filename({"sha256": "a" * 64, "name": "../oops", "version": "1", "url": "https://example.org/a"})

    def test_cairo_source_hash_and_archive_safety(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            source = root / "wrong.tar"
            source.write_bytes(b"not the pinned source")
            with self.assertRaisesRegex(ValueError, "SHA-256 mismatch"):
                cairo.extract_source(source, root / "unpacked")
            self.assertFalse((root / "unpacked").exists())
            with tarfile.open(source, "w") as out:
                info = tarfile.TarInfo("../../oops")
                info.size = 1
                out.addfile(info, io.BytesIO(b"x"))
            with patch.object(cairo, "SOURCE_SHA256", fetch.digest(source)):
                with self.assertRaisesRegex(ValueError, "Unsafe source archive path"):
                    cairo.extract_source(source, root / "unpacked")
            self.assertFalse((root.parent / "oops").exists())

    def test_cairo_abi_audit_rejects_missing_exports_and_lzo(self):
        with patch.object(cairo, "output", side_effect=lambda *a: "arm64" if a[0] == "lipo" else "libcairo:\n /opt/liblzo2.2.dylib"):
            with self.assertRaisesRegex(ValueError, "LZO survived"):
                cairo.audit(Path("stage"), Path("reference"))
        with patch.object(cairo, "output", side_effect=lambda *a: "arm64" if a[0] == "lipo" else "libcairo:\n /usr/lib/libSystem.B.dylib"), patch.object(cairo, "exported_symbols", side_effect=[{"_api"}, set()]):
            with self.assertRaisesRegex(ValueError, "lost exported symbols"):
                cairo.audit(Path("stage"), Path("reference"))
        self.assertIn("-Dlzo=disabled", cairo.OPTIONS)
        self.assertNotIn("-Dscript=disabled", cairo.OPTIONS)

    def test_each_crate_requires_actual_license_without_special_exceptions(self):
        package = {"name": "synthetic", "version": "1"}
        with self.assertRaisesRegex(ValueError, "Missing license text: synthetic 1"):
            pack.require_crate_license(package, [Path("README.md"), Path("NOTICE")])
        pack.require_crate_license(package, [Path("LICENSE-MIT")])
        pack.require_crate_license(package, [Path("COPYING")])

    def test_launcher_is_native_and_never_writes_bundle(self):
        source = Path(__file__).with_name("launcher.c").read_text()
        self.assertIn("_NSGetExecutablePath", source)
        self.assertIn('execv(binary, argv)', source)
        self.assertIn('mkstemp(cache)', source)
        self.assertNotIn("system(", source)
        self.assertNotIn("AXIsProcessTrusted", source)


if __name__ == "__main__":
    unittest.main()
