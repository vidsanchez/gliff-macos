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

### Validation status

Automated tests do not open a GUI, capture keyboard input, or read the real clipboard.
Complete QA of the downloaded app on a Mac without Homebrew remains **pending**.
Intel, OS versions below the artifact's measured minimum, compatibility with all
servers, and universal shortcut capture are not promised. CI artifacts are
candidates: publication requires the documented manual gate.
