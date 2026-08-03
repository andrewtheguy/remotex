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
   │  /ws: JSON control/input, binary image batches
   │  /ws/audio: the audio format, then binary audio frames
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
| `regions.rs`, `h264.rs` | which regions get an H.264 stream, and the encoder behind them |
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

How a target's pixels reach a client is a per-target choice on two flat axes, plus a
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
| `motion` + `render_motion_subtype = "h264"` | `png` / `jpeg` / `webp` | base as above; an H.264 stream per coalesced moving region at `render_motion_quality` |
| `video` | *(refused)* | the whole desktop as one H.264 stream at `render_quality` |

`video` is the one row where `render_subtype` is empty, and that is what it is
saying: it sends no per-region streams and no tiles at all — one fixed region, the
whole desktop, for the whole session — so there is no per-tile codec left to name.
The `h264` motion row still has one, because the base encode is still a still image:
only what is moving becomes a stream.

No classifier runs in either fixed lossy combination: `jpeg` sends *every* tile as
JPEG, so flat UI and text soften along with photographic content. That is the
honest trade of a single fixed knob, and choosing `webp` over `jpeg` spends fewer
bytes for the same visible result.

The still dial costs no wire change. A tile record's first byte is already its format
(`Tile::FORMAT_PNG` / `FORMAT_JPEG` / `FORMAT_WEBP`) and both clients decode all
three through `createImageBitmap` from a MIME type — `remotex.app` draws on a web
canvas too, so this is one implementation rather than two that could disagree
about a format. What streams costs one: a `VIDEO` record, described under the
client protocol below.

The engines never see the config enums. The axes and the qualities collapse to one
`RenderPlan` at the config boundary in `TargetConfig::render_plan`, which reaches
the encode call through the engine-agnostic `TileSink`. `RenderPlan` is an enum with
one arm per transport — `Tiles { base, motion, debug }` and `Video { quality }` —
rather than a struct with a flag, because the two share no code path worth sharing
and the compiler is what stops a consumer handling only the first. `motion` is itself
a `MotionEncode`, `Tile(codec)` or `Stream { quality }`, for the same reason one
level down: a cheaper still and an inter-frame stream are not two settings of one
mechanism.

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

The moving encode has its own axis (`MotionSubtype`, which admits no `png` and does
admit `h264` — see below), not just its own quality. A settled cell is sent once and can afford WebP's slower,
smaller encode, while a moving cell is re-encoded every frame, where JPEG's faster
encode may beat WebP's smaller output; cheapest and smallest are not the same
question at quality 60 as at 10. `render_motion_subtype` defaults to
`render_subtype`, and is required when the base is `png` — lossless has no dial to
turn down.

**`motion` is refused on `subtype = "ard-high-performance"`.** A resize under both
corrupts the desktop until the whole gateway is restarted — a reconnect does not
clear it, and both clients see it, so it is engine state rather than anything the
render dial owns. Neither half is proven at fault: High Performance is reverse
engineered with no specification behind it (see [apple-vnc-889.md](apple-vnc-889.md)),
and `motion` is the newer code. The pairing waits until one of them is understood
well enough to say which. Every other subtype may use `motion`, and a High
Performance target may use every other strategy.

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
- **What counts as change.** `Shadow::accept` returns a `Changed`: one bounding box
  round everything that differs, *and* the grid cells that actually differ. The two
  are not the same, and conflating them was a real fault — a video at one end of the
  screen and an animated banner at the other put every cell between them inside one
  box, and four reports like that inside the churn window read as the whole screen in
  motion, which is how a still sidebar, a menu bar and a Windows taskbar ended up at
  quality 10. The box still decides what is *sent*, because those pixels are correct
  and only redundant; the cell list decides what is *counted*. A cell only along for
  the ride is left at the base encode and settled — its pixels are going out anyway,
  so exact costs only bytes, and exact is what discharges a debt.
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
  by `MAX_STASH_BYTES`. A cell holds *one* debt, so a debt already standing may only
  be replaced by a rectangle covering it; anything else takes the base encode
  instead. Damage is clipped to the cell rather than snapped out to it, so two sends
  can be two different slivers of one cell, and overwriting the first debt with the
  second left the first sliver lossy with nothing that knew it was owed. That is the
  pointer trail on RDP — the cursor is composited into the framebuffer, so crossing a
  cell leaves a run of small rectangles of which only the last would ever have been
  cleaned up. A debt a crisp send only *partly* covers is not cancelled but brought
  up to date, the newer pixels written over the ones it is holding. A debt holds the
  frame it was recorded on, and the cleanup restores it faithfully — including
  whatever has changed underneath it since and already gone out crisp. On RDP that is
  the composited pointer painted back onto a spot it has left: wrong content rather
  than coarse content, and permanent, since the shadow counts the newer pixels as
  delivered and nothing sends them twice. A `CLEANUP_TICK` interval in `order_loop`
  re-sends cells
  idle past `CLEANUP_IDLE` at the *base* encode, `MAX_CLEANUPS_PER_TICK` at a time
  and oldest first, so a paused screen sharpens on its own without a client
  repaint. The timer has to be its own, because the case it exists for is a remote
  that has stopped sending frames. The debt is timed at dispatch rather than when
  the encode lands, which is what keeps a cleanup from overtaking fresher pixels: a
  cell with a tile still in the queue cannot also be idle.
