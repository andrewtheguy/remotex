# Architecture

remotex is a single-user gateway for RDP, VNC, and the optional RXA macOS
agent. A Rust backend owns the remote protocol session and exposes one common
HTTP/WebSocket interface to the React SPA and to `remotex.app`, the native macOS
client — which runs a gateway of its own rather than reaching a deployed one (see
[`macos-viewer.md`](macos-viewer.md)).

## Data path

```text
browser SPA, or remotex.app over loopback
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary image batches, Opus audio
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through IronRDP
                                         ├─ built-in RFB 3.8 client
                                         └─ RXA macOS agent
```

RDP and VNC frames are decoded in the gateway and encoded as WebP tiles. The
RXA agent sends browser-ready WebP, which the gateway relays unchanged. RDP
audio is converted from PCM to Opus and sent on the same WebSocket independently
of the tile encoder and batching queue.

## Constraints

- There is one active session slot per gateway instance. A new client may force a
  takeover and evict the previous holder; concurrent and shared sessions are not
  supported.
- An `rxa` target has a *second*, independent slot at the far end: the macOS agent
  serves one session at a time and a gateway claims it (see
  [`docs/mac-agent-architecture.md`](mac-agent-architecture.md)). That slot is
  keyed on the claim's session id and not on the gateway's key, so being
  authorized to reach a Mac and holding its session are separate questions. The
  first of those is a list on the Mac (`authorized_gateways`), so several gateways
  can be entitled to one Mac while exactly one holds it.
- Remote credentials remain in the server-side TOML configuration.
- Clients speak only the remotex protocol and never implement RDP, RFB, or RXA.
- Protocol engines prefer broadly supported baseline features over
  server-specific behavior.

## Backend

| Module | Responsibility |
|---|---|
| `server.rs`, `auth.rs` | HTTP routes, SPA serving, login sessions |
| `session.rs` | target selection, takeover, detach, and reattach |
| `ws.rs`, `protocol.rs`, `wire.rs` | WebSocket bridge and client wire format |
| `rdp.rs` | RDP connection, framebuffer, input, clipboard, audio, resize |
| `vnc.rs` | RFB connection, framebuffer, input, cursor, clipboard, resize |
| `rxa.rs` | authenticated Mac-agent connection and tile relay |
| `encode.rs`, `tiles.rs` | ordered WebP encoding and change detection |
| `audio.rs`, `opus_stream.rs`, `rdp_audio.rs` | PCM queue, Opus encoding, MS-RDPEA |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine consumes `ClientMsg` input and emits the same `ServerMsg` stream.
RDP and VNC pass dirty pixels through the ordered encoder before reaching that
boundary. RXA already carries encoded tiles.

Ordering is a correctness requirement throughout the frame path. Tiles replace
rectangles without delta state, and a resize changes their coordinate space.
The encoding and outbound queues therefore keep tiles, resizes, and cursor
updates in source order even when individual tile encodes finish concurrently.

## Session lifecycle

Authentication and desktop ownership are separate:

1. `POST /api/auth/login` creates the login cookie.
2. `POST /api/session` claims the single slot. A conflicting claim returns
   `409` unless the request reclaims its token or forces takeover.
3. `/ws?session=<token>` attaches to the slot and reports either the target
   picker or the current connected target.
4. `connect` starts the selected engine. `disconnect` stops it and returns to
   the picker.
5. Losing the WebSocket detaches the client. The engine remains available for a
   60-second reattach grace period while frames are discarded.
6. Logging out ends the login and session immediately, closes the engine, and
   releases the claim.

A forced takeover closes the previous WebSocket but preserves the selected
target and engine for the replacement client. Attaching to an existing engine
requests a full repaint.

Login tokens are held in memory with sliding expiry and delivered through an
`HttpOnly`, `SameSite=Strict` cookie. The cookie is marked `Secure` when
`x-forwarded-proto` reports HTTPS. Restarting the gateway invalidates all
logins.

## Client protocol

`src/protocol.rs`, `frontend/src/protocol.ts`, and the viewer's `Protocol`
sources define the client contract. `GET /api/config` publishes the protocol
version so the independently shipped viewer can reject an incompatible gateway.

Control and input messages are tagged JSON. Server messages cover picker and
connected state, desktop size, display selection, cursor shape, clipboard,
audio format, and errors. The `connected` message includes `resize`,
`clipboard`, and `audio` capability flags so clients expose only supported
controls.

### Image batches

Screen updates use little-endian binary frames:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | records

