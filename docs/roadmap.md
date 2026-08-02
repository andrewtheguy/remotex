# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — motion, then video

The gateway used to encode every tile as lossless PNG. It now has a two-axis
per-target render dial — a `render_type` (quality strategy) and a
`render_subtype` (codec), plus `render_quality` — whose lossy combinations
`fixed-quality` + `jpeg` and `fixed-quality` + `webp` (every tile encoded at a
fixed quality; WebP is ~30% smaller than JPEG) are **implemented**. The default,
`full` + `png`, is byte-identical to the PNG-only gateway. Zero wire change: the
tile format byte carries the codec and both clients decode all three. That much is
built, and is described in [architecture.md](architecture.md#the-render-dial).

Everything still planned on the dial is **motion-based**. The earlier plan of a
content classifier (`adaptive-jpeg` subtype) and a quality that follows the link
(`adaptive` type) is **scrapped**: neither is on the way to video, and content is
the wrong question when what costs bandwidth is what moves.

`render_type = "motion"` is not a third way to encode every tile. It **builds on
the base encode a target already has** and changes nothing about it. The base is
whatever the target is configured for today — lossless PNG, or JPEG/WebP at
`render_quality = 60` — and it remains what a settled cell is sent as. What
`motion` adds is a second, much cheaper encode used *only* for cells currently
changing fast:

```toml
[[targets]]
render_type           = "motion"
render_subtype        = "webp"   # base codec, unchanged meaning: what a settled cell gets
render_quality        = 60       # base quality (omit with png — the base is then lossless)
render_motion_subtype = "jpeg"   # moving cells: need not be the base codec
render_motion_quality = 10       # moving cells: as cheap as it takes
```

The moving encode gets its **own codec axis**, not just its own quality, for two
reasons. The base is sent once when a cell settles and can afford WebP's slower,
smaller encode, while a moving cell is re-encoded every frame, where JPEG's faster
encode may beat WebP's smaller output — cheapest and smallest are not the same
question at 60 as at 10. And the moving codec is the one that becomes H.264 in step
2 while the base stays a still codec, so it has to be nameable on its own.
`render_motion_subtype` defaults to `render_subtype` when omitted; it must be a
lossy codec, and it is *required* when the base is `png`, which has no quality dial
to turn down.

The base strategy is read from `render_subtype` and `render_quality` rather than
from `render_type`, which `motion` now occupies: `png` with no quality is a
lossless base, a lossy subtype with a quality is a fixed-quality base. Both are
worth trying. A lossless base is the more interesting one — text and flat UI stay
perfect and *only* what moves gets ugly — and it is also the configuration the
current dial cannot express at all.

A cell that stops changing is re-sent once at the base encode, so a paused screen
returns to full quality on its own. That is the whole shape: **the base is the
truth, motion is a temporary discount on cells too busy to notice.**

Two steps, and the split is **detection first, encoder second** — the moving-cell
encoder is meant to be replaceable, and replacing it is step 2. If H.264 turns out
to be easy to stand up, step 1 can start with very-low-quality H.264 directly and
the cheap-still encoder is skipped entirely; it exists only so the detection can be
proven without waiting on a codec.

1. **Get the detection right, with a cheap still as the placeholder encoder.**
   Moving cells go out as a lossy still at something like quality 10 — no new
   codec, no new wire format, both clients already decode it — so the whole step is
   the detection itself, and every tile stays an independent still image, which is
   what makes it cheap to debug. The pieces:

   - **Cell identity.** The gateway `Shadow` is pixel-exact and has no stable cell
     identity, so the churn key is a fixed **320×64 grid** (`CELL_W` / `CELL_H`,
     already declared in `src/protocol.rs`). Snap each changed rect *outward* to
     cell boundaries; each cell then has a stable `(col, row)`.
   - **Churn → encode.** Per cell, count changed frames in a short window; past a
     threshold the cell is "moving" and takes the motion codec and quality instead
     of the base encode. Whether that is a hard switch or a ramp between the two is
     for measurement to answer — a hard switch is the thing to build first.
   - **Cleanup.** Stash the source RGB of any cell sent at the motion encode,
     bounded by a byte cap. A timer in `order_loop` (a `tokio::select!` with a
     `MissedTickBehavior::Delay` interval) re-sends cells idle past a threshold at
     the **base** encode, capped per tick, so a settled screen returns to full
     quality without a client repaint.
   - **Placement.** Churn and stash state in `src/encode.rs`, owned by the
     `order_loop` task; encoders stay in `src/protocol.rs`. `TileSink` is shared,
     so RDP and VNC both get this from one implementation. Motion state must be
     cleared on resize and on reattach/forget in both engines.

   An earlier prototype of the same scheme is pinned at commit `8990971`; find its
   classifier, cell splitting, churn tracking and cleanup with
   `git grep -n 'quality_for_churn\|split_cells\|CLEANUP_IDLE' 8990971`. Its
   constants are a starting point, not a conclusion: `CHURN_WINDOW=8`,
   `CHURN_FULL_SPEED=4`, `CLEANUP_IDLE=500ms`, `CLEANUP_TICK=250ms`,
   `MAX_CLEANUPS_PER_TICK=8`, `MAX_STASH_BYTES=8MB`.

   Measure it against the fixed dial on the real RDP and macOS VNC/ARD targets:
   moving bytes down at the same surface size, static content untouched (a target
   with no motion must be byte-identical to its base configuration today), cleanups
   observed after pausing, and — the payoff the grid buys — a *windowed* video
   degrading only its own cells rather than the whole screen.
2. **Swap the moving-cell encoder for H.264** — `render_motion_subtype = "h264"`,
   a value legal only on that axis. The detection, the base encode and the cleanup
   path are all unchanged; what changes is that a moving cell feeds an
   inter-frame stream instead of one independent still per frame, which is where
   the real win is. That is a wire change — a new record or format byte, keyframe
   and stream lifetime rules, and a decode path in each client (VideoToolbox,
   WebCodecs) — and the open question is what the stream is *of*, since a 320×64
   cell is a poor unit for a video encoder: the likely answer is a coalesced moving
   region rather than a stream per cell. Deliberately not designed yet; step 1's
   measurements decide its shape. If it proves easy, it becomes step 1 instead.

Neither `motion` nor `h264` is an enum variant yet; the config refuses both by
name until each is built.

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
