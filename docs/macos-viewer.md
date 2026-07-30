# macOS viewer

`remotex-viewer.app` is a native macOS 26 client for the gateway's HTTP and
WebSocket protocol. It owns login, target selection, session recovery, tile
decoding, Metal rendering, input, clipboard synchronization, and audio playback.
It contains no `WKWebView` and shares protocol behavior, not implementation,
with the browser client.

The viewer has no RDP, VNC, or RXA implementation. Engine-specific behavior is
reported by the gateway, with resize policy as the only client-side branch.
Whether the remote is a Mac is likewise discovered from the gateway's
`remoteOs` message and affects only keyboard conventions.

## Protocol compatibility

The viewer and gateway ship independently. Before opening a session, the viewer
requests `GET /api/config` and requires its `protocolVersion` to match
`PROTOCOL_VERSION` in `src/protocol.rs`. A mismatch is shown on the login screen.

The version covers client messages, control messages, and binary frame layouts.
Unknown additive control messages are ignored, but a change that makes an older
peer fail without a useful error requires a version bump.

Contract tests protect both sides:

- `ProductInfoTests` compares the Swift protocol version with the Rust constant;
- `WireContractTests` compares the Rust message tags with the tags handled by
  the viewer;
- `ServerMessage` tests reuse the JSON literals pinned by the Rust protocol
  tests.

## Entry and session lifecycle

Entry has two steps:

1. **Server** validates the address, requests `/api/config`, checks the protocol,
   and requests `/api/auth/status`. The address is remembered only after the
   gateway answers.
2. **Login** submits credentials. This step is skipped while the stored cookie
   remains valid.

Changing the gateway returns to the server step. Logging out keeps the current
address and returns to login. A gateway restart invalidates its in-memory login
sessions, so a later `401` also returns to login.

The viewer keeps login and session ownership separate:

- the login cookie authorizes the gateway;
- the claim token owns the program's one active session slot.

`SessionStateMachine` implements claim, attach, reconnect, takeover, and return
to login as a pure state machine. Network reconnects use capped exponential
backoff up to 15 seconds. A busy slot and a session taken over by another client
wait for an explicit user decision because resolving either case may evict the
current owner.

The reconnect backoff resets after a control message proves that the connection
is usable, not merely when the WebSocket opens. Any interruption clears the
framebuffer and releases held input; the gateway requests a full repaint when a
client attaches again.

## Rendering

The viewer maintains one `MTLTexture` at the remote framebuffer's pixel size.
Tiles overwrite their rectangles with `replaceRegion`, and a paused `MTKView`
redraws after every complete batch.

The gateway may replace a tile payload with a reference to one of 256 cache
slots. The viewer stores encoded WebP payloads in those slots and re-decodes a
payload when it is referenced. The gateway chooses which slot to overwrite. A
missing slot or a cached payload that cannot decode sends `cacheReset` and drops
that record.

Frames are processed strictly in arrival order. The socket loop completes every
decode and upload for one frame before receiving the next, so a resize cannot
overtake tiles in the preceding coordinate space and older tiles cannot land
over newer ones.

`TileDecoder` leaves decoded rows in raster order. The Metal shader flips the
texture's vertical coordinate because Metal clip space and the desktop texture
use opposite vertical origins.

The framebuffer view is laid out at the remote's point size:

```text
point size = framebuffer pixels / remote backing scale
```

The window's screen then rasterizes that view at its own backing scale. A remote
larger than the available area scrolls; the viewer does not zoom or fit the
framebuffer to the window.

## Display and resize behavior

`ViewportPolicy` separates two questions: whether this session may resize the
remote, which the gateway answers, and whether the window drives it, which this
viewer does.

Permission comes from the `connected` message and, for RXA, the active display:

| Target | May resize |
|---|---|
| RDP or VNC with `resize` | yes |
| RXA with `resize`, private display active | yes |
| Any other case | no — no viewport request is ever sent |

RXA permits it only for a display created by the agent; a Mac-owned display is
never resized by the viewer, and switching onto one withdraws the permission
mid-session.

The three View menu items are one decision:

- **Auto Resize** hands the remote's size to the window, which then follows every
  change, debounced and deduplicated. Off by default, per session: it is not
  remembered, and every connection starts manual.
- **Resize to Window** asks the remote to adopt the viewer's available size, once.
- **Resize to Display** changes the local window so the current remote desktop
  fits at its point size; it sends nothing to the gateway.

All three remain in the menu and are disabled when they do not apply. The two
one-shots are disabled while **Auto Resize** is on: one is what it does
continuously, and the other cannot fit a window to a desktop that is already
fitting itself to the window.

RXA is the only engine that reports individually selectable displays. The
Display menu sends `selectDisplay` and follows the `active` flag returned by the
gateway; it does not infer selection locally. RDP and VNC expose one framebuffer
and therefore no display choice.

### Viewport measurement

The viewer observes the scroll view's `frameDidChange` and reports the scroll
view's size. It does not use `NSClipView.boundsDidChange`, which represents
scrolling rather than window resizing, or the clip view's size, which changes
when legacy scrollbars appear and can cause resize oscillation.

Reports are not sent before initial layout or while the target picker is active.
Each axis is clamped to `1...u16.max`. Starting a new target clears both the
policy and outbound-queue deduplication so the first `connected` event can resend
an already measured viewport.

### Window chrome

The desktop toolbar is hidden while a remote is displayed. The remote surface
stays inside the window's safe area so the title bar never overlaps interactive
remote content. The window remains titled because an untitled window cannot
become key and accept keyboard input; full screen is the chrome-free mode.

