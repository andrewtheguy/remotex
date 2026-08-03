# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — what the region streams do not decide yet

`render_motion_subtype = "h264"` ships: the `motion` detection chooses the regions,
an H.264 stream carries each one, and the still codecs carry everything else. Three
of its numbers are policy that was chosen to be legible rather than measured, and the
measurements are what should settle them:

- **`MAX_STREAMS` is four**, and the merge that keeps the count under it is judged by
  one ratio (`MERGE_WASTE`). A desktop with five genuinely independent moving regions
  is not obviously a desktop where merging beats dropping the smallest to stills.
- **`RETUNE` and `STREAM_IDLE` are both 500 ms**, and a retune costs a keyframe only
  when the region it wants no longer fits inside the rectangle its stream already
  has. Shrinking is free and so is any change of shape that stays inside it; what
  pays is growing, and a region that ended and came back. One measurement exists —
  25 s of a pointer swept in a circle on a 1280×800 RDP desktop, which grows and
  moves the wanted rectangle about as often as anything real would: 12 keyframes
  costing 38 KB of the 140 KB the streams sent, so **27% of the stream went on
  rectangles that had to be replaced**. Whether a longer `RETUNE`, or a rectangle
  deliberately grown past what is moving, recovers that is the question, and it wants
  more than one kind of content behind it.
- **A component's own bounding box is not checked against `MERGE_WASTE`.** Only
  merges are. A single diagonal streak of moving cells therefore streams a box mostly
  full of still ones — safe, since every cell inside is owed a cleanup, but wasteful
  if it turns out to be common.

None of these is worth changing on argument. They want the same treatment `video`
got: a measurement first.

### Raising quality above the dial

The congestion loop both streaming dials share can notice a backlog but never find
headroom: it walks the quantizer up when the outbound queue says the link is behind
and back down to the configured quality when it is not, and never past it. Under
`render_motion_subtype = "h264"` it is blunter still, because that target's outbound
queue is sized for its still tiles and so absorbs a backlog before the signal
appears. Both are sound where they are used — exceeding the operator's setting was
never a goal — but it means a link with room to spare is never discovered.

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

### remotex.app

- **Native clipboard and display panels.** The app's **Remote › Clipboard…** and
  **Display** menu items drive the client's own panels through the bridge rather
  than presenting AppKit ones. That is right for now — one clipboard editor, one
  consent boundary, one display list — but a Mac app whose only sheet is a web
  panel is a compromise, not a design. Native versions are worth having once
  there is a reason to touch that layout again; they were deliberately not done
  in the shell refactor, where the cost was a regression risk in a docked-panel
  layout that had already needed fixing once.

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
