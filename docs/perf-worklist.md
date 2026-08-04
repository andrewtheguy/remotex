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
  (`frontend/src/useRemoteDesktop.ts`, `rectOf`). `toRemote` called
  `getBoundingClientRect()` per mousemove — a forced layout flush at pointer
  rate. The rect is now read at most once per displayed frame; anything that
  moves the canvas shows no sooner than the frame that clears the cache.

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

- **~610 KB of copy/memmove per 32 KB wave buffer** on the Opus path:
  the broadcast channel deep-clones per receiver (`src/audio.rs` — carry
  `Bytes`/`Arc<[u8]>` instead of `Vec<u8>`); byte-at-a-time deinterleave with a
  `%` per sample (`src/pcm48.rs`); front-`drain` on plain `Vec`s in the group
  loop (`VecDeque` or a cursor). PCM passthrough — the mode whose point is
  touching nothing — copies each buffer three times.
- **`AUDIO_SOCKET_BUFFER = 16` is deeper than its own justification**
  (`src/session.rs`): "absorb a socket write in flight" needs 2–3 slots. The
  client's 1.5 s ceiling bounds what the user hears, but a congested link first
  receives ~3 s of stale audio — bandwidth spent against the video that is
  causing the congestion, then an audible client-side splice — before losses
  move to the drop-oldest bridge where they belong.
- Minor: the resampler's 160-sample delay is not added to `OpusHead.pre_skip`
  (`src/opus_stream.rs`); `packet[..len].to_vec()` per packet defeats the
  scratch buffer beside it.

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
