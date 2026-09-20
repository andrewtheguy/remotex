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
base tiles. RemoteX ships two of them — PNG and WebP; JPEG appears here as prior
art in the reviewed projects and as the encoder several of the measurements below
were taken against. Considering any of them does not make one a video codec or
change the VP9-only motion path. Video facilities found in the reviewed projects
are out of scope regardless of their performance.

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
looks photographic goes to the lossy still, while flat UI, text, small input and
uncertain input stay in PNG. A false negative costs bytes; a false positive can
leave text soft until the rectangle changes, so PNG is deliberately the
conservative answer. The lossy still is WebP and only WebP, so the classifier has
no encoder question to answer: it answers what the pixels are and nothing else.

The implementation in [`src/classify.rs`](../src/classify.rs) currently uses:

- a 1,024-pixel minimum for lossy candidates;
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
classifier path against whichever QA machine `REMOTEX_UAT_TARGET` names — the
suite names no device itself, so it does not go stale as a lab changes — as a
plain classify base and as a classify base under motion. They report the lossless/lossy
split and verify complete, correctly labelled and decodable wire output. Their screens are mutable, so they deliberately
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
  it cleared the pixel floor 118 times out of 118 and produced 145 KB of JPEG, the
  lossy still of the time. The
  density-dependent difference showed that the old pixel floor was not a sound
  proxy for small screen furniture.
- After lowering the floor, the same 45-second chip session at 1× encoded 1.50 MB
  instead of 2.26 MB and consumed 8.8 seconds of encoder CPU instead of 28.1
  seconds. In this measured case PNG was both larger and more expensive.
- Three real 2× sessions included scrolled photographs, terminal glyphs and chrome
  with streams and cleanup active. No classifier-admitted tile fell below a
  point-scaled version of the old floor because real damage rectangles were wide.
- The ignored `weigh_the_size_floor` instrument carries admitted synthetic content
  through the still encoders at a size ladder. When it still carried JPEG, JPEG was
  about one third of PNG at 32×32, about half at 24×24, and lost at 16×16 because
  of roughly 640 bytes of JPEG tables; a real 9×10 caret was 261 bytes as PNG and
  683 bytes as JPEG. WebP at the same quality dial is around a tenth of the PNG at
  every size on that ladder, 16×16 included — it has no table cost to amortize. So
  `MIN_PHOTO_PIXELS` was set where JPEG turned over and is well clear of where WebP
  does; with JPEG gone the floor is conservative rather than load-bearing, and
  lowering it is an open question the corpus below should settle. That is a byte
  comparison at a matched dial number, not a matched visual quality, and it says
  nothing about the encode time measured below.
- The real-device end-to-end tests paint a complete desktop over Windows RDP,
  TigerVNC and Apple High Performance, plus a classify base under motion. They
  validate the operational classifier path and report its actual PNG/lossy verdict
  counts without pretending that a mutable desktop has a deterministic content
  split.

Those results directly support the current size floor and show that the classifier
runs usefully on real desktops. They do not compare proposed two-dimensional or
run-based signals. They also do not preserve one fixed set
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

A blanket lossless refresh is not suitable for classified lossy base tiles: it
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
gate for still tiles: a static photograph can still benefit from a lossy encode.
The useful lesson is that a small predictor is preferable when its errors and costs
are measured.

### KasmVNC: budget expensive still encoders

