# Development and diagnostics

For installation, start with the [README](../README.md). This technical guide
covers the independent GTK4/libadwaita client, Quartz→evdev keyboard mapping,
macOS layouts, and native VideoToolbox H.264 video. System-shortcut capture is
**optional**, requires manual permission, and still needs real-session QA.
**Automated tests do not replace a real session:** validate keyboard, video,
mouse, and clipboard against your own Linux server. The original project outside
this workspace is not modified.

## Build and connect

Requirements: Rust ≥1.92, Xcode C/C++ tools, `pkg-config`, GTK4 ≥4.14 with the
macOS backend, and libadwaita ≥1.5. A small C shim uses the Apple SDK for
VideoToolbox/CoreMedia/CoreVideo; no external codec is compiled or downloaded.
`Cargo.lock` pins workspace dependencies. Locked GTK/glib arm64 metadata requires
Rust 1.92; `scripts/check-msrv.py` checks that declaration, and CI also runs
`cargo +1.92.0 check`. A local build with a newer compiler does not prove the
MSRV was tested locally.

```sh
cd /path/to/gliff-macos
pkg-config --modversion gtk4 libadwaita-1
cargo build --locked -p gliff
# Prepare SSH first: keys/agent, passphrase handling, and host-key verification.
# Install gliff-server separately on Linux with an active Hyprland session.
cargo run --locked -p gliff -- user@linux-server
```

To type Mac Option symbols, including when testing a Spanish layout:

```sh
cargo run --locked -p gliff -- --no-clipboard --option-key altgr user@linux-server
```

`--headless` creates a **remote Linux** output. It does not install a server or
capture the Mac screen. Start from Terminal; SSH has no graphical dialog here.

## Opt-in: send Command+Space remotely

**Read the status bar first.** `Native keyboard active` means capture is active;
`GTK normal` means system shortcuts are local again. After release, the bar shows
`Focus`, `Emergency`, `Stale` (lease/event), `Queue`, `Lock`, `Disabled`, or `Panic`.
Permission/startup failures have their own messages. Only another explicit click
on connected video can rearm capture.

Without the flag, normal GTK remains in use: **no event tap is installed and
Accessibility trust is not queried**. To test native capture:

```sh
cargo run --locked -p gliff -- --capture-system-shortcuts --option-key altgr --no-clipboard user@linux-server
```

1. If permission is missing or no authorizable entry appears, perform a **separate
   explicit action**, without a host or other flags. It starts no window/tap:
   ```sh
   RUST_LOG=info ./target/debug/gliff --request-capture-permissions
   ```
   This requests Accessibility **once** using `AXIsProcessTrustedWithOptions`.
   The prompt is asynchronous: `AX trusted now=false` means neither that the
   dialog failed nor that trust was granted. It prints executable/PID and
   read-only AX/listen/post preflight information; it does not request or grant
   every listed permission.
2. Review **System Settings → Privacy & Security → Accessibility** manually;
   labels can vary by macOS build. TCC may identify Terminal rather than Gliff.
   Use the printed executable path to identify this launch. Ad-hoc signing does
   not authenticate a Developer ID. Approve only the correct identity, close/
   restart Gliff with the capture command, and do not combine the two flags.
   There is no automatic grant, `sudo`, repeated prompt, or Spotlight/shortcut
   modification. The normal capture flag only performs a non-prompting preflight.
3. Once connected, **explicitly click the video**. Check for
   `Native keyboard active` before testing Command+Space.
4. **Shift+Esc always disarms locally**, even with `--release-hotkey none` or a
   different hotkey. The trigger is not sent remotely. Remote key releases are
   best-effort: unavailable network/channels/locks can prevent delivery. Another
   click is required to recapture; moving the pointer is insufficient.

