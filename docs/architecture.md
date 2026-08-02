# Architecture

remotex is a single-user gateway for RDP and VNC targets, including Macs reached
through their built-in Screen Sharing service. A Rust backend owns the remote
protocol session and exposes one common HTTP/WebSocket interface to the React SPA
and to `remotex.app`, the native macOS client. The app either starts its bundled
gateway on loopback or connects to a deployed gateway (see
[`macos-viewer.md`](macos-viewer.md)).

## Data path

```text
browser SPA, or remotex.app over loopback or the network
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary image batches, Opus audio
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through IronRDP
                                         └─ built-in RFB 3.8 client
```

RDP and VNC frames are decoded in the gateway and encoded as tiles — lossless PNG
by default, or JPEG or WebP at a fixed quality when a target says so. A Mac is
reached with `subtype = "ard"`, Apple Screen Sharing's Standard mode over RFB 3.8
with Apple Remote Desktop authentication. RDP audio is converted from PCM to
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
| `encode.rs`, `tiles.rs` | ordered tile encoding and change detection |
| `audio.rs`, `opus_stream.rs`, `rdp_audio.rs` | PCM queue, Opus encoding, MS-RDPEA |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine consumes `ClientMsg` input and emits the same `ServerMsg` stream.
RDP and VNC pass dirty pixels through the ordered encoder before reaching that
boundary.

Ordering is a correctness requirement throughout the frame path. Tiles replace
rectangles without delta state, and a resize changes their coordinate space.
The encoding and outbound queues therefore keep tiles, resizes, and cursor
updates in source order even when individual tile encodes finish concurrently.

### The render dial

How a target's tiles are encoded is a per-target choice on two flat axes, plus a
quality: `render_type` is the quality strategy, `render_subtype` the codec, and
`render_quality` (1–100) the fixed quality a lossy strategy uses. Two axes rather
than one flat mode list because strategy and codec vary independently. The legal
pairings are validated at config-load time in `ConfigFile::parse_with`; the
combinations that exist are:

| `render_type` | `render_subtype` | behavior |
|---|---|---|
| `full` | `png` | lossless PNG. The default, and byte-identical to the PNG-only gateway that preceded the dial |
| `fixed-quality` | `jpeg` | every tile JPEG at `render_quality` |
| `fixed-quality` | `webp` | every tile WebP at `render_quality` — typically ~30% fewer bytes than JPEG at a matched quality |
| `motion` | `png` | lossless base; cells in motion at `render_motion_subtype`/`render_motion_quality` |
| `motion` | `jpeg` / `webp` | base at `render_quality`; cells in motion cheaper still |

No classifier runs in either fixed lossy combination: `jpeg` sends *every* tile as
JPEG, so flat UI and text soften along with photographic content. That is the
honest trade of a single fixed knob, and choosing `webp` over `jpeg` spends fewer
bytes for the same visible result.

The dial costs no wire change. A tile record's first byte is already its format
(`Tile::FORMAT_PNG` / `FORMAT_JPEG` / `FORMAT_WEBP`) and both clients decode all
three — the browser through `createImageBitmap` from a MIME type, the Swift viewer
through ImageIO from the container itself. WebP decode is why the app's deployment
target is macOS 15.

The engines never see the config enums. The axes and the qualities collapse to one
`RenderPlan` — a base `TileCodec` (`Png | Jpeg(q) | Webp(q)`) and, for `motion`
only, a second one — at the config boundary in `TargetConfig::render_plan`, which
reaches the per-tile encode call through the engine-agnostic `TileSink`:

```text
render_type / render_subtype / render_quality / render_motion_*
  → TargetConfig::render_plan() → RenderPlan → vnc::run / rdp::run
  → TileSink::new(engine, frame_tx, plan)
  → Tile::from_rgb / from_rgb_jpeg / from_rgb_webp
```

Because `TileSink` is shared, RDP and VNC get every codec from one implementation,
and a `Png` codec calls `Tile::from_rgb` unchanged without touching lossy code.
`encode_webp` wraps the `webp` crate's `libwebp`, built by `cc` with the target's
SIMD and no cmake, at `thread_level = 1` so one encode can use all cores.

#### `motion`: a discount on what is too busy to notice

`motion` is not a third way to encode every tile. It builds on the base encode a
target already has and changes nothing about it — the base is read from
`render_subtype` and `render_quality` rather than from `render_type`, which
`motion` occupies — and adds a second, much cheaper encode used *only* for cells
currently changing fast. A lossless base is the configuration the fixed dial cannot
express at all, and the interesting one: text and flat UI stay perfect, and only
what moves gets ugly.