- **Resets.** Motion state is cleared on resize, where the keys no longer name the
  same pixels, and on reattach, where the repaint re-sends every pixel at the base
  encode anyway.
- **`render_motion_debug`.** A QA aid, off unless asked for, that outlines every
  piece a split region emits in the pixels themselves: magenta for the motion
  encode, cyan for a quiet cell beside it, green for a cleanup. It exists because
  the alternative is inferring the decision from how blurry something looks, and
  the two failures that produces look alike from a screenshot: motion armed on
  something that is not moving, and a stale lossy region nothing is going to
  replace. Under the overlay they are distinct — the first is magenta, the second
  carries no mark at all, since an unmarked region was sent whole at the base
  encode. The mark goes on the copy handed to the encoder, never on the pixels the
  shadow recorded or the stash owes, so a cleanup erases the outline it replaces
  rather than restoring it.

Cleanups ride the wire as ordinary tiles; nothing about the record changed. What it
cost is in the `encode totals` line, where `motion` and `cleanup` are read together:
every cleanup is a tile sent twice, so a scheme paying more in re-sends than it
saves in motion shows up as a cleanup byte count rivalling the saving.

##### `render_motion_subtype = "h264"`: a stream per moving region

The third thing the motion axis can be, and the only one that is not a still. The
detection above is unchanged — the same cell grid, the same churn window, the same
hard switch — but what it hands the moving cells to is an inter-frame H.264 stream
per coalesced region (`src/regions.rs`, encoding through `src/h264.rs`), with the
base codec carrying every cell outside one. A video in a window costs its own pixels;
the text beside it stays exactly what `render_subtype` says and is never re-encoded.

`RenderPlan`'s `motion` is a `MotionEncode` rather than a codec, so the compiler is
what makes every consumer answer which of the two it is holding.

- **Which regions.** `coalesce` in `src/regions.rs` takes the cells in motion,
  groups them into 4-connected components, and takes each component's bounding box.
  Over `MAX_STREAMS` (4) it merges the pair whose merged box adds the fewest cells;
  a merge that would cover more than twice the cells actually moving inside it is
  refused, and the smallest region goes to the still codecs instead. That last rule
  is `Changed::cells`' fault one level up: a banner ad in one corner must not put the
  screen in a stream because a video is playing in the other.
- **When one starts and stops.** Geometry moves at most once per `RETUNE` (500 ms).
  A region that shrinks keeps its stream — the idle margin codes as skipped
  macroblocks, where a restart costs an encoder and a keyframe — and one that grows
  past its rectangle gets a new stream, because an inter-frame stream means nothing
  if its rectangle moves. A region with nothing moving in it for `STREAM_IDLE` ends.
  A screen that has stopped changing produces no frame boundary at all, so the
  cleanup tick expires idle streams itself; it may only *end* them, never start one,
  which is what keeps a cell from being delivered twice.
- **The debt is a cell key, not a picture.** Every cell a stream covers is owed a
  crisp re-send from the moment it is streamed, moving or not — the stream codes
  them lossily either way and nothing else will send them. When the stream ends they
  come due, and the cleanup crops them out of the **mirror**, which holds the exact
  current source for every pixel. So none of the still path's staleness applies here:
  no stash, no cap, no partial-cover patching, and no way to restore a frame that has
  been overtaken. A crisp send discharges a cell only if it covered that cell in full.