Failures must not lock the local keyboard. Tap timeout, lost permission/focus/
session/layout, closed consumers, full queues, and internal contention **fail
open without automatic rearming**. Disconnect/close releases the previous
session's state; it never redirects releases to a new session. Leaving the video
with the pointer or actually changing focus/window intentionally disarms capture;
Shift+Esc is not required for those releases.

### Unexpected release: retain reasons, not keystrokes

Run capture with `RUST_LOG=info` as below. When it releases, copy the exact status
text and **`Native capture released`** line. One line is emitted per release,
containing `reason`, `foreground_check`, and window/video/session/layout booleans,
never keycodes, typed text, events, or another application's PID.

- `Focus` with `foreground_check=QueryError(code)` / `PidError(code)` identifies
  existing AX query failures. `OtherProcess` indicates another focused process
  without identifying it. `MissingApplication` means AX returned no application.
- `SelfProcess` is the **last** AX result, not a current-focus guarantee;
  `NotChecked` means no query has occurred since arming. Also inspect
  `window_active`, `video_focus`, `connected`, and `keymap_available`. Invalidated
  layouts/sessions also release capture.
- `Stale` retains the **150 ms** UI lease; the **5 ms** AX timeout was neither
  extended nor removed. `Lock` still fails open when processing is unsafe.

One internally reproducible contention bug was fixed: observation/heartbeat
previously competed with mutable callback state and could disarm under ordinary
contention. They now use separate metadata; a busy tick does not renew the lease,
and the watchdog expires even while a lock is busy. This **does not establish**
the cause of any particular `Focus` release; use the diagnostics above. These
changes have automated coverage, not a claim of real GUI capture validation.

### Capture does not start: diagnose the stage

```sh
RUST_LOG=info ./target/debug/gliff --capture-system-shortcuts --no-clipboard user@linux-server
```

Retain `Native capture preflight` and the failing stage. They contain identity,
booleans, stages, and error codes, not keystrokes. `listen_preflight=false` or
`post_preflight=false` is information, **not** an instruction to grant everything
or proof that AX failed.

- `AxTrust`: make the separate explicit request above, approve, and restart.
- `AxSystemWide` / `AxAttribute` / `AxTimeout (code=...)`: a specific AX failure.
  `-25201` is IllegalArgument; `-25202` is InvalidUIElement. Do not hide it by
  removing the 5 ms timeout.
- `EventTap`: `CGEventTapCreate` returned NULL; permission, policy, or resources
  may be responsible. It is not automatically diagnosed as Accessibility denial.
- `RunLoop` / `RunLoopSource` / `ThreadSpawn` / `ThreadPanic`: that stage failed.
- `starting`: not ready/capturing yet; after five seconds it becomes
  `StartupTimeout`, without silently permitting late arming. Restart to retry.
  Closing does not wait for a blocked native initialization: the worker retains
  its context/resources until the call returns and cleans up itself, without
  activating capture after stop. A system call that never returns can retain
  those resources until process exit; GTK must not free resources still in use.
- `Clock`: no valid time boundary was obtained; capture remains disarmed.

Real permission/tap behavior was not validated through development tools:
a Codex/sandbox identity is not your Terminal identity. Automated tests neither
request permissions nor create real taps; mocks cover stage-specific errors.

### Capture limits and guarantees

- This is an **active session keyboard filter**, not HID/root access. Outside
  valid capture it returns events before reading keycodes. It does not log typed
  text or intercept mouse, clipboard, or screen contents. No flag means no filter.
- Every callback checks foreground application PID through AX
  `AXFocusedApplication`/`AXUIElementGetPid`, with a configured 5 ms timeout.
  It does not inspect UI elements/text or call AppKit on the tap thread. It also
  requires video focus, valid connection/generation, and a 150 ms GTK lease.
  The lease bounds UI-state freshness, not real-time GTK/AX execution.
