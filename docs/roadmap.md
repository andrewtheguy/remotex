# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — video per moving region

The per-target render dial ships four strategies, described in
[architecture.md](architecture.md#the-render-dial): `full`, `fixed-quality`,
`motion`, and `video`.

**`video` sends the whole desktop as one H.264 stream**, which is the simple shape
and the one built first, deliberately: it makes what the codec actually costs on
real screens measurable before anything is built on top of it. What remains planned
is the thing the `motion` detection was built for — **a stream per coalesced moving
region**, with the still codecs carrying everything else, so a video in a window
costs its own pixels and the text beside it stays exact.

That is not a smaller version of `video`; it is the open question `video` exists to
inform. A 320×64 cell is a poor unit for a video encoder, so the shape is a coalesced
region rather than a stream per cell — and then the hard parts are which regions to
coalesce, when a region's stream starts and ends, and what happens to a region that
stops moving while its stream is still the truth on screen. `motion`'s cleanup pass
answers that last one for stills by re-sending the settled cell at the base encode;
a stream has no equivalent yet. Deliberately not designed further until `video`'s
measurements say what a stream costs.

`h264` is not a `MotionSubtype` variant and the config refuses it by name, which is
now a statement about this rather than about the codec: the motion axis hands out a
cheaper encode *per cell*, and a stream has no per-cell dial to turn down.

### Raising quality above the dial

`video`'s congestion loop can notice a backlog but never find headroom: it walks the
quantizer up when the outbound queue says the link is behind and back down to the
configured quality when it is not, and never past it. That is sound where it is used
— exceeding the operator's setting was never a goal — but it means a link with room
to spare is never discovered.

Going further needs the receiver's view, which TCP hides: loss and jitter are behind
retransmission, and the only thing the sending side can observe is how fast its own
socket drains. So this wants a client-reported measurement — bytes received and
arrival timing as a new `ClientMsg` — and work in both clients. A separate feature,
and one whose value should be argued from `video`'s measurements rather than assumed.

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
