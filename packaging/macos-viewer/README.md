# remotex.app for macOS

`remotex.app` is a native macOS 26 **shell** around the browser client. It starts
`Contents/MacOS/remotex-gateway` on a fixed loopback port, authenticates with a
random token delivered through a private pipe, shows the SPA that gateway serves in
a `WKWebView`, and stops the gateway when the app quits.

There is one gateway, in this bundle. A gateway elsewhere is reached with a
browser.

The app owns what a page cannot:

- starting, supervising, and stopping the embedded gateway;
- editing its configuration, validated before it is written;
- application-level keyboard capture ahead of menu shortcuts, which is what sends
  ⌘Q, ⌘W, ⌘T and the rest to the guest instead of to this app or to WebKit;
- native `NSPasteboard` synchronization in both directions;
- the window, and the sizing behind **Resize to Display**;
- the menu bar, whose Remote and Display items stand in for the floating menu the
  client hides inside this window.

Everything else — the login the gateway refuses, the target picker, the desktop,
the claim, reconnects, takeover, tiles, cursor, audio, video — is the client's,
and is the same code a browser runs. The app holds no session and speaks no wire
protocol, so there is no version pair to keep in step: the client and the gateway
are built together and shipped in one bundle.

See [`../../docs/macos-viewer.md`](../../docs/macos-viewer.md) for the handshake,
the cookie, the bridge, the process contract, and the instance directory.

Nothing to configure per target beyond the target itself: the gateway's engine
discovers whether the remote is a Mac while connecting and sends the one bit that
decides the keyboard convention.

## Build

```sh
(cd frontend && bun run check && bun test src)
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh
```

The script builds both executables — the Swift app and the `remotex` gateway binary —
plus the SPA it shows (so `bun` is required), and signs the nested executable first. It
ends with the installable disk image, whose file name keeps the
`remotex-viewer-<version>` prefix because `remotex-<version>-macos-arm64.tar.gz` is
already the CLI gateway's release asset. Both land in `dist/`: the image no longer
consumes the bundle it was made from.

Run bare, that command is the one `release.yml` runs — no flags, no environment — so
a local result and a release are not two commands apart. `--no-dmg` stops after the
bundle for a fast loop; it produces the same bundle, since signing happens before the
image.

The build is ad-hoc signed by default, which is enough for a locally built personal
app. Set `CODESIGN_IDENTITY` explicitly to use another identity. A build downloaded by
someone else needs Developer ID signing and notarization for the normal Gatekeeper
launch path.

For development, give the run its own instance:

```sh
packaging/macos-viewer/build-viewer-app.sh
open -n dist/remotex.app --args --instance-dir "$PWD/tmp/app-instance"
```

`--instance-dir` is the only GUI-launch argument and isolates the config and the
gateway log from the real instance in `~/Library/Application Support/remotex`.
Delete the directory to start over.

To run a second instance *without* a flag, stamp out a bundle of its own:

```sh
packaging/macos-viewer/make-instance-bundle.sh remotex-work ~/Pictures/work.png
```

It copies the app, gives it its own identifier, name and icon, and re-signs it into
`~/Applications`. The instance directory follows `CFBundleName`, so the variant is
double-clickable with nothing to pass — see docs/macos-viewer.md, "Running more than
one instance". It is a copy, so re-run it after each update.

Each launch starts the gateway and reaches the client's target picker, or a launch
screen carrying the gateway's explanation. Add targets through **Remote ›
Configuration…**, which validates before saving and restarts the gateway.

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
Uncheck it to send Command unchanged as remote Meta. The preference is the
client's and persists across launches with the client's other two.

The mapping itself is the client's as well (`frontend/src/macKeys.ts`), shared with
the browser. The one difference here is a bigger table: ⌘L, ⌘N, ⌘O, ⌘R, ⌘T and ⌘W
are mapped only in this app, because only this app is given them.

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
