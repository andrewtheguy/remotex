# Proposal: a fixed per-target quality dial

Status: **proposed, not started.** This is the intended next release after the
rxa removal (0.0.72). It is the path-of-least-resistance version of lowering
bandwidth; the dynamic, motion-adaptive scheme is a separate, deferred proposal
(see [motion-adaptive-jpeg.md](motion-adaptive-jpeg.md)).

## Why

The gateway encodes every tile as PNG (`Tile::from_rgb`, `src/protocol.rs`).
PNG is lossless and correct for text and flat UI, but wasteful for photographic
or full-motion content, where a JPEG at moderate quality is a fraction of the
bytes and indistinguishable in motion. There is no way to trade that today.

The wire already carries the choice: a tile record's first byte is its format,
`Tile::FORMAT_PNG` / `Tile::FORMAT_JPEG`, and the browser and viewer already
decode JPEG tiles. So this is an encoder-side change with **zero wire change**
and no protocol-version bump.

## The dial

One new per-target field in `TargetConfig` (`src/config.rs`), alongside
`resize` / `clipboard` / `audio`:

```toml
[[targets]]
name = "..."
quality = "full"   # the default; or an integer 1–100
```

- `quality = "full"` (or unset) → **today's PNG-only path, byte-identical to the
  0.0.72 release.** No JPEG, no classifier — the existing `Tile::from_rgb`. This
  is the default, so an existing config behaves exactly as it does now.
- `quality = <1–100>` → encode tiles as **JPEG at that fixed quality**. One dial,
  set once in config: no churn tracking, no cleanup pass, no per-frame decision.

The field parses to an enum (`Full` | `Jpeg(u8)`), rejecting `0` and anything
over `100` at config-load time with a typed error, the same way other target
fields validate.

## Threading

The value flows from config to the encoder and nowhere else:

```
TargetConfig.quality
  → vnc::run / rdp::run          (read the resolved target)
  → TileSink::new(engine, frame_tx, quality)
  → the per-tile encode call
```

`TileSink` is engine-agnostic and shared by both engines, so one change gives
RDP and VNC the feature together. When `quality` is `Full`, `TileSink` calls
`Tile::from_rgb` unchanged and touches no new code; only a numeric quality
reaches a JPEG encode path.

## The JPEG encoder

`encode_jpeg` and the `jpeg-encoder` dependency are retrievable from the deleted
agent tree at commit `8990971`:

```sh
git show 8990971:crates/rxa-agent/src/encode.rs
```

Port `encode_jpeg(w, h, rgb, quality)` next to `encode_png` in
`src/protocol.rs`, rewiring its format byte to `Tile::FORMAT_JPEG`. Add
`jpeg-encoder = { version = "0.7", features = ["simd"] }` to the root
`Cargo.toml`. Add a **separate** entry point (e.g. `Tile::from_rgb_jpeg`) rather
than changing `from_rgb`, so the PNG path and its
`from_rgb_still_marks_its_payload_as_png` test stay exactly as they are.

## One question to settle when building

Whether a numeric `quality` should encode **every** tile as JPEG, or run the
PNG/JPEG classifier first (`is_photographic` in the same `8990971` file — flat
UI and text stay lossless PNG, photographic tiles go JPEG at the set quality).
The classifier is the safer default: it never blurs text. Decide when building,
with a real target in front of you; both pieces are in the same salvaged file.

## Verification

- `cargo clippy -- -D warnings` and `cargo test`; then in `frontend/`, biome +
  `tsc -b` + `bun test` + **`bun run build`**.
- Unit: `quality = "full"` produces a PNG tile (unchanged); a numeric quality
  produces a smaller JPEG tile on a photographic input.
- A/B on the real RDP target and on the macOS Screen Sharing (VNC/ARD) target:
  full-motion content is materially smaller at a numeric quality, static content
  looks unchanged, and a `"full"` target is byte-identical to 0.0.72.
- Playwright stays format-agnostic — assert record/frame relationships and
  header fields, never PNG magic bytes (a target may now emit JPEG).

## Out of scope for this proposal

- Any dynamic, per-cell, or motion-adaptive behaviour — that is
  [motion-adaptive-jpeg.md](motion-adaptive-jpeg.md).
- Progressive JPEG or an H.264 region path.
- Any wire or protocol-version change.
