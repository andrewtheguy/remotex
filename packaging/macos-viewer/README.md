# remotex.app for macOS

`remotex.app` is the foreground macOS 26 client. Its first screen chooses between:

- **On This Mac:** start `Contents/MacOS/remotex-gateway` on an ephemeral
  loopback port, authenticate with a random bearer token delivered through a
  private pipe, and stop the gateway when the app quits.
- **Somewhere Else:** connect to a deployed gateway using its address and login.

Both choices use the same config, target, session, and WebSocket APIs and differ
only in the credential header on protected requests. The client owns:

- starting, supervising, and stopping the embedded gateway;
- editing embedded-gateway configuration, validated before it is written;
- target selection;
- the session socket, including reconnects, takeover, and the single-slot claim;
- the remote surface, which is a `WKWebView` showing
  `Contents/Resources/canvas` — the app's own page, served from its own loopback
  listener and handed wire frames. It presents the desktop at the remote point
  size (`pixels / remote scale`) without fit-to-window scaling. Not the SPA, and
  not a web UI on the embedded gateway, which still serves none;
- keyboard input, and the window sizing behind **Resize to Display**;
- application-level keyboard capture ahead of menu shortcuts;
- Command translation that follows the remote (Control shortcuts for a non-Mac,
  unchanged Command shortcuts for a Mac);
- native `NSPasteboard` synchronization;
- Remote menu commands for refresh, resize, clipboard, target switching,
  takeover and, for the embedded gateway, configuration and restart.

The client checks `protocolVersion` from `GET /api/config` before either gateway
opens a session. An embedded mismatch means a broken bundle; a remote mismatch
means incompatible deployments. Both are reported before login or target selection.
See [`../../docs/macos-viewer.md`](../../docs/macos-viewer.md) for both gateway
paths, the embedded process contract, and the instance directory.

Nothing to configure per target beyond the target itself: the gateway's engine
discovers whether the remote is a Mac while connecting and sends the one bit that
decides the keyboard convention.

## Build

```sh
(cd frontend && bun run check && bun test src)
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
```

The script builds both executables — the Swift app and the `remotex` gateway binary —
plus the canvas page (so `bun` is required), and signs the nested executable first. Omit `--no-dmg` for the installable disk image, whose
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

`--instance-dir` is the only GUI-launch argument and isolates config, gateway log,
and preferences from the real instance in
`~/Library/Application Support/remotex`. Delete the directory to start over.
Gateway choice remains on the app's home screen.

To run a second instance *without* a flag, stamp out a bundle of its own:

```sh
packaging/macos-viewer/make-instance-bundle.sh remotex-work ~/Pictures/work.png
```

It copies the app, gives it its own identifier, name and icon, and re-signs it into
`~/Applications`. The instance directory follows `CFBundleName`, so the variant is
double-clickable with nothing to pass — see docs/macos-viewer.md, "Running more than
one instance". It is a copy, so re-run it after each update.

The home screen appears on every launch with the previous gateway choice selected.
The embedded branch starts its gateway and reaches the picker or a launch screen
carrying the gateway's explanation. Add embedded targets through **Remote ›
Configuration…**, which validates before saving. Remote targets are configured on
their gateway, so the local configuration command is hidden on that branch.

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
synchronization is still gated by the selected target's `clipboard = true`, set on
the gateway currently in use.

Keyboard capture uses an AppKit local event monitor and only sees events sent
to the viewer's own window. It does not require Accessibility or Input
Monitoring permission, and code signing does not change either permission
rule.
