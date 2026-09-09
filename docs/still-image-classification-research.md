# Still-image classification in remote desktop implementations

This note records a source review of content-aware still-image encoding in other
remote desktop implementations and the parts that may be useful to RemoteX. The
review is about classifying framebuffer rectangles for a still-image codec. It is
not about semantic object recognition or machine-learning image labels.

## Non-negotiable video boundary

RemoteX has exactly one video codec: VP9. This applies both to the full-desktop
`video` transport and to the regions introduced by `render_motion`. There is no
H.264, HEVC or AV1 video path, codec probe, alternative or fallback to add.

PNG, JPEG, WebP and AVIF in this note are independent still-image encodings for
base tiles. Considering one of them does not make it a video codec or change the
VP9-only motion path. Video facilities found in the reviewed projects are out of
scope regardless of their performance.

See [The render dial](architecture.md#the-render-dial) and
[The codec](architecture.md#the-codec) for the shipped boundary.

## Source snapshot

The repositories were shallow-cloned below `tmp/references/`, which is ignored by
Git. The revisions reviewed were:

| Project | Revision |
| --- | --- |
| FreeRDP | `168925dac792142f6d0b66e7e2d568a3d439521c` |
| Apache Guacamole server | `d3b7828977c63a5b197158d6cbbdaf1846b579fb` |
| RustDesk | `f28ac38ccfa662fd06639a062e0d06249860b142` |
| TigerVNC | `886800c78567931a2e7dfbb48861f84f11672b0b` |
| SPICE | `91d42c4d8de76ca00420fc112c17a82772bd1dd0` |
| Xpra | `d95058b0916913fe6ae5296fb702f66d833898b0` |
| KasmVNC | `98d25c652dde03b41d5379fbdfe2cb26fc2f9461` |
| TurboVNC | `87e636b988f974445c527f0e635b9488d5810566` |

The local clones are research inputs, not project dependencies. Follow their
licenses and reimplement ideas rather than copying source.

## The RemoteX baseline

`render_subtype = "classify"` already has the right high-level split: a tile that
looks photographic goes to JPEG, while flat UI, text, small input and uncertain
input stay in PNG. A false negative costs bytes; a false positive can leave text
soft until the rectangle changes, so PNG is deliberately the conservative answer.

The implementation in [`src/classify.rs`](../src/classify.rs) currently uses:

- a 1,024-pixel minimum for JPEG candidates;
- a palette gate that rejects tiles with at most 256 distinct colours;
- counts of nonzero soft and hard transitions; and
- a requirement for more than four soft transitions per hard transition.

Only horizontally adjacent pixels are examined. This creates a directional bias:
a vertically changing image can receive a different answer after transposition or
rotation even though its encoding character has not changed. That is a property of
the code, not a measured production failure: no recorded real-device measurement
has shown that the current direction changes a useful verdict.

The classifier tests cover synthetic content and the measured size floor. The
ignored device tests in
[`tests/classify_render_e2e.rs`](../tests/classify_render_e2e.rs) exercise the real
classifier path over RDP, TigerVNC, Apple High Performance and a classify base with
motion enabled. They report the PNG/JPEG split and verify complete, correctly
labelled and decodable wire output. Their screens are mutable, so they deliberately
do not assert a fixed split or measure visual regret against the codec not selected.

Production encode totals currently aggregate the still formats. They cannot answer
how many pixels and bytes each classifier verdict produced or how much encode time
each format consumed. Browser tiles are decoded through `createImageBitmap()` in
[`frontend/src/tilePainter.ts`](../frontend/src/tilePainter.ts), so browser decode
and paint cost is also part of a codec decision.

## Measurements already on record

Classification was measured shortly before this review; it was not chosen only
from the synthetic unit cases. Commit `16e938b9b297e5d4862d25fabe718b71c1d1a52b`
records the real wlshare measurement that cut `MIN_PHOTO_PIXELS` from 4,096 to
1,024, and the current constant's comment preserves the important observations.

- A controlled 60×24-point gradient chip repainting on its own was refused by the
  old 4,096-pixel floor 117 times out of 117 at 1×, producing 378 KB of PNG. At 2×
  it cleared the pixel floor 118 times out of 118 and produced 145 KB of JPEG. The
  density-dependent difference showed that the old pixel floor was not a sound
  proxy for small screen furniture.
- After lowering the floor, the same 45-second chip session at 1× encoded 1.50 MB
  instead of 2.26 MB and consumed 8.8 seconds of encoder CPU instead of 28.1
  seconds. In this measured case PNG was both larger and more expensive.
- Three real 2× sessions included scrolled photographs, terminal glyphs and chrome
  with streams and cleanup active. No classifier-admitted tile fell below a
  point-scaled version of the old floor because real damage rectangles were wide.
- The ignored `weigh_the_size_floor` instrument carries admitted synthetic content
  through PNG and JPEG at a size ladder. JPEG is about one third of PNG at 32×32,
  about half at 24×24, and loses at 16×16 because of roughly 640 bytes of JPEG
  tables. A real 9×10 caret was 261 bytes as PNG and 683 bytes as JPEG.
- The real-device end-to-end tests paint a complete desktop over Windows RDP,
  TigerVNC and Apple High Performance, plus a classify base under motion. They
  validate the operational classifier path and report its actual PNG/JPEG verdict
  counts without pretending that a mutable desktop has a deterministic content
  split.

Those results directly support the current size floor and show that the classifier
runs usefully on real desktops. They do not compare proposed two-dimensional or
run-based signals or JPEG chroma choices. They also do not preserve one fixed set
of real pixels against which every alternative can be replayed. WebP has separate
real-screen measurements recorded below. The measurement work proposed here
extends the existing evidence for new classifier questions; it does not restart or
discount the completed validation.

## Findings from other implementations

### TigerVNC: classify structure and measure the result

TigerVNC's
[`EncodeManager.cxx`](https://github.com/TigerVNC/tigervnc/blob/886800c78567931a2e7dfbb48861f84f11672b0b/common/rfb/EncodeManager.cxx)
classifies rectangles as solid, bitmap, indexed or full colour using palette and
run information. Its statistics are split by encoder class and type and include
rectangle count, pixels, bytes and compression ratio. It also tracks where lossy
output remains and schedules bounded lossless refresh once an area stops changing.

Useful for RemoteX:

- add per-verdict and per-codec accounting;
- add a cheap run/repetition signal after the palette gate; and
- retain the principle that lossy provenance must survive copies if a later
  correctness mechanism depends on knowing it.

A blanket lossless refresh is not suitable for classified JPEG base tiles: it
would erase the intended bandwidth saving for a static photograph. RemoteX already
repaints settled VP9 motion regions with their base encoding.

### SPICE: sampled two-dimensional graduality

SPICE's `spice-bitmap-utils.tmpl.c` samples pixel squares at a fixed stride and
compares right, down and diagonal neighbours. Equal pairs, high-contrast pairs and
smaller nonzero changes have different weights, producing a graduality score used
when choosing synthetic-image or photographic-image compression. Its user manual
describes the automatic LZ/GLZ-versus-Quic decision and the risk that video-area
false positives reduce text quality: [SPICE user manual](https://spice.pages.freedesktop.org/spice-space/spice-user-manual.html).

This is the strongest direct adaptation candidate. A bounded two-dimensional
sample can remove RemoteX's structural horizontal bias without requiring a full
second pass or a semantic classifier. It is not yet a demonstrated improvement:
rotation and transpose replay should first show a real wrong verdict or measurable
regret. If it does, RemoteX should tune its own thresholds rather than importing
SPICE's constants.

SPICE also uses temporal and geometry continuity before starting a video stream.
RemoteX already has a project-specific churn detector, region selection and base
cleanup, so that part supplies comparison rather than a replacement design.

### Apache Guacamole: a deliberately inexpensive predictor

Guacamole's `src/libguac/display-worker.c` estimates PNG suitability from the
ratio of equal and different horizontal neighbours. It restricts JPEG and WebP by
rectangle size and update rate and adapts lossy quality using observed lag. A
source comment notes that an ideal choice would include lossless requirements,
time until the next frame and estimated encode cost.

RemoteX already adapts quality from receiver feedback and deliberately separates
still content type from motion. It should therefore not copy Guacamole's frame-rate
gate for still JPEG: a static photograph can still benefit from JPEG. The useful
lesson is that a small predictor is preferable when its errors and costs are
measured.

### KasmVNC: budget expensive still encoders

KasmVNC includes JPEG, WebP and QOI still encoders. It benchmarks encoder work,
assigns WebP a fraction of the frame-time budget, and uses JPEG after that budget is
spent. Its end-to-end benchmark replays frames repeatedly and reports timing,
rectangle counts and bytes. The published baseline shows materially better WebP
compression accompanied by much slower encoding in that implementation:
[KasmVNC performance testing](https://github.com/kasmtech/KasmVNC/wiki/Performance-Testing).

This independently agrees with RemoteX's own reason for deprioritizing WebP. If
WebP is revisited later, KasmVNC's encoder-time budget is a useful improvement to
test: admit WebP only while that budget permits and retain JPEG/PNG when it does
not. It is not a reason to move WebP ahead of the current classifier work.
KasmVNC's video codecs, scaling modes and fit-to-window policies are not applicable.

### TurboVNC: chroma subsampling is part of text quality

TurboVNC's
[`performance.txt`](https://github.com/TurboVNC/turbovnc/blob/87e636b988f974445c527f0e635b9488d5810566/doc/performance.txt)
documents the trade between JPEG 4:4:4 and chroma-subsampled JPEG. Photographs tend
to tolerate subsampling, while sharp coloured boundaries can blur.

The `jpeg-encoder` version currently used by RemoteX defaults to 4:2:0 below quality
90 and 4:4:4 at quality 90 or above. Explicit 4:4:4 for borderline, colourful
tiles is therefore worth benchmarking before introducing a new still format. It
keeps the existing wire image type and browser decode path.

TurboVNC also tracks lossy rectangles and supports lossless refresh after
inactivity. As with TigerVNC, that mechanism should not be copied wholesale onto
classified base JPEG.

### Xpra: broad selection with content and congestion inputs

Xpra chooses among still encoders using dimensions, alpha, requested quality and
speed, congestion, bandwidth and window content hints. It has scheduled refresh
for lossy regions. Its WebP path uses fast encoder methods and content presets; its
AVIF experiments use screen-content tuning, palette tools and 4:4:4 for sharp
screen content. Its automatic policy nevertheless reaches faster WebP/JPEG choices
before AVIF. See [Xpra encoding documentation](https://github.com/Xpra-org/xpra/blob/d95058b0916913fe6ae5296fb702f66d833898b0/docs/Usage/Encodings.md).

RemoteX receives a composed framebuffer rather than trusted per-window metadata,
so Xpra's application content hints do not transfer. Its useful lessons are to
include encoder speed in selection and to treat AVIF as an experiment rather than
assuming its compression ratio makes it suitable for a damage hot path.

### FreeRDP and RustDesk: little classifier logic to reuse

The reviewed FreeRDP source is primarily protocol capability and decoding code,
not the proprietary RDP host's content classifier. RustDesk's relevant logic is
mainly decoder-queue, delay, frame-rate and quality control. RemoteX already has
receiver paint acknowledgements and adaptive quality, so neither supplied a clear
still-image classification technique to copy.

## Still-codec candidates

### PNG

The current path uses the fastest compression mode and an `Up` filter because tile
encoding is hot-path work. A corpus should compare `Sub`, `Up`, a cheap sampled
filter choice and a full adaptive choice. A smaller PNG is not an improvement if
the filter search costs more CPU or delays later rectangles.

### JPEG

JPEG remains the low-risk photographic codec because it is fast, is already on the
wire and decodes natively in the browser. Benchmark these changes independently:

- explicit 4:4:4 for uncertain or sharp colourful candidates;
- the current 4:2:0 behavior for clear photographs;
- optimized Huffman tables, including the extra encode cost; and
- a native libjpeg-turbo path only if its measured gain justifies another native
  dependency and the packaging obligations that follow.

### WebP: measured and deprioritized, for later revisit

RemoteX has already implemented and measured WebP rather than relying on general
web-image claims. Commit `2e62a0bbc2c243d5b7ffdf6c6b69c631f93c11f3`
sampled engine-shaped tiles from real 1,600×1,000 screen pixels on an arm64 NEON
Mac and compared lossless WebP with PNG `Compression::Fast`:

| WebP setting | Bytes relative to PNG | 16×16 encode time | 320×64 encode time |
| --- | ---: | ---: | ---: |
| method 0, quality 20 | 0.85× | 29.9× | 6.8× |
| method 2, quality 50 | 0.66× | 43.7× | 46.0× |
| method 4, quality 75 | 0.66× | 45.6× | 156.6× |

The affordable setting saved only 15% of the bytes and was still much slower than
PNG. The larger byte gains required encode costs unsuitable for the tile hot path.
The same work found lossy WebP quality 80 at 0.50–0.70× the bytes of
`jpeg-encoder` quality 80 on ordinary tiles and 0.92× on uniform noise. When the
WebP tile path subsequently shipped, two real screenshots put lossless output at
0.64–0.81× PNG for a working window and 0.83–0.88× for photographic wallpaper;
lossy WebP took 2.5× JPEG's encode time on the deployment host. Commits
`18cc5c58e156d80b09c41f7e9406986de61907db` and
`e4ad86e57ddf9959dce2777b072ae6cd914b520d` preserve that history.
It was later revisited as an optional fixed-quality subtype in
`f38e6992aa301d3b9cf2f8d1b0127acc6b9bd868`, then removed again in
`05aa6d94ed17f6deabdd0ec15d745e79e97399c6` while JPEG remained the sole lossy
still codec.

WebP was therefore deprioritized because its encoder was slower than both PNG on
the lossless side and JPEG on the lossy side, despite saving bytes. It is not
permanently rejected. Revisit it later if the encoder implementation, target
hardware or measured workload changes enough to alter that trade. A revisit should
start by reproducing the real-screen benchmark, then test method 0 or 1 with a
strict per-frame or rolling time budget and immediate PNG/JPEG fallback.

Google's general corpus reports smaller WebP output than comparable PNG and JPEG,
but those figures do not override RemoteX's hot-path measurements:
[WebP compression study](https://developers.google.com/speed/webp). Lossy WebP is
also 4:2:0, which matters around coloured glyphs and sharp edges:
[WebP FAQ](https://developers.google.com/speed/webp/faq).

Reintroducing it later would require a tile format discriminator, frontend MIME
mapping and an independent wire-format test. It would remain a still-image codec
and would not change the VP9 video path.

### AVIF and QOI

AVIF should remain behind the deferred WebP revisit unless materially different
evidence gives it a credible hot-path advantage. QOI is unattractive for this
design because the browser has no native `createImageBitmap()` QOI decoder and its
larger WAN payload trades away the central benefit sought here.

## Extending the existing measurement

The controlled wlshare sessions and real-device end-to-end runs answer the current
size-floor and operational questions. Proposed classifier signals need a replayable
comparison because they must see exactly the same input pixels. An opt-in capture
can record representative damage tiles or frames under `tmp/` and replay them
through every candidate. Desktop captures may contain passwords, personal messages
or other private material, so capture must never be enabled by default or
committed. The same corpus can be reused when the deferred WebP work is revisited.

The corpus should contain:

- flat controls, terminals and monochrome text;
- antialiased and subpixel-coloured text;
- sharp colourful icons and diagrams;
- smooth horizontal and vertical gradients;
- photographs, video frames and noisy imagery;
- mixed UI/photo boundary tiles;
- small damage rectangles at 1x and 2x density; and
- rotations and transposes of the same samples.

For each candidate and content class, record:

- encoded bytes and compression ratio;
- encoder CPU microseconds;
- browser worker decode-to-paint time;
- SSIM or DSSIM for photographic error;
- an edge-weighted error metric for glyphs and sharp UI; and
- selection regret: cost or unacceptable quality relative to the best candidate
  that the classifier could have selected.

False-positive text damage should be weighted more heavily than sending a photo
losslessly. The former remains visibly degraded until the area changes; the latter
only spends bandwidth.

## Recommended implementation order

1. Preserve the recent wlshare and real-device results as the baseline. Add a
   privacy-safe replay corpus and per-codec/per-verdict measurements for comparisons
   those sessions could not make, without changing the classifier decision.
2. Return or internally record a reason and score instead of only a Boolean:
   minimum size, palette, graduality, repetition and final confidence.
3. Measure horizontal-only evidence against bounded right/down/diagonal sampling
   on rotations and transposes. Replace it only if the comparison demonstrates a
   useful reduction in regret.
4. Add a repetition/run signal as a conservative JPEG rejector for colourful UI.
5. Benchmark explicit JPEG 4:4:4 for borderline sharp content.
6. Leave WebP deprioritized. Revisit it later only after the preceding work, or
   when a materially different encoder or deployment CPU justifies rerunning the
   existing real-screen benchmark; test an encoder-time budget at that point.
7. Keep AVIF behind that revisit unless new measurements justify moving it ahead.

The likely policy shape is intentionally conservative:

| Evidence | Still-tile choice |
| --- | --- |
| Clear UI or text | PNG |
| Ambiguous | PNG |
| Clear photograph | JPEG |
| Motion region | VP9 video stream, followed by the existing base-tile cleanup |

## Explicit exclusions

- No H.264, HEVC or AV1 video path.
- No runtime probing or switching among video codecs.
- No fit-to-window, downscaled video mode or new density behavior.
- No neural classifier: semantic labels do not directly predict rate, distortion
  or encoder time and would add substantial CPU and dependency cost.
- No application/window metadata assumption for a composed framebuffer.
- No blanket lossless refresh that cancels the intended static-photo savings.
- No codec choice justified solely by generic web-image compression claims.
