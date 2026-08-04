# Client ↔ gateway performance worklist

Findings from the 2026-08-04 audits of the screen and audio hot paths, ranked by
expected payoff. Each item names the code it is about; move it to Done as it
lands. The audits trace the data path from engine read loop to canvas and speaker
and compare it with the reference implementations under
`tmp/programs_for_reference` (IronRDP, FreeRDP, Guacamole, noVNC, RoyalVNC and
rustdesk).

What the audit found *sound* — and later work should not regress — is at the
bottom.

## Next, not backlog

Nothing. The two items that were here are in Done.

## Backlog — explicitly not prioritized

- **IronRDP EGFX.** Negotiate `Microsoft::Windows::RDS::Graphics`, use its real
  frame boundaries/acknowledgments and surface compositor, then separately assess
  AVC420 pass-through. The tested Windows server offers the channel and the pinned
  IronRDP revision contains a client, but this is a larger protocol integration than
  the common browser feedback work.
- **Tight/JPEG/H.264 VNC decode or pass-through.** Generic VNC intentionally
  advertises only the current lossless encodings. Tight-family decoding and direct
  source-payload delivery can remove upstream bytes and a transcode, but neither is
  a near-term priority.
- **Apple Adaptive media.** High Performance currently supplies its virtual display
  over zlib rectangles; Apple's HEVC/AAC-over-SRTP path remains reverse-engineering
  work with no specification. Do not deepen it ahead of the shared path or standard
  `ard` improvements.

## Done

- **VNC Continuous Updates** (`src/vnc.rs`). Generic `vnc` now advertises the
  ContinuousUpdates pseudo-encoding (-313) and, where a server answers with the
  `EndOfContinuousUpdates` that is the only announcement the extension has, asks
  for the whole desktop and stops polling. A round trip leaves every frame: RFB's
  request-per-update cycle meant the gateway learned a region had changed no
  sooner than one LAN round trip after the server knew, on every frame of every
  scroll and every keystroke.

  What that removes is the pacing this engine had, which the backpressure entry
  below already found insufficient — 461 batches deep on generic VNC under motion
  — but which was not *nothing*. Fence (-312) is what replaces it, and is why the
  two are advertised together: a TigerVNC-family server measures the link by
  sending a marker down the stream and asking for it back, and cannot do that
  unless the pseudo-encoding is offered. The echo goes out from the read task
  rather than the input side, because a fence that waited behind a queue would
  report a link slower than it is; `SyncNext` is not implemented and so is not
  claimed back in the echoed flags.

  Non-incremental requests are untouched. They are not polling — they are the
  repaint that no amount of waiting for damage produces — and a reattach, a
  resize and a CopyRect from an unlearned source all need one. A resize also
  re-sends the enable, since the enabled region is part of the request and a
  server left holding the old rectangle would push updates for pixels that are
  gone. A second `EndOfContinuousUpdates` is the acknowledgement of a disable this
  client never asks for, so it is read as the server stopping on its own and the
  polling loop starts again rather than the screen freezing.

  Not offered to either Apple subtype: those encoding lists are measured exact and
  adding to one costs the display layout. Verified against a real Xtigervnc
  (`vnc_tiles_e2e`, which now runs the whole test with the extension on), and
  pinned at the wire by a scripted server in `protocol_e2e` that reads the
  client's own messages — the enable arrives, the fence comes back, and no
  incremental request is ever sent.

- **Browser CopyRect** (`COPY`, op `0x04`; `src/wire.rs`, `src/tiles.rs`,
  `frontend/src/tilePainter.ts`). CopyRect saved the RFB link its pixels and then
  paid the browser link in full: the source was read back out of the shadow and
  re-encoded, which for a scrolling window is most of a desktop per frame. It is
  now a thirteen-byte record naming where the client already has the pixels, and
  the painter blits its own canvas.

  Three things make it sound rather than merely small. A copy **reads** the
  canvas, so `wire.rs`'s coverage rule may not reach back past one — a tile it
  would drop as "a paint nobody could have seen" is the copy's input — which is
  what `copy_barrier` holds; the scan is forward from it, so tiles *after* a copy
  still supersede each other and the common case (no copies) is one comparison.
  A copy is never itself dropped, cached or referenced: it is an instruction, not
  a picture. And it is only sent where the client's canvas is made **entirely** of
  tiles (`TileSink::copies`) — under a `Tile` motion encode a moving cell owes a
  cleanup from stashed pixels that a later tick would restore over anything copied
  in, permanently, since the shadow has already recorded them as delivered; under
  either streaming plan the client's pixels come from a decoder and the mirror,
  not the canvas, is what a region encodes from. Both fall back to reading the
  source out of the shadow, exactly as before. A lossy `base` codec is not an
  objection: the canvas has always been JPEG's or WebP's reading of the shadow
  there, and moving those pixels is no further from the truth than drawing them
  was.

  The shadow moves its own copy in step (`Shadow::copy_within`), reading the
  source whole before writing so an overlapping copy moves the original pixels —
  which is what a canvas blit does anyway, and what RFB requires. A destination
  already holding those exact pixels sends nothing, the same dedup `accept`
  already does. A source the shadow never learned still costs one non-incremental
  repaint rather than a guess.

