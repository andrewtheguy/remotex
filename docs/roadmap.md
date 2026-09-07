# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — what the region streams do not decide yet

`render_motion = true` ships: the motion detection chooses the regions,
a video stream carries each one, and the still codecs carry everything else. Three
of its numbers are policy that was chosen to be legible rather than measured, and the
measurements are what should settle them:

- **`MAX_STREAMS` is four**, and the merge that keeps the count under it is judged by
  one ratio (`MERGE_WASTE`). A desktop with five genuinely independent moving regions
  is not obviously a desktop where merging beats dropping the smallest to stills.
- **`RETUNE` and `STREAM_IDLE` are both 500 ms**, and a retune costs a keyframe only
  when the region it wants no longer fits inside the rectangle its stream already
  has. Shrinking is free and so is any change of shape that stays inside it; what
  pays is growing, and a region that ended and came back. One measurement exists —
  25 s of a pointer swept in a circle on a 1280×800 RDP desktop, with the grid cut
  at 320×64 pixels, the cell of the time (the cell-count figures below are in those
  cells, and a 64-point grid grows a box at a different rate), which grows and
  moves the wanted rectangle about as often as anything real would: 12 keyframes
  costing 38 KB of the 140 KB the streams sent, so **27% of the stream went on
  rectangles that had to be replaced**. Whether a longer `RETUNE`, or a rectangle
  deliberately grown past what is moving, recovers that is the question, and it wants
  more than one kind of content behind it. `STREAM_IDLE` has one measurement on the
  other axis, the client's decoders: replaying the same 98 retunes of a real 1920×1080
  scroll with it at 1 s instead of 500 ms took the decoder builds from 37 to 31, for
  76% more streamed cells — lossy cells, each owed a cleanup. With `VideoEnd`
  already keeping churn from breaking a hardware decoder, that trade was left
  untaken; it is there if a future measurement asks for it.
- **A component's own bounding box is not checked against `MERGE_WASTE`.** Only
  merges are. A single diagonal streak of moving cells therefore streams a box mostly
  full of still ones — safe, since every cell inside is owed a cleanup, but wasteful
  if it turns out to be common.

None of these is worth changing on argument. They want the same treatment `video`
got: a measurement first.

### Raising quality above the dial

The congestion loop both streaming dials share can notice a backlog but never find
headroom: it walks the dial down when the outbound queue says the link is behind
and back up to the configured quality when it is not, and never past it. Under
`render_motion = true` it is blunter still, because that target's outbound
queue is sized for its still tiles and so absorbs a backlog before the signal
appears. Both are sound where they are used — exceeding the operator's setting was
never a goal — but it means a link with room to spare is never discovered.

The existing `paintAck` feedback supplies the receiver's view of *queueing*: it
reports when a batch finished the client's ordered decode-and-draw pass, and the
adaptive loop subtracts the link's recent floor to detect falling behind. An empty
paint window still says only that the configured quality fits; it does not measure
how much more would fit. Going further therefore wants richer receiver feedback —
delivered bytes and arrival timing added to that contract, for example — plus an
explicit upper-bound policy. It is a separate feature whose value should be argued
from `video`'s measurements rather than assumed.

### Source payloads the gateway decodes instead of forwarding

Three places where a remote could hand this gateway something closer to what the
browser needs, and it decodes or re-encodes instead. Each is real work with a real
payoff, and none of them is near-term — they are here so that "why not this one"
has an answer rather than being rediscovered.

- **RDP EGFX, past what FreeRDP's GDI already gives.** The pipeline itself is
  **on**: `SupportGraphicsPipeline` with `RemoteFxCodec` beside it, which is the
  pair guacamole-server ships and the resolution of the black-framebuffer fault
  this entry used to open with — the pipeline advertised *without a codec next to
  it* was the whole of that bug, and the e2e that measured exactly black now
  measures a painted desktop. Its frame boundaries are taken too — the wrapper
  marks the pipeline's once-per-frame surface flush (and the legacy markers
  besides) as `Event::Frame`, and the engine flushes on it. What remains planned
  is using more of the channel than FreeRDP's software GDI surfaces: the surface
  compositor, and a separate assessment of AVC420 pass-through. The parts exist
  in the archives; what makes the rest large is that it is a second graphics
  pipeline beside the one every engine shares, not an option on it.
