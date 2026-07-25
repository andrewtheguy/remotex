# remotex viewer for macOS

`remotex-viewer.app` is the foreground macOS 26 client. It hosts the ordinary
remotex SPA in `WKWebView`; the SPA continues to own login, target selection,
session takeover, reconnects, the WebSocket, and canvas rendering.

The native shell replaces the floating web menu after an exact bridge
handshake. It provides:

- application-level keyboard capture before WebKit or menu shortcuts;
- target-OS-aware Command translation (Control shortcuts for Windows/Linux,
  unchanged Command shortcuts for macOS);
- native `NSPasteboard` synchronization;
- native Remote menu commands for resize, clipboard sync, target switching,
  takeover, and logout.

The viewer and gateway frontend must have exactly the same product and bridge
versions. They are released together intentionally; compatibility shims are not
maintained.

Set `os = "windows"`, `"macos"`, or `"linux"` on every gateway target. The
viewer uses that explicit metadata instead of guessing from RDP, VNC, or RXA.

## Build

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
```

Omit `--no-dmg` for the installable disk image. The build is ad-hoc signed by
default, which is enough for a locally built personal app. Set
`CODESIGN_IDENTITY` explicitly to use another identity. A build downloaded by
someone else needs Developer ID signing and notarization for the normal
Gatekeeper launch path.

For development against a specific gateway:

```sh
swift run --package-path apps/remotex-viewer remotex-viewer \
  --gateway http://127.0.0.1:52380
```

The address can also be changed under **remotex → Settings**.

## Keyboard capture

Capture is active only while the SPA reports a connected remote desktop.
Press **Control-Option-Command-Escape** to release it. Use the same Remote menu
item to capture it again.

Application-delivered shortcuts such as Command-W, Command-R, F5, and F11 can
therefore reach the remote. macOS-global shortcuts such as Command-Tab and
Command-Space remain owned by the operating system.

## Clipboard permission

Programmatic reads of the general pasteboard can produce the macOS
**Paste from Other Apps** prompt. Choose Allow and, if desired, change the
per-app behavior later in **System Settings → Privacy & Security**. Clipboard
synchronization is still gated by the selected target's `clipboard = true`.

Keyboard capture uses an AppKit local event monitor and only sees events sent
to the viewer's own window. It does not require Accessibility or Input
Monitoring permission, and code signing does not change either permission
rule.