- **RDP cursor rendered locally** (`src/rdp.rs`, `Pointer`). IronRDP is no longer
  asked to composite the pointer into the framebuffer
  (`pointer_software_rendering: false`); it decodes each shape and hands it over,
  and the engine forwards it as the `ServerMsg::Cursor` the browser has always
  drawn for VNC. A mouse move now costs the session nothing — it moved the
  browser's own hardware pointer — where before every one of them went through a
  damage rectangle, the 16ms flush interval, an encode, the socket, a decode and a
  paint before the pointer appeared to have moved, and left a trail of small
  rectangles behind it for the motion cleanup to sharpen.

  Three details are not incidental. A pointer the server re-selects from its cache
  arrives as the same `Arc`, so identity — not a pixel compare, and not a second
  PNG — is what says "already sent"; that matters because such a selection is
  exactly what a mouse crossing a window edge produces. A cached selection also
  produces a hide *and* a shape in one batch of outputs, so the change is taken
  once per batch rather than per output, or the browser would flicker through its
  own arrow on the way to every shape. And an attaching browser is told the
  pointer whether or not it has changed: a client that has heard no `cursor`
  message hides its own pointer, so silence here would mean no pointer at all
  until the server next happened to change shape.

  Positions are dropped. A `PointerPosition` says where the *server* thinks the
  pointer is, the browser's is already where its mouse is, and nothing on this end
  can move a hardware pointer — so a server-initiated warp is a desync neither
  this engine nor the VNC one can fix.

- **End-to-end screen backpressure** (`src/ws.rs`). The completion feedback below
  made the browser's backlog observable; this bounds it. A screen batch now waits
  before its socket write until the painter admits another — under `PAINT_WINDOW`
  (24) owed, and the oldest owed no longer than `PAINT_LAG_LIMIT` (150ms) — so the
  one hop with no backpressure of its own has the same discipline as every queue
  behind it. The wait is between encode and write, which is what keeps a control
  message from overtaking a batch already ahead of it; inbound JSON and the audio
  socket are separate tasks and never wait. Heartbeats continue while parked, and
  a client that acknowledges nothing is paced by a 500ms grace rather than
  wedging the session — the raw-socket e2e tests never acknowledge anything and
  are unchanged.

  Both numbers come from the UAT profiles in `tmp/test_uat.toml`, measured live
  across idle, continuous motion and interactive input against real RDP, generic
  VNC and Apple Standard desktops — a headless browser attached per workload, and
  the gateway's own `ws: paint totals` line kept for each attachment. That run is
  local and its harness gitignored with the config it drives; the numbers below
  are the record of it. Apple High Performance was measured alongside the rest and
  deliberately kept out of the decision — it is the experimental, specification-less
  path, so it does not get to set a constant every target lives under.

  Under motion the deepest a *working* attachment ran was 22 (1280x800 RDP
  playing video). Generic VNC on the same LAN
  and the same video ran **461 deep, 49ms behind on average and 503ms at worst** —
  RFB has no pacing of its own, so nothing between that engine and the canvas was
  telling it to stop. Windowed, that run held 24 in flight at 2/43ms end-to-end
  and carried the same picture in *half* as many tile records: the backlog was
  paying to encode detail the client was already too late to show.

  The window sits above the deepest working depth on purpose — one a healthy
  attachment hits is a throughput tax, not backpressure. Eight was measured too
  and is worse in the way that matters: the gateway coalesces harder while a batch
  is parked, batches get fatter, and one fat batch takes longer to draw than the
  queue it saved (end-to-end max 124ms against 43ms). The floor on latency is a
  batch.

  Depth alone cannot pace video, which is why the lag rule exists. With the
  renderer throttled twenty times, a VP9 attachment ran 222ms behind while never
  exceeding 7 batches in flight: nothing parked, nothing filled the queues behind
  the socket, and `encode`'s congestion loop — which reads exactly that blocking —
  coarsened not one round. Depth is the wrong unit for a path whose messages are
  whole frames. Nothing is dropped to achieve the pacing, so access-unit
  dependency order is untouched and no keyframe recovery is needed.

  What the same throttled run did *not* show is a case for coarsening video on
  paint lag: even at twenty times slower, that attachment averaged 17ms behind,
  because the throttle lands on the main thread while `VideoDecoder` runs off it.
  The lag rule fired twice in thirty seconds and quality never moved. So the
  quality half of the original plan is left unbuilt — there is no measurement
  asking for it, and the pacing it would have been built on top of is now in.

  Accepted on twenty-four live attachments — every profile under connect, idle,
  motion and interactive input with the final constants. Nothing exceeded the
  window, no attachment sent a batch past it, and no acknowledgment was stale or
  from a dead attachment. Twenty-one of the twenty-four never waited at all,
  which is the property being aimed for: a window a healthy client does not
  notice, and a bound the unhealthy one cannot cross. Pinned by unit test at both
  rules and at the grace — `a_full_window_holds_the_next_batch_until_an_acknowledgment`,
  `a_painter_that_is_behind_holds_a_batch_the_depth_window_would_admit`, and
  `a_client_that_acknowledges_nothing_is_paced_not_wedged`. No browser test: the
  window is a gateway decision about when to write, and the Playwright rules put
  paint timing out of scope for exactly the reason that would make such a spec
  meaningless.