- **Tight/JPEG/H.264 VNC decode or pass-through.** Generic `vnc` advertises only
  the lossless standard encodings on purpose: Tight and TightPNG are vendor
  encodings, JPEG and H.264 are lossy, and advertising an encoding is a promise to
  decode it. Tight-family decoding, and handing a lossy source payload to the
  browser untouched, would remove upstream bytes and a transcode — for a target
  where the operator has already accepted lossy, the transcode is pure loss. The
  cost is a decoder this repo would then own.
- **Apple High Performance screen video (HEVC).** High Performance supplies its
  virtual display over zlib rectangles today. The same media stream that carries
  system audio (shipped behind the `apple-hp-audio` feature — see
  [`architecture.md`](architecture.md)) can also carry the screen as an HEVC
  stream over SRTP, which would remove the zlib transcode on that subtype. Only
  the audio leg has been reverse-engineered and implemented; the video leg's
  offer is sent because the Mac refuses audio without it, but its payload was
  never received — see [`apple-vnc-889.md`](apple-vnc-889.md). It is the larger,
  less certain half, and widening standard `ard` still comes before deepening
  this subtype.

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

### Automatic density on generic VNC

Today a plain VNC server's density is declared by hand from the menu's Density
toggle — offered only under `render_type = "video"`, so the tile grid is never
cut at a client's word — and the compositor's scale is set by hand beside it
([`docs/generic-vnc-hidpi.md`](generic-vnc-hidpi.md)). Two manual steps that have
to agree, for something RDP does with none: it declares the browser's density in
the monitor layout and the host renders at it. Making generic VNC follow the
browser the same way — a Retina window gets a 2x desktop on connect, and dragging
it to a 1x screen gives a 1x one — wants both halves closed, and neither is small:

- **Telling the compositor.** Standard RFB has no field for it, so the density
  must reach sway some other way: an IPC call the gateway makes on the sway host
  (`swaymsg output <name> scale <n>`, over SSH or a small agent there), or a
  wayvnc or neatvnc extension that carries a scale with `SetDesktopSize`, which
  means patches upstream. Either is a second channel beside the VNC connection,
  with its own reachability, credentials and failure modes; the gateway has none
  of that for VNC today.
- **Learning the answer.** Whatever the compositor was asked, the gateway needs
  to *know* what it did before labelling the framebuffer, because a label the
  server did not honour is a desktop shown at the wrong size. RDP and Apple both
  answer on the wire; standard RFB never will. The label would have to come from
  the same side channel, and be re-read whenever the desktop changes size.
- **Per-server semantics.** wayvnc forwards a resize as a headless output's
  custom mode; other servers (TigerVNC, x11vnc, a KVM console) have no notion of
  scale at all, and asking one for twice the pixels gives twice the desktop. So
  the automatic path is really "sway through wayvnc", and belongs behind a
  per-target opt-in that names the compositor it is talking to.

The value is one click saved per session on one kind of server. Until that costs
more than the two channels above, the declaration stays manual. The sway
session dialect below is the opt-in that closes it for that one server, by
putting the scale on the VNC connection itself rather than beside it.

### A virtual-display remote session for sway

Console-style remote control of a physical sway machine, the way Apple's High
Performance mode and the Windows console session work: every physical display
is folded into one resizable headless output for the length of the session, the
gateway renders it at the browser's density, and the person at the keyboard
takes control back through a virtual console switch. It is a VNC dialect —
one private pseudo-encoding and one message type each way, behind
`subtype = "sway"` — plus a session daemon on the sway host built from wayvnc,
a small neatvnc hook and a controller speaking sway IPC. The design, the byte
layouts, the sway command sequence, the open questions and the staged plan are
in [`sway-remote-session.md`](sway-remote-session.md). Stage 1 of that plan —
a resizable headless output beside a live DRM panel — is the whole risk, and
nothing after it starts until that is measured on macintel.

## Not planned

### The screen path's remaining queue depths

About 144 tile records can buffer across three queues in series on the way to the
socket, and `wire.rs`'s supersede rule sees only the final batch — so a record two
queues back is not a candidate for the drop that would make it unnecessary.

Left alone deliberately. The measurements behind `PAINT_WINDOW` say those depths
are not what binds: under the window the same motion carried its picture in half
as many records, and the queue the client actually waited on was the one past the
socket, which the window now bounds. Shrinking a depth here would be another
number adjusted in isolation, which is how the audit found these in the first
place.

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
