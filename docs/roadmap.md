# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — video

The per-target render dial and its `motion` strategy are **built** and are
described in [architecture.md](architecture.md#the-render-dial): a `render_type`
(quality strategy), a `render_subtype` (codec), `render_quality`, and — under
`motion` — `render_motion_subtype` and `render_motion_quality` for the cells
currently changing fast. Detection is a 320×64 churn grid, the moving encode is a
cheap lossy still, and a cell that settles is re-sent at the base encode. The
earlier plan of a content classifier (`adaptive-jpeg` subtype) and a quality that
follows the link (`adaptive` type) is **scrapped**: neither is on the way to video,
and content is the wrong question when what costs bandwidth is what moves.

What remains is the encoder the detection was built to be replaceable underneath.

**Swap the moving-cell encoder for H.264** — `render_motion_subtype = "h264"`, a
value legal only on that axis, which is why the axis exists. The detection, the
base encode and the cleanup path are all unchanged; what changes is that a moving
cell feeds an inter-frame stream instead of one independent still per frame, which
is where the real win is. That is a wire change — a new record or format byte,
keyframe and stream lifetime rules, and a decode path in each client
(VideoToolbox, WebCodecs) — and the open question is what the stream is *of*, since
a 320×64 cell is a poor unit for a video encoder: the likely answer is a coalesced
moving region rather than a stream per cell. Deliberately not designed yet; what
the shipped detection measures on real targets decides its shape.

`h264` is not a `MotionSubtype` variant yet; the config refuses it by name until it
is built.

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