- **One mirror, several encoders.** `damage` blits every rectangle into the whole-
  framebuffer mirror whether or not anything is streaming it, which is what lets a
  stream start mid-session with correct pixels for its whole rectangle. A round takes
  the mirror and every stream to one blocking worker: their rectangles are disjoint
  and bounded by the desktop, so a round costs what one whole-desktop frame costs.
- **A region is even, or the desktop's own edge is odd.** A region is a union of
  whole grid cells and `CELL_W`/`CELL_H` are both even, so an odd side can only come
  from the clip at the desktop's right or bottom edge — where the mirror's own
  padding is already the column or row the encoder needs. That is why `Stream::new`
  can assert its geometry rather than pad defensively.

One measurement, so that the shape of the trade is on the record rather than assumed
— 25 s of the same driven motion on a 1280×800 RDP desktop, release build:

| dial | to the client | encode CPU |
|---|---|---|
| `motion` + `webp` 10 | 4.5 MB | 0.17 s |
| `motion` + `h264` 30 | 0.70 MB | 1.39 s |
| `video` 60 | 0.45 MB | 5.48 s |

So the regions cost about a sixth of the still motion encode's bytes with the still
parts left lossless, and a quarter of whole-desktop `video`'s CPU — because only what
moved was coded, rather than 1280×800 every frame. What it buys over `video` is
exactness everywhere else; what it costs is bytes.

Both dials that stream share `Congestion`, one verdict for one link: the quantizer
walks up when a round's push blocks and back down to `render_motion_quality`, never
past it. Unlike `video`, a target here keeps the ordinary `FRAME_BUFFER` depth,
because the same queue carries its still tiles — so `coarsened` in the totals is a
less sharp signal, which is worth knowing when reading it.

#### `video`: a different transport, not a fourth codec

`render_type = "video"` sends the whole framebuffer as one inter-frame H.264 stream
for the session — the degenerate case of the region streams above, and it runs the
same code: one region, fixed at the whole desktop, never retuned (`Policy::Whole` in
`src/regions.rs`). It is on the `render_type` axis and refuses
`render_subtype` because an access unit is not the same kind of thing as a tile: a tile is an independent picture — reorderable, cacheable,
droppable once something covers it — and an access unit is one link in a chain,
where losing any link decodes wrongly until the next keyframe. `RenderPlan` being an
enum is that distinction made structural.

Five consequences, each of which is a rule somewhere — and every one of them holds
for the region streams above too, which is why they run the same code:

- **`Shadow::accept` is a promise.** It records source pixels as delivered the
  moment it accepts them, and nothing re-sends them, so the encoder may never drop a
  frame (`skip_frames(false)`) and an encode that yields no bitstream leaves the
  mirror dirty for the next frame to carry rather than clearing it.
- **The stream is fed rectangles, not frames.** `damage` is called once per damage
  *rectangle*, and VNC's pixels can only be cropped out of the rect just decoded, so
  the stream keeps its own whole-framebuffer RGB copy — the mirror — to blit into.
  `TileSink::frame` encodes it, called at RDP's outputs-loop end (and its `Refresh`
  arm, which `continue`s past that) and at VNC's `FramebufferUpdate` end. It is a
  no-op when nothing was blitted, because RDP's loop turns once per PDU and most
  redraw nothing.
- **A frame boundary is a proposal, not a frame rate.** Those boundaries occur at
  whatever rate the remote reports damage — 126 a second, measured, on a busy RDP
  desktop against a 30 Hz stream and a 60 Hz screen — and every one of them used to
  cost a full encode, which is how a session carrying under 800 kbit/s spent 88% of
  itself inside the encoder. `VIDEO_FRAME_INTERVAL` caps it at one access unit per
  33 ms; damage in between accumulates in the mirror and rides the next one, which
  is cheaper than coding the same movement four times over. A forced keyframe skips
  the cap, because a repaint, reattach, takeover or resize is a client with nothing
  on screen. And because a deferral leaves pixels the shadow has already promised,
  `TileSink::due_at` tells the engines when to come back for them whether or not
  more damage arrives — RDP in a `select!` arm beside its layout retry, VNC raced
  against its next message read, at a message boundary so a flush cannot split a
  `FramebufferUpdate`.
