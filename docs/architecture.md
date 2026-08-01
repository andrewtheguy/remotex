# Architecture

remotex is a single-user gateway for RDP and VNC targets, including Macs reached
through their built-in Screen Sharing service. A Rust backend owns the remote
protocol session and exposes one common HTTP/WebSocket interface to the React SPA
and to `remotex.app`, the native macOS client — which runs a gateway of its own
rather than reaching a deployed one (see [`macos-viewer.md`](macos-viewer.md)).

## Data path

```text
browser SPA, or remotex.app over loopback
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary image batches, Opus audio
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through IronRDP
                                         └─ built-in RFB 3.8 client
```

RDP and VNC frames are decoded in the gateway and encoded as PNG tiles. A Mac is
reached as a VNC target with `subtype = "ard"`, which selects macOS Screen
Sharing's Apple Remote Desktop authentication. RDP audio is converted from PCM to
Opus and sent on the same WebSocket independently of the tile encoder and batching
queue.

## Constraints

- There is one active session slot per gateway instance. A new client may force a
  takeover and evict the previous holder; concurrent and shared sessions are not
  supported.
- Remote credentials remain in the server-side TOML configuration.
- Clients speak only the remotex protocol and never implement RDP or RFB.
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
| `encode.rs`, `tiles.rs` | ordered PNG encoding and change detection |
| `audio.rs`, `opus_stream.rs`, `rdp_audio.rs` | PCM queue, Opus encoding, MS-RDPEA |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine consumes `ClientMsg` input and emits the same `ServerMsg` stream.
RDP and VNC pass dirty pixels through the ordered encoder before reaching that
boundary.

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

Tile formats are PNG and JPEG. One frame carries multiple ready updates so a
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

`hostScale` reports the density of the screen the client's window is on. RDP with
resize acts on it, quantizing to 1x or 2x at the same midpoint; the resulting
density travels back as the `scale` on `resize`, and clients present the
framebuffer at `pixels / scale`. Other engines ignore the message.

A client shows the display picker exactly when the target sends it a
`ServerMsg::Displays`, and hides it otherwise. The VNC engine sends one for both
Apple subtypes: it parses an `AppleDisplayLayout` into a `displays` message and
acts on a `selectDisplay` by binding that screen. RDP and generic VNC expose a
single framebuffer spanning every remote screen and have nothing to enumerate,
so they never send the message and the picker stays hidden on those targets.

Where the list is sent, the engine prepends an *All Displays* entry of its own so a
client that picks a screen can get back, and it moves the checkmark only when a
layout comes back naming the screen the Mac is now sending — never on the click. See
[`apple-vnc-889.md`](apple-vnc-889.md).

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine.

### Clipboard

Clipboard support is a per-target opt-in available on all engines. The backend
holds the latest remote value and its observed change time:

- generic VNC forwards and buffers `ServerCutText` or Extended Clipboard data;
- Apple Standard VNC reads and writes the Mac's native compressed pasteboard;
- RDP requests `CF_UNICODETEXT` after a remote format announcement.

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
RDP and RFB have no portable application ping.

## Engines

### RDP

IronRDP handles TLS and optional NLA/CredSSP. The engine maintains a decoded
framebuffer, compares dirty rectangles with a shadow of pixels already sent,
splits remaining damage into bands, and encodes PNG off the protocol read loop.
Input uses fast-path PDUs after DOM-code-to-scancode mapping.

With `resize = true`, the Display Control Virtual Channel applies explicit
desktop-size requests, and also matches the client's display density: a monitor
layout carries `DesktopScaleFactor` beside the geometry, so a Retina client gets
twice the pixels with the host's UI drawn at 200% rather than the same UI
stretched. The connect itself is always 1x — the density belongs to whichever
client attaches, which has not spoken yet — so a Retina client costs one
reactivation. RDP reports no scale factor back, so the density here is declared
rather than measured. With `clipboard = true`, MS-RDPECLIP carries
`CF_UNICODETEXT` with CRLF/LF conversion. With `audio = true`, the engine
negotiates the static and dynamic MS-RDPEA transports described above.

