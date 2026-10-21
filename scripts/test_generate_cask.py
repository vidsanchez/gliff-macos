"""No install/tap/network: validate manifest parsing, archive match, Ruby syntax."""
import hashlib
import importlib.util
import json
from pathlib import Path
import plistlib
import shutil
import subprocess
import tempfile
import unittest
import zipfile

spec = importlib.util.spec_from_file_location("cask", Path(__file__).with_name("generate-cask.py"))
cask = importlib.util.module_from_spec(spec)
spec.loader.exec_module(cask)


def report():
    version = "0.1.0-alpha.1"
    return {"version": version, "tag": "v" + version, "architecture": "arm64", "minimum_macos": "15.2",
            "archive_sha256": "0123456789abcdef" * 4,
            "archive_url": f"{cask.REPO}/releases/download/v{version}/Gliff-{version}-arm64.zip"}


class CaskTests(unittest.TestCase):
    def test_strict_manifest(self):
        baseline = report()
        for field, value in (("version", '1.0.0#{system("bad")}'), ("tag", "latest"),
                             ("minimum_macos", '15.0"; exec("bad")'), ("minimum_macos", "10.15"),
                             ("archive_sha256", "a" * 64), ("archive_sha256", "no_check"),
                             ("archive_url", "https://example.org/app.zip"),
                             ("architecture", "x86_64"), ("archive_url", baseline["archive_url"] + "?token=secret")):
            with self.subTest(field=field, value=value):
                with self.assertRaises(ValueError):
                    cask.render({**baseline, field: value})

    def test_real_zip_and_ruby_syntax(self):
        with tempfile.TemporaryDirectory(prefix="cask fixture ") as tmp:
            root = Path(tmp)
            manifest = report()
            archive = root / "Gliff-0.1.0-alpha.1-arm64.zip"
            info = {"GliffReleaseVersion": manifest["version"], "LSMinimumSystemVersion": manifest["minimum_macos"],
                    "CFBundleIdentifier": "io.github.vidsanchez.gliff-macos"}
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("Gliff.app/Contents/Info.plist", plistlib.dumps(info))
            manifest["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
            path = root / "report.json"
            path.write_text(json.dumps(manifest))
            result = root / "artifacts/gliff-macos.rb"
            cask.generate(path, archive, result)
            text = result.read_text()
            self.assertIn('depends_on arch: :arm64', text)
            self.assertIn('depends_on macos: :sequoia', text)
            self.assertIn('Version.new("15.2")', text)
            self.assertNotIn('postflight', text)
            self.assertNotIn('no_check', text)
            self.assertNotIn('zap', text)
            if ruby := shutil.which("ruby"):
                subprocess.check_call([ruby, "-c", str(result)], stdout=subprocess.DEVNULL)
            manifest["archive_sha256"] = "0123456789abcdef" * 4
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "checksum"):
                cask.generate(path, archive, result)

    def test_named_major_and_exact_patch_preflight_evaluate_dsl(self):
        if not shutil.which("ruby"):
            self.skipTest("Ruby unavailable for DSL evaluation")
        harness = Path(__file__).with_name("test-cask-dsl.rb")
        import os
        with tempfile.TemporaryDirectory() as tmp:
            path = Path(tmp) / "candidate.rb"
            for minimum, symbol in (("15.2.1", "sequoia"), ("27.0", "golden_gate")):
                text = cask.render({**report(), "minimum_macos": minimum})
                self.assertIn("depends_on macos: :" + symbol, text)
                path.write_text(text)
                env = {**os.environ, "CASK_TEST_MACOS": minimum}
                subprocess.run(["ruby", str(harness), str(path)], check=True, env=env)
                env["CASK_TEST_MACOS"] = "15.2.0" if minimum == "15.2.1" else "26.9"
                result = subprocess.run(["ruby", str(harness), str(path)], env=env, capture_output=True, text=True)
                self.assertNotEqual(result.returncode, 0)
                self.assertIn("Gliff requires macOS " + minimum, result.stderr)
            path.write_text(cask.render(report()).replace("depends_on macos: :sequoia", 'depends_on macos: ">= 15.2"'))
            result = subprocess.run(["ruby", str(harness), str(path)], env={**os.environ, "CASK_TEST_MACOS": "27.0"}, capture_output=True, text=True)
            self.assertNotEqual(result.returncode, 0)
            self.assertIn("must be a supported Symbol", result.stderr)
        for minimum in ("16.0", "28.0"):
            with self.assertRaisesRegex(ValueError, "Unknown Homebrew"):
                cask.render({**report(), "minimum_macos": minimum})

    def test_supported_symbols_match_installed_homebrew_when_available(self):
        path = Path("/opt/homebrew/Library/Homebrew/macos_version.rb")
        if not path.is_file():
            self.skipTest("Installed Homebrew source unavailable; portable DSL fixture still evaluated")
        import re
        releases = dict(re.findall(r'([a-z_]+):\s+"([0-9.]+)"', path.read_text().split("RELEASE_ALIASES", 1)[0]))
        for major, symbol in cask.MACOS_SYMBOLS.items():
            self.assertEqual(releases[symbol], str(major))

    def test_workflow_has_no_release_publish_and_shell_is_valid(self):
        path = Path(__file__).resolve().parents[1] / ".github/workflows/macos.yml"
        text = path.read_text()
        self.assertIn("contents: read", text)
        self.assertNotIn("contents: write", text)
        self.assertNotIn("gh release", text)
        self.assertNotIn("pull_request_target", text)
        self.assertIn("libxkbcommon", text)
        self.assertIn("GLIFF_TEST_KEYMAP_DIR", text)
        self.assertIn("-- --exact", text)
        self.assertIn('test -s "$keymap_dir/alt.xkb"', text)
        self.assertIn('test -s "$keymap_dir/altgr.xkb"', text)
        self.assertIn("scripts/validate-keymap.py", text)
        self.assertIn("cargo +1.92.0 check", text)
        if not shutil.which("ruby") or not shutil.which("bash"):
            self.skipTest("Ruby/Bash unavailable for YAML and shell syntax validation")
        code = 'require "yaml"; require "json"; puts YAML.load_file(ARGV[0]).fetch("jobs").fetch("arm64").fetch("steps").map { |s| s["run"] }.compact.to_json'
        runs = json.loads(subprocess.check_output(["ruby", "-e", code, str(path)], text=True))
        for run in runs:
            subprocess.run(["bash", "-n"], input=run, text=True, check=True)
            self.assertNotIn("\n  dist/release/gliff-macos.rb", run)

    def test_zip_plist_mismatch(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            manifest = report()
            archive = root / "Gliff-0.1.0-alpha.1-arm64.zip"
            with zipfile.ZipFile(archive, "w") as output:
                output.writestr("Gliff.app/Contents/Info.plist", plistlib.dumps({"GliffReleaseVersion": "0.0.0"}))
            manifest["archive_sha256"] = hashlib.sha256(archive.read_bytes()).hexdigest()
            path = root / "report.json"
            path.write_text(json.dumps(manifest))
            with self.assertRaisesRegex(ValueError, "Info.plist"):
                cask.generate(path, archive, root / "out.rb")


if __name__ == "__main__":
    unittest.main()
