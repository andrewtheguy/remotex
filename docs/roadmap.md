# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — the modes still planned

The gateway used to encode every tile as lossless PNG. It now has a two-axis
per-target render dial — a `render_type` (quality strategy) and a
`render_subtype` (codec), plus `render_quality` — whose lossy combinations
`fixed-quality` + `jpeg` and `fixed-quality` + `webp` (every tile encoded at a
fixed quality; WebP is ~30% smaller than JPEG) are **implemented**. The default,
`full` + `png`, is byte-identical to the PNG-only gateway. Zero wire change: the
tile format byte carries the codec and both clients decode all three. Detailed
proposal and the full type×subtype matrix:
[proposals/quality-dial.md](proposals/quality-dial.md).

What remains planned are further points on those two axes, each a new enum variant
the config already refuses by name until it is built:

- **`adaptive-jpeg` subtype** — a per-tile PNG/JPEG classifier (flat UI and text
  stay lossless, photographic tiles go JPEG), so a fixed quality no longer softens
  text. The content-based cousin of the motion scheme below.
- **`adaptive` type** — quality chosen automatically rather than fixed: from how
  fast a region is changing, or from the connection's speed.
- **`video` subtype** — an inter-frame codec for full-motion regions.

The dynamic, motion-adaptive form of `adaptive` — quality chosen per cell from how
fast it is changing, with a cleanup pass when it settles — existed in an earlier
prototype. It is deferred until the fixed dial proves insufficient; the design and
its salvage point are recorded in
[proposals/motion-adaptive-jpeg.md](proposals/motion-adaptive-jpeg.md).

### Apple Screen Sharing display modes

- **Make Standard mode's All Displays view point-correct on mixed-density Macs.**
  Standard mode's combined framebuffer is a mosaic of each physical screen's
  backing pixels, but the current wire `Resize` describes the whole canvas with one
  scale. No one scale is true for a 1× display beside a 2× display, so the combined
  view falls back to 1× and the Retina screen appears at twice its logical size.
  Apple's own client instead composes each screen in logical coordinates. The
  gateway needs a density-aware compositor that normalizes each screen's backing
  rectangle into one logical coordinate space, with the corresponding tile and
  pointer transforms. High Performance mode is unaffected because it uses one
  virtual display rather than a mosaic of physical displays.

## Not planned

### Multiple sessions

**Concurrent sessions, shared sessions, and a session broker are outside the
product model.** This is one user's program, and that is not a limitation waiting
to be lifted.

There is one active session slot: one active session per gateway instance,
permanently. A new browser takes over and evicts the previous holder
(`src/session.rs`), which a client offers with a Take over button — the same
shape as Windows Remote Desktop. A reconnect, a target switch and a browser
takeover all reclaim the slot in silence: they are the same session coming back,
whatever else has changed.