- Rearming uses `clock_gettime_nsec_np(CLOCK_UPTIME_RAW)`: absolute uptime
  nanoseconds, equivalent to `mach_absolute_time` converted to ns, not wall time
  or a continuous clock including sleep. Apple IOHIDSystem converts that absolute
  timestamp to ns for `NXEvent.payload.time`. Zero clocks and events predating
  arming remain rejected. **Observed diagnostic correction:** synthetic
  `CGEventCreate`/`CGEventCreateKeyboardEvent` constructors returned timestamp
  zero on this Mac; they are not clocks. The resulting click-time error was
  previously misreported as tap initialization failure. Real events after sleep
  still require manual testing.
- Callback locks are nonblocking. Its bounded FIFO has 256 slots, a normal
  threshold of 128, and shared remaining space for keyboard/mouse releases,
  axis-stop, Bye, and clipboard control. The network writer is also bounded.
  Motion/scroll/zoom can be dropped under pressure. If an essential message
  cannot fit, the connection fails visibly without waiting to drain backlog or
  a blocked writer; cancellations/offers are not silently lost. Normal close
  preserves FIFO releases before Bye. Saturation/contention disarms without
  waiting for GTK/network. **Shift+Esc guarantees local disarming**, not remote
  delivery over a failed network.
- Event modifier/Caps flags are authoritative; a current snapshot might already
  correspond to a later key-up. Native side-specific bits take priority, then
  the known side and a coherent snapshot. With no side clues, the fallback is
  explicitly left-sided; an unidentified side cannot be promised. Repeated
  clicks do not rearm or release modifiers. The first capture click synchronizes
  held modifiers before Button-down.
- Captured events are not processed again by GTK; Linux supplies autorepeat.
  Command/Option policies and layout mapping remain in effect. The configurable
  release hotkey supplements the fixed Shift+Esc emergency exit.
- Secure Input, protected combinations, hardware/Fn, custom remaps, and system
  policy may prevent capture. **Not every macOS shortcut is supported.** Remote
  Command+Space without Spotlight still needs manual testing with permission;
  automated tests do not create a real tap.

## Keyboard and shortcuts

Video accepts keyboard input while focused and the window is active. Pointer
entry focuses it; **Shift+Esc releases remote keys** and leaves local focus.
Click the video, or leave/re-enter it, to regain focus. Configure the hotkey with
`--release-hotkey ctrl+alt+q`, `super+escape`, or `double-escape`; `none` disables
it only in GTK mode. Native capture retains the fixed Shift+Esc emergency exit.
Autorepeat does not count as a new hotkey press. Hotkey names use the layout's
base symbol: `shift+q` also recognizes `Q`, and `ctrl+alt+q` recognizes the `q`
key even when Option produces a different character.

| Mac key | Default | Alternative |
|---|---|---|
| Left/right Command | Left/right Linux Super | `--command-key control` → Control |
| Left/right Option | Left/right Linux Alt | `--option-key altgr` → Option symbols |
| Control, Shift | Same modifier and side | Not remapped |
| Caps Lock | Synchronizes observed state on focus | Independent of remote US layout |

**Option has two distinct policies.** `alt` preserves Linux Alt+letter shortcuts,
not Mac Option characters. `altgr` uses the four local layout levels: normal,
Shift, Option, and Shift+Option. For example, try Option+2 for `@` **only if
Keyboard Viewer shows it for your Spanish variant**; no PC/AltGr combination or
specific Spanish layout is assumed. Option→AltGr means that same Option key is
not Linux Alt. Command→Control and physical Control share state correctly:
releasing one does not release Control while the other is held.

XKB is generated from GDK's macOS layout without installing xkbcommon, Wayland,
or Linux layout files. It includes layout symbols/dead keys, not a fixed US map.
Layout changes first release held keys and send the new map before subsequent
presses. Key state belongs to each connection: tab/focus changes, address editing,
disconnects, and close release the owning session, not the new tab. Closing tries
to drain releases/Bye within two seconds; network loss/forced exit prevents a
delivery guarantee.

### Current limits

