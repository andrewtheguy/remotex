# Architecture

remotex is a single-user gateway for RDP and VNC targets, including Macs reached
through their built-in Screen Sharing service. A Rust backend owns the remote
protocol session and exposes one common HTTP/WebSocket interface to the React SPA,
which is the only client.

## Data path

```text
browser SPA over loopback or the network
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary image batches
   │  /ws/audio: the audio format, then binary audio frames
   │  /ws/camera: the camera format and H.264 samples up, start/stop down
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through FreeRDP
                                         └─ built-in RFB client (3.8 or Apple 003.889)
```

RDP and VNC frames are decoded in the gateway and sent as independent image tiles
or as VP9 streams, according to the target's render plan. Tiles are lossless
PNG by default, with JPEG available at fixed quality. A Mac is reached
with `subtype = "ard"`, Apple Screen Sharing's Standard mode over RFB 3.8 with
Apple Remote Desktop authentication, or with the experimental
`ard-high-performance` RFB 003.889 path. Redirected RDP audio is either encoded as
Opus or passed through as PCM and sent on `/ws/audio`, never on the picture queue.
The browser's camera goes the other way on `/ws/camera`: browser-encoded H.264,
passed through to the host over MS-RDPECAM.

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
| `rdp.rs` | RDP session over FreeRDP: framebuffer, input, cursor, clipboard, resize |
| `vnc.rs` | RFB connection, framebuffer, input, cursor, clipboard, resize |
| `encode.rs`, `tiles.rs` | ordered tile encoding and change detection |
| `regions.rs`, `video.rs` | which regions get a video stream, and what both encoders share |
| `vp9.rs` | libvpx — the video codec |
| `audio.rs`, `opus_stream.rs`, `pcm48.rs`, `pcm_stream.rs` | PCM queue, Opus encoding or PCM passthrough, resampling |
| `rdp_audio.rs` | the adapter between FreeRDP's `rdpsnd` device and that queue |
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
quality: `render_type` is the *strategy* — what kind of thing goes on the wire —
and `render_subtype` the codec of the base tiles, with `render_quality` (1–100)
the fixed quality of that codec's lossy side. Two axes rather
than one flat mode list because strategy and codec vary independently: every
tiles-carrying strategy takes every base codec. The legal
pairings are validated at config-load time in `ConfigFile::parse_with`:

`render_type`, the strategy:

- `tiles` — every changed region as an independent still image at the base codec,
  and nothing else. The default; with the default subtype it is byte-identical to
  the PNG-only gateway that preceded the dial.
- `motion` — the base, plus a much cheaper encode for the cells changing fast
  (`render_motion_subtype` / `render_motion_quality`), each re-sent at the base
  once it settles.
- `video` — the whole desktop as one video stream at `render_quality`.

`render_subtype`, the base codec — any of the three under `tiles` and `motion`
alike:

- `png` — lossless, no quality key. The default.
- `jpeg` — every base tile JPEG at `render_quality`.
- `classify` — per tile, what its own pixels are: photographic content JPEG at
  `render_quality`, flat UI and text lossless PNG.

`video` is the one strategy with nothing on the subtype axis, and refuses it: it
sends no tiles at all — one fixed region, the whole desktop, for the whole session
— so there is no per-tile codec left to name. `motion` under
`render_motion_subtype = "stream"` still has a base codec, because the base encode
is still a still image: only what is moving becomes a stream.

