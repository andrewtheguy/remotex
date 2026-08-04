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

## Gateway — screen

- **Per-pixel swizzle loops on the engine read loop.** `pack_rgb`
  (`src/rdp.rs`) does one 3-byte `extend_from_slice` per pixel and each rect is
  packed twice (shadow compare, then per band for the encoder; a third time
  into the mirror under video). `bgrx_to_rgb` (`src/vnc_encodings.rs`) is the
  same pattern plus a fresh full-rect allocation per rectangle. Rewrite as
  vectorizable fixed-stride loops and reuse buffers across calls.
- **No pacing on the tile path.** `VIDEO_FRAME_INTERVAL` exists because a busy
  RDP desktop reports damage ~126×/s against a 60 Hz screen; tiles still take
  every `GraphicsUpdate` straight through pack + shadow + encode. The shadow
  suppresses unchanged pixels, so the waste is pack/compare passes and streams
  of small encodes rather than full re-encodes — an accumulation interval
  (~16 ms) coalescing overlapping damage would cut both.
- **Whole-desktop `video` encodes on one thread.** `g_threads = 1`
  (`src/vp9.rs`) and `.num_threads(1)` (`src/h264.rs`) are justified by
  inter-region parallelism, which `Policy::Whole` does not have. Enable
  threads + `VP9E_SET_ROW_MT` (rustdesk also sets tile columns) when the plan
  is the whole desktop; keep 1 for region streams.
- **Video mutex held across the encode** (`src/encode.rs`, `frame()`): the
  engine read loop waits out the whole `spawn_blocking` encode, so decode and
  encode never overlap. A double-buffered mirror would pipeline them.
- **Shadow hot loop** (`src/tiles.rs`): `first_unknown` scans the `known` flags
  for every row even when the row's memcmp said identical (the comment above it
  claims otherwise); `differing_bytes` ends with a reverse scalar scan; the
  per-cell classification is computed then discarded on the default
  `Tiles { motion: None }` plan.
- **Whole-mirror copy per video frame** (`src/video.rs`, `crop_into` under
  `Policy::Whole`): a full-framebuffer memcpy whose only purpose is letting the
  debug outline draw on a copy; with `mark == None` the mirror could feed
  `I420::read_rgb` directly.
- Smaller: encoder output `Vec::new()` realloc growth (`src/protocol.rs`);
  ~144 tile records bufferable across three queues in series while supersede
  sees only the final batch; region streams encode serially within a round.

## Gateway — audio

All addressed — see Done.

## Client

- **The slot cache stores encoded bytes, so every `TILE_REF` re-decodes**
  (`frontend/src/tilePainter.ts`): Blob + `createImageBitmap` + GPU upload per
  reference. Cache decoded `ImageBitmap`s under a byte budget (decoded bands
  are far larger than the 32 KB encoded cap) with refcounting against
  `paintBatch`'s close.
- **Whole-batch `Promise.all` before any paint** (`tilePainter.ts`): the batch
  paints at its slowest decode, and up to 4096 decoded images can be alive at
  once. Await sequentially in wire order while decodes run concurrently to fix
  both, and to stop still tiles gating video frames on the `stream` dial.
- **Control messages queue behind pixel decode** (`useRemoteDesktop.ts`, the
  serial promise chain): only `resize` needs ordering against draws; cursor,
  clipboard, `videoFormat` do not. Nothing reads `VideoDecoder.decodeQueueSize`,
  so decoder lag is invisible.
- **Touch path layout thrash** (`useRemoteDesktop.ts`): `applyView` writes
  canvas styles then `syncCursor` reads the rect it just invalidated — a forced
  layout per pinch/pan frame on the weakest devices.
- **Mousemove coalescing never engages** (`frontend/src/outbound.ts`): gated on
  `bufferedAmount > 0`, which a healthy link never shows. rAF-align to one
  newest-position send per frame.
- **PCM passthrough sample conversion** (`frontend/src/audioPlayer.ts`,
  `pcmChannels`): ~96k `DataView.getInt16` calls/s on the main thread; an
  `Int16Array` view (when byte-aligned) is several times faster.
- **Audio schedule seams** (`audioPlayer.ts`, `audioSchedule.ts`): both
  boundary corrections — underrun re-cushion and ceiling trim — are hard
  splices; no crossfade or gradual drift correction.
- **Architectural**: no worker/`OffscreenCanvas`; the parse→decode→paint path
  shares the main thread with React and input. The painter is already factored
  behind `options.context()` if jank isolation is ever wanted.

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