### VNC

The built-in client speaks two dialects, chosen by the target's `subtype`, that
share everything below the handshake — one read loop, one input path, one tile
path. Both request raw 32-bit true-color pixels in the same forced BGRX format and
use the same shadow and encoder path as RDP.

**RFB 3.8** (`vnc`, and `subtype = "ard"`) is every VNC server. It supports None,
classic VNC authentication, and Apple's Diffie-Hellman security, plus the Cursor
pseudo-encoding. `subtype = "ard"` selects Apple's authentication and requires the
macOS account username and password; plain VNC uses `vnc_password`. The explicit
subtype prevents an anonymous macOS Screen Sharing connection from landing at a
separate login-window session rather than the user's screen. With `resize = true`,
the client advertises DesktopSize and ExtendedDesktopSize against servers that
accept them. Generic VNC clipboard support uses Extended Clipboard when the server
advertises it and falls back to Latin-1 `ServerCutText` otherwise. The Apple subtype
also negotiates Apple's display metadata, display picker and native pasteboard on
the ordinary byte stream; it deliberately leaves pixels raw.

**RFB 003.889** (`subtype = "ard-high-performance"`) is Apple's own protocol
revision. It authenticates identically — the same security type 30 — and then
differs in three places and nowhere else: the version banner, the `0xC1` ClientInit
byte, and a cleartext `SetEncryption` prelude after which every byte in both
directions rides inside an AES-128-CBC record layer keyed by a rekey message the
server delivers, of all places, inside a framebuffer rectangle. `src/vnc_record.rs`
is that transport, exposed to the rest of the engine as an ordinary `AsyncRead` and
a per-message sink; `src/vnc_apple.rs` is the message and payload layer above it.

**High Performance mode is a virtual-display mode.** The gateway sends one
`SetDisplayConfiguration` (`0x1d`) during setup, with one 1x mode built from the
target's `width` and `height`. The Mac then supplies the virtual display over the
003.889 record transport, with zlib rectangles instead of raw pixels. Apple's own
client has undocumented controls for choosing one or two virtual displays,
resolution presets, and dynamic resolution in this mode; this gateway implements
none of them and does not resend the descriptor after setup.

The wire constraints remain load-bearing: the *first* `SetEncodings` must be the
measured exact list, so zlib is requested in a second one after a layout has arrived;
and a layout payload is two bytes shorter than its own length prefix claims. The
byte layouts and measured protocol corrections are in
[`apple-vnc-889.md`](apple-vnc-889.md) — read that before touching this path.

Deliberately absent: Apple's own still-image codecs and the Adaptive HEVC media
transport (the reference leaves their payload formats unresolved, and a client must
not advertise an encoding it cannot decode); the pasteboard, which Apple carries over
messages of its own rather than RFB's, so `clipboard` is refused for the subtype at
configuration time; and Apple's undocumented dynamic resolution, so `resize` is
refused there too. See
[`roadmap.md`](roadmap.md).

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
credential fields and unsupported feature combinations.

`config::Audience` names the two readers of that schema. A served gateway needs a
target to offer and a credential to guard it, and is told where to listen. `remotex.app`'s
gateway is told none of those — it refuses a `[server]` block, and comes up with no
targets at all, which is what a first launch has. `remotex check-config [--embedded]`
applies either set of rules without starting anything; the app's configuration editor
calls it before writing, so what the editor accepts is what the gateway starts on.

`branding` is top-level for exactly that reason: it is the one setting both audiences
share, and a key inside `[server]` could not name a gateway whose config has no
`[server]` block. There is one place to write it and no second spelling.

Unit tests cover protocol parsing, configuration, authentication, key mapping,
audio, and engine helpers. Tests under `tests/` exercise HTTP/WebSocket session
flow and protocol engines. Containerized dummy servers cover RDP and VNC.

Stable headless browser tests under
[`tests/playwright`](../tests/playwright/README.md) cover deterministic DOM,
control-plane, HTTP, and WebSocket behavior. Rendering races and timing
measurements remain in raw-protocol and container tests.
