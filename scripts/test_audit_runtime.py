"""Read-only graph diagnostics: synthetic Mach-O output, no native code execution."""
import ast
import hashlib
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest
from unittest.mock import patch
from test_cairo_reference import fixture, report

spec = importlib.util.spec_from_file_location("runtime_audit_tests", Path(__file__).with_name("audit-runtime.py"))
audit = importlib.util.module_from_spec(spec)
spec.loader.exec_module(audit)


class RuntimeAuditTests(unittest.TestCase):
    def test_shared_production_seeds_include_modules_and_helper(self):
        with tempfile.TemporaryDirectory() as tmp:
            prefix = Path(tmp).resolve()
            paths = ["lib/gdk-pixbuf-2.0/2/loaders/a.so", "lib/gio/modules/a.dylib",
                     "lib/gtk-4.0/4/printbackends/b.so", "lib/gtk-4.0/4/immodules/c.so"]
            for path in paths:
                target = prefix / path
                target.parent.mkdir(parents=True, exist_ok=True)
                target.touch()
            seeds = list(audit.pack.runtime_binary_seeds(prefix))
            self.assertEqual({str(p.relative_to(prefix)) for p, _ in seeds}, set(paths + ["bin/gdk-pixbuf-query-loaders"]))
            source = Path(__file__).with_name("package-macos.py").read_text()
            method = next(n for n in ast.walk(ast.parse(source)) if isinstance(n, ast.FunctionDef) and n.name == "resources_copy")
            self.assertTrue(any(isinstance(n, ast.Call) and isinstance(n.func, ast.Name)
                                and n.func.id == "runtime_binary_seeds" for n in ast.walk(method)))

    def graph(self, root):
        fixture(root)
        reference = audit.pack.read_reference(root)
        override = root / "override"
        (override / "lib").mkdir(parents=True)
        build = report(reference)
        for name in reference["library_sha256"]:
            data = name.encode()
            (override / "lib" / name).write_bytes(data)
            build["libraries"][name]["sha256"] = hashlib.sha256(data).hexdigest()
        (override / "cairo-build.json").write_text(json.dumps(build))
        paths = []
        for family in ("unknown-a", "unknown-b"):
            keg = root / "Cellar" / family / "1.2.3_2"
            (keg / "lib").mkdir(parents=True)
            (keg / ".brew").mkdir()
            (keg / ".brew" / (family + ".rb")).write_text('url "https://example.org/exact-source"\n')
            (keg / "INSTALL_RECEIPT.json").write_text(json.dumps({"source": {"versions": {"stable": "1.2.3"}}}))
            path = keg / "lib" / (family + ".dylib")
            path.write_bytes(b"fixture")
            paths.append(path)
        binary = root / "gliff"
        binary.write_bytes(b"client")
        cairo = Path(reference["keg_path"]) / "lib/libcairo.2.dylib"
        links = {binary: [paths[0], cairo], paths[0]: [paths[1]], paths[1]: [paths[0]], override / "lib/libcairo.2.dylib": []}
        def run(command, flag, path):
            self.assertIn(command, ("otool", "lipo"))
            if command == "lipo": return "arm64"
            if flag == "-l": return "cmd LC_BUILD_VERSION\nminos 15.0"
            return str(path) + ":\n" + "".join(f"\t{p} (compatibility version 1.0.0)\n" for p in links[path])
        return binary, override, paths, run, links

    def test_all_unknowns_cycles_edges_and_exact_metadata_without_writes(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            binary, override, paths, run, _ = self.graph(root)
            before = {p: p.read_bytes() for p in root.rglob("*") if p.is_file()}
            with patch.object(audit.pack, "run", side_effect=run), patch.object(audit.pack, "runtime_binary_seeds", return_value=[]):
                result = audit.audit(binary, root, override)
            self.assertTrue(result["complete"])
            self.assertFalse(result["distributable"])
            self.assertEqual({r["path"] for r in result["rejections"]}, {str(p) for p in paths})
            self.assertTrue(all(r["incoming_edges"] and r["keg_version"] == "1.2.3_2" for r in result["rejections"]))
            self.assertEqual(len(result["nodes"]), 4)
            self.assertTrue(all(k["recipe"]["sha256"] and k["source_version"] for k in result["kegs"]))
            self.assertEqual(before, {p: p.read_bytes() for p in root.rglob("*") if p.is_file()})

    def test_unresolved_edges_and_graph_bounds_reject_without_losing_unknowns(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            binary, override, paths, run, links = self.graph(root)
            links[binary].append(root / "missing.dylib")
            with patch.object(audit.pack, "run", side_effect=run), patch.object(audit.pack, "runtime_binary_seeds", return_value=[]):
                result = audit.audit(binary, root, override)
                self.assertFalse(result["complete"])
                self.assertEqual(len(result["rejections"]), 2)
                with patch.object(audit, "MAX_NODES", 1):
                    self.assertFalse(audit.audit(binary, root, override)["complete"])
                outside = root / "outside.dylib"
                outside.write_bytes(b"not inspected")
                links[binary].append(outside)
                rejected = audit.audit(binary, root, override)
                self.assertTrue(any(e.get("path") == str(outside) for e in rejected["errors"]))
                with patch.object(audit, "MAX_TOTAL_EVIDENCE", 1):
                    self.assertFalse(audit.audit(binary, root, override)["kegs"])

    def test_metadata_symlinks_and_size_are_rejected(self):
        with tempfile.TemporaryDirectory() as tmp:
            root = Path(tmp).resolve()
            keg = fixture(root)
            recipe = keg / ".brew/cairo.rb"
            recipe.unlink()
            recipe.symlink_to(root / "outside")
            with patch.object(audit.os, "open", side_effect=AssertionError("must not follow symlink")):
                with self.assertRaisesRegex(ValueError, "symlink evidence"):
                    audit.evidence(keg)
            recipe.unlink()
            recipe.write_text("too large")
            with patch.object(audit, "MAX_EVIDENCE", 2), self.assertRaisesRegex(ValueError, "Oversize"):
                audit.evidence(keg)

    def test_cli_writes_rejection_report_and_exits_nonzero_without_an_app(self):
        with tempfile.TemporaryDirectory() as tmp:
            output = Path(tmp).resolve() / "diagnostic"
            result = {"diagnostic_only": True, "distributable": False, "complete": True,
                      "rejections": [{"path": "unreviewed.dylib"}], "errors": []}
            argv = ["audit-runtime.py", "--output", str(output), "--cairo-prefix", str(output / "unused")]
            with patch.object(audit.sys, "platform", "darwin"), patch.object(audit.sys, "argv", argv), \
                    patch.object(audit, "audit", return_value=result), self.assertRaises(SystemExit) as error:
                audit.main()
            self.assertEqual(error.exception.code, 1)
            self.assertEqual([p.name for p in output.iterdir()], ["runtime-closure.json"])
            self.assertEqual(json.loads((output / "runtime-closure.json").read_text()), result)

    def test_malformed_receipt_shapes_preserve_other_unknowns_and_evidence(self):
        for receipt in ([], {"source": None}, {"source": {"versions": []}}):
            with self.subTest(receipt=receipt), tempfile.TemporaryDirectory() as tmp:
                root = Path(tmp).resolve()
                binary, override, paths, run, _ = self.graph(root)
                keg = paths[0].parents[1]
                (keg / "INSTALL_RECEIPT.json").write_text(json.dumps(receipt))
                with patch.object(audit.pack, "run", side_effect=run), patch.object(audit.pack, "runtime_binary_seeds", return_value=[]):
                    result = audit.audit(binary, root, override)
                self.assertFalse(result["complete"])
                self.assertEqual(len(result["rejections"]), 2)
                self.assertTrue(any(e.get("keg") == str(keg) and "Invalid receipt" in e["error"] for e in result["errors"]))
                self.assertIn(str(paths[1].parents[1]), {k["keg"] for k in result["kegs"]})