- Normal GTK leaves reserved shortcuts such as Command+Tab/Space with macOS.
  Native capture attempts filtering only while armed, subject to Secure Input
  and permission limits; universal inhibition is not promised.
- ANSI/ISO, Spanish/US, navigation, keypad, and F1–F20 have physical mappings.
  F-keys may require Fn or a macOS setting. **Fn alone**, multimedia controls,
  JIS, IME, multi-character input sources, and custom system modifier remaps
  are not guaranteed/fully validated.
- Dead keys are transmitted as keysyms; Linux application/locale performs final
  composition. Map generation and an XKB composition example are tested, not
  your server's composition configuration.
- Native VideoToolbox Single420 (4:2:0), with no custom Metal renderer or advertised
  Dual420 4:4:4 fidelity. Apple selects hardware/software; acceleration is not
  guaranteed. Retina, trackpad scrolling, fullscreen, and performance need real QA.
- Clipboard is enabled by default; `--no-clipboard` disables both directions.
  **Linux→Mac: UTF-8 text only, up to 1 MiB and five seconds per offer.** The active
  host's offer is prefetched before paste and published through a native GDK
  provider holding ready bytes. No network/data reads occur inside NSPasteboard's
  synchronous callback. New offers, local copies, machine changes, disconnects,
  and close cancel old preparation and withdraw our stale selection without
  clearing a newer local copy. GTK macOS withdrawal replaces it with an empty
  native text provider: `set_content(None)` does not reliably clear bytes already
  materialized in NSPasteboard. Notices show status, never content. Invalid,
  sensitive, oversized, or incomplete text is not published. Accepted MIME types
  are `text/plain;charset=utf-8`, `text/plain`, and `UTF8_STRING`, always valid
  UTF-8, not legacy `STRING`/`TEXT`. Images/files/other remote formats are neither
  downloaded nor advertised as available on Mac. This text is not persisted.
  **Mac→Linux** retains the existing MIME/file bridge: connected hosts can request
  advertised contents without a local paste. Spools remove their own files on
  completion; a crash can leave private files in `GLIFF_CLIPBOARD_DIR` or
  `~/.cache/gliff/clipboard`. Clean up manually only with every instance closed.
  Finder/images and real cross-application text round trips need manual QA;
  tests do not read the real clipboard. For GTK macOS 4.24.1, external reads of
  `text/plain;charset=utf-8` include a C terminator (`strlen + 1`). The bridge
  removes exactly that final NUL at EOF, retaining interior NULs, whitespace,
  line breaks, and other MIME/local-provider bytes. It does not generally trim
  content or convert binary formats to text.
- The experimental packager creates an ad-hoc signed, unnotarized `.app`;
  see [distribution](distribution.md).

### Application icon

The **original Gliff design** is embedded and assigned to
`NSApplication.applicationIconImage` at startup for the Dock/app switcher. It
needs no working-directory assumption, installed icon theme, or external file.
Restart the executable to check; Dock/Command+Tab appearance still needs manual
QA. The packager uses the same asset for Finder.

`resources/gliff.svg` is unchanged. `resources/gliff.png` is its transparent
1024×1024 rasterization (librsvg 2.63.3), not a redesign:

```sh
# Asset regeneration only; this tool is not needed to run or build the client.
rsvg-convert --width 1024 --height 1024 resources/gliff.svg --output resources/gliff.png
```

## Offline checks

```sh
cargo metadata --offline --no-deps --format-version 1
python3 scripts/check-isolation.py
python3 scripts/check-msrv.py
python3 -m unittest discover -s scripts -p 'test_*.py' -v
cargo fmt --all --check
cargo check --offline --locked --workspace --all-targets
cargo clippy --offline --locked --workspace --all-targets -- -D warnings
cargo test --offline --locked --workspace
cargo build --offline --locked -p gliff
```

