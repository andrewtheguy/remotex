# macOS viewer

`remotex-viewer.app` is a macOS 26 SwiftUI shell around the ordinary remotex
SPA. It deliberately keeps the web boundary intact:

- the SPA owns login, target selection, status interstitials, takeover,
  reconnects, WebSocket lifetime, canvas rendering, pointer input, and touch;
- native code owns application-delivered keyboard events, Mac shortcut
  translation, `NSPasteboard`, and the Remote menu;
- all remote actions re-enter the existing browser-to-gateway message path.

There is no RDP, VNC, or RXA branch in the viewer. It receives capability
booleans such as `canResize` and `canClipboard`. Once a native key becomes the
existing `{ type: "key", code, pressed, caps }` message, differences belong to
the engine adapters and are tested as backend conformance.

Every target declares `os = "windows"`, `"macos"`, or `"linux"`. That metadata
travels with the generic connected-session status; it is not inferred from
RDP, VNC, or RXA.

## Host bridge

The viewer injects `window.__remotexNativeHost` at document start and installs
the reply-capable `remotexNative` WebKit script-message handler. The frontend
requires exact agreement on:

- the integer native-host bridge version;
- the product version shared with the Cargo workspace.

Only a successful handshake hides the web floating menu and disables the DOM
keyboard path. A missing or mismatched handshake leaves the web UI intact while
the viewer presents an incompatibility error.

Frontend-to-native messages publish:

- `ready`;
- the current screen, connection state, target, and capability snapshot;
- remote clipboard text.

Native-to-frontend commands cover:

- key press/release and release-all;
- clipboard send/request;
- resize, switch target, takeover, and logout.

Native commands call the functions owned by `useRemoteDesktop`; the viewer does
not open a second session WebSocket.

## Keyboard

An AppKit local event monitor consumes `keyDown`, `keyUp`, and `flagsChanged`
before WebKit and application menu equivalents while the SPA reports a
connected desktop. macOS virtual keycodes map to the same physical DOM `code`
values the browser protocol already uses.

For Windows and Linux guests, standard Mac Command shortcuts map to remote
Control shortcuts. A bare Command taps remote Meta, and other Command chords
are sent as remote Meta chords. For a macOS guest, Command remains Meta for
every chord, so Command-V arrives as Command-V rather than Control-V. This is
selected by target OS, not by backend. **Control-Option-Command-Escape**
releases capture; the Remote menu captures it again.

Focus loss, takeover, navigation, target switching, and capture release all
send the protocol's release-all command. macOS-global shortcuts that never
reach the application, including Command-Tab and Command-Space, remain local.

## Clipboard

While a connected target advertises `canClipboard`, native code polls the
general pasteboard's change count:

- a local text change queues the ordinary browser `clipboard` message;
- remote clipboard text writes to `NSPasteboard`;
- echo guards keep either direction from bouncing the same value back.

Command-V queues the current pasteboard value before the translated remote
Control-V events. Commands share one serial JavaScript queue, preserving their
order without backend-specific sleeps.

Programmatic pasteboard reads follow macOS's Paste from Other Apps permission.
The target's `clipboard = true` remains the server-side security boundary.

## Navigation security

Only HTTP and HTTPS gateway roots are accepted. Main-frame navigation stays on
the configured scheme/host/port; external links open in the default browser.
Bridge messages are accepted only from the trusted main frame.

Plain HTTP is allowed inside `WKWebView` because direct private-network gateway
deployments are supported. The Info.plist exception is scoped to web content,
not arbitrary native URL loading.

## Build and test

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
```

See [`../packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for installation, signing, permissions, and development launch arguments.