**No strategy names a video codec, because video is VP9 only** — see
[the codec](#the-codec).

No classifier runs under the `jpeg` subtype: it sends *every* tile as
JPEG, so flat UI and text soften along with photographic content. That is the
honest trade of a single fixed knob. `classify` is that trade removed for a little
CPU: a picture classifier (`src/classify.rs`) reads each tile on the encode worker
— a palette gate, then the shape of neighbour-to-neighbour deltas — and only what
reads as photographic takes the JPEG; PNG is the verdict for everything flat,
sharp, small or ambiguous, because a photo sent lossless only costs bytes while
text sent lossy costs legibility until that region next changes. The decision is
per tile and stateless, so the same window answers differently as content scrolls
through it. As a `motion` base it composes: a settled cell is classified, a moving
one takes the motion encode (which defaults to `jpeg` there — a cell changing fast
is not worth classifying). `render_classify_debug = true` outlines the tiles sent
as JPEG in yellow — drawn on the copy handed to the encoder, never on the pixels
the shadow records, so the mark lives exactly as long as the lossy tile it
describes, and a colour of its own so it stays readable beside the motion marks.

Two more keys sit across the whole dial rather than on either axis.
`render_adaptive = true` lets every lossy quality the target configures track the
measured link between `render_adaptive_min` (default 20) and its configured value,
which stays the ceiling — see [what the link will bear](#the-codec) for the signal
and the walks. It is refused on lossless PNG tiles, the one plan with no quality to
move. The floor is
one number for the whole plan: whichever dials exist — `render_quality`,
`render_motion_quality`, a stream's — all stop at it.

The still dial costs no wire change. A tile record's first byte is already its format
(`Tile::FORMAT_PNG` / `FORMAT_JPEG`) and the client decodes both
through `createImageBitmap` from a MIME type. What streams costs one: a
`VIDEO` record, described under the client protocol below.

The engines never see the config enums. The axes and the qualities collapse to one
`RenderPlan` at the config boundary in `TargetConfig::render_plan`, which reaches
the encode call through the engine-agnostic `TileSink`. `RenderPlan` is an enum with
one arm per transport — `Tiles { base, motion, debug, adaptive }` and
`Video { quality, adaptive }` —
rather than a struct with a flag, because the two share no code path worth sharing
and the compiler is what stops a consumer handling only the first. `motion` is itself
a `MotionEncode`, `Tile(codec)` or `Stream { quality }`, for the same reason one
level down: a cheaper still and an inter-frame stream are not two settings of one
mechanism.

```text
render_type / render_subtype / render_quality / render_motion_*
  → TargetConfig::render_plan() → RenderPlan → vnc::run / rdp::run
  → TileSink::new(engine, frame_tx, plan)
  → Tile::from_rgb / from_rgb_jpeg
```

Because `TileSink` is shared, RDP and VNC get every codec from one implementation,
and a `Png` codec calls `Tile::from_rgb` unchanged without touching lossy code.
`encode_jpeg` wraps `jpeg-encoder`, built with the target's SIMD, so the encode on
the session hot path costs no C toolchain and no runtime dependency.

#### `motion`: a discount on what is too busy to notice

`motion` is not a third way to encode every tile. It builds on the base encode a
target already has and changes nothing about it — the base is read from
`render_subtype` and `render_quality`, same as under `tiles` — and adds a second,
much cheaper encode used *only* for cells
currently changing fast. A lossless base is the configuration a fixed quality cannot
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
admit `stream` — see below), not just its own quality: what a settled cell gets is a
still picture, and what is moving may stop being one at all.
`render_motion_subtype` defaults to `jpeg` under a lossy base — a `classify` base
included, since a cell changing fast is not worth classifying — and is required
when the base is `png`: lossless has no dial to turn down.

The strategy is protocol-independent and has no subtype restrictions: every engine
normalizes its damage before it reaches the shared sink that detects and encodes
motion.

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
  second left the first sliver lossy with nothing that knew it was owed. That was the
  pointer trail on RDP, back when the cursor was composited into the framebuffer, so
  crossing a cell left a run of small rectangles of which only the last would ever
  have been cleaned up (the browser draws that pointer now — see `Pointer` in
  `rdp.rs` — but the case is general, and any small object crossing a cell repeats
  it). A debt a crisp send only *partly* covers is not cancelled but brought
  up to date, the newer pixels written over the ones it is holding. A debt holds the
  frame it was recorded on, and the cleanup restores it faithfully — including
  whatever has changed underneath it since and already gone out crisp. That is wrong
  content rather than coarse content, and permanent, since the shadow counts the
  newer pixels as delivered and nothing sends them twice. A `CLEANUP_TICK` interval in `order_loop`
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

##### `render_motion_subtype = "stream"`: a stream per moving region

The third thing the motion axis can be, and the only one that is not a still. The
detection above is unchanged — the same cell grid, the same churn window, the same
hard switch — but what it hands the moving cells to is an inter-frame video stream
per coalesced region (`src/regions.rs`, encoding through `src/vp9.rs`), with the
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
- **Where a stream's rectangle is allowed to be.** A region's box is snapped out to a
  `SPANS` rung — cell spans in roughly 1.5× steps — on a grid of that rung, before any
  encoder is built from it. The rung holds the *picture size* still and the alignment
  holds the *rectangle* still, and the second is worth more than the first: a region
  that drifts one cell down is not contained by a box that merely has the right size
  at the old origin, so it would be rebuilt for a drift exactly as for a resize. A box
  straddling a rung boundary takes the next rung up and is tried again; only the last
  rung, the grid itself, can run off the end, and it slides back rather than clipping,
  because a clipped box would have a span that depended on where it started.

  The reason any of this exists is the far end. A decoder is configured for one
  picture size; a unit at another is a different picture, so the decoder is replaced —
  and a *hardware* decoder answers that by tearing down a platform decode session and
  asking for another, which is slow, scarce, and the first thing to fail when it is
  asked for too often. Region geometry meanwhile is a bounding box of whatever moved
  half a second ago, and it moves a cell for reasons nothing on screen would call a
  change. Measured by replaying 98 retunes captured from a real scroll on a 1920×1080
  Windows desktop: exact boxes cost 124 streams and 56 client decoder builds, the
  ladder alone 102 and 40, the ladder aligned 97 and 37 — for 14% more streamed cells,
  which are lossy and owed a cleanup like every other cell of a region.

  Quantizing grows boxes, so two that shared no cell can share one afterwards;
  `stabilize` merges any overlapping pair and re-quantizes the union, because a cell in
  two live regions is a cell two streams both carry.
- **A stream's end is said, not guessed.** `ServerMsg::VideoEnd { stream }` — the
  counterpart of `VideoFormat`, and about the resource rather than the picture. A
  client keys its decoders by stream id and is otherwise never told one is finished;
  an id simply goes quiet, indistinguishable from a region that is merely still, so a
  browser accumulated a decoder per id it had ever seen and held them for the session.
  The gateway drains the ended ids on the ordered path — `retune` before the round it
  took them from, `expire` inside the task the units go out on — because ids are
  reused, and an end that overtook the next stream's units would close a decoder that
  had just been built. An id a retune both frees and hands straight back is not an end
  at all: the decoder on it is still the right decoder for that id.

  A resize ends streams too, and says so. `forget` drops every live stream, but a
  resize is not an attachment boundary on the client: the worker's `resize` command
  reallocates the bitmap and touches nothing else, and only `clear` — a repaint or a
  reattach — empties the decoder table. So the ids `forget` drops are recorded as ends
  like any other, and the round that was out on a worker when the resize landed —
  which `take_round` had already taken the live table for — records its own in
  `put_back`'s stale-epoch branch, the last place that knows a client is holding a
  decoder for them. Under `Policy::Whole` neither records anything: that dial rebuilds
  its one stream on the same id and never drains this.

  What the client does with it is *retire*, not close. In the same 98-retune replay the
  gateway ended a stream 88 times, and most of those ids went straight back to another
  region at the same picture size — which an open decoder decodes from the next
  keyframe without being rebuilt. Closing on the spot turned 37 decoder builds into 97,
  so `videoDecoder.ts` starts a four-second clock instead (`RETIRE_MS`) and any unit on
  the id cancels it: 39 builds, and the session still comes back.
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
| `motion` + a still encode at 10 | 4.5 MB | 0.17 s |
| `motion` + `stream` 30 | 0.70 MB | 1.39 s |
| `video` 60 | 0.45 MB | 5.48 s |

So the regions cost about a sixth of the still motion encode's bytes with the still
parts left lossless, and a quarter of whole-desktop `video`'s CPU — because only what
moved was coded, rather than 1280×800 every frame. What it buys over `video` is
exactness everywhere else; what it costs is bytes.

Both dials that stream share `Congestion`, one verdict for one link: the quality dial
walks down when a round's push blocks and back up to `render_motion_quality`, never
past it. Unlike `video`, a target here keeps the ordinary `FRAME_BUFFER` depth,
because the same queue carries its still tiles — so `coarsened` in the totals is a
less sharp signal, which is worth knowing when reading it.

#### `video`: a different transport, not a fourth codec

`render_type = "video"` sends the whole framebuffer as one inter-frame video stream
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
- **The picture may be a pixel larger than the region.** The I420 conversion needs
  even sides — VP9 itself does not and is held to them anyway — so the mirror is
  padded up with its edge repeated (black would be a seam the encoder paid for every
  frame). The record header carries the *true* rectangle and the
  client crops — reporting the padded size would push a paint past the framebuffer,
  which the renderer drops outright rather than clamps.

`render_quality` maps to a constant quantizer: the dial spans 63 → 8 of VP9's own
0–63 (the floor is where screen content goes visually lossless — mapping past it
would give a dial whose top third did nothing but spend bandwidth). The quantizer
never leaves the codec module —
the dial is what everything above it speaks. A constant quantizer *is* variable
bitrate — bits go where the
picture needs them, so a motionless desktop costs almost nothing.

The dial is a **ceiling**, and that framing is what makes adaptation tractable here.
`Congestion` in `src/encode.rs` watches one local signal — how long queueing an
access unit blocked — and walks the 1–100 dial down towards 1 when the link is behind,
back up towards the configured quality when it is not; never past it. It moves the
dial rather than a quantizer because a quantizer is the codec module's own scale
and never leaves it. What TCP hides is
*headroom*, and this never needs headroom, because exceeding the operator's setting
was never a goal. "Am I behind?" is the whole question, and the outbound queue
answers it. Quality moves through `Stream::set_quality`, which re-tunes the running
encoder rather than rebuilding it: a rebuild would force a keyframe per adjustment,
spending a few hundred KB exactly when bytes are scarce.

`render_adaptive = true` gives the same walk a second signal and an operator's
floor. The signal is the client's own lag: the paint window already tracks how
long the oldest unacknowledged batch has been owed, and `LinkFeedback`
(`src/feedback.rs`) publishes that age minus a baseline — the smallest recent
end-to-end time, so distance never reads as queueing; RustDesk and Guacamole
both make the same subtraction. Sixty milliseconds of queueing lag counts as
a behind frame even when nothing local blocked, which is exactly the case the
paint window measured a VP9 attachment falling 222 ms behind at 7 batches in
flight while every queue stayed shallow. The walk's floor moves from 1 to
`render_adaptive_min`, and the same key puts a *per-encode* quality on the lossy
tile paths: Guacamole's curve — one quality point per millisecond of lag past
20 ms, clamped at the floor — applied at `Shared::adapted` wherever a JPEG
tile is about to be encoded, cleanups included. PNG passes through
untouched; which cells deserve losslessness was the operator's call, not the
link's. Without the key, nothing changes: pressure-only walk for streams, fixed
quality for tiles.

That signal only works because those queues are shallow. `FRAME_BUFFER` is 64, sized
for tiles — a 1080p repaint is ~17 bands — but under `video` one message is a whole
frame, and 64 of them in each of two queues in series is seconds of buffered
picture. A video target gets `VIDEO_FRAME_BUFFER` (4) at both hops.

**The client decodes it with WebCodecs** `VideoDecoder`, reached through
`frontend/src/videoDecoder.ts` and driven from `tilePainter.ts` — the shared batch
loop, which keys a decoder per `stream` id and replaces one whose region has
restarted on a different size. **Which decoder is the platform's choice**: the
configuration states no `hardwareAcceleration`. A `prefer-software` hint was tried
and removed, because it bought one platform's decoder at most — WebKit honours it
only on macOS (the clause routing it to a local software decoder is compiled
`#if PLATFORM(MAC)`), Firefox disregards it, and iOS has no software VP9 decoder to
route to at all, so VP9 there is VideoToolbox or nothing (measured against WebKit
main, 2026-08-21). What makes a hardware decoder safe on the region dial is the
gateway rather than a hint: the `SPANS` ladder holding picture sizes still and
`VideoEnd` handing finished decode sessions back, above. Verified by fast touchscreen
scrolling on the region dial and whole-desktop `video` on a Mac and an iPad with no
decode errors. The stall backstop in `createVideoStream` — silence where a decode
error belongs, then a failed end-of-stream flush, which is how a GPU-process decoder
fails — stands whatever ends up decoding. That whole loop — parse, decode, paint, and the
decoders with it — runs in a dedicated worker drawing on an `OffscreenCanvas`
(`desktopPainterWorker.ts`, handled from the page by `desktopPainter.ts`); each
binary frame is transferred there, not copied. What that boundary buys is narrower
than it looks — `createImageBitmap` and `VideoDecoder` were never doing their work
on the main thread anyway — and is mostly presentation: a transferred canvas commits
from the worker, so a frame reaches the screen without the thread carrying input and
React being scheduled for it.

A browser without `VideoDecoder` never reaches this code — the preflight gate turns
it away before React mounts (`preflight.ts`). What survives is the narrower failure:
a decoder that exists and refuses this *configuration*, which no keyframe and no
neighbouring region repairs. That is *said* rather than logged, because a video
target sends no still tiles and the alternative is a desktop that never paints and
never explains itself: a banner that stays up, naming the configuration the browser
would not take.

#### The codec

Video is **VP9 only** (`src/vp9.rs`), and there is no codec key. VP9 is
BSD-3-Clause with a patent grant and present in every browser build, the ones that
carry no proprietary codecs included. On synthetic screen content at 1080p and
quality 60 it encodes a frame in **4.7 ms** at **18 KB** — measure with
`cargo test --release measure_the_encoder -- --ignored --nocapture`; a debug build
reports nonsense, because the RGB→I420 conversion it also times is scalar Rust and runs
66× slower unoptimised.

Nothing downstream of `TargetConfig::render_plan` names a codec: `encode.rs`,
`regions.rs` and the wire carry access units, a keyframe bit and a configuration
string, and `vp9.rs` is reachable only from `regions.rs`.

**The browser is not asked, and that is a deliberate reversal.** The client used to
probe: `/api/config` published the gateway's ordered codecs with a WebCodecs string for
each, the client asked `VideoDecoder.isConfigSupported` about them before login, and
`ClientMsg::Connect` carried the accepted names for `connect` to pick from. It worked,
and it was removed. It put a round trip and a decoder query in front of every video
session; `isConfigSupported` is not reliable enough on the same browser twice to build a
refusal on; and because the refusal was phrased as "this browser accepted neither", any
fault anywhere near the path — a serde field-name mismatch, for one — surfaced as an
accusation against the browser and sent the reader to the wrong half of the system.

What replaces it is one honest failure. The gateway announces the
configuration in `ServerMsg::VideoFormat` before the stream's first unit,
`VideoDecoder.configure` accepts it or refuses it, and a refusal is reported by name —
"this browser cannot decode the video this target sends" — with the configuration
string beside it.

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

Any claim by a different browser — a forced takeover, or a plain claim while
nobody is attached — closes the previous WebSocket and its engine but
preserves the selected target: the new claimant's attach reconnects that
target for its own screen (named on the `/ws` URL), so a desktop opened for
one display never carries its size and density over to a different device.
Only the owner reclaiming its token resumes the running engine, with a
full-repaint request instead of a reconnect.

Login tokens are held in memory with sliding expiry and delivered through an
`HttpOnly`, `SameSite=Strict` cookie. The cookie is marked `Secure` when
`x-forwarded-proto` reports HTTPS. Restarting the gateway invalidates all
logins.

## Client protocol

`src/protocol.rs` and `frontend/src/protocol.ts` define the client contract.
`GET /api/config` publishes the deployment branding before authentication —
the display name, and whether `GET /api/logo` (equally public) serves an icon
the page then sets as its favicon. There
is no client/server version negotiation: the gateway serves the matching SPA from
the same build, and no second client is supported.

Control and input messages are tagged JSON. Server messages cover picker and
connected state, desktop size, display selection, cursor shape, clipboard,
audio format, and errors. The `connected` message includes `resize`,
`clipboard`, and `audio` capability flags so clients expose only supported
controls.

It also carries three things a client cannot work out and nothing else reveals:
`render`, the resolved render dial; `video`, the codec family or null; and
`subtype`, the target's `ard` or `ard-high-performance` where it has one. The
last is there because `protocol` is not an answer on VNC — a plain server, a Mac
in Standard mode and a Mac in High Performance mode all say `vnc`, and they
differ in whether there is a display list, whether resize is offered, and
whether the path beneath is the reverse-engineered one. All three appear on the
client's session card, which `frontend/src/connectionLabel.ts` and
`videoLabel.ts` word.

`GET /api/targets` carries `subtype` too, so the picker names it one step
earlier — the difference between two Macs in that list is a choice being made,
not something to discover after connecting. The row uses the config spelling
alone (`VNC · ard · 192.0.2.10:5900`); the card, which describes one target and
has the room, spells it out.

### Image batches

Screen updates use little-endian binary frames:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | u32 sequence | records

TILE     op 0x01: u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
TILE_REF op 0x02: u16 slot | u16 x | u16 y
VIDEO    op 0x03: u8 stream | u8 flags | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
COPY     op 0x04: u16 sx | u16 sy | u16 x | u16 y | u16 w | u16 h
```

Tile formats are PNG and JPEG. One frame carries multiple ready updates so a
repaint does not require one WebSocket event per tile. Receivers reject unknown
operations, truncated records, and unsupported formats, and reject a nonzero frame
flags byte. A `VIDEO` record's own flags byte is `0x01` for a keyframe and nothing
else — any other bit is rejected the same way.

`sequence` starts at one and increases for the lifetime of one session-socket
attachment. After the paint worker has finished the batch's ordered
parse/decode/draw pass, the client sends `paintAck` with that sequence plus its
worker queue and draw times. `ws.rs` consumes this transport feedback rather than
forwarding it to the remote engine, and logs those measurements with the
attachment totals. A socket generation travels through the worker so a late
completion from a dead attachment cannot acknowledge a new one. This is the
measurement contract for application-level backpressure, and the gateway acts on
it twice: the paint window in `ws.rs` holds the next batch when too many are owed
or the oldest is owed too long, and on a `render_adaptive` target the same
measurement — published through `LinkFeedback` — moves quality before the window
ever parks. Nothing is dropped either way; an access unit's dependency order is
untouched.

`TILE` draws a payload and optionally stores it in a gateway-selected cache
slot. `TILE_REF` redraws the encoded payload already stored in that slot.
`NO_SLOT` means the payload must not be retained. Clients keep a fixed
`SLOT_COUNT` array and never choose eviction themselves.

`VIDEO` carries one VP9 access unit for one region,
and is a separate record rather than a fourth tile format because it is not the same
kind of thing: a tile is a self-contained picture and an access unit is one link in a
chain. Making it its own record is what keeps the cache and coverage rules above from
ever having to ask whether they apply — they see tiles only. `stream` names which
decoder it belongs to, since a session may run several at once, and its keyframe bit
comes from the encoder rather than from parsing the payload — VP9 carries no parameter
sets to read one out of. The rectangle is the region's true one, and the decoded picture
may exceed it by a pixel on either axis (see the render dial).

`COPY` moves pixels the client already holds from `(sx, sy)` to `(x, y)`, both
`w`x`h`: RFB's CopyRect carried through to the browser instead of stopping at the
gateway, which used to read the source out of its shadow and re-encode it. Thirteen
bytes whatever the rectangle, which for a scrolling window is most of a desktop.
The client blits its own canvas, so an overlapping copy moves the original pixels.

A copy *reads* the canvas at its place in the order, which is one more constraint
than the other records carry: everything before it in the batch has to have been
drawn, so `wire.rs` will not drop a tile that precedes one — coverage reaches back
only as far as the last `COPY`. A copy is never itself dropped, cached or
referenced; it is an instruction and not a picture. Only a target whose canvas is
made entirely of tiles is sent them (`TileSink::copies`): under a motion strategy a
cell owes a cleanup from stashed pixels that would be restored over anything copied
in, and under either streaming plan the client's pixels come from a decoder rather
than from tiles at all. Both fall back to reading the source out of the shadow.

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
lost wave buffer is a hole. The dedicated socket removes that picture-induced loss
path entirely.

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
| `opus` (default) | `opus` | `audio_bitrate`, default 96 kbit/s | 48 000 | 960 (20 ms) | `OpusHead` |
| `pcm` | `pcm-s16le` | 1.41 Mbps | 44 100 | 0 (self-describing) | empty |

Opus's rate is a per-target key, and `audio_adaptive = true` makes it a ceiling
the link may fall below: `AudioCongestion` (`src/audio.rs`) lives beside the
pump's send. The audio socket's queue is deliberately two deep; two consecutive
sends that each wait at least 20 ms are a behind verdict. The walk moves the
encoder's bitrate down by a third toward `audio_bitrate_min` (default 32 kbit/s),
and back up by an eighth after sustained clear sends. The change reaches the live
encoder through `OPUS_SET_BITRATE`; packets stay 20 ms and independently
decodable, so nothing is re-announced. While the link is
*behind*, wave buffers that are pure silence are shed before the encoder instead
of queued — silence is the one content whose loss cannot be heard, the client
just receives no packets for a while (what a quiet remote already produces), and
the backlog drains by exactly that much. Both keys are Opus-only and refused
beside `pcm`, which has no encoder to tune.

`pcm` is passthrough: the remote's wave buffer becomes one packet, byte for
byte, with no encoder in the gateway and no decoder in the client. `pcm-s16le`
is deliberately not a WebCodecs codec string — the packets are interleaved
signed 16-bit little-endian samples, which is what an `AudioBuffer` holds
already, so the client builds one directly and schedules it on the same path an
Opus packet reaches after decoding. That makes it the only option whose packets
reach no decoder at all — which is a property of the path, not a compatibility
escape hatch: the client refuses to start without WebCodecs either way.

It also makes it the only option whose `sampleRate` is not 48 000. An
`AudioBuffer` carries its own rate, so a context built at 48 kHz before the
format arrived simply resamples on playback, exactly as the OS mixer would for
any buffer that is not at the device's rate.

The bandwidth is the whole of the trade: 1.41 Mbit/s is fifteen times Opus, and
is a local-network proposition only. It is not a quality argument — Opus at 96
kbps is well clear of audible loss on this material. Guacamole carries desktop
audio this way and only this way (its single encoder emits
`audio/L16;rate=44100,channels=2`), which is where the option came from — see
[`rdp-perf-vs-guacamole.md`](rdp-perf-vs-guacamole.md) for the comparison.

An audio-enabled RDP engine negotiates one 44.1 kHz, 16-bit stereo PCM format
when it connects, and offers no other — MS-RDPEA identifies a buffer's format by
index, so one advertised format makes the index unambiguous. The gateway does not
implement the channel itself: it registers as FreeRDP's `rdpsnd` output *device*,
the piece an ordinary client points at ALSA or CoreAudio (`src/rdp_audio.rs`).
Both transports are registered — the static `rdpsnd` channel and the dynamic
`AUDIO_PLAYBACK_DVC` — because which one a server drives is the server's choice,
and the wrapper lets only the first to open fill the queue. Windows also requires
`rdpdr` to be advertised alongside them. Under `opus` the gateway resamples that PCM to
48 kHz in exact 882-to-960 groups (`src/pcm48.rs`) and cuts packets out of the
result; under `pcm` it does neither, and the buffer is only cut on a frame
boundary so a split sample cannot transpose the channels.

The queue never blocks the RDP read loop. `AudioBridge` retains sixteen remote
wave buffers (about three seconds at the measured Windows cadence) and drops the
oldest when a listener falls behind; no receiver means audio is discarded. Between
the bridge and the socket sits a second, shallower two-buffer FIFO
(`AUDIO_SOCKET_BUFFER`) whose only job is to absorb a socket write in flight.
Losses belong at the bridge, which keeps sound that is still live, rather than in
that FIFO, which would deliver stale audio faithfully.

The client owns its playback schedule. It starts at the current audio playhead
with no added cushion and clamps accumulated lead to 300 ms, trimming the front
of an incoming buffer instead of turning temporary jitter into lasting latency. What
reaches that schedule differs by codec, and only there. The client does not decode
anything itself: an *encoded* stream goes to WebCodecs, so a codec a browser will
not take surfaces as a decoder error naming it rather than as silence. A
`pcm-s16le` stream reaches no decoder at all; the client turns the packet into an
`AudioBuffer` and schedules it directly.

A quiet remote and one that never negotiates audio are indistinguishable to the
client, so detailed negotiation status remains in the gateway log.

### Camera frames

The browser's camera goes the other way, on a third socket, and only to an RDP
target that opted in with `camera = true` (refused on VNC at parse time: the
channel is MS-RDPECAM and RFB has no equivalent). **Opening
`/ws/camera?session=<token>` is the enable** — explicit, per session, and never a
remembered preference, unlike audio's "sound by default". Its refusals add one
code to the family: 401 before the upgrade, 4000 for a stale token, 4001 on
eviction, and **4002** when the running target carries no camera (or no engine is
running at all). Where the audio socket is bound to the claim alone and survives a
target switch, the camera socket is bound to the claim *and the engine*: every
engine end and every claim change closes it, so the next session always starts
with the camera off. Closing it — either side — unplugs the virtual device from
the remote.

The socket's first message is `cameraFormat`, naming the H.264 the browser's
`VideoEncoder` is configured for (geometry and a rational frame rate); its arrival
is what announces the device to the host. Binary frames follow, one encoded access
unit each:

```text
u8 kind = 0x04 | u8 flags (bit 0: keyframe) | the Annex B access unit
```

Downstream the gateway relays the host's MS-RDPECAM decisions as `cameraStart`
(with the confirmed format), `cameraStop`, and `cameraKeyframe`; the browser
encodes only between start and stop, restarting at an IDR, and honors a keyframe
request on the next frame. Streaming begins when an application on the host opens
the camera, which is the host's move alone — an enabled camera on an idle desktop
sends nothing.

The host also decides whether redirection exists at all, before any client
message: the MS-RDPECAM enumeration channel is created by the server, and a
**Windows Server without the Remote Desktop Session Host role never creates it**.
Measured on Server 2025 Datacenter — Microsoft's own client gets no camera
against it either, and neither installing Media Foundation nor clearing the
`fDisableCameraRedir` policy changes it — so an enabled camera against such a
host is an announcement nobody asks about: the socket stays open and
`cameraStart` never comes. Installing the role on the host is what turns the
channel on:

```powershell
Install-WindowsFeature RDS-RD-Server -IncludeAllSubFeature -Restart
```

Windows 11 creates
the channel and installs the redirected device as a real camera
("Remotex Camera (redirected)", enumerable by every capture application) for
exactly as long as the camera socket holds it plugged.

The gateway never transcodes — the PCM-passthrough bargain in the other
direction. The browser encodes Annex B Constrained Baseline H.264
(`frontend/src/cameraSender.ts`), the host's own camera stack decodes it, and the
gateway advertises exactly one media type: the announced geometry. There is no
codec key beside `camera`, and a browser that cannot encode H.264 reports that by
name instead of falling back.

The channel itself is implemented in Rust in the FreeRDP wrapper
(`crates/freerdp/src/camera.rs` in libfreerdp-prebuilt) as a generic dynamic
virtual channel plugin: the archives compile FreeRDP's own `rdpecam` out, because
that implementation is a V4L capture stack with its own H.264 encoder and this
camera's source is a browser. The wrapper answers the enumeration channel's
version exchange, announces one device, serves its stream and media-type queries,
and meters samples by the host's credits — one SampleResponse per SampleRequest,
a short queue while credit is owed, and on overflow the queue is dropped whole
and the browser is asked for a keyframe, because H.264 cannot resume mid-GOP.
`src/rdp_camera.rs` adapts that endpoint to the gateway's `CameraBridge`
(`src/camera.rs`), which is all the session layer sees.

Client JSON messages cover pointer, wheel, keyboard, clipboard, display
selection, viewport size, refresh, cache reset, and session control. Pointer
motion is coalesced while the socket has queued bytes; any non-motion input
flushes the latest held position first.

A target's `resize` means the window drives the remote's size, continuously and
on every engine alike: an engine that has it applies every `viewport` it is
sent — in points, the window's CSS pixels, rendered at the engine's own density —
an engine without it drops them all, and the client sends them exactly
when `connected` said `resize` — on every window change, with no toggle, no
manual button and no remembered preference beside it. Standard `ard` rejects
`resize` at config parse because it shares physical displays. On RDP the `egfx`
key (default true) keeps the graphics pipeline on, making each resize a graphics
reset instead of a reactivation; that trade is the operator's, not the client's.

The opening size is one rule for every engine that can ask for one: the pinned
`width`/`height` when the config sets both, else the full resolution of the
client's own screen — carried in the `connect` message so it exists before the
engine's handshake — else the built-in default. See
`TargetConfig::opening_size`.

What is engine-specific is the mechanism:

| Engine | With `resize` |
|---|---|
| Generic VNC | applies a requested size, on servers accepting SetDesktopSize |
| Apple Standard VNC | rejects `resize`: it shares physical displays |
| Apple High Performance VNC | applies dynamic-resolution sizes within its fixed 3840×2160 backing ceiling |
| RDP | applies a requested size, and the client's reported display density |

`hostDisplay` reports the screen the client's window is on — its full resolution
and its density. Mid-session only the density is acted on, and only with
`resize`: RDP quantizes it to 1x or 2x at a midpoint, a High Performance virtual
display re-renders the same points at it; the resulting density travels back as
the `scale` on `resize`, and clients present the framebuffer at `pixels / scale`.
Other engines ignore the message.

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
  while a fetch is pending, the normal framebuffer cycle finishes its one
  outstanding response and pauses before requesting another, leaving the ordered
  server stream free to deliver the pasteboard reply;
- RDP requests `CF_UNICODETEXT` after a remote format announcement.

Clients may request the current value after attaching, since they may have
missed earlier pushes. Replies to that explicit request are marked separately
from unsolicited changes. Only unsolicited changes are eligible for automatic
remote-to-local synchronization; an explicit fetch fills the UI until the user
chooses Copy.

Transfers are capped at 512 KiB and refused rather than truncated. Browser
clipboard integration is best effort because Safari's permission rules, and an
unfocused tab, may prevent automatic access.

### Liveness

The gateway sends a WebSocket ping every five seconds. Browsers answer at the
protocol layer, independent of application timers. About 60
seconds without a pong ends the engine; an orderly close starts a fresh
60-second reattach window.

All remote sockets use `TCP_NODELAY`, a 20-second connect budget, a 30-second
handshake budget, and TCP keepalive. Linux also uses `TCP_USER_TIMEOUT` to bound
unacknowledged writes. These checks prove only that the peer's kernel responds.
RDP and RFB have no portable application ping.

Browser-facing sockets get `TCP_NODELAY` too — `NodelayListener` in
`src/server.rs` sets it on every accepted connection, in both the served and
embedded shapes. Those sockets feed an ack-gated paint window, and a segment
Nagle holds back is that window stalled for a round trip.

## Engines

### RDP

A statically prebuilt **FreeRDP 3** does the protocol, behind the safe wrapper in
the `freerdp` crate (`github.com/andrewtheguy/libfreerdp-prebuilt`) — the same
prebuilt-archive bargain libvpx and libopus already strike here, so this build
needs no cmake, no pkg-config, no OpenSSL and no libclang. FreeRDP owns the socket
and does TCP, TLS and optional NLA/CredSSP on a thread of its own, keeping a
complete framebuffer in Rust-owned memory and posting an event per damaged
rectangle. The engine compares those rectangles with a shadow of pixels already
sent, splits the remainder into bands, and encodes off the event loop. Input is
mapped from DOM codes to scancodes and queued to FreeRDP's thread.

Damage is flushed at the server's own frame boundaries where the server marks
them, which every server measured so far does: the wrapper requests both legacy
frame-marker capabilities and surfaces the END — and EGFX's once-per-frame surface
flush — as a `Frame` event. On the first one, the engine stops guessing: the
16 ms coalescer (`DAMAGE_INTERVAL`) that reconstructed boundaries by timing
demotes to a 100 ms safety net under the marker, so a frame is presented when the
server says it is whole, not up to 16 ms later and never cut in half.

Under a plan that takes copies, each flush first searches the damage for regions
the client already holds elsewhere on its canvas (`src/copies.rs`, guacamole-
server's cell-hash search over this gateway's shadow): a scroll goes out as a few
`COPY` records instead of image bytes, and the tile pass carries only what the
copies did not — including repainting anything a copy got wrong, which is what
makes a wrong copy waste rather than corruption.

It replaced IronRDP, which was not stable enough against real Windows hosts.
The `egfx` target key controls the Graphics Pipeline and defaults to true,
independently of `resize`. EGFX is advertised with RemoteFX beside it, and that
pairing is load-bearing: the pipeline advertised *alone* was
measured broken — against a Windows 11 host, FreeRDP decoded 21 surface commands
with no errors into a framebuffer that summed to exactly black — and the codec
next to the flag is what guacamole-server ships against the same Windows
generation. With the pair, the same measurement is a painted desktop. Under EGFX,
a resize is a graphics reset with no reactivation or reconnect; the trade is that
a Windows host's text stays soft afterward. `egfx = false` selects the legacy
bitmap path, whose full reactivation re-renders the desktop sharp. Servers without
the pipeline (xrdp among them here) also use that legacy path, which the wrapper
keeps working through resizes by resizing FreeRDP's decoder contexts alongside the
framebuffer — FreeRDP itself sizes them once, at connect, which is an upstream bug
this repository stops carrying at its own layer.

The pointer is not part of that framebuffer. RDP servers send the cursor's shape
rather than drawing it, and each shape goes to the client as `cursor`, which draws
it on its own hardware pointer. A mouse move therefore costs the session nothing
at all, where compositing the pointer into the framebuffer put every one of them
through damage, the flush interval, an encode, the socket, a decode and a paint.
The server's own pointer *positions* are dropped: the browser's pointer is already
where the mouse is, and nothing here can move a hardware pointer.

With `resize = true`, the Display Control Virtual Channel applies explicit
desktop-size requests, and also matches the client's display density: a monitor
layout carries `DesktopScaleFactor` beside the geometry, so a Retina client gets
twice the pixels with the host's UI drawn at 200% rather than the same UI
stretched. The opening RDP handshake is always 1x; the client applies its screen
density after `connected`, so a Retina client costs a graphics reset on the default
EGFX path or a reactivation on the legacy path. RDP reports no scale factor back,
so the density here is declared rather than measured. With `clipboard = true`,
MS-RDPECLIP carries `CF_UNICODETEXT` with CRLF/LF conversion.

With `audio = true`, `rdpsnd` carries the remote's sound — see [Audio
frames](#audio-frames). Enabling it has one side effect worth knowing: a Windows
host starts measuring the link, and this gateway has declined to be measured
(`ConnectionType` is declared a LAN rather than probed, because a server's own
estimate of the hop between it and a gateway beside it throttled updates badly).
Declining the answer is not enough on its own — the message channel those PDUs
arrive on has to be closed too, or the session dies when one is asked. That is
the wrapper's business, and it is measured there.

A size change that is *real* costs a graphics reset on EGFX. On the legacy path it
costs a full Deactivation-Reactivation Sequence; FreeRDP runs it internally and
reports a new desktop size. A Windows host cannot carry sound across the legacy
event: its audio redirector dies at reactivation — measured
mid-playback, five
resizes of six left the channel open and mute, the last wave within a second of
the reactivation, no close, no re-announce, and nothing in MS-RDPEA for a client
to restart it with. The wrapper therefore resizes such a session — recognised by
its sound having negotiated on the dynamic `rdpsnd` transport, which is how
Windows and only Windows carries it — by *reconnecting* at the new size, the way
Guacamole's `resize-method: reconnect` does: ~800 ms measured, channels and sound
renegotiated, surfacing as the same resize it always was, with one line on stderr
saying a reconnect is what it cost. The wrapper debounces reconnect-resizes for
300 ms. xrdp's static-channel audio rides out its reactivation, so it keeps the
plain monitor-layout resize untouched, as does any legacy session without sound.
Asking twice for the same size triggers one change, and a request equal to the
current size never triggers one. A layout is asked for on a bounded schedule rather
than once, because a Windows host discards one sent before the session it is
starting has settled and acknowledges nothing either way — measured through both
engines.

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
source region rather than carrying pixels, and where the client's canvas is made
entirely of tiles that carries straight through as a `COPY` record: the shadow
moves its own copy of the pixels and the browser blits its own canvas, so a scroll
costs thirteen bytes on both links instead of an encode on the second. Where it
does not — a motion or streaming plan — the pixels are read back out of the shadow
as before. Either way a source the shadow does not know costs one non-incremental
repaint rather than an invented picture.

Generic `vnc` also advertises **ContinuousUpdates** and **Fence**, which go
together. A server that supports the first answers the `SetEncodings` carrying it
with an `EndOfContinuousUpdates` message — the only way it is ever announced — and
the client then asks for the whole desktop and stops polling: updates arrive as
the screen changes rather than one per request, which takes a round trip out of
every frame. Non-incremental requests are unaffected and still go where they went,
because a repaint no amount of waiting for damage will produce is exactly what a
reattach, a resize and an unknown CopyRect source need; a resize also re-sends the
enable, since the region is part of the request. What that removes is this
engine's only pacing, which is what Fence restores: the server sends a marker down
the stream and asks for it back, and the read loop echoes it immediately, so its
congestion control can measure this end. A server offering neither is unaffected —
it says nothing and the polling loop never stops. The Apple subtypes are not
offered either: their encoding lists are measured exact, and adding to one costs
the display layout.

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
dynamic-resolution path behind `resize = true` remains reverse engineered. It
authenticates identically — the same security type 30 — and then
differs in three places and nowhere else: the version banner, the `0xC1` ClientInit
byte, and a cleartext `SetEncryption` prelude after which every byte in both
directions rides inside an AES-128-CBC record layer keyed by a rekey message the
server delivers, of all places, inside a framebuffer rectangle. `src/vnc_record.rs`
is that transport, exposed to the rest of the engine as an ordinary `AsyncRead` and
a per-message sink; `src/vnc_apple.rs` is the message and payload layer above it.

**High Performance mode is a virtual-display mode.** The gateway sends
`SetDisplayConfiguration` (`0x1d`) during setup, with one mode built from the
pinned `width` and `height` when both are set, or from the connecting client's
screen resolution otherwise, at that screen's density. The mode sits under the
native descriptor's fixed 3840×2160 backing ceiling. Once connected, the remote
Mac's physical displays are disabled and all of its windows are placed on that
virtual display. Apple's
official macOS Screen Sharing client can choose up to two virtual displays, while
Remotex always requests one. The full descriptor enables dynamic resolution on
every fresh session. With `resize = true`, the window continuously drives the
virtual display through Apple's dynamic-resolution feature: later viewport reports
resend the same full descriptor with the requested mode, and the Mac's answering
display layout sets the actual framebuffer geometry. There is no client-side
resize mode or one-shot button. The Mac supplies that virtual display over the
003.889 record transport, with zlib rectangles instead of raw pixels. Apple's
virtual-display-count and resolution-preset controls remain unimplemented.

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

**It refuses to start without a secure context and both WebCodecs decoders**
(`preflight.ts`, before React mounts), and that refusal is what lets the rest of the
client be simple: nothing downstream tests for either again or carries a fallback for
its absence. `navigator.clipboard`, `navigator.keyboard` and WebCodecs itself all
require a secure context; the gateway speaks plain HTTP and has no TLS listener, so
one comes from how the page is reached — loopback (`localhost`, `127.0.0.1`, `[::1]`,
any `.localhost` label), or a TLS-terminating reverse proxy. A LAN address over
plain `http://` is the case this refuses,
by name. `VideoDecoder` and `AudioDecoder` are asked for together rather than either
alone, because audio is a target's choice and video is a render dial's: a browser
with one and not the other would play some targets and not others, which is the
half-working session the gate exists to prevent. What remains reportable mid-session
is a *codec* a decoder refuses, which is a different sentence and arrives from the
decoder itself.

There are two ways for this page to be given the six Command chords a browser
otherwise keeps — ⌘W, ⌘T, ⌘N, ⌘L, ⌘O, ⌘R. A **Chrome app window** (`appWindow.ts`:
*Install page as app…*, or `--app=`) reserves no keys at all, so they arrive as
ordinary keydowns and `preventDefault` is the whole of it; that is the configuration
the client is meant to be run in. A plain tab gets the same from full screen plus
`navigator.keyboard.lock`
(`keyboardLock.ts`), which also locks ⌘Q. That lock is an automatic browser enhancement,
not a mode or menu control; it follows fullscreen because Chromium does not grant it to
a windowed tab. The Command translation table itself is always complete and never
changes with fullscreen. App windows therefore send every chord in windowed and
fullscreen use alike, while a normal windowed tab remains subject to the
shortcuts Chrome consumes before the page sees them. The window kind moves in one
direction only: *Install page as app…* reparents the live document into the new window
instead of reloading it, so `appWindow.ts` latches its answer true and notifies rather
than answering once at load — and full screen, which reports `display-mode: fullscreen`
and would otherwise unmake an app window mid-session, is what the latch defends
against. A close chord the page never sees — and Alt+F4, which no window catches —
ends the session without asking: the client raises no leave-site dialog, because a
dialog on every deliberate window close is worse than the session it saves.

The canvas is presented at the remote's point size, derived from framebuffer
pixels and remote scale. Desktop clients scroll when necessary. Touch clients
use fit-to-width presentation, pinch zoom, pan, a virtual cursor, and
multi-finger gestures without changing framebuffer coordinates.

That touch layer is a trackpad, and there is a second one that is a touchscreen.
Against a Windows host the RDP engine offers MS-RDPEI (`Connect::touch`, always
on), and when the host opens the channel the gateway says `touchReady`; a
touch-capable client then shows a **Touchscreen** switch (remembered, off by
default) that forwards fingers as `touch` contacts — down, move, up, cancel, in
framebuffer pixels, named by small slot ids — instead of interpreting them. The
guest recognises the gestures itself: Windows' tap, drag, press-and-hold, pinch,
two-finger scroll and edge swipes are its own. VNC has no touch and never sends
`touchReady`; a reattach re-announces it and cancels the contacts of whoever was
attached before. See `frontend/src/touchPassthrough.ts` and `src/rdp.rs`.

On a Mac host connected to a non-Mac remote, selected Command shortcuts are
translated to Control. A Mac-keyboard toggle disables translation, and the
gateway's `remoteOs` message suppresses it for Mac remotes.

Each tab stores its claim token in `sessionStorage`, allowing reconnects to
reclaim the same slot. Busy and evicted states require explicit takeover or
reclaim actions.

### Local multi-instance control plane

`remotex tui --port <port>` is the native local control plane. It discovers one
instance per immediate subdirectory, creates and edits the same serverless
`remotex.toml` format the former Electron viewer used, and starts, stops or
restarts each gateway from its own list. `remotex.localhost:<port>` is a landing
page; `<instance>.remotex.localhost:<port>` is that instance's browser origin.

```text
browser: <instance>.remotex.localhost:<port>
                    │ Host-routed HTTP and WebSockets
                    ▼
             TUI master process
                    │ <instance>/gateway.sock
                    ▼
       hidden serve-embedded subprocess
```

The master is the only TCP listener, and it takes its port the way `serve` takes
its address: `DEFAULT_PORT` (52380, the one `[server].listen` defaults to —
they are two ways to serve, never two servers) unless `--port` or
`REMOTEX_TUI_PORT` overrides it. Both loopbacks are bound through the same
`server::bind_all` a served gateway uses, so the policy is one implementation: a
family this host does not have is a warning, and a port already in use is fatal
on either of them, because a browser picks the family and a master left holding
`[::1]:<port>` would keep routing to its own workers. Nothing asks the kernel
for a port — an ephemeral one is a control plane nobody can be told how to
reach, and `SharedPort::bind` refuses `0` on every path, tests included.

Each hidden worker binds
`<instance>/gateway.sock` at mode `0600`, prints one JSON readiness line —
`{"socket","token"}` — after binding, reads only that instance's
`remotex.toml`, and stops when its parent's stdin closes (`src/embedded.rs`,
`Audience::Embedded`). The master seeds the token as a host-only HttpOnly
`remotex_session` cookie before proxying the browser to the child. Raw connection
proxying preserves both ordinary HTTP and WebSocket upgrades without another
gateway protocol implementation.