- **Cross-boundary paint completion feedback** (`src/wire.rs`, `src/ws.rs`,
  `frontend/src/desktopPainterWorker.ts`, `useRemoteDesktop.ts`). Batch-envelope
  v4 carries an attachment-local sequence starting at one. The ordered worker
  echoes it only after asynchronous decode and draw finish, with queue and draw
  durations; a socket generation prevents a completion from a dead attachment
  acknowledging a new one. The WebSocket bridge consumes `paintAck` before the
  engine boundary, keeps a bounded timestamp table, treats acknowledgments as
  cumulative, and logs sent/acknowledged/in-flight counts plus queue, draw and
  end-to-end average/max totals on detach. This slice deliberately measures only:
  it neither stalls nor drops a batch. Unit tests pin sequence layout, ordered
  completion, bounded/cumulative tracking and the engine boundary; the independent
  Playwright parsers pin the v4 header seen on the SPA's real socket.

- **PNG tiles: `Compression::Fast` → `Fastest`** (`src/protocol.rs`,
  `encode_png`). In png 0.18 the two select the same `FdeflateUltraFast`
  deflate; the difference is the row filter, where `Fast` means
  `Filter::Adaptive` — all five PNG filters run and scored per row — and
  `Fastest` a single `Up` pass. PNG is the default tile codec on both engines,
  so this multiplied the default path's per-row cost by roughly five.
- **Opus complexity 9 → 5** (`src/opus_stream.rs`). libopus defaults to 9;
  5 is a fraction of the encoder CPU with no audible difference for desktop
  audio at 96 kbps. The `opus-prebuilt` fork exposes `set_complexity`.
- **Canvas 2D context attributes** (`frontend/src/useRemoteDesktop.ts`,
  `desktop2dContext`). `alpha: false` — the framebuffer is opaque, so the
  compositor was blending an alpha channel nothing uses — and
  `desynchronized: true` for the low-latency present path where the browser
  offers one. One helper serves both `getContext` call sites (connect and
  resize) because a second `getContext` on the same canvas silently ignores
  differing options.
- **`createImageBitmap` decode options** (`frontend/src/tilePainter.ts`).
  `colorSpaceConversion: "none"` and `premultiplyAlpha: "none"`: tiles are
  opaque sRGB screen pixels, so color management and premultiplication were
  per-tile work the paint never used.
- **Cached canvas rect for pointer mapping**
  (`frontend/src/pointerRect.ts`, wired in `useRemoteDesktop.ts`). `toRemote`
  called `getBoundingClientRect()` per mousemove — a forced layout flush at
  pointer rate. The cache invalidates on every known geometry change —
  `applyCanvasCss` (zoom, pan, resize message, soft-keyboard inset), scroll on
  capture, window resize — so a pointer event after an in-frame change measures
  again, with a per-frame clear as the backstop for anything unannounced.
  Unit-tested in `pointerRect.test.ts`; a Playwright version would need
  synthetic pointer input with layout-dependent coordinates, which the browser
  test rules place out of scope.

