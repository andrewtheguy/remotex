# Client ↔ gateway performance worklist

Findings from the 2026-08-04 audit of the screen and audio hot paths, ranked by
expected payoff. Each item names the code it is about; strike through or move to
Done as they land. The audit's method: three passes (gateway screen, gateway
audio, browser client) over the data path from engine read loop to canvas and
speaker, checked against the reference implementations under
`tmp/programs_for_reference` (rustdesk's libvpx tuning, Guacamole's audio
transport, noVNC's paint loop).

What the audit found *sound* — and later work should not regress — is at the
bottom.

## Done

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
  bounding-box collapse), so the pointer rectangle repeated per mouse event is
  packed once per interval, against the newest framebuffer. Leading edge kept:
  a batch on a quiet screen still leaves on the spot. Cleared where the
  geometry dies (reactivation) or a full repaint subsumes it (`Refresh`). VNC
  is left unpaced on purpose — RFB updates arrive only when the client asks,
  so the client's request cadence is already the pacing.
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

All addressed — see Done — except one smaller item, assessed and left:
~144 tile records can buffer across three queues in series while supersede sees
only the final batch (speculative, no measurement saying the depths bind).

## Gateway — audio

All addressed — see Done.

## Client

All addressed — see Done.

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