Dependencies must already be cached for `--offline`. `metadata` checks manifests,
not compilation. Tests cover translation, Control aliases, side-specific modifiers,
repeat, session releases, message ordering, layouts/dead keys, protocol, clipboard,
and conversion. The inherited sensitive-selection GUI test is ignored; it does
not run or count as macOS validation. Capture tests use synthetic events and a
pure core for gating, emergency/locks, lease, epochs/timestamps, saturation,
autorepeat/double-tap, release ownership, and no-tap fallback. They neither
request permissions nor read real keyboard input. FIFO tests cover reserve/drain
before Bye; the clipboard weak sender does not retain a closed connection.

Clipboard regression tests use a C provider and `MemoryOutputStream` on a private
GLib context without iterating the global context or opening a display/window or
real clipboard. They cover exact/excess limits, invalid UTF-8, silent/incomplete
peers, timeout/cancellation, closed/full queues, and invalidation by local
selection/offer/session changes. The earlier Rust provider failed this regression.

### Text clipboard QA (pending)

- [ ] Without `--no-clipboard`, copy synthetic text using the Linux browser menu,
      wait for `Clipboard: remote text ready`, and paste on Mac.
- [ ] Copy synthetic text on Mac; paste through the Linux browser menu. Test
      Unicode, empty text, and line breaks; no extra trailing NUL may appear.
      Read errors must abort rather than complete partial text.
- [ ] During preparation, copy locally or switch machine/disconnect: do not publish
      an old result or clear the newer local copy.
- [ ] Materialize remote text in NSPasteboard by pasting, then disconnect/switch
      or receive an unsupported offer: the old remote text must not keep pasting.
      A subsequent local copy must survive.
- [ ] Oversized (>1 MiB) or interrupted text: visible notice, responsive UI,
      no stale remote selection. Remote images/files: clear unsupported-format notice.
- [ ] Test shortcuts separately. Remote Lua `send_key_state: key not found` is
      independent of NSPasteboard blocking; see Insert compatibility below.

Optional development XKB validation is **not a client runtime dependency**:

```sh
GLIFF_TEST_KEYMAP_DIR=/tmp/gliff-keymap-fixtures cargo test --offline --locked -p gliff keymap::tests::export_xkb_fixtures_for_optional_native_validator -- --exact
python3 scripts/validate-keymap.py /path/to/libxkbcommon.dylib /tmp/gliff-keymap-fixtures
```

The script downloads/installs nothing. A separately extracted temporary validation
library, downloaded with approval, compiled both maps and checked Shift,
Option/AltGr, Caps, ñ, and accent composition.

For C/V diagnosis, a **configuration-only** Carbon harness called
`TISCopyCurrentKeyboardLayoutInputSource` → `kTISPropertyUnicodeKeyLayoutData` →
`UCKeyTranslate(kUCKeyActionDisplay, (mods >> 8) & 0xff, LMGetKbdType(), 0, ...)`,
using Quartz codes 8/9 and 0/Shift/Option/Shift+Option, as GTK 4.24.1 does. It used
no NSApplication/display, taps, events, keystrokes, or clipboard. Spanish-ISO
returned C: `c C © space`; V: `v V √ ◊`, state zero and one character per level.
These C/V values were added to fixtures; validation checks that Hyprland 0.56.2's
keysym resolver finds them. **This is not an active Hyprland/fcitx5 map dump or
proof that a Lua binding was fixed.** Standard foot shortcuts are Ctrl+Shift+C/V;
Command→Super invokes remote Omarchy configuration. `--command-key control` is
explicit opt-in, never automatic, and foot would still require Shift.

### Insert and Omarchy universal copy/paste

In the inspected Omarchy installation, `clipboard.lua` maps Super+C/V to
Ctrl+Insert/Shift+Insert for terminal-tagged windows; browsers use Ctrl+C/V.
Checking only C/V therefore cannot diagnose that Lua toast. The sent map now
guarantees **Insert at Linux evdev 110 / XKB 118**, even when GDK reports Help,
NoSymbol, or omits that entry. Shift/Control remain unconsumed so Ctrl+Insert and
Shift+Insert arrive as complete shortcuts.

