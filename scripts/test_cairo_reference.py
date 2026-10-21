"""Synthetic installed-reference checks: no network, build, Homebrew execution or GUI."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

import cairo_reference as reference

spec = importlib.util.spec_from_file_location("package_reference_tests", Path(__file__).with_name("package-macos.py"))
pack = importlib.util.module_from_spec(spec)
spec.loader.exec_module(pack)
PINS = {entry["version"]: entry for entry in json.loads(reference.PINS.read_text())["sources"]}


def fixture(root, version="1.18.6", revision=0):
    keg_version = version + ("_" + str(revision) if revision else "")
    keg = root / "Cellar/cairo" / keg_version
    (keg / ".brew").mkdir(parents=True)
    (keg / "lib").mkdir()
    (root / "opt").mkdir()
    (root / "opt/cairo").symlink_to(keg, target_is_directory=True)
    receipt = {"arch": "arm64", "source": {"spec": "stable", "versions": {"stable": version}}}
    (keg / "INSTALL_RECEIPT.json").write_text(json.dumps(receipt))
    pin = PINS.get(version, {"url": f"https://cairographics.org/releases/cairo-{version}.tar.xz", "sha256": "a" * 64})
    text = f'class Cairo < Formula\n  url "{pin["url"]}"\n  sha256 "{pin["sha256"]}"\n'
    if revision:
        text += f"  revision {revision}\n"
    (keg / ".brew/cairo.rb").write_text(text + "end\n")
    for name in reference.LIBRARIES:
        (keg / "lib" / name).write_bytes(name.encode())
    return keg


def report(desc):
    return {"version": desc["version"], "reference": desc, "source_sha256": desc["source"]["sha256"],
            "lzo": "disabled", "script_interpreter_preserved": True, "options": ["-Dlzo=disabled"],
            "libraries": {name: {"architecture": "arm64", "original_symbols": 4, "rebuilt_symbols": 4,
                "missing_symbols": [], "sha256": "a" * 64, "reference_sha256": desc["library_sha256"][name]}
                for name in reference.LIBRARIES}}


class CairoReferenceTests(unittest.TestCase):
    def test_supported_versions_and_revision_normalization(self):
        for version, revision in (("1.18.4", 0), ("1.18.4", 2), ("1.18.6", 0), ("1.18.6", 1)):
            with self.subTest(version=version, revision=revision), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                fixture(root, version, revision)
                desc = reference.read_reference(root)
                self.assertEqual(desc["version"], version)
                self.assertEqual(desc["revision"], revision)
                self.assertEqual(desc["source"]["source_version"], version)
                self.assertEqual(desc["source"]["version"], desc["keg_version"])
                self.assertEqual(desc["source"]["sha256"], PINS[version]["sha256"])
                self.assertEqual(reference.source_lock(desc)["sources"], [desc["source"]])
                self.assertIn("-cairo-" + desc["keg_version"] + ".source", reference.source_filename(desc))
                reference.verify_build_report(report(desc), desc)

    def test_missing_or_unsupported_reference_reports_actual_keg(self):
        with tempfile.TemporaryDirectory() as tmp:
            with self.assertRaisesRegex(ValueError, "not an installed Cairo keg"):
                reference.read_reference(Path(tmp))
        for version in ("1.18.2", "1.18.8", "2.0.0"):
            with self.subTest(version=version), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                fixture(root, version)
                with self.assertRaisesRegex(ValueError, f"keg={version}.*unsupported source version"):
                    reference.read_reference(root)

    def test_receipt_version_architecture_and_recipe_revision_must_match(self):
        for field in ("version", "arch", "spec", "revision"):
            with self.subTest(field=field), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                keg = fixture(root, revision=1)
                receipt = json.loads((keg / "INSTALL_RECEIPT.json").read_text())
                if field == "version":
                    receipt["source"]["versions"]["stable"] = "1.18.4"
                elif field == "arch":
                    receipt["arch"] = "x86_64"
                elif field == "spec":
                    receipt["source"]["spec"] = "head"
                else:
                    p = keg / ".brew/cairo.rb"
                    p.write_text(p.read_text().replace("revision 1", "revision 0"))
                (keg / "INSTALL_RECEIPT.json").write_text(json.dumps(receipt))
                with self.assertRaisesRegex(ValueError, "does not match"):
                    reference.read_reference(root)

    def test_ambiguous_unpinned_dynamic_or_modified_recipes_fail(self):
        changes = [lambda s: s + '  url "https://cairographics.org/duplicate"\n',
                   lambda s: s.replace('  url "', '  url ENV["') ,
                   lambda s: s.replace("https://cairographics.org", "https://example.org"),
                   lambda s: s.replace(PINS["1.18.6"]["sha256"], "b" * 64),
                   lambda s: s + '  version "1.18.4"\n',
                   lambda s: s + '  patch do\n  end\n',
                   lambda s: s + '  resource "extra" do\n  end\n']
        for change in changes:
            with self.subTest(change=changes.index(change)), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                keg = fixture(root)
                recipe = keg / ".brew/cairo.rb"
                recipe.write_text(change(recipe.read_text()))
                with self.assertRaises(ValueError):
                    reference.read_reference(root)

    def test_conditional_source_fields_are_rejected_even_with_identical_pins(self):
        pin = PINS["1.18.6"]
        for block in ("on_macos do", "on_linux do", "if OS.mac?", "stable do"):
            for key, value in (("url", pin["url"]), ("url", "https://example.org/cairo.tar.xz"),
                               ("sha256", pin["sha256"]), ("sha256", "b" * 64),
                               ("version", "1.18.6"), ("revision", 0)):
                with self.subTest(block=block, key=key, value=value), tempfile.TemporaryDirectory() as tmp:
                    root = Path(tmp)
                    keg = fixture(root)
                    recipe = keg / ".brew/cairo.rb"
                    literal = str(value) if key == "revision" else f'"{value}"'
                    nested = f"  {block}\n    {key} {literal}\n  end\n"
                    recipe.write_text(recipe.read_text().removesuffix("end\n") + nested + "end\n")
                    with self.assertRaisesRegex(ValueError, "conditional/dynamic"):
                        reference.read_reference(root)

    def test_disguised_or_dynamic_source_declarations_are_rejected(self):
        pin = PINS["1.18.6"]
        for declaration in (f'  url "{pin["url"]}" if OS.mac?',
                            f'  url("{pin["url"]}")', f'  self.url "{pin["url"]}"',
                            f'  send(:url, "{pin["url"]}")', '  url "#{source_url}"',
                            f'  if OS.mac?\n  url "{pin["url"]}"\n  end',
                            f'  desc "Cairo"; if OS.mac?\n  url "{pin["url"]}"\n  end'):
            with self.subTest(declaration=declaration), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                keg = fixture(root)
                recipe = keg / ".brew/cairo.rb"
                recipe.write_text(recipe.read_text().replace(f'  url "{pin["url"]}"', declaration))
                with self.assertRaises(ValueError):
                    reference.read_reference(root)

    def test_nonstable_metadata_blocks_are_allowed_with_strict_boundaries(self):
        metadata = ('  head "https://example.org/cairo.git"\n'
                    '  head do\n    url "https://example.org/head.tar.xz"\n  end\n'
                    '  livecheck do\n    url "https://example.org/releases/"\n  end\n'
                    '  bottle do\n    on_macos do\n      sha256 arm64_sequoia: "bottle-digest"\n    end\n  end\n')
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            keg = fixture(root)
            recipe = keg / ".brew/cairo.rb"
            original = recipe.read_text()
            recipe.write_text(original.removesuffix("end\n") + metadata + "end\n")
            self.assertEqual(reference.read_reference(root)["source"]["sha256"], PINS["1.18.6"]["sha256"])
            for block in ('  bottle do\n    end; url "override"\n  end\n',
                          '  bottle do\n  url "override"\n  end\n',
                          '  bottle do\n    end\n    url "override"\n  end\n',
                          '  bottle do\n    on_macos do\n  end\n', '  bottle do\n'):
                with self.subTest(block=block):
                    recipe.write_text(original.removesuffix("end\n") + block)
                    with self.assertRaisesRegex(ValueError, "metadata block"):
                        reference.read_reference(root)

    def test_changed_recipe_receipt_or_reference_binary_invalidates_snapshot(self):
        for name in (".brew/cairo.rb", "INSTALL_RECEIPT.json", "lib/libcairo.2.dylib"):
            with self.subTest(name=name), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp)
                keg = fixture(root)
                desc = reference.read_reference(root)
                path = keg / name
                path.write_bytes(path.read_bytes() + b"\n")
                with self.assertRaisesRegex(ValueError, "reference changed"):
                    reference.require_reference(desc, root)

    def test_old_or_mismatched_build_report_is_not_relabelled(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            fixture(root, "1.18.4")
            desc = reference.read_reference(root)
            for field, value in (("version", "1.18.6"), ("source_sha256", PINS["1.18.6"]["sha256"]),
                                 ("lzo", "enabled"), ("reference", None)):
                with self.subTest(field=field), self.assertRaisesRegex(ValueError, "mismatch"):
                    reference.verify_build_report({**report(desc), field: value}, desc)
            broken = report(desc)
            broken["libraries"][reference.LIBRARIES[0]]["missing_symbols"] = ["_required"]
            with self.assertRaisesRegex(ValueError, "invalid ABI/hash"):
                reference.verify_build_report(broken, desc)
            broken = report(desc)
            broken["libraries"][reference.LIBRARIES[0]]["architecture"] = "x86_64"
            with self.assertRaisesRegex(ValueError, "invalid ABI/hash"):
                reference.verify_build_report(broken, desc)

    def test_packager_source_inventory_uses_exact_rebuilt_reference(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp)
            keg = fixture(root, "1.18.4", 1)
            desc = reference.read_reference(root)
            override = root / "override"
            override.mkdir()
            (override / "cairo-build.json").write_text(json.dumps(report(desc)))
            bundle = pack.Bundle(root / "Gliff.app", root, cairo_prefix=override)
            bundle.kegs.add(keg)
            plan = bundle.source_plan()
            self.assertEqual(plan["cairo_reference"], desc)
            self.assertEqual(next(e for e in plan["sources"] if e["name"] == "cairo"), desc["source"])
            self.assertNotIn("1.18.6", json.dumps(desc))
            recipe = keg / ".brew/cairo.rb"
            recipe.write_text(recipe.read_text() + "# changed\n")
            with self.assertRaisesRegex(ValueError, "reference changed"):
                bundle.source_plan()


if __name__ == "__main__":
    unittest.main()
