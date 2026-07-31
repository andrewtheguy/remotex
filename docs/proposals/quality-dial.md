# Proposal: a per-target render dial

Status: **fixed-quality JPEG implemented; the rest is the scaffolding it grew.**
This is the path-of-least-resistance version of lowering bandwidth; the dynamic,
motion-adaptive scheme is a separate, deferred proposal (see
[motion-adaptive-jpeg.md](motion-adaptive-jpeg.md)).

## Why

The gateway encoded every tile as PNG (`Tile::from_rgb`, `src/protocol.rs`).
PNG is lossless and correct for text and flat UI, but wasteful for photographic
or full-motion content, where a JPEG at moderate quality is a fraction of the
bytes and indistinguishable in motion. There was no way to trade that.

The wire already carries the choice: a tile record's first byte is its format,
`Tile::FORMAT_PNG` / `Tile::FORMAT_JPEG`, and both clients decode either (the
browser's `createImageBitmap` from a MIME type, the Swift viewer's ImageIO from
the container itself). So this is an encoder-side change with **zero wire change**
and no protocol-version bump.

## The two axes

Rather than a single `quality` scalar, the dial is two flat per-target fields —
a **type** (the quality strategy) and a **subtype** (the codec) — plus a
`render_quality` for the strategies that take one. Flat sibling keys, not a
nested table, matching the rest of the target schema (`resize` / `clipboard` /
`audio`):

```toml
[[targets]]
render_type    = "fixed-quality"   # full (default) | fixed-quality
render_subtype = "webp"            # png (default) | jpeg | webp
render_quality = 60                # 1–100, for fixed-quality
# default (all omitted) = full + png, byte-identical to the PNG-only gateway
```

Two axes because the strategy and the codec vary independently, and future
strategies (below) each compose with a codec rather than each being one more
flat mode. The pairing is validated at config-load time (`ConfigFile::parse_with`
in `src/config.rs`), the same way other target fields validate.

### The matrix

| `render_type` | `render_subtype` | status | behaviour |
|---|---|---|---|
| `full` | `png` | **shipped** (default) | lossless PNG, byte-identical to before the dial |
| `fixed-quality` | `jpeg` | **shipped** | every tile JPEG at `render_quality`, no classifier |
| `fixed-quality` | `webp` | **shipped** | every tile WebP at `render_quality` — ~30% fewer bytes than JPEG at a matched quality |
| `adaptive` | *(any)* | future | quality varies automatically — by motion or by link speed |
| `fixed-quality` / `adaptive` | `adaptive-jpeg` | future | per-tile classify: photographic → JPEG, flat UI/text → PNG |
| *(any)* | `video` | future | an inter-frame codec for full-motion regions |

Both lossy codecs decode natively in both clients — the browser via
`createImageBitmap`, the Swift viewer via ImageIO (WebP decode since macOS 11;
the app's minimum is 15). Neither runs a classifier, so flat UI and text soften
with everything else; that is the fixed dial's trade, and picking `webp` over
`jpeg` simply spends fewer bytes for the same visible result.

Only the shipped variants are `RenderType` / `RenderSubtype` enum variants; a
config naming a future one is refused by serde with the list of what is
accepted. The future rows are recorded here, not in code. The `adaptive-jpeg`
classifier is the content-based cousin of the churn-based
[motion-adaptive scheme](motion-adaptive-jpeg.md); `adaptive` is where a quality
that follows motion or connection speed would live.

## Settled: no classifier in the fixed dial

An earlier draft asked whether a numeric quality should run a PNG/JPEG classifier
first (flat UI stays lossless, photographic tiles go JPEG). **The fixed dial does
not**: `render_subtype = "jpeg"` sends *every* tile as JPEG at `render_quality`,
so flat UI and text soften along with everything else. That is the honest trade
of a single fixed knob, and it is the least code. A classifier is a strictly
better behaviour but belongs to a distinct subtype (`adaptive-jpeg`, future),
not smuggled into `jpeg` — so the name says what it does.

## Threading

The value flows from config to the encoder and nowhere else. The two axes and the
quality collapse to a single [`TileCodec`] at the config boundary
(`TargetConfig::tile_codec`), so the engines never see the config enums:

```
TargetConfig.render_type / render_subtype / render_quality
  → TargetConfig::tile_codec()  →  TileCodec   (Png | Jpeg(q) | Webp(q))
  → vnc::run / rdp::run
  → TileSink::new(engine, frame_tx, codec)
  → the per-tile encode call: Tile::from_rgb / from_rgb_jpeg / from_rgb_webp
```

`TileSink` is engine-agnostic and shared by both engines, so one change gave RDP
and VNC every codec together. When the codec is `Png`, `TileSink` calls
`Tile::from_rgb` unchanged and touches no lossy code.

## The encoders

`encode_jpeg` and the `jpeg-encoder` dependency were salvaged from the deleted
agent tree at commit `8990971` (`git show 8990971:crates/rxa-agent/src/encode.rs`);
`encode_webp` wraps the `webp` crate's `libwebp` (built by `cc` with the target's
SIMD, no cmake, and `thread_level = 1` so an encode can use all cores). Both sit
next to `encode_png` in `src/protocol.rs`, with `Tile::from_rgb_jpeg` /
`Tile::from_rgb_webp` as the lossy counterparts of `Tile::from_rgb`. The classifier
(`is_photographic`) and the churn ramp (`quality_for_churn`) in the salvaged JPEG
file were **left behind** — they belong to the future `adaptive-jpeg` subtype and
the motion-adaptive scheme, not to the fixed dial.

## Verification

- `cargo clippy -- -D warnings` and `cargo test`. Unit tests cover: default →
  PNG; `fixed-quality` + `jpeg`/`webp` → that codec and smaller than PNG on
  photographic input, with WebP smaller than JPEG at a matched quality; the
  mismatched axis pairings and an out-of-range or missing quality are rejected;
  `TileSink` with a codec emits that codec's tiles.
- Clients: the browser maps the format byte to `image/webp` for
  `createImageBitmap`; the Swift viewer adds `TileFormat.webp` and decodes a
  checked-in fixture through ImageIO (`swift test`). WebP decode is why the app's
  deployment target is macOS 15.
- A/B on the real RDP target and on the macOS Screen Sharing (VNC/ARD) target:
  full-motion content is materially smaller at a numeric quality, WebP below JPEG,
  and a target on the default is byte-identical to the PNG-only gateway.
- Playwright stays format-agnostic — assert record/frame relationships and header
  fields, never PNG magic bytes (a target may now emit JPEG).

## Out of scope for this proposal

- Any dynamic, per-cell, or motion-adaptive behaviour — that is
  [motion-adaptive-jpeg.md](motion-adaptive-jpeg.md), the future `adaptive` type /
  `adaptive-jpeg` subtype.
- Progressive JPEG or an H.264/`video` region path.
- Any wire or protocol-version change.