The entire substrate is behind the default `embedded-gateway` Cargo feature:
the module, token authentication, config audience, CLI commands, and their
`check-config --embedded` validation mode compile out together. Native packages
retain it. Container artifacts are built separately with
`--no-default-features`; the build script and Dockerfile reject a
binary that exposes any embedded CLI surface.

There is still no separate native client: every instance is the same SPA loaded
by Chrome or Edge from its subdomain. The TUI is process and configuration
control, not another remote-desktop implementation.

## Configuration and testing

Configuration is one TOML file with `[server]` and `[[targets]]` sections.
Protocol-specific fields are validated at startup, including mutually exclusive
credential fields and unsupported feature combinations.

A gateway needs a target to offer and a credential to guard it, and is told where
to listen. `remotex check-config` applies those rules to a file — or to text on
stdin, which is what an unsaved edit is — without starting anything.

Where it listens is one key, `[server].listen`, and the one setting a deployment
can give from outside the file: `--listen`, or `REMOTEX_LISTEN` for a container
that has an environment but no argv to edit. An override replaces the address
whole rather than either half of it, so the running address is always the one
somebody wrote in one place.

It takes two forms. `host:port` is the one a browser can reach. `unix:<path>`
binds a socket instead, for a gateway that only ever answers a reverse proxy on
the same machine: the socket is created `0660` so the filesystem decides who may
connect, a leftover from a killed gateway is taken over on the next start, one
that something is still serving refuses the start, and the file is removed when
the gateway stops. No client addresses that form directly — the page reaches its
gateway over one HTTP origin and two WebSockets, all of which need a host and a
port, so whatever terminates the proxy is what a browser talks to. An embedded
gateway is that arrangement in one process tree: the worker listens on
`<instance>/gateway.sock` and never on TCP, and the thing terminating the proxy
is the TUI master, which the browser reaches over loopback TCP and which
forwards each connection to that socket.

`[branding]` is a top-level table rather than `[server]` keys: it names the
deployment rather than the server, and one value with two spellings is one of
them going stale. There is one place to write it and no second spelling. `text`
is the display name; `logo` is the image the gateway serves at `GET /api/logo`
and the page sets as its tab icon.

A logo is written either as a path to an image file or as a `data:` URL holding
the image itself, and the value decides which — nothing else begins `data:`, so
one key covers both and no config can set two. Either way the content type is
settled at resolution, from the extension or from the URL's own media type
against the same closed list of what a tab can show; a `data:` URL is decoded
there too, so `check-config` refuses an icon the browser would have. A file is
then read per request, which is what lets an operator swap the image without a
restart; an inline one is held in the resolved config as `Bytes`, cheap to clone
with the state around it.

Unit tests cover protocol parsing, configuration, authentication, key mapping,
audio, and engine helpers. Tests under `tests/` exercise HTTP/WebSocket session
flow and protocol engines. Containerized dummy servers cover RDP and VNC.

Stable headless browser tests under
[`tests/playwright`](../tests/playwright/README.md) cover deterministic DOM,
control-plane, HTTP, and WebSocket behavior. Rendering races and timing
measurements remain in raw-protocol and container tests.