The physical map already sent Quartz Help (114) as Linux KEY_INSERT (110);
the canonical symbol aligns XKB with that contract. A Mac Help key acts as remote
Insert, not Linux Help. C/V, Command/Super, other modifiers, and Linux configuration
are not remapped/changed. The symbol is also available for compositor injection
on compact keyboards without a physical Insert key.

Regression tests make Help/NoSymbol/omission produce the same Insert row. The
real XKB validator reproduces Hyprland 0.56.2 resolution and checks unconsumed
Ctrl+Insert/Shift+Insert under both Option policies. Before the fix, Insert was
missing. A later read-only remote snapshot found fcitx5's Spanish map, which had
Insert; **it does not prove which map was active during the earlier Lua callback**.
Hyprland resolves against the seat keyboard, not necessarily the Gliff device map.
After restarting, the user confirmed Command+C/V work in foot with this fix.
That does not validate the rest of this checklist or another installation.

### Native capture QA (pending)

- [ ] Without flag/permission: GTK remains usable, clear notice, no prompts.
- [ ] `--request-capture-permissions` alone: one explicit request, no video/window/
      tap/host. Approve the right identity and restart. If no dialog/entry appears,
      retain preflight/executable/PID for diagnosis.
- [ ] With permission and a video click: active status and remote Command+Space
      without Spotlight; outside Gliff and in its address bar, Spotlight stays local.
- [ ] Shift+Esc with `--release-hotkey none`: local disarm, no remote Escape;
      Spotlight works afterwards; only another click rearms.
- [ ] Focus/window/tab/layout changes, reconnect/close while holding keys: no
      retained keys in the wrong session or automatic rearm.
- [ ] Shift/Ctrl/Command-click, repeated clicks, drag: modifiers precede Button-down,
      no intermediate Key-up, and Button-up survives pressure.
- [ ] Clipboard cancellation/sensitive-selection changes under pressure: withdrawal/
      cancellation is processed or the connection fails explicitly, never silently lost.
- [ ] Revoked permission, disabled/timed-out tap, slow UI, and queue/network
      saturation: fail open with notice and explicit recapture; remote releases best-effort.
- [ ] Suspend/resume and pending events on recapture: no delivery to another session.
      Repeat Command→Control, Option→Alt/AltGr, and clipboard policies.

### Mac→Linux QA (pending)

- [ ] Spanish/ISO: `ñ Ñ`, punctuation, `á é í ó ú ü`, accent+space.
- [ ] `--option-key altgr`: Option/Shift+Option symbols shown in Keyboard Viewer;
      `--option-key alt`: Linux Alt shortcuts, without promising those symbols.
- [ ] Switch Spanish↔US while connected, including while Shift is held.
- [ ] Left/right Control/Shift/Command/Option, both sides together; Command→Control
      with physical Control, releasing them in both orders.
- [ ] Hold a letter/Escape: one remote down, Linux autorepeat; holding Escape must
      not trigger a double-tap release hotkey.
- [ ] Shift+Esc releases without sending Escape; click recaptures; address input
      accepts local text; leaving video/Command+Tab must not leave held remote keys.
- [ ] Hold modifiers outside, enter and release inside; switch tabs, disconnect/
      reconnect, and close with a key held.
- [ ] Navigation/editing, available F-keys, keypad, Caps, Retina; synthetic-text
      clipboard both ways. Remote images/files remain unsupported; Mac→Linux keeps
      the inherited bridge.
- [ ] After restart, the original Gliff icon appears in Dock/Command+Tab.

## Workspace boundaries