- **`src/wire.rs` must leave an access unit alone.** Never cached into a slot, never
  a `TILE_REF`, and outside the coverage relation in both directions. Coverage is
  sound reasoning about pixels nobody could have seen; under `video` every record
  covers the whole framebuffer, so each covers its predecessor exactly. That is not
  enforced by a check but by the record kinds: a `VIDEO` record never reaches the
  cache or the coverage test, so neither has to know about it.
- **The picture may be a pixel larger than the region.** H.264 needs even sides, so
  the mirror is padded up with its edge repeated (black would be a seam the encoder
  paid for every frame). The record header carries the *true* rectangle and the
  client crops — reporting the padded size would push a paint past the framebuffer,
  which the viewer's renderer drops outright rather than clamps.

`render_quality` maps to a constant quantizer (1 → 51, 100 → 12; the floor is
openh264's own `GOM_MIN_QP_MODE`, and mapping past it would give a dial whose top
third did nothing). A constant quantizer *is* variable bitrate — bits go where the
picture needs them, so a motionless desktop costs almost nothing.

The dial is a **ceiling**, and that framing is what makes adaptation tractable here.
`Congestion` in `src/encode.rs` watches one local signal — how long queueing an
access unit blocked — and walks the quantizer up towards 51 when the link is behind,
back down towards the dial when it is not; never below it. What TCP hides is
*headroom*, and this never needs headroom, because exceeding the operator's setting
was never a goal. "Am I behind?" is the whole question, and the outbound queue
answers it. Quality moves through `Stream::set_qp`, which re-tunes the running
encoder rather than rebuilding it: a rebuild would force a keyframe per adjustment,
spending a few hundred KB exactly when bytes are scarce.

That signal only works because those queues are shallow. `FRAME_BUFFER` is 64, sized
for tiles — a 1080p repaint is ~17 bands — but under `video` one message is a whole
frame, and 64 of them in each of two queues in series is seconds of buffered
picture. A video target gets `VIDEO_FRAME_BUFFER` (4) at both hops.

**Both clients decode it, with one decoder between them.** It is WebCodecs
`VideoDecoder`, reached through `frontend/src/videoDecoder.ts` and driven from
`tilePainter.ts` — the shared batch loop, which keys a decoder per `stream` id and
replaces one whose region has restarted on a different size. `remotex.app` shows
this same client in a `WKWebView`, so video was not a second implementation there
and could not have been.

`VideoDecoder` is secure-context only — the same limit remote audio already has, but a
worse one to hit, since no audio decoder means silence beside a working desktop and no
video decoder means no desktop. So a failure is *said* rather than logged: a banner
that stays up. `remotex.app` is served from `http://127.0.0.1`, which is a secure
context whatever else is true, so in practice only a browser can land on the
insecure-origin half of that.

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
`autoResize`, `clipboard`, and `audio` capability flags so clients expose only
supported controls.

### Image batches

Screen updates use little-endian binary frames:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | records