- **`Bytes` through the audio packet path** (`src/audio.rs`, `src/pcm_stream.rs`,
  `src/opus_stream.rs`, `src/protocol.rs`). The bridge's broadcast queue
  deep-cloned every 32 KB wave buffer per receiver; it now carries
  `bytes::Bytes`, so a receive bumps a refcount. Passthrough — the mode whose
  point is touching nothing — went from three full copies per buffer to one:
  a packet is a refcounted slice of the buffer the bridge holds, cut on frame
  boundaries by `Bytes::slice`, and `ServerMsg::Audio` carries that same `Bytes`
  through the queues. The copy that remains is `audio::frame` serializing each
  packet into the wire frame's `Vec<u8>` — unavoidable while packets share one
  frame with headers between them. On the Opus side the equivalent leftover is
  the ~240-byte encoded packet leaving the encoder's scratch buffer, which is
  noise.
- **`pcm48` hot loops** (`src/pcm48.rs`). The deinterleave was byte-at-a-time
  with a `%` per sample; aligned stereo — every buffer the negotiated format
  produces — now deinterleaves whole frames through `chunks_exact`. The
  per-group `drain(..)` on `pending` and `ready` shifted ~550 KB of tail per
  32 KB wave buffer; both are cursors now (`pending_taken`/`ready_taken`,
  through `SequentialSliceOfSlices`), compacted once per push when only the
  sub-group remainder is left to move.
- **Resampler delay in `pre_skip`** (`src/opus_stream.rs`). `OpusHead` carried
  only the encoder's lookahead; the resampler's own leading transient
  (`Pcm48::output_delay`, 160 samples at 48 kHz) is now added, so the decoder
  discards it instead of playing it.
- **Audio queue depths** (`src/session.rs`, `src/audio.rs`).
  `AUDIO_SOCKET_BUFFER` 16 → 2: the queue is FIFO and its one job is keeping a
  socket write in flight from stalling the pump — every slot past that was ~3 s
  of stale sound faithfully delivered over a link already behind, then discarded
  by the client's ceiling. `AUDIO_QUEUE_DEPTH` 64 → 16: the bridge drops oldest,
  and retaining ~12 s when the client discards past 1.5 s on arrival could only
  ever waste the congested link's bandwidth.

- **Decoded `ImageBitmap` cache for `TILE_REF`** (`frontend/src/tilePainter.ts`).
  The slot cache stored encoded bytes, so every reference paid a Blob, a decode
  and a GPU upload for pixels this client had already decoded. Decoded bitmaps
  are now kept per slot under a 16 MiB budget (LRU by re-insertion into a
  `Map`), refcounted so an overwrite mid-batch closes the old bitmap after the
  last pending draw rather than under it. The encoded table stays authoritative:
  an evicted slot re-decodes from its bytes, and adoption happens in wire order
  at draw time so an in-batch double write keeps the bitmap that matches the
  bytes.
- **Wire-order painting without the whole-batch barrier** (`tilePainter.ts`).
  `Promise.all` made every batch paint at its slowest decode with up to 4096
  decoded images alive at once. Decodes still all start together, but the paint
  loop awaits them in wire order and draws each as it lands — one slow decode
  holds back what follows it and nothing before it, and each image is released
  the moment it is drawn.
- **Control messages out of the decode queue** (`frontend/src/useRemoteDesktop.ts`).
  Every message rode one promise chain behind tile decodes, so a cursor shape or
  a clipboard answer queued behind a pixel backlog. Only `resize`, `videoFormat`,
  `connected` and `picker` still do — `resize` clears the canvas, and the audit's
  claim that `videoFormat` needs no ordering was wrong: with a changed decode
  string it drops a live decoder that queued units still need (`setFormat`).
  Everything else runs on arrival. `VideoDecoder.decodeQueueSize` observability
  was not added; it is a diagnostic, not a hot path.
- **Cursor repaint coalesced to the frame** (`useRemoteDesktop.ts`,
  `syncCursor`; `paintCursor` now reads through the shared pointer rect cache).
  `applyView` wrote canvas styles and `syncCursor` immediately read the rect
  those writes invalidated — a forced layout per pinch/pan event. The repaint
  now runs once per frame in a rAF callback, after the writes and before the
  paint, so a gesture frame pays for at most one layout flush shared with the
  pointer mapping, and nothing on screen shows the deferral.
