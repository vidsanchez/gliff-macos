#!/usr/bin/env python3
"""Validate bundled GTK icon lookup without a display, window, clipboard or Gliff."""
import argparse
import ctypes as c
import json
import os
from pathlib import Path
import re
import subprocess
import sys
import tempfile

UNKNOWN_ICON = "gliff-validation-icon-that-does-not-exist"
BACKEND = "gliff-validation-no-display"


def icon_names(source):
    names = set(re.findall(r'(?:\.icon_name|::from_icon_name|::set_default_icon_name)\(\s*"([^"]+)"', source))
    names.update(re.findall(r'const (?:APP_NAME|\w*ICON):\s*&str\s*=\s*"([^"]+)"', source))
    if not names or "gliff" not in names:
        raise ValueError("No complete Gliff UI icon-name set found")
    return sorted(names)


def worker(library, icons, source):
    gtk = c.CDLL(str(library))
    def function(name, args, result):
        method = getattr(gtk, name)
        method.argtypes, method.restype = args, result
        return method
    init = function("gtk_init_check", [], c.c_int)
    display = function("gdk_display_get_default", [], c.c_void_p)
    if init() or display():
        raise ValueError("Unexpected display in no-window icon validation")
    theme = function("gtk_icon_theme_new", [], c.c_void_p)()
    paths = (c.c_char_p * 2)(str(icons).encode(), None)
    function("gtk_icon_theme_set_search_path", [c.c_void_p, c.POINTER(c.c_char_p)], None)(theme, paths)
    function("gtk_icon_theme_set_theme_name", [c.c_void_p, c.c_char_p], None)(theme, b"hicolor")
    has_icon = function("gtk_icon_theme_has_icon", [c.c_void_p, c.c_char_p], c.c_int)
    # Never ask affected GTK to look up a missing fallback: it recurses indefinitely.
    if not has_icon(theme, b"image-missing"):
        return {"status": "rejected", "reason": "missing-fallback"}
    lookup = function("gtk_icon_theme_lookup_icon", [c.c_void_p, c.c_char_p, c.POINTER(c.c_char_p),
                                                    c.c_int, c.c_int, c.c_int, c.c_int], c.c_void_p)
    resolved_name = function("gtk_icon_paintable_get_icon_name", [c.c_void_p], c.c_char_p)
    names = icon_names(source) + ["image-missing", UNKNOWN_ICON]
    count = 0
    for name in names:
        expected = "image-missing" if name == UNKNOWN_ICON else name
        if name != UNKNOWN_ICON and not has_icon(theme, name.encode()):
            raise ValueError(f"Required UI icon is unavailable: {name}")
        for size, scale in ((16, 1), (16, 2), (32, 1), (32, 2)):
            paintable = lookup(theme, name.encode(), None, size, scale, 0, 0)
            if not paintable or resolved_name(paintable) != expected.encode():
                raise ValueError(f"Incorrect icon/fallback lookup: {name}, size={size}, scale={scale}")
            count += 1
    return {"status": "passed", "display_opened": False, "ui_icons": icon_names(source),
            "lookup_count": count, "unknown_resolves_to": "image-missing"}


def probe(args, icons, home):
    env = {"PATH": "/usr/bin:/bin:/usr/sbin:/sbin", "HOME": str(home), "GDK_BACKEND": BACKEND,
           "XDG_DATA_HOME": str(home / "data"), "XDG_CONFIG_HOME": str(home / "config"),
           "XDG_CACHE_HOME": str(home / "cache"), "XDG_DATA_DIRS": str(home / "data"),
           "GTK_PATH": str(home / "modules"), "GIO_MODULE_DIR": str(home / "modules"),
           "GIO_EXTRA_MODULES": "", "PYTHONDONTWRITEBYTECODE": "1"}
    command = [sys.executable, str(Path(__file__).resolve()), "--worker", "--gtk-library", str(args.gtk_library),
               "--icons-dir", str(icons), "--source", str(args.source)]
    result = subprocess.run(command, env=env, capture_output=True, text=True, timeout=15)
    if result.returncode not in (0, 2):
        raise ValueError(f"Icon worker failed ({result.returncode}): {result.stdout[-4096:]} {result.stderr[-4096:]}")
    data = json.loads(result.stdout)
    if not isinstance(data, dict) or (result.returncode == 0) != (data.get("status") == "passed"):
        raise ValueError("Inconsistent icon worker result")
    return data


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--gtk-library", type=Path, required=True)
    parser.add_argument("--icons-dir", type=Path, required=True)
    parser.add_argument("--source", type=Path, required=True, help="Actual client main.rs for UI icon names")
    parser.add_argument("--worker", action="store_true", help=argparse.SUPPRESS)
    args = parser.parse_args()
    try:
        if args.worker:
            if os.environ.get("GDK_BACKEND") != BACKEND:
                raise ValueError("Icon worker requires the forced unavailable display backend")
            result = worker(args.gtk_library.resolve(), args.icons_dir.resolve(), args.source.read_text())
        else:
            with tempfile.TemporaryDirectory(prefix="gliff icon QA ") as tmp:
                root = Path(tmp)
                home = root / "isolated-home"
                home.mkdir()
                bad = root / "old-icons/hicolor"
                bad.mkdir(parents=True)
                (bad / "index.theme").write_text("[Icon Theme]\nName=Old regression fixture\nDirectories=1024x1024/apps\n\n[1024x1024/apps]\nSize=1024\nType=Fixed\n")
                if probe(args, bad.parent, home) != {"status": "rejected", "reason": "missing-fallback"}:
                    raise ValueError("Old-index regression did not reject the hidden fallback")
                result = probe(args, args.icons_dir.resolve(), home)
                if result.get("status") != "passed":
                    raise ValueError("Bundled icon theme hides the required fallback")
                result["old_index_rejected"] = True
    except (ValueError, OSError, subprocess.SubprocessError) as error:
        print(json.dumps({"status": "failed", "error": str(error)}))
        return 1
    print(json.dumps(result, indent=2))
    return 0 if result["status"] == "passed" else 2


if __name__ == "__main__":
    sys.exit(main())