```toml
[[targets]]
render_type           = "motion"
render_subtype        = "png"    # base: what a settled cell gets
render_motion_subtype = "jpeg"   # moving cells: need not be the base codec
render_motion_quality = 10       # moving cells: as cheap as it takes
```

The moving encode has its own codec axis (`MotionSubtype`, which admits no `png`),
not just its own quality. A settled cell is sent once and can afford WebP's slower,
smaller encode, while a moving cell is re-encoded every frame, where JPEG's faster
encode may beat WebP's smaller output; cheapest and smallest are not the same
question at quality 60 as at 10. `render_motion_subtype` defaults to
`render_subtype`, and is required when the base is `png` — lossless has no dial to
turn down.

Detection is in `src/encode.rs`, owned by the sink both engines already funnel
their damage through:

- **Cell identity.** `Shadow` is pixel-exact and has no stable cell identity, so
  churn is keyed to the fixed 320×64 grid (`CELL_W`/`CELL_H`). `Rect::cells` cuts a
  rectangle at the grid lines on both axes, and `Rect::cell_key` names the piece.
  Cutting rather than snapping outward matters: RDP and VNC describe the same
  moving region with different rectangles from frame to frame, and a key that moved
  with them would count no churn, but snapping outward would ship pixels that did
  not change — and VNC could not reach them anyway, since it crops from the
  rectangle it just read.
- **Churn → encode.** Each cell keeps an 8-bit shift register of which of the last
  `CHURN_WINDOW` slots of `CHURN_SLOT` wall time changed it — 4 of the last 8
  hundred-millisecond slots at `CHURN_MOVING`, at which the cell is in motion and
  takes the motion codec. A hard switch rather than a ramp, because the switch is
  what a measurement can read.

  Slots of time rather than frames, because neither engine has a frame worth
  counting. RDP's outer loop turns once per PDU received, most of which redraw
  nothing, so a counter driven by it races ahead of the repaints and a cell's
  history ages out between its own changes. VNC's turns once per
  `FramebufferUpdate`, which is damage-driven and so much closer, but its rate is
  set by the update-request loop rather than by the remote: a cell changing in every
  update reads the same whether that is sixty times a second or twice. Several
  changes inside one slot count once, so an engine that reports one change as ten
  rectangles does not read as ten times as busy, and "in motion" stays one statement
  about the remote rather than two about the transports.
- **Splitting only where it matters.** A band whose cells are all quiet is sent
  whole and at the base encode, so a target with nothing moving is byte-for-byte
  what the same target sends without `motion` at all. Only a band containing a
  moving cell is cut at the grid — which is what makes a video in a window cost its
  own cells their quality and cost the text beside it nothing.
- **Cleanup.** A piece sent at the motion encode keeps its source pixels, bounded
  by `MAX_STASH_BYTES`. A `CLEANUP_TICK` interval in `order_loop` re-sends cells
  idle past `CLEANUP_IDLE` at the *base* encode, `MAX_CLEANUPS_PER_TICK` at a time
  and oldest first, so a paused screen sharpens on its own without a client
  repaint. The timer has to be its own, because the case it exists for is a remote
  that has stopped sending frames. The debt is timed at dispatch rather than when
  the encode lands, which is what keeps a cleanup from overtaking fresher pixels: a
  cell with a tile still in the queue cannot also be idle.
- **Resets.** Motion state is cleared on resize, where the keys no longer name the
  same pixels, and on reattach, where the repaint re-sends every pixel at the base
  encode anyway.

Cleanups ride the wire as ordinary tiles; nothing about the record changed. What it
cost is in the `encode totals` line, where `motion` and `cleanup` are read together:
every cleanup is a tile sent twice, so a scheme paying more in re-sends than it
saves in motion shows up as a cleanup byte count rivalling the saving. Replacing the
moving-cell encoder with H.264 is the [roadmap](roadmap.md)'s business.

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

Tile formats are PNG, JPEG and WebP. One frame carries multiple ready updates so a
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
| Generic VNC | applies a requested size, on servers accepting SetDesktopSize |
| Apple High Performance VNC | supports Resize to Window through Apple dynamic resolution |
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
- both Apple VNC subtypes read and write the Mac's native compressed pasteboard;
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
path. Both force the same 32-bit true-color BGRX pixel format rather than
negotiating one, and use the same shadow and encoder path as RDP. `src/vnc_encodings.rs`
decodes whichever encoding a server picks into the packed RGB888 the tile path
takes, so nothing above it knows which was chosen.