- **Frame-aligned motion coalescing** (`frontend/src/outbound.ts`). Coalescing
  was gated on `bufferedAmount > 0`, which a healthy link never shows. The first
  move of a burst still leaves immediately; after it, moves within one frame
  collapse to the newest, sent at the rAF boundary. The congestion path (drain
  poll) is unchanged and takes over when the socket backs up. Unit-tested in
  `outbound.test.ts` with an injected frame boundary.
- **`Int16Array` fast path in `pcmChannels`** (`frontend/src/audioPlayer.ts`).
  ~96k `DataView.getInt16` calls/s on the main thread become a typed-array view
  when the packet's byte offset is even (which frame layout makes the common
  case) and the platform is little-endian (checked, not assumed). The DataView
  loop remains as the floor, pinned against the fast path by test.
- **Splice-seam fades** (`audioPlayer.ts`). Both boundary corrections — the
  underrun re-cushion and the ceiling's trim — were hard splices, a click each.
  Every source now runs through its own gain node: a buffer that does not butt
  seamlessly onto its predecessor fades in over 4 ms, and sources stopped by the
  ceiling clamp fade out into the splice instead of cutting mid-waveform.

- **Swizzle loops on the engine read loop** (`src/rdp.rs` `pack_rgb`,
  `src/vnc_encodings.rs` `bgrx_to_rgb`). Both were one small `extend_from_slice`
  per pixel; both are now sized writes at a literal 4-in/3-out stride, which is
  what lets the compiler vectorize the shuffle. Hextile's raw tiles reuse one
  scratch buffer through `bgrx_to_rgb_into` instead of allocating per 16×16
  tile, and RDP's per-piece repack for the encoder is gone: pieces are row-wise
  `tiles::crop`s out of the pack the shadow compare already made.
- **Tile-path pacing** (`src/rdp.rs`, `DAMAGE_INTERVAL`). Damage now
  accumulates for 16 ms and coalesces before pack + shadow + encode —
  overlapping reports union into one rectangle (`stage_damage`, capped at a
  bounding-box collapse), so a region re-reported many times over an interval is
  packed once, against the newest framebuffer. (The case that motivated it was
  the pointer rectangle repeated per mouse event, which the browser now draws
  instead — see the cursor entry above.) Leading edge kept:
  a batch on a quiet screen still leaves on the spot. Cleared where the
  geometry dies (reactivation) or a full repaint subsumes it (`Refresh`). VNC
  is left unpaced on purpose. That read at the time as "RFB updates arrive only
  when the client asks, so the request cadence is already the pacing", which
  Continuous Updates has since made false — a server now pushes as it changes.
  The conclusion stands on the other leg it always had: the pacing that matters
  is at the browser hop, which the window below bounds, and the fence echo is
  what lets an RFB server pace itself.
- **Whole-desktop encode threading** (`src/vp9.rs`, `src/h264.rs`,
  `video::threads_for`). A stream covering the whole mirror has no sibling
  regions to overlap with, so it now gets up to half the cores (capped at 4),
  with `VP9E_SET_ROW_MT` and tile columns to make VP9's threads real work.
  Region streams keep one thread each; their parallelism is with each other.
