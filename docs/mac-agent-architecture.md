# macOS agent architecture

`remotex-agent` is the macOS endpoint for `protocol = "rxa"`. It captures the
logged-in user's display, encodes changed regions, and accepts input from the
remotex gateway. The agent and gateway share the protocol crate so their wire
types and key handling stay in sync.

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

- agent to gateway: desktop size, PNG/JPEG tiles, and cursor shape;
- gateway to agent: mouse, wheel, and keyboard input;
- both directions: handshake, ping/pong, and session control.

The gateway translates these into the same browser protocol used by RDP and
VNC. It passes tile payloads through byte-for-byte.

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
old one. When an established link drops, the gateway reconnects with capped
backoff. Application ping/pong detects half-open connections faster than TCP
keepalive. On recovery the gateway requests a full repaint and reports a resize
only if the display dimensions changed. Input accumulated during an outage is
discarded. An initial connection or authentication failure is reported
immediately.

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
- Clipboard and audio are not part of the current protocol.