TILE     op 0x01: u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
TILE_REF op 0x02: u16 slot | u16 x | u16 y
VIDEO    op 0x03: u8 stream | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
```

Tile formats are PNG, JPEG and WebP. One frame carries multiple ready updates so a
repaint does not require one WebSocket event per tile. Receivers reject nonzero
flags, unknown operations, truncated records, and unsupported formats.

`TILE` draws a payload and optionally stores it in a gateway-selected cache
slot. `TILE_REF` redraws the encoded payload already stored in that slot.
`NO_SLOT` means the payload must not be retained. Clients keep a fixed
`SLOT_COUNT` array and never choose eviction themselves.

`VIDEO` carries one H.264 access unit for one region, and is a separate record
rather than a fourth tile format because it is not the same kind of thing: a tile is
a self-contained picture and an access unit is one link in a chain. Making it its own
record is what keeps the cache and coverage rules above from ever having to ask
whether they apply — they see tiles only. `stream` names which decoder it belongs to,
since a session may run several at once; the rectangle is the region's true one, and
the decoded picture may exceed it by a pixel on either axis (see the render dial).

A client that cannot decode a cached tile or receives a reference to a missing
slot sends `cacheReset`. This clears the outbound slot table and requests a
repaint. A normal `refresh` alone cannot repair a cache disagreement because it
does not reset the table.

### Audio frames

RDP audio is opt-in, and it has a socket of its own. **Opening
`/ws/audio?session=<token>` is the subscription** — there is no message that turns
sound on, and closing the socket is the only way to stop.

The separation is the point. Sound and pictures used to share the session socket and
the bounded queue behind it, which is four frames deep on `render_type = "video"`; an
audio pump waiting behind a video backlog stops draining the bridge, and what the
bridge then drops is wave buffers. A lost tile is replaced by the next repaint, and a
lost wave buffer is a hole. Every reference client that does not stutter keeps the two
apart — see [`rdp-audio-prior-art.md`](rdp-audio-prior-art.md).

The socket is bound to the *claim*, not to an attachment, so it survives a session
socket reconnecting and a target switch: the gateway re-announces the format when it
arms the next engine. It ends when the claim does — a takeover, or a log out — and is
superseded by a newer audio socket on the same claim. Its refusals mirror the session
socket's: 401 before the upgrade without a login, close code 4000 for a token that is
not the current claim, 4001 on eviction.

The gateway answers with `audioFormat` — the codec string, the decoder
configuration, and the samples in one packet — followed by binary frames:

```text
u8 kind = 0x03 | u8 flags = 0 | u16 packet count
repeated: u16 packet length | packet bytes
```

There is no codec byte in the binary frame; the codec is named once, out of
band, in `audioFormat`. Two options exist, chosen per target by `audio_codec`:

| `audio_codec` | `codec` | bitrate | `sampleRate` | `packetFrames` | `head` |
|---|---|---|---|---|---|
| `opus` (default) | `opus` | 96 kbps | 48 000 | 960 (20 ms) | `OpusHead` |
| `pcm` | `pcm-s16le` | 1.41 Mbps | the remote's | 0 (self-describing) | empty |

`pcm` is passthrough: the remote's wave buffer becomes one packet, byte for
byte, with no encoder in the gateway and no decoder in the client. `pcm-s16le`
is deliberately not a WebCodecs codec string — the packets are interleaved
signed 16-bit little-endian samples, which is what an `AudioBuffer` holds
already, so the client builds one directly and schedules it on the same path an
Opus packet reaches after decoding. That makes it the only option that plays
without WebCodecs, and therefore the only one that works over plain `http://` to
a host that is not `localhost`.

It also makes it the only option whose `sampleRate` is not 48 000. An
`AudioBuffer` carries its own rate, so a context built at 48 kHz before the
format arrived simply resamples on playback, exactly as the OS mixer would for
any buffer that is not at the device's rate.

The bandwidth is the whole of the trade: 1.41 Mbit/s is fifteen times Opus, and
is a local-network proposition only. It is not a quality argument — Opus at 96
kbps is well clear of audible loss on this material. Guacamole carries desktop
audio this way and only this way (its single encoder emits
`audio/L16;rate=44100,channels=2`), which is where the option came from — see
[`rdp-audio-prior-art.md`](rdp-audio-prior-art.md) for that measurement and for
the other implementations worth comparing against.

An audio-enabled RDP engine negotiates one 44.1 kHz, 16-bit stereo PCM format
when it connects. Windows requires `rdpdr` to be advertised alongside the
static `rdpsnd` or dynamic `AUDIO_PLAYBACK_DVC` channel; both audio transports
feed the same bounded queue. Under `opus` the gateway resamples that PCM to
48 kHz in exact 882-to-960 groups (`src/pcm48.rs`) and cuts packets out of the
result; under `pcm` it does neither, and the buffer is only cut on a frame
boundary so a split sample cannot transpose the channels.

The queue never blocks the RDP read loop. A slow consumer loses old buffers
instead of accumulating latency, and no receiver means audio is discarded. Between the
bridge and the socket sits a second, shallower queue (`AUDIO_SOCKET_BUFFER`, sixteen
wave buffers — about three seconds) whose only job is to absorb a socket write in
flight: losses belong at the bridge, which drops its *oldest* and keeps sound that is
still live, rather than here, which is FIFO and would deliver stale audio faithfully.

Both clients own their playback schedule, and it is the same code: a 0.5-second
cushion, and backlog beyond a 1.5-second ceiling discarded. Those numbers are a jitter
budget rather than a latency target, and they are deliberately generous — audio trails
video by roughly the cushion, which is the trade Myrtille and FreeRDP both make. What reaches that
schedule differs by codec, and only there. The client does not decode anything
itself: an *encoded* stream goes to WebCodecs, so a codec a browser will not take
surfaces as a decoder error naming it rather than as silence. A `pcm-s16le` stream reaches no decoder at all; the
client turns the packet into an `AudioBuffer` and schedules it directly.