- **Pipelined video encode** (`src/encode.rs` `frame()`, `src/regions.rs`,
  `src/video.rs`). The video lock is no longer held across the
  `spawn_blocking`: a round is pushed as a *handle* into the ordered queue —
  the tile path's own contract — and the mirror is double-buffered, so the
  engine keeps decoding and blitting (into the spare, synced rect-by-rect at
  the swap, bounded by damage rather than desktop size) while the worker
  encodes. Rounds stay serial (`round_out`); damage that lands mid-round is
  replayed as dirty marks at `put_back`; a round that outlives a resize is
  discarded by epoch; and `TileSink::round_returned` wakes an engine parked on
  a clean `due_at` when the returning round re-dirties the mirror.
  Regression-tested in `regions.rs` (mid-round damage survives into the next
  round's mirror; a stale round is dropped whole).
- **Shadow hot loop** (`src/tiles.rs`). An `unknown` counter skips the per-row
  scan of the `known` flags outright in the steady state (everything seen);
  `differing_bytes` compares eight bytes at a time from both ends instead of
  byte-at-a-time scans (the reverse one defeated autovectorization); and the
  per-cell classification is skipped entirely when nothing reads it —
  `Shadow::classify_cells`, driven by `TileSink::wants_cells`, since only a
  motion strategy consults `Changed::cells`.
- **Whole-mirror copy per video frame** (`src/video.rs` `Mirror::whole`,
  both encoders). A stream whose rectangle is the coded picture feeds the
  mirror's own buffer to `I420::read_rgb`; the full-framebuffer crop remains
  only for sub-rectangles and for the debug outline, which must not paint on
  the source.
- **Encoder output capacity** (`src/protocol.rs`). PNG and JPEG tiles encode
  into a buffer sized to a conservative compression ratio up front instead of
  growing a `Vec::new()` through repeated reallocation.
- **Concurrent region encodes within a round** (`src/regions.rs`,
  `Round::encode`). The serial loop was defended by a CPU-bound argument
  ("total work is one desktop frame") that answered the wrong question: the
  pipelined queue pays a round's *wall-clock*, and no new round can be taken
  while one is out, so a round of several streams cost the sum unconditionally,
  where with cores free to run them it can cost the max — under CPU contention
  the overlap shrinks back toward the sum, but never past what the serial loop
  always cost. Dirty streams now encode on scoped threads (the first on the
  worker itself — most rounds have one), sharing `&Mirror` and each holding its
  own stream `&mut`, checked by `std::thread::scope` with nothing `unsafe`.
  Units keep stream order regardless of finish order, and an error surfaces
  only after every stream's attempt. This also makes the one-thread-per-region
  rationale in `video::threads_for` true rather than circular — region streams
  really do parallelize with each other now.
- **The parse→decode→paint path off the main thread**
  (`frontend/src/desktopPainterWorker.ts`, `desktopPainter.ts`,
  `desktopPainter.worker.ts`; the audit's one architectural item). The
  painter — slot table, decoded-bitmap cache, `VideoDecoder` table, batch draw
  loop — is unchanged but runs in a dedicated worker drawing on an
  `OffscreenCanvas`, so a decode backlog costs that worker's thread rather
  than React's and input's. Each binary frame is *transferred* (zero-copy),
  and the main thread's per-connection promise queue is gone: postMessage
  order is the wire order, one chain in the worker preserves it across async
  decodes, and `clear` holds its place in that chain — jumping it would let a
  frame posted earlier paint the previous desktop onto the next attachment's
  canvas (pinned in `desktopPainterWorker.test.ts`). A resize's *state* half
  (CSS box, `size`, the status overlay's "is there a desktop yet") waits for
  the worker's `resized` echo, keeping the old behind-the-backlog semantics —
  applying it on arrival could flash the previous desktop after a target
  switch. One worker per canvas element, held at module level, because
  `transferControlToOffscreen` works once per element and StrictMode reruns
  the effect. Costs accepted: the element context's `desynchronized` present
  path does not exist on the commit path a transferred canvas uses, and
  `cacheReset` gains a postMessage hop.

## Gateway — screen

The paint acknowledgment and the window it now enforces are both in Done. One
smaller local item remains assessed and left: ~144 tile records can buffer across
three queues in series while supersede sees only the final batch. The UAT
measurements say those depths are not what binds — under the window the same
motion carried its picture in half as many records, and the queue the client
actually waited on was the one past the socket — so this stays unchanged rather
than becoming another depth adjusted in isolation.

## Gateway — audio

All addressed — see Done.

## Client

The parse/decode/paint hot path, the completion feedback and the window that acts
on it are all in Done, and the client needed no change for the last of them: the
worker already answers only after its ordered decode and draw, which is what the
gateway paces against. The worker is no longer treated as the endpoint merely
because the main thread handed it an `ArrayBuffer` — under motion on generic VNC
its queue went from 461 batches deep to 24.

## Audited and sound — do not regress

Binary batched wire format with ~1–2% overhead and no base64 on the pixel path;
`try_recv` run collection growing batches naturally under load; bounded,
awaited sends end to end with stall measurement; VP9/H.264 rate control (zero
lag, no periodic keyframes, pinned quantizer, screen-content tuning, quality
retune without encoder rebuild); tile-encode parallelism with ordered handle
collection; drop-oldest at the audio bridge with loss accounting; the 882→960
exact resample grouping matching the Opus granule; client zero-copy parsing,
`optimizeForLatency`, prompt frame/bitmap closing on every path; and the
refs-not-state discipline keeping React out of the frame path.
