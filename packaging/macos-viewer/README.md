# remotex viewer for macOS

`remotex-viewer.app` is the foreground macOS 26 client. It hosts the ordinary
remotex SPA in `WKWebView`; the SPA continues to own login, target selection,
session takeover, reconnects, the WebSocket, and canvas rendering.

The native shell replaces the floating web menu after an exact bridge
handshake. It provides:

- application-level keyboard capture before WebKit or menu shortcuts;
- Command translation that follows the remote (Control shortcuts for a non-Mac,
  unchanged Command shortcuts for a Mac);
- native `NSPasteboard` synchronization;
- native Remote menu commands for resize, resolution, clipboard sync, target
  switching, takeover, and logout.

The viewer and gateway frontend must have exactly the same product and bridge
versions. They are released together intentionally; compatibility shims are not
maintained.

Nothing to configure per target: the gateway's engine discovers whether the
remote is a Mac while connecting and tells the frontend, which passes the one
bit to the viewer.

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
packaging/macos-viewer/build-viewer-app.sh --no-dmg
open -n dist/remotex-viewer.app --args \
  --gateway http://127.0.0.1:52380
```

Always launch the packaged `.app` during development. `swift run`, a standalone
`swift build`, and directly launching the executable under `.build` bypass the
application bundle and can behave differently, including missing menus and
`Info.plist` metadata.

The address can also be changed under **remotex → Settings**.

## Keyboard capture

Capture is active only while the SPA reports a connected remote desktop.
Press **Control-Option-Command-Escape** to release it. Use the same Remote menu
item to capture it again.

Application-delivered shortcuts such as Command-W, Command-R, F5, and F11 can
therefore reach the remote. macOS-global shortcuts such as Command-Tab and
Command-Space remain owned by the operating system.

The default-on **Enable macOS Keyboard Overrides** item in the **Remote** menu
maps standard Command shortcuts to remote Control for Windows and Linux guests.
Uncheck it to send Command unchanged as remote Meta. The preference persists
across launches.

## Clipboard permission

Programmatic reads of the general pasteboard can produce the macOS
**Paste from Other Apps** prompt. Choose Allow and, if desired, change the
per-app behavior later in **System Settings → Privacy & Security**. Clipboard
synchronization is still gated by the selected target's `clipboard = true`.

Keyboard capture uses an AppKit local event monitor and only sees events sent
to the viewer's own window. It does not require Accessibility or Input
Monitoring permission, and code signing does not change either permission
rule.
