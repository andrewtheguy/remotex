# Proposal: motion-adaptive JPEG quality (deferred)

Status: **on paper only, deferred.** The fixed
[render dial](quality-dial.md) has shipped (`fixed-quality` + `jpeg`); do not build
this before that dial proves insufficient. This is the dynamic form of its future
`adaptive` type — quality chosen per cell from motion rather than fixed. It is
recorded here so the design is not re-derived, and so the salvage point is not lost.

An earlier implementation of this scheme is pinned at commit `8990971`. Use
`git grep -n 'quality_for_churn\|split_cells\|CLEANUP_IDLE' 8990971` to locate
the classifier, cell splitting, churn tracking, and cleanup logic in that tree.

## Idea

Instead of one fixed quality per target, choose quality **per tile per frame**
from how fast that region is changing: a region in full motion is sent at a low
JPEG quality (few bytes, motion hides the loss), and when it settles it is
re-sent once at high quality (a crisp "cleanup" frame). Static regions are never
touched. The payoff over a fixed dial is that a *windowed* video degrades only
its own cells, not the whole screen, and a paused screen sharpens on its own.

## Why it is deferred, not built

More moving parts than the fixed dial earns until measured: a churn tracker with
stable cell identity, a bounded RGB stash for cleanup re-encodes, and a cleanup
timer in the encode loop. The fixed dial captures most of the bandwidth win with
none of that machinery. Build this only if A/B testing shows the fixed dial
leaves real, wanted bandwidth on the table.

## Design sketch (for when it is picked up)

This is a new point on the existing render dial, not a new flag: the future
`render_type = "adaptive"` paired with `render_subtype = "adaptive-jpeg"` (a
per-cell classify-and-vary codec). The default `full` + `png` is untouched, and a
target on any other dial setting keeps its current path — nothing below runs
unless a target opts into the adaptive type. The pieces:

- **Cell identity.** The gateway `Shadow` is pixel-exact and has no stable cell
  identity, so add the earlier implementation's fixed **320×64 grid** (`CELL_W` /
  `CELL_H`, already declared in `src/protocol.rs`) as the churn key. Snap each
  changed rect *outward* to cell boundaries; each cell has a stable `(col, row)`
  identity. Mirror `split_cells` from commit `8990971`.
- **Churn → quality.** Per cell, count changed frames in a short window
  (`CHURN_WINDOW`), map it through `quality_for_churn` (`JPEG_QUALITY_STATIC=80`,
  `JPEG_QUALITY_MOVING=45`, `CHURN_FULL_SPEED=4`), and encode that cell at the
  resulting quality.
- **Cleanup.** Stash the source RGB of any cell sent below static quality
  (bounded by `MAX_STASH_BYTES`). A timer in `order_loop` (a `tokio::select!`
  with a `MissedTickBehavior::Delay` interval) re-encodes cells idle for
  `CLEANUP_IDLE` at `JPEG_QUALITY_STATIC`, at most `MAX_CLEANUPS_PER_TICK` per
  tick, so a settled screen becomes crisp without a client repaint.
- **Placement.** `ChurnCell` / `Motion` / stash logic → `src/encode.rs`, owned by
  the `order_loop` task; the classifier and encoders → `src/protocol.rs`. Because
  `TileSink` is shared, RDP and VNC both get the feature from one implementation.
  Clear `Motion` on resize and on reattach/forget in both engines.

Constants from the earlier implementation: `CHURN_WINDOW=8`, `CLEANUP_IDLE=500ms`,
`CLEANUP_TICK=250ms`, `MAX_CLEANUPS_PER_TICK=8`, `MAX_STASH_BYTES=8MB`.

## Verification (when built)

- A target **not** on the adaptive type (the default `full` + `png`, or a
  `fixed-quality` target) is byte-identical to its current path.
- A/B on the real RDP and macOS VNC/ARD targets: full-motion bytes down
  substantially at the same surface size, static content unchanged, cleanups seen
  after pausing (~40 samples, median/p90, initial repaint separated from steady
  state).
- A windowed video degrades only its own cells, not the whole row — the
  320×64-grid payoff over the fixed dial.
- Playwright stays format-agnostic (record/frame relationships, header fields).