The secure-context requirement belongs to that first path alone. WebCodecs is
unavailable on an insecure origin, so a browser playing Opus needs HTTPS or
localhost, while passthrough plays anywhere. `remotex.app` is unaffected either
way: its gateway serves the page from `127.0.0.1`, which is always a secure
context.

A quiet remote and one that never negotiates audio are indistinguishable to the
client, so detailed negotiation status remains in the gateway log.

### Client input and display control

Client JSON messages cover pointer, wheel, keyboard, clipboard, display
selection, viewport size, refresh, cache reset, and session control. Pointer
motion is coalesced while the socket has queued bytes; any non-motion input
flushes the latest held position first.

A target's `resize` is permission, not behavior: an engine that has it applies
every `viewport` it is sent and an engine without it drops them all.

*How often* a client sends one is governed by a second permission, `autoResize`
on the `connected` message. Both clients offer two ways to drive a size: a manual
"Resize to Window", and a mode that hands the size to the window so every change
reports one. The manual control follows `resize`. The mode follows `autoResize`,
which the gateway grants to plain `vnc` alone — its DesktopSize renegotiation
costs a new framebuffer and nothing else, where RDP's costs a
Deactivation-Reactivation Sequence and High Performance's replaces a virtual
display, and both of those have a fault in [`known-issues.md`](known-issues.md)
that a window drag reaches far more often than a button press does. Where the
mode is refused the clients grey it and label it inapplicable rather than hiding
it, since the manual control beside it plainly works. Neither client decides any
of this: `TargetConfig::auto_resize` does, and it is not a config key — the
operator has no way to know which engines survive a stream of resizes.

Within the mode, the client's own choice is remembered across connections and
applied "if compatible" — which covers both a target that refuses resize and one
that resizes only when asked.
See [`macos-viewer.md`](macos-viewer.md) and `useRemoteDesktop.ts`.

What is engine-specific is the shape of the permission:

| Engine | With `resize` | Window may drive it |
|---|---|---|
| Generic VNC | applies a requested size, on servers accepting SetDesktopSize | yes |
| Apple Standard VNC | refuses `resize`: it shares physical displays | — |
| Apple High Performance VNC | supports Resize to Window through Apple dynamic resolution | no |
| RDP | applies a requested size, and the client's reported display density | no |

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

A size change that is *real* costs a Deactivation-Reactivation Sequence; asking
twice for the same size triggers it once, and a request equal to the current size
never triggers it. That sequence can fail and end the session — see
[`docs/known-issues.md`](known-issues.md).

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
dynamic-resolution path behind `resize = true` is the least settled part, and can
leave the desktop wrong until the session is reconnected — see
[`known-issues.md`](known-issues.md). It authenticates identically — the same security type 30 — and then
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

### remotex.app, the native macOS shell

`remotex.app` is not a second client. It starts the gateway in its own bundle,
shows **the SPA above** in a `WKWebView`, and owns what a page cannot: the menu
bar, keyboard capture ahead of it, `NSPasteboard`, and the window.

Nothing about the session is the app's. The page is served by the gateway beside
it on `http://127.0.0.1`, so it talks to that gateway directly — same claim, same
`/ws`, same `/ws/audio` — and this app holds no socket, no claim and no wire
format. A protocol change is a change to one client, in one language.

The loopback origin is load-bearing rather than incidental: it is a secure
context, so WebCodecs is available for Opus and H.264 whatever the app is showing.

Two things cross the boundary, over one `WKScriptMessageHandler` and one
`evaluateJavaScript` call (`frontend/src/nativeHost.ts`): the page reports one
state object the menus are derived from, and the app sends the keys a browser is
never given, the Mac's pasteboard, and the menu commands standing in for the
floating menu it hides. See [`macos-viewer.md`](macos-viewer.md#the-bridge).

The gateway it starts is `serve-embedded`: an ephemeral loopback port, the SPA out
of `Contents/Resources/web`, and a random token minted per launch that the app puts
in the web view's cookie store instead of a login. It dies with the app. Reaching
a gateway elsewhere is a browser's job.

That path is the one place `GatewayAuth` (in `src/auth.rs`, of which exactly one is
active per process) and `config::Audience` differ from a served gateway, because the
app decides everything `[server]` would say.

See [`macos-viewer.md`](macos-viewer.md) for the handshake, the shutdown contract,
the instance directory, the bridge, resize behavior, and QA.

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
