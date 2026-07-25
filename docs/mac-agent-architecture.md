# macOS agent architecture

`remotex-agent` is the optional macOS endpoint for `protocol = "rxa"`, offered
as a dedicated-agent alternative to connecting the gateway directly to macOS
Screen Sharing over VNC. Its PSK authenticates reconnects directly instead of
returning to Screen Sharing's login gate. It captures the logged-in user's
display, encodes changed regions, and accepts input from the remotex gateway.
The agent and gateway share the protocol crate so their wire types and key
handling stay in sync.

Installation and operation are documented in
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Components

```
remotex gateway                         macOS
src/rxa.rs                              crates/rxa-agent
    │                                      │
    └──── Noise-encrypted rxa over TCP ────┤
                                           ├─ ScreenCaptureKit capture
                                           ├─ PNG/JPEG tile encoder
                                           ├─ Core Graphics input injection
                                           └─ menu bar UI and SMAppService

crates/rxa-proto: PSKs, handshake, framing, messages, and key mapping
```

- `rxa-proto` is cross-platform and contains everything both endpoints must
  interpret identically.
- `rxa-agent` is macOS-only. ScreenCaptureKit supplies frames and dirty
  rectangles; Core Graphics supplies cursor data and input events.
- `src/rxa.rs` connects the agent to the common browser/session interface. It
  relays encoded tiles without decoding and reconnects established sessions
  after transient network failures.

## Transport

The transport is TCP on port 52381 by default, protected with
`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`. The protocol version is included in
the Noise prologue. A generated `rxa...` pre-shared key authenticates both
endpoints and includes a checksum to catch transcription errors.

Noise transport frames carry length-prefixed `rxa-proto` messages:

- agent to gateway: desktop size, PNG/JPEG tiles, cursor shape, pasteboard text
  (on request or when the watched pasteboard changes), and heartbeat pongs;
- gateway to agent: mouse, wheel, and keyboard input, session control, clipboard
  read requests, writes and the watch toggle, and heartbeat pings.

The gateway translates these into the same browser protocol used by RDP and
VNC. It passes tile payloads through byte-for-byte. RXA ping/pong independently
checks the gateway-agent link; browser liveness is handled by the gateway's
shared WebSocket/session layer.

## Capture and encoding

The agent captures one selected physical display. ScreenCaptureKit frame
metadata identifies changed regions; those regions are divided into tiles and
encoded in order. PNG is used for flat or lossless-friendly content and JPEG
for image-like content.

Encoding is intentionally ordered on one worker. Parallel tile completion
could allow an older tile to overwrite a newer update to the same region.
Desktop-size changes are ordered with tiles for the same reason.

Cursor shapes are read separately from the framebuffer and sent with their
hotspot. The representation closest to the capture display's backing scale is
used.

## Input

Browser DOM key codes are mapped to macOS virtual key codes in `rxa-proto`.
Mouse coordinates are clamped to the captured display and injected with Core
Graphics. The agent requires Accessibility permission for input and Screen
Recording permission for capture.

## Lifecycle

The app registers its embedded LaunchAgent with `SMAppService` and runs in the
logged-in user's GUI session. Its menu bar item exposes status, settings, the
PSK, permission shortcuts, logs, and the login-item toggle.

Only one gateway may be connected. A new authenticated connection replaces the
old one. The shared browser heartbeat ends the engine under the same policy as
RDP and VNC: a missing pong expires it after about 60 seconds, while an orderly
WebSocket close allows 60 seconds for reattachment. Ending the RXA engine closes
the agent connection, stops capture, and changes the menu from "Sharing this
screen" to "No gateway connected."

Separately, the gateway sends the agent an RXA ping every five seconds. The
agent answers with an RXA pong; a silent agent link is reconnected after its
15-second deadline. The agent has a longer idle timeout as a final guard against
a silent gateway.

When an established agent link drops while the browser remains live, the
gateway reconnects with capped backoff. On recovery it requests a full repaint
and reports a resize only if the display dimensions changed. Input accumulated
during an outage is discarded. An initial connection or authentication failure
is reported immediately.

Saving settings restarts the agent so address, display, and key changes take
effect together. A deliberate quit stays stopped; crashes are restarted by
launchd.

## Constraints

- The agent mirrors a physical display and does not resize it from browser
  viewport reports.
- It runs only in a logged-in GUI session. It does not support the macOS login
  window or an unattended service mode.
- Screen Recording and Accessibility grants are tied to the app's signing
  identity. Ad-hoc-signed builds generally require approval again after an
  upgrade; stable Developer ID signing preserves identity.
- The clipboard has no change notification to subscribe to. AppKit's
  `NSPasteboard` posts none — unlike iOS `UIPasteboard` — so the agent polls
  `changeCount`, which is a counter rather than a pasteboard access, and reads
  the *contents* only when it moves. That keeps content reads to one per copy.
  Reads happen only while the gateway has enabled the watch, which it does only
  for a target with `clipboard = true`; otherwise the agent never looks.
- macOS 15.4+ governs those content reads. The general pasteboard asks the user
  by default, and only after the first alert does the app appear in System
  Settings › Privacy & Security › Paste from Other Apps, where "Always Allow"
  makes sync silent. The agent reads `NSPasteboard.accessBehavior` and reports
  it in the menu bar, since the property is read-only and the fix is the user's
  to apply. This is the sole reason the deployment target is 15.4.
- Audio is not part of the current protocol.