Four local crates: `gliff` (UI/sessions), `gliff-proto` (wire protocol),
`gliff-transport` (SSH/transfers), and `gliff-sw` (VideoToolbox decoder, BGRA buffers,
BT.709 reference). Every local dependency stays inside this movable workspace.
It contains no server, probes, local Hyprland capture/control, Wayland, VA-API,
Vulkan/dmabuf, encoder, Omarchy theme, Arch packaging, or Linux CI. Protocol GPU
labels describe the **remote server**, not local dependencies. The original
`resources/gliff.svg` and MIT license from Gliff 0.4.0 are retained. `NOTICE`
documents attribution and the current Mac frameworks.

Implementation references:
[GTK 4.24.1 keycodes/Command META](https://github.com/GNOME/gtk/blob/4.24.1/gdk/macos/gdkmacosdisplay-translate.c),
[GTK layouts/four levels/dead keys/flagsChanged](https://github.com/GNOME/gtk/blob/4.24.1/gdk/macos/gdkmacoskeymap.c),
[GTK private NSPasteboard context](https://github.com/GNOME/gtk/blob/4.24.1/gdk/macos/gdkmacospasteboard.c#L461-L514),
[GDK translate_key](https://docs.gtk.org/gdk4/method.Display.translate_key.html),
[Apple CGEventSourceKeyState](https://developer.apple.com/documentation/coregraphics/cgeventsource/keystate(_:key:)),
[Apple CGEventTapCreate](https://developer.apple.com/documentation/coregraphics/cgevent/tapcreate(tap:place:options:eventsofinterest:callback:userinfo:)),
[Apple Accessibility](https://support.apple.com/guide/mac-help/allow-accessibility-apps-to-access-your-mac-mh43185/mac),
[Apple AXIsProcessTrustedWithOptions](https://developer.apple.com/documentation/applicationservices/1459186-axisprocesstrustedwithoptions),
[Apple Mach uptime](https://developer.apple.com/documentation/kernel/1462446-mach_absolute_time),
[Apple IOHIDSystem timestamps](https://github.com/apple-oss-distributions/IOHIDFamily/blob/main/IOHIDSystem/IOHIDSystem.cpp#L1563-L1564),
[Apple CGEventTimestamp](https://developer.apple.com/documentation/coregraphics/cgeventtimestamp),
[Apple applicationIconImage](https://developer.apple.com/documentation/appkit/nsapplication/applicationiconimage),
[XKB text format](https://xkbcommon.org/doc/current/keymap-text-format-v1-v2.html).
Physical codes were also checked against the Xcode SDK's `HIToolbox/Events.h`.

## Native decoder tests (no GUI)

`cargo test --offline --locked -p gliff-sw` tests bounded parsing and owned synthetic
bitstreams: IDR/P, parameter changes, multislice, macroblock cropping, BGRA stride,
and teardown/recreation. It does not access screens, cameras, or clipboard.
The parser limits each AU to 64 MiB, 4096 NALs, and 64 KiB per SPS/PPS. It accepts
progressive 8-bit 4:2:0 AVC I/P (Baseline/Main/High without custom scaling matrices)
and rejects coded sizes above 4096×4096, Dual420, B/field/FMO, and new formats
without IDR. SPS geometry must match StreamConfig before output allocation.
VideoToolbox may conceal partial corruption; a returned frame does not prove
every compressed byte was correct. Detected failures reset the decoder/request IDR.

The shim calls `VTDecompressionSessionDecodeFrame` with flags **0**: the callback
finishes before return, copying rows read-only into owned memory. GTK retains no
CVPixelBuffer, and temporal/asynchronous processing is not enabled. The Worker
already runs separately from GTK; synchronous decoding shares its network/input
thread and still needs real-session latency measurements.

On this host a restricted run returned `kVTVideoDecoderMalfunctionErr` (-12911),
while the same fixtures passed outside the sandbox. This is environment-sensitive,
not proof of denied TCC permission. Do not skip the test or change global
permissions: request approval for the specific headless run outside the sandbox
and retain both the failure and retry result in the report.
