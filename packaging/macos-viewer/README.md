# remotex.app for macOS

`remotex.app` is the foreground macOS 26 client, and it carries the gateway it
talks to. There is no server to install and no address to enter: the app starts
`Contents/MacOS/remotex-gateway` on an ephemeral loopback port at launch,
authenticates to it with a token printed on a pipe, and stops it when the app quits.
There is no embedded web view either. It owns:

- starting, supervising and stopping its own gateway;
- editing that gateway's configuration, validated before it is written;
- target selection;
- the session socket, including reconnects, takeover, and the single-slot claim;
- Metal framebuffer rendering at one texel per device pixel;
- pointer and keyboard input, plus the remote pointer shape;
- application-level keyboard capture ahead of menu shortcuts;
- Command translation that follows the remote (Control shortcuts for a non-Mac,
  unchanged Command shortcuts for a Mac);
- native `NSPasteboard` synchronization;
- Remote menu commands for refresh, resize, clipboard, target switching,
  takeover, configuration, and restarting the gateway.

Both halves ship in one bundle, so the `protocolVersion` in `GET /api/config` is
still checked but now catches a broken build rather than an old server; a mismatch is
reported on the launch screen. See
[`../../docs/macos-viewer.md`](../../docs/macos-viewer.md) for the embedded gateway's
contract, the two pipes, and the instance directory.

Nothing to configure per target beyond the target itself: the gateway's engine
discovers whether the remote is a Mac while connecting and sends the one bit that
decides the keyboard convention.

## Build

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
```

The script builds both executables — the Swift app and the `remotex` gateway binary —
and signs the nested one first. Omit `--no-dmg` for the installable disk image, whose
file name keeps the `remotex-viewer-<version>` prefix because
`remotex-<version>-macos-arm64.tar.gz` is already the CLI gateway's release asset.

The build is ad-hoc signed by default, which is enough for a locally built personal
app. Set `CODESIGN_IDENTITY` explicitly to use another identity. A build downloaded by
someone else needs Developer ID signing and notarization for the normal Gatekeeper
launch path.

For development, give the run its own instance:

```sh
packaging/macos-viewer/build-viewer-app.sh --no-dmg
open -n dist/remotex.app --args --instance-dir "$PWD/tmp/app-instance"
```

`--instance-dir` is the only argument the app takes and it isolates everything —
config, gateway log, preferences — from the real instance in
`~/Library/Application Support/remotex`. Delete the directory to start over. There is
deliberately no way to point the app at another gateway, on the command line or in the
UI.

Getting in is one step and it is automatic: the app starts its gateway and lands on
the target picker, or on a launch screen carrying the gateway's own explanation.
Targets are added in **Remote › Configuration…**, which checks the file before saving
it. There is no Settings window.

Always launch the packaged `.app` during development. `swift run`, a standalone
`swift build`, and directly launching the executable under `.build` bypass the
application bundle and behave differently — missing menus, no `Info.plist` version, no
App Transport Security exception, and **no bundled gateway at all**, so the app comes
up reporting that this copy is incomplete.

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
synchronization is still gated by the selected target's `clipboard = true`, set in the
app's own configuration.

Keyboard capture uses an AppKit local event monitor and only sees events sent
to the viewer's own window. It does not require Accessibility or Input
Monitoring permission, and code signing does not change either permission
rule.