**RFB 3.8** is used by generic `vnc` and Apple Screen Sharing Standard mode
(`subtype = "ard"`). It supports None,
classic VNC authentication, and Apple's Diffie-Hellman security, plus the Cursor
pseudo-encoding. `ard` selects Apple's authentication and physical-display
metadata and requires the macOS account username and password; plain VNC uses
`vnc_password`. The explicit
subtype prevents an anonymous macOS Screen Sharing connection from landing at a
separate login-window session rather than the user's screen.

Generic `vnc` advertises the standard lossless encodings in preference order —
CopyRect, ZRLE, zlib, Hextile, RRE, Raw — and a server encodes with the first it
supports, so a modern one settles on ZRLE and uses CopyRect for scrolls and window
moves. Tight, TightPNG, JPEG and H.264 are deliberately absent: vendor or lossy,
and this gateway re-encodes every tile for the browser anyway. CopyRect names a
source region rather than carrying pixels; the clients cannot blit, so the pixels
are read back out of the shadow, and a source the shadow does not know costs one
non-incremental repaint rather than an invented picture.

With `resize = true`,
the client advertises DesktopSize and ExtendedDesktopSize against servers that
accept them. Generic VNC clipboard support uses Extended Clipboard when the server
advertises it and falls back to Latin-1 `ServerCutText` otherwise. The Apple subtype
also negotiates Apple's display metadata, display picker and native pasteboard on
the ordinary byte stream, and asks for zlib in the second `SetEncodings` exactly as
High Performance does — the upgrade waits on a display layout, not on a dialect.

**RFB 003.889** (`subtype = "ard-high-performance"`) is Apple's own protocol
revision, and is **experimental**: none of it is documented by Apple, so every
claim in this section is measurement rather than specification, holding for the
Macs in [apple-vnc-889.md](apple-vnc-889.md) rather than for the protocol. The
dynamic-resolution path behind `resize = true` is the least settled part. It
authenticates identically — the same security type 30 — and then
differs in three places and nowhere else: the version banner, the `0xC1` ClientInit
byte, and a cleartext `SetEncryption` prelude after which every byte in both
directions rides inside an AES-128-CBC record layer keyed by a rekey message the
server delivers, of all places, inside a framebuffer rectangle. `src/vnc_record.rs`
is that transport, exposed to the rest of the engine as an ordinary `AsyncRead` and
a per-message sink; `src/vnc_apple.rs` is the message and payload layer above it.

**High Performance mode is a virtual-display mode.** The gateway sends
`SetDisplayConfiguration` (`0x1d`) during setup, with one 1x mode built from the
target's `width` and `height`. Once connected, the remote Mac's physical displays
are disabled and all of its windows are placed on that virtual display. Apple's
official macOS Screen Sharing client can choose up to two virtual displays, while
Remotex always requests one. The full descriptor enables dynamic resolution on
every fresh session. With `resize = true`, it supports **Resize to Window** like
RDP, using Apple's dynamic-resolution feature: later viewport reports resend the
same full descriptor with the requested mode, and the Mac's answering display
layout sets the actual framebuffer geometry. The Mac supplies that virtual display
over the 003.889 record transport, with zlib rectangles instead of raw pixels.
Apple's virtual-display-count and resolution-preset controls remain unimplemented.

The wire constraints remain load-bearing: the *first* `SetEncodings` must be the
measured exact list, so zlib is requested in a second one after a layout has arrived
— for both Apple subtypes;
and a layout payload is two bytes shorter than its own length prefix claims. The
byte layouts and measured protocol corrections are in
[`apple-vnc-889.md`](apple-vnc-889.md) — read that before touching this path.

Deliberately absent: Apple's own still-image codecs and the Adaptive HEVC media
transport (the reference leaves their payload formats unresolved, and a client must
not advertise an encoding it cannot decode). The native Apple pasteboard works on
both subtypes; 003.889 enables monitoring before the rekey and carries the fetch and
data messages inside its encrypted record layer. See
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

The first screen chooses its gateway. **On This Mac** starts the bundled binary via
`serve-embedded`: an ephemeral loopback port, no web UI, and a random bearer token
instead of a login. The gateway dies with the app. **Somewhere Else** connects to a
deployed gateway with its address and login. Both expose the same config, target,
session, and WebSocket APIs; only the credential header on protected requests
differs.

The embedded path adds a second authentication mode (`GatewayAuth`, in
`src/auth.rs`, of which exactly one is active per process) and different config
rules (`config::Audience`) because the app decides everything `[server]` would say.

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
