# Changelog

## 0.1.0-alpha.1 — first independent release, experimental

- Native VideoToolbox H.264 decoding, without distributing/downloading OpenH264.
- Mac client separated from the Linux server, based on upstream Gliff 0.4.0.
- Quartz→evdev keyboard mapping, macOS layout, and configurable Command/Option policies.
- Optional native capture, fixed local Shift+Esc escape, and fail-open behavior.
- Linux→Mac text clipboard prefetch without blocking NSPasteboard; GTK macOS
  trailing-NUL correction. Mac→Linux retains the inherited bridge.
- Canonical Insert in the keymap: a user confirmed Command+C/V in foot/Omarchy.
- Apple Silicon `.app` packaging with relocated dependencies, ad-hoc signing,
  checksums, and a measured minimum macOS report. No Developer ID or notarization.

- Fixed startup recursion caused by icon-theme metadata hiding GTK's embedded
  missing-image fallback. The earlier crashing candidate was never released.
  CI now checks all seven UI icons and missing-icon fallback with 36 no-window lookups.

### Validation status

Experimental prerelease for Apple Silicon, measured minimum **macOS 15.0**.
[CI 39587341227](https://github.com/vidsanchez/gliff-macos/actions/runs/39587341227)
built `b7b1417fd0e8b2d51b20847a3d5b8e5bf8e8a4a8`: 52 Python tests and
184 Rust tests passed (including four native VideoToolbox tests); one Rust test
was ignored. Automated tests do not open a GUI, capture keyboard input, or read
the real clipboard.

The user confirmed the downloaded replacement opens, connects to Linux, displays
video, and supports typing and copy/paste on Apple Silicon macOS 27.0.1. The latest
server version/layout were not recorded; reconnects and opt-in system capture/
permissions were not newly tested. A clean Mac without Homebrew remains
**untested**. Intel, untested macOS/server/layout combinations, hardware decoding,
and universal shortcut capture are not promised. See [distribution limits](docs/distribution.md).
Homebrew installation uses this repository's tap; see the [README](README.md#homebrew).
