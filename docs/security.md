# Permissions, privacy, and limits

## Normal use

Opening `Gliff.app` does not request Accessibility or install native capture.
SSH uses `/usr/bin/ssh`: prepare keys/agent and verify the host key in Terminal
first. Gliff does not store passwords. Remembered connections and local
configuration are not encrypted secrets. Connect only trusted hosts.

Clipboard sharing is enabled by default; a connected host can request advertised
contents without a local paste. Disable both directions with:

```sh
/Applications/Gliff.app/Contents/MacOS/Gliff --no-clipboard user@server
```

Linux→Mac supports UTF-8 text up to 1 MiB. A crash can leave private Mac→Linux
spool files in `GLIFF_CLIPBOARD_DIR` or `~/.cache/gliff/clipboard`; clean up only
with every instance closed. Capture diagnostics do not log typed text. Avoid
sharing logs/configuration that identify your hosts.

## Shortcut capture: opt-in only

The bundle identifier is `io.github.vidsanchez.gliff-macos`; its ad-hoc signature
does not authenticate a developer. An update/re-sign may require authorizing the
correct identity again. Permissions are not changed automatically.

1. Make an explicit, separate request without a host or other flags. This does
   not create a window/event tap:
   ```sh
   /Applications/Gliff.app/Contents/MacOS/Gliff --request-capture-permissions
   ```
2. Review System Settings → Privacy & Security → Accessibility manually. Use the
   printed executable path/PID; macOS may attribute the launch to Terminal. An
   initial `false` does not mean denial: the prompt is asynchronous.
3. Restart with `--capture-system-shortcuts user@server` and click connected
   video. Only `Native keyboard active` indicates active capture.
4. **Shift+Esc disarms locally**, even with `--release-hotkey none`. Lost focus,
   disconnects, layout changes, full queues, and timeouts also disarm without
   automatic rearming. Remote key releases are best-effort when the network fails.

Secure Input and protected shortcuts can prevent capture; not every shortcut is
promised. Do not indiscriminately grant other permissions or disable system
protections. [Official Accessibility help](https://support.apple.com/guide/mac-help/mh43185/mac).
See [development](development.md) for diagnostics and checklists.

## Report a problem

Open an issue without clipboard data, SSH keys, or keystrokes. Include the app
version, macOS version, architecture, `build-report.json`, status-bar text, and
reproduction steps using synthetic text. Do not connect a sensitive server to
reproduce a problem.