TILE     op 0x01: u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
TILE_REF op 0x02: u16 slot | u16 x | u16 y
```

The only tile format is WebP. One frame carries multiple ready updates so a
repaint does not require one WebSocket event per tile. Receivers reject nonzero
flags, unknown operations, truncated records, and unsupported formats.

`TILE` draws a payload and optionally stores it in a gateway-selected cache
slot. `TILE_REF` redraws the encoded payload already stored in that slot.
`NO_SLOT` means the payload must not be retained. Clients keep a fixed
`SLOT_COUNT` array and never choose eviction themselves.

A client that cannot decode a cached tile or receives a reference to a missing
slot sends `cacheReset`. This clears the outbound slot table and requests a
repaint. A normal `refresh` alone cannot repair a cache disagreement because it
does not reset the table.

### Audio frames

RDP audio is opt-in per attachment. A client sends:

```json
{"type":"audio","enabled":true}
```

The gateway answers with `audioFormat`, describing bare Opus packets at 48 kHz
stereo and carrying `OpusHead`, followed by binary frames:

```text
u8 kind = 0x03 | u8 flags = 0 | u16 packet count
repeated: u16 packet length | packet bytes
```

An audio-enabled RDP engine negotiates one 44.1 kHz, 16-bit stereo PCM format
when it connects. Windows requires `rdpdr` to be advertised alongside the
static `rdpsnd` or dynamic `AUDIO_PLAYBACK_DVC` channel; both audio transports
feed the same bounded queue. The gateway resamples PCM to 48 kHz and encodes
20 ms Opus packets.

The queue never blocks the RDP read loop. A slow consumer loses old buffers
instead of accumulating latency, and no receiver means audio is discarded.
Audio frames bypass a tile batch still being collected, although a batch already
being written may delay them.

Both clients own their playback schedule. They start with a 0.1-second cushion
and discard backlog beyond a 0.3-second ceiling. The browser uses WebCodecs and
therefore requires HTTPS or localhost; the native viewer uses
`AVAudioConverter`. A quiet remote and one that never negotiates audio are
indistinguishable to the client, so detailed negotiation status remains in the
gateway log.

### Client input and display control

Client JSON messages cover pointer, wheel, keyboard, clipboard, display
selection, viewport size, refresh, cache reset, and session control. Pointer
motion is coalesced while the socket has queued bytes; any non-motion input
flushes the latest held position first.

A target's `resize` is permission, not behavior: an engine that has it applies
every `viewport` it is sent and an engine without it drops them all. Whether a
client sends one per window change or only when the user asks is the client's own
choice, made per session and defaulting to on request — see
[`macos-viewer.md`](macos-viewer.md) and `useRemoteDesktop.ts`.

What is engine-specific is the shape of the permission:

| Engine | With `resize` |
|---|---|
| VNC | applies a requested size, on servers accepting SetDesktopSize |
| RDP | applies a requested size, and the client's reported display density |
| RXA | applies a requested size only while the agent's private display is active |

`hostScale` reports the density of the screen the client's window is on. RDP with
resize and RXA both act on it, quantizing to 1x or 2x at the same midpoint; the
resulting density travels back as the `scale` on `resize`, and clients present the
framebuffer at `pixels / scale`. Other engines ignore the message.

RDP and VNC expose one framebuffer. RXA can report individual displays and acts
on `selectDisplay`; choosing a display does not itself change that display's
resolution.

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine.

### Clipboard

Clipboard support is a per-target opt-in available on all engines. The backend
holds the latest remote value and its observed change time:

- VNC forwards and buffers `ServerCutText` or Extended Clipboard data;
- RDP requests `CF_UNICODETEXT` after a remote format announcement;
- RXA watches the Mac pasteboard while the gateway enables the watch.

Clients may request the current value after attaching, since they may have
missed earlier pushes. Replies to that explicit request are marked separately
from unsolicited changes. Only unsolicited changes are eligible for automatic
remote-to-local synchronization; an explicit fetch fills the UI until the user
chooses Copy.

Transfers are capped at 64 KiB and refused rather than truncated. Browser
clipboard integration is best effort because insecure origins and Safari
permission rules may prevent automatic access.

### Liveness

The gateway sends a WebSocket ping every five seconds. Browsers and the viewer
answer at the protocol layer, independent of application timers. About 60
seconds without a pong ends the engine; an orderly close starts a fresh
60-second reattach window.

All remote sockets use `TCP_NODELAY`, a 20-second connect budget, a 30-second
handshake budget, and TCP keepalive. Linux also uses `TCP_USER_TIMEOUT` to bound
unacknowledged writes. These checks prove only that the peer's kernel responds.
RDP and RFB have no portable application ping; RXA adds its own ping/pong to
verify the agent process and reconnect transient failures.

## Engines

### RDP

IronRDP handles TLS and optional NLA/CredSSP. The engine maintains a decoded
framebuffer, compares dirty rectangles with a shadow of pixels already sent,
splits remaining damage into bands, and encodes WebP off the protocol read loop.
Input uses fast-path PDUs after DOM-code-to-scancode mapping.

With `resize = true`, the Display Control Virtual Channel applies explicit
desktop-size requests, and also matches the client's display density: a monitor
layout carries `DesktopScaleFactor` beside the geometry, so a Retina client gets
twice the pixels with the host's UI drawn at 200% rather than the same UI
stretched. The connect itself is always 1x — the density belongs to whichever
client attaches, which has not spoken yet — so a Retina client costs one
reactivation. RDP reports no scale factor back, so unlike RXA the density here is
declared rather than measured. With `clipboard = true`, MS-RDPECLIP carries
`CF_UNICODETEXT` with CRLF/LF conversion. With `audio = true`, the engine
negotiates the static and dynamic MS-RDPEA transports described above.

### VNC

The built-in RFB 3.8 client supports None, classic VNC authentication, and
Apple's Diffie-Hellman security. It requests raw 32-bit true-color pixels,
supports the Cursor pseudo-encoding, and uses the same shadow and encoder path
as RDP.

`subtype = "ard"` selects Apple's authentication and requires the macOS account
username and password. Plain VNC uses `vnc_password`. The explicit subtype
prevents an anonymous macOS Screen Sharing connection from landing at a
separate login-window session.

With `resize = true`, the client advertises DesktopSize and
ExtendedDesktopSize. macOS Screen Sharing accepts but ignores these requests, so
an ARD target rejects the option during configuration. Clipboard support uses
Extended Clipboard when the server advertises it and falls back to Latin-1
`ServerCutText` otherwise.

### RXA

RXA connects to the optional `remotex-agent` through a mutually authenticated
Noise session. The agent captures and encodes the selected Mac display, while
the gateway relays tiles and adapts messages to the common client protocol.

Established links reconnect with capped backoff for up to 30 seconds and request
a repaint on recovery. Input during an outage is discarded. Initial connection
or authentication failures return immediately to the picker.

See [`mac-agent-architecture.md`](mac-agent-architecture.md) for transport,
capture, private-display, permission, and lifecycle details.

## Clients

### Browser SPA

The React SPA has login, target picker, and remote desktop states. It renders
tiles to a canvas, applies incoming frames serially, and overlays mouse,
keyboard, touch, clipboard, display, and audio controls.

The canvas is presented at the remote's point size, derived from framebuffer
pixels and remote scale. Desktop clients scroll when necessary. Touch clients
use fit-to-width presentation, pinch zoom, pan, a virtual cursor, and
multi-finger gestures without changing framebuffer coordinates.

On a Mac host connected to a non-Mac remote, selected Command shortcuts are
translated to Control. A Mac-keyboard toggle disables translation, and the
gateway's `remoteOs` message suppresses it for Mac remotes.

Each tab stores its claim token in `sessionStorage`, allowing reconnects to
reclaim the same slot. Busy and evicted states require explicit takeover or
reclaim actions.

A `remoteBusy` message reports the separate case of the *remote's* own session
being held by a different client, which only `rxa` can produce. Both clients show
it against the target picker with a Take over button, which reconnects with
`force` on `ClientMsg::Connect`. It is a distinct action from the slot takeover
above: one claims this gateway, the other claims the Mac at the far end of it, and
both can be needed in the same sitting.

Its `takenOver` flag separates the two ways a client arrives there. False means
this client asked for a target somebody else holds and was refused. True means it
*held* the session and another client took it — in which case only the remote's
session changed hands: the login, this gateway's session slot and the browser's
socket all remain the loser's, and it returns to the target picker with a message
saying so rather than to the login screen.

### remotex.app, the native macOS client

`remotex.app` is a separate native client of the same HTTP and WebSocket protocol,
with its own session state machine, Metal rendering, AppKit input, pasteboard
synchronization, and Opus playback.

It is also its own deployment. The bundle carries the gateway binary and starts it —
`serve-embedded`, an ephemeral loopback port, no web UI, a bearer token instead of a
login — so the app needs no server, address or credentials, and the gateway dies with
it. Two things follow for this document: such a gateway has a second authentication
mode (`GatewayAuth`, in `src/auth.rs`, of which exactly one is alive per process), and
its config is held to different rules (`config::Audience`) because the app has already
decided everything `[server]` would say.

See [`macos-viewer.md`](macos-viewer.md) for the handshake, the shutdown contract, the
instance directory, compatibility, resize behavior, and QA.

## Configuration and testing

Configuration is one TOML file with `[server]` and `[[targets]]` sections.
Protocol-specific fields are validated at startup, including mutually exclusive
credential fields, RXA key roles and checksums, and unsupported feature
combinations.

`config::Audience` names the two readers of that schema. A served gateway needs a
target to offer and a credential to guard it, and is told where to listen. `remotex.app`'s
gateway is told none of those — it refuses a `[server]` block, and comes up with no
targets at all, which is what a first launch has. `remotex check-config [--embedded]`
applies either set of rules without starting anything; the app's configuration editor
calls it before writing, so what the editor accepts is what the gateway starts on.

Unit tests cover protocol parsing, configuration, authentication, key mapping,
audio, and engine helpers. Tests under `tests/` exercise HTTP/WebSocket session
flow and protocol engines. Containerized dummy servers cover RDP and VNC, while
RXA uses an in-process fake agent plus optional live-agent probes.

Stable headless browser tests under
[`tests/playwright`](../tests/playwright/README.md) cover deterministic DOM,
control-plane, HTTP, and WebSocket behavior. Rendering races and timing
measurements remain in raw-protocol and container tests.