KasmVNC includes JPEG, WebP and QOI still encoders. It benchmarks encoder work,
assigns WebP a fraction of the frame-time budget, and uses JPEG after that budget is
spent. Its end-to-end benchmark replays frames repeatedly and reports timing,
rectangle counts and bytes. The published baseline shows materially better WebP
compression accompanied by much slower encoding in that implementation:
[KasmVNC performance testing](https://github.com/kasmtech/KasmVNC/wiki/Performance-Testing).

RemoteX reached the opposite arrangement — WebP is the one lossy still and PNG the
one lossless — so the transferable part is not KasmVNC's codec ladder but its
accounting: it is worth knowing what a frame's WebP encodes cost before proposing
any budget that would send a tile lossless instead. KasmVNC's video codecs, scaling
modes and fit-to-window policies are not applicable.

### TurboVNC: chroma subsampling is part of text quality

TurboVNC's
[`performance.txt`](https://github.com/TurboVNC/turbovnc/blob/87e636b988f974445c527f0e635b9488d5810566/doc/performance.txt)
documents the trade between JPEG 4:4:4 and chroma-subsampled JPEG. Photographs tend
to tolerate subsampling, while sharp coloured boundaries can blur.

This no longer has a dial in RemoteX to turn. Lossy WebP is 4:2:0 and has no other
mode, so there is nothing to pin and nothing for the adaptive walk to change by
accident — the walk moves quantization and only quantization. The observation
survives as a caution about `render_subtype = "webp"`, which hands text and
coloured UI to a 4:2:0 encoder that cannot route them to PNG; `classify` is the
answer for a screen where that matters. It is emphatically not a reason to extend
or reuse `render_chroma`, which belongs only to VP9 streams.

TurboVNC also tracks lossy rectangles and supports lossless refresh after
inactivity. As with TigerVNC, that mechanism should not be copied wholesale onto
classified base tiles.

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

### WebP: the one lossy still

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
still codec, then restored beside JPEG as an operator's choice — and JPEG was
finally removed, leaving WebP as the one lossy still.

Those measurements say the same thing twice: WebP saves bytes and spends encoder
time to do it. Offering both encoders made that trade the operator's to weigh per
target, and the weighing never happened. The choice cost a config key, a second
enum threaded through the whole encode path and a second answer to every question
about a tile — and what it bought back was JPEG, faster to encode and larger on the
wire, on a gateway whose whole reason for compressing at all is the wire. So JPEG
is gone. `render_subtype = "webp"` sends every tile through WebP and `classify`
sends only the tiles the classifier reads as photographic; nothing decides at run
time, because there is nothing left to decide between.

Lossless is untouched by this. PNG is the only lossless tile encoder, on the
evidence above — lossless WebP saved 15% of the bytes at an affordable setting and
was still many times slower than PNG `Compression::Fast` — so `render_subtype` has
no lossless choice to make and the classifier's lossless verdict is always PNG.

What the measurements still leave open is an encoder-time budget of the kind
KasmVNC uses: admit a lossy encode only while a per-frame or rolling budget permits
and send PNG when it does not. That would be a run-time policy on top of the
classifier's verdict, and it should not be built until the corpus below can show
what it would buy.

Google's general corpus reports smaller WebP output than comparable PNG and JPEG,
but those figures do not override RemoteX's hot-path measurements:
[WebP compression study](https://developers.google.com/speed/webp). Lossy WebP is
also 4:2:0, which matters around coloured glyphs and sharp edges:
[WebP FAQ](https://developers.google.com/speed/webp/faq).

It remains a still-image codec: the format byte tells a WebP tile from a PNG one,
the browser maps it to `image/webp` for `createImageBitmap`, and the VP9 video path
is unaffected in either direction.

### AVIF and QOI

AVIF should not be added unless materially different evidence gives it a credible
hot-path advantage over WebP. QOI is unattractive for this
design because the browser has no native `createImageBitmap()` QOI decoder and its
larger WAN payload trades away the central benefit sought here.

## Extending the existing measurement

The controlled wlshare sessions and real-device end-to-end runs answer the current
size-floor and operational questions. Proposed classifier signals need a replayable
comparison because they must see exactly the same input pixels. An opt-in capture
can record representative damage tiles or frames under `tmp/` and replay them
through every candidate. Desktop captures may contain passwords, personal messages
or other private material, so capture must never be enabled by default or
committed. The same corpus can settle whether `MIN_PHOTO_PIXELS` still needs to sit
where JPEG's tables put it.

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
4. Add a repetition/run signal as a conservative lossy rejector for colourful UI.
5. Re-weigh `MIN_PHOTO_PIXELS` against WebP alone. It was measured where JPEG's
   tables broke even, and WebP has none; the floor may be costing small
   photographic tiles for nothing.
6. WebP is the one lossy still. Do not add a second one, and do not add a run-time
   policy that falls back to PNG on encoder time until the corpus above can weigh
   that budget against the bytes it would give back.
7. Keep AVIF out unless new evidence justifies moving it ahead of WebP.

The likely policy shape is intentionally conservative:

| Evidence | Still-tile choice |
| --- | --- |
| Clear UI or text | PNG |
| Ambiguous | PNG |
| Clear photograph | WebP at `render_subtype_quality` |
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
- No second lossy still codec beside WebP, and no run-time switching to one.
- No lossless codec beside PNG.
