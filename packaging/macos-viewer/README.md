# remotex viewer for macOS

`remotex-viewer.app` is the foreground macOS 26 client. It speaks the gateway's
HTTP and WebSocket protocol directly — there is no embedded web view — and owns:

- gateway selection, login, and target selection;
- the session socket, including reconnects, takeover, and the single-slot claim;
- Metal framebuffer rendering at one texel per device pixel;
- pointer and keyboard input, plus the remote pointer shape;
- application-level keyboard capture ahead of menu shortcuts;
- Command translation that follows the remote (Control shortcuts for a non-Mac,
  unchanged Command shortcuts for a Mac);
- native `NSPasteboard` synchronization;
- Remote menu commands for refresh, resize, clipboard, target switching,
  takeover, and logout.

The viewer and the gateway are separate artifacts and do not have to be released
together. `GET /api/config` carries a `protocolVersion` the viewer checks before
opening a session; a mismatch is refused with the reason shown on the login
screen. See [`../../docs/macos-viewer.md`](../../docs/macos-viewer.md) for the
protocol details.

Nothing to configure per target: the gateway's engine discovers whether the
remote is a Mac while connecting and sends the one bit that decides the keyboard
convention.

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
  --settings qa --gateway http://127.0.0.1:52380
```

`--settings qa` isolates defaults and cookies from normal use. `--gateway` only
prefills the first screen's address field. Getting in is two
steps: **Continue** validates the gateway (reachable, and speaking a protocol
this build knows), then the credentials. Step two is skipped while the login
cookie is still good, and the last address that answered is what the next launch
starts from. There is no Settings window.

Clear the QA defaults with `defaults delete remotex-viewer.qa`.

Always launch the packaged `.app` during development. `swift run`, a standalone
`swift build`, and directly launching the executable under `.build` bypass the
application bundle and can behave differently — missing menus, no
`Info.plist` version, and, since the viewer does its own networking, **no App
Transport Security exception**, so a plain-HTTP gateway fails only in the
unbundled build.

## Keyboard capture

Capture is active only while a connected remote desktop is painting.

Application-delivered shortcuts such as Command-W, Command-R, F5, and F11
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