## Keyboard and pointer input

While a connected desktop is focused, an AppKit local event monitor consumes
`keyDown`, `keyUp`, and `flagsChanged` before application menu equivalents.
Remote-menu commands therefore have no keyboard shortcuts. macOS-global
shortcuts such as Command-Tab and Command-Space remain local because the
application never receives them.

The Edit menu remains available for text fields and supplies the standard
copy/paste/cut/select-all actions through the responder chain. `ViewerMenus`
restores it when SwiftUI rebuilds the main menu.

Keyboard translation depends on `remoteOs`:

- for a non-Mac remote, standard Command shortcuts become remote Control
  shortcuts, a bare Command taps remote Meta, and other Command chords remain
  Meta chords;
- for a Mac remote, Command remains remote Meta for every chord.

The default-on **Enable macOS Keyboard Overrides** preference disables the
Command-to-Control translation when turned off. `PressedInput` tracks every
pressed code and releases them on focus loss, window deactivation, target
switch, socket closure, takeover, or teardown.

Before the first `cursor` message, the viewer hides the local pointer because an
engine may already composite its cursor into the framebuffer. After a cursor
message, the viewer renders the remote shape; a null image uses a local arrow so
the pointer remains visible. Hotspots arrive in remote pixels and are converted
to points.

AppKit scroll deltas are inverted on both axes to match DOM wheel direction.
Trackpad deltas pass through directly; line-based wheel deltas are scaled for the
Mac agent's line conversion.

## Clipboard

For a connected target with `clipboard`, the viewer polls
`NSPasteboard.changeCount`:

- local text changes send the ordinary `clipboard` message;
- unsolicited remote changes write to `NSPasteboard`;
- echo guards prevent either direction from bouncing the same value back.

A response to an explicit **Clipboard…** fetch fills the panel but does not
write to the local pasteboard. The panel's Copy action is the consent boundary.
The synchronizer uses a local request token so a late response cannot populate a
closed or replaced panel.

Clipboard values are capped at 64 KiB in either direction and refused rather
than truncated. Command-V queues the current local pasteboard value before
sending the translated remote paste chord. Programmatic reads follow macOS's
**Paste from Other Apps** permission, while `clipboard = true` remains the
gateway-side boundary.

## Audio

**Remote → Enable Audio** is available when `connected.audio` is true. The
gateway owns the wire format and bounded audio queue; the viewer owns decoding
and playback scheduling.

The viewer decodes bare Opus packets with `AVAudioConverter` and
`kAudioFormatOpus`; it needs neither a container nor a vendored decoder.
`AVAudioConverter` does not apply the `OpusHead` pre-skip, so `OpusDecoder`
discards the reported priming frames itself.

Decoded buffers are scheduled explicitly on an `AVAudioPlayerNode`.
`AudioSchedule` uses the same 0.1-second start cushion and 0.3-second latency
ceiling as the browser. When the ceiling is exceeded, the viewer stops the
player, discards its queued audio, and restarts the timeline at the cushion.

Audio remains enabled in view-only mode because it sends no input to the remote.
An ordinary reconnect reasserts the subscription; a target switch clears it.
The output follows the Mac's default device, rebuilding the engine after
`AVAudioEngineConfigurationChange`.

The viewer can report local decoder or output failures. It cannot distinguish a
quiet remote from an RDP server that never opens its audio channel; the gateway
log carries that diagnosis.

## Networking

Plain HTTP and `ws://` gateways are allowed for private-network deployments, so
the app bundle uses `NSAllowsArbitraryLoads`.

The viewer attaches the login cookie to the WebSocket upgrade explicitly with
`httpShouldHandleCookies` disabled. This also handles a `Secure` cookie issued
behind a TLS-terminating proxy when the socket uses `wss`.

`URLSessionWebSocketTask.maximumMessageSize` is set to 16 MiB. Exceeding the
limit ends the socket rather than dropping one frame.

## Build and QA

Run the tests, build the packaged app, and launch QA with isolated settings:

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
open -n dist/remotex-viewer.app --args \
  --settings qa --gateway http://127.0.0.1:<test-port>
```

`--settings qa` uses a separate defaults suite and an ephemeral cookie jar. Clear
that suite with:

```sh
defaults delete remotex-viewer.qa
```

Always validate the packaged `.app`; `swift run`, standalone `swift build`, and
the executable under `.build` bypass bundle menus and `Info.plist` behavior.

For socket-level diagnostics, `--probe` attaches to a gateway and prints
received control and frame information:

```sh
REMOTEX_PROBE_USERNAME=… REMOTEX_PROBE_PASSWORD=… \
  dist/remotex-viewer.app/Contents/MacOS/remotex-viewer \
  --probe --gateway http://127.0.0.1:52380 \
  --probe-target mac --probe-seconds 90
```

Automated tests cover message decoding, frame parsing, tile ordering, geometry,
audio framing, schedule arithmetic, and Opus fixtures produced by the gateway.
Audio playback still requires manual QA. The in-process tone harness provides a
repeatable source without a Windows host:

```sh
cargo test --lib serve_a_test_tone -- --ignored --nocapture
open -n dist/remotex-viewer.app --args \
  --settings qa --gateway http://127.0.0.1:<test-port>
```

Enable audio during a quiet phase and verify that the tone starts, stops, and
returns without another action. Use a source that announces its left and right
channels when checking stereo order.

See
[`packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for signing, packaging, permissions, and development launch details.
