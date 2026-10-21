"""Portable icon-gate checks; real bundled GTK is exercised explicitly in CI."""
import argparse
import configparser
import importlib.util
import json
from pathlib import Path
import subprocess
import unittest
from unittest.mock import patch
from test_package_macos import pack

spec = importlib.util.spec_from_file_location("icon_tests", Path(__file__).with_name("validate-icons.py"))
icons = importlib.util.module_from_spec(spec)
spec.loader.exec_module(icons)


class IconValidationTests(unittest.TestCase):
    def test_original_metadata_exposes_embedded_fallback_and_app_icon(self):
        index = configparser.ConfigParser()
        index.read_string(pack.ICON_THEME_INDEX)
        self.assertIn("16x16/status", index["Icon Theme"]["Directories"].split(","))
        self.assertEqual(index["16x16/status"]["Size"], "16")
        self.assertIn("1024x1024/apps", index.sections())

    def test_collects_actual_literals_and_named_icons(self):
        source = (Path(__file__).resolve().parents[1] / "crates/gliff/src/main.rs").read_text()
        names = icons.icon_names(source)
        self.assertEqual(set(names), {"gliff", "go-next-symbolic", "list-add-symbolic", "view-fullscreen-symbolic",
                                      "emblem-system-symbolic", "media-playback-stop-symbolic", "window-close-symbolic"})
        self.assertIn("future-icon", icons.icon_names(source + '\ngtk::Image::from_icon_name("future-icon");'))
        with self.assertRaises(ValueError):
            icons.icon_names("no icon declarations")

    def test_probe_isolates_environment_and_bounds_subprocess(self):
        args = argparse.Namespace(gtk_library=Path("/bundle with spaces/libgtk.dylib"), source=Path("/source/main.rs"))
        result = subprocess.CompletedProcess([], 0, json.dumps({"status": "passed"}), "")
        with patch.object(icons.subprocess, "run", return_value=result) as run:
            self.assertEqual(icons.probe(args, Path("/icons with spaces"), Path("/private/home")), {"status": "passed"})
        call = run.call_args
        self.assertEqual(call.kwargs["timeout"], 15)
        self.assertEqual(call.kwargs["env"]["GDK_BACKEND"], icons.BACKEND)
        self.assertNotIn("SSH_AUTH_SOCK", call.kwargs["env"])
        self.assertIn("/icons with spaces", call.args[0])

    def test_worker_crash_malformed_result_and_timeout_fail(self):
        args = argparse.Namespace(gtk_library=Path("/gtk"), source=Path("/source"))
        for result in (subprocess.CompletedProcess([], -11, "", "crash"),
                       subprocess.CompletedProcess([], 0, "[]", ""),
                       subprocess.CompletedProcess([], 0, json.dumps({"status": "rejected"}), "")):
            with patch.object(icons.subprocess, "run", return_value=result), self.assertRaises(ValueError):
                icons.probe(args, Path("/icons"), Path("/home"))
        with patch.object(icons.subprocess, "run", side_effect=subprocess.TimeoutExpired([], 15)), self.assertRaises(subprocess.TimeoutExpired):
            icons.probe(args, Path("/icons"), Path("/home"))
