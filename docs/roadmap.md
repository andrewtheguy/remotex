# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Render dial — what the region streams do not decide yet

`render_motion_subtype = "stream"` ships: the `motion` detection chooses the regions,
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
  25 s of a pointer swept in a circle on a 1280×800 RDP desktop, which grows and
  moves the wanted rectangle about as often as anything real would: 12 keyframes
  costing 38 KB of the 140 KB the streams sent, so **27% of the stream went on
  rectangles that had to be replaced**. Whether a longer `RETUNE`, or a rectangle
  deliberately grown past what is moving, recovers that is the question, and it wants
  more than one kind of content behind it. `STREAM_IDLE` has one measurement on the
  other axis, the client's decoders: replaying the same 98 retunes of a real 1920×1080
  scroll with it at 1 s instead of 500 ms took the decoder builds from 37 to 31, for
  76% more streamed cells — lossy cells, each owed a cleanup. With the ladder and
  `VideoEnd` already keeping churn from breaking a hardware decoder, that trade was
  left untaken; it is there if a future measurement asks for it.
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
`render_motion_subtype = "stream"` it is blunter still, because that target's outbound
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

### AV1, if it ever measures better than VP9

VP9 shipped and is the codec: `render_type = "video"` and
`render_motion_subtype = "stream"` carry it, and it is licence-free —
present in every browser build, the proprietary-codec-free ones included. See
[`architecture.md`](architecture.md) for the mechanism.

AV1 is the codec that might still be worth adding, and it is a measurement rather than
a preference. It compresses better and costs more to encode, and this gateway encodes
in real time on whatever machine it is running on — so the case for it is a number
from `video::measure_the_encoder` against VP9's, on real content, not an argument
from the format. Adding it would mean an encoder module and a codec choice threaded
back through `RenderPlan`; it
would **not** be a default, because
AV1 is refused by engines with no hardware path for it and a codec nobody can play is
a worse default than a codec everybody can.

It would not bring the probe back either. The client-side codec negotiation was built
and removed (see [`architecture.md`](architecture.md)); a second codec is a value
for a config key, not a reason to ask the browser again.

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
  system audio (below) can also carry the screen as an HEVC stream over SRTP, which
  would remove the zlib transcode on that subtype. Only the audio leg has been
  reverse-engineered; the video leg's payload was never received — see
  [`apple-vnc-889.md`](apple-vnc-889.md). It is the larger, less certain half, and
  widening standard `ard` still comes before deepening this subtype.

### Apple High Performance system audio, behind a non-default feature

High Performance mode routes the Mac's **system audio** to the viewer, and the
whole path has been reverse-engineered and proven end to end: a from-scratch client
negotiated the stream and decrypted 1,794 live AAC-ELD packets from a Mac that had
sound playing. [`apple-vnc-889.md`](apple-vnc-889.md) records the wire — the `0x1c`
negotiation message, the `1010`/`1011` reply encodings, the AVConference offer
plist, and the SRTP/AAC-ELD stream. This is a genuine capability the product lacks,
and it is the one thing High Performance does that `ard` cannot, so it is worth
shipping — but **only behind a Cargo feature that is off by default**, for three
reasons that are all real costs rather than caution:

- **The decoder licence.** The audio is AAC-ELD (MPEG-4 object type 39), which no
  browser's WebCodecs and no native FFmpeg decoder will decode, so passthrough is
  impossible and the gateway must decode to PCM itself. The only portable open
  decoder is Fraunhofer **fdk-aac**, whose licence is not OSI-approved — the same
  class of cost that got HE-AAC built and reverted (see
  [`architecture.md`](architecture.md) on remote audio). Gating the feature keeps
  that dependency out of the default build and the default binaries entirely.
  Apple's own AudioToolbox decodes AAC-ELD, but only when the gateway runs on
  macOS, so it is not a substitute for the general build.
- **It drags in the HEVC video negotiation.** The Mac refuses an audio-only media
  stream: the `0x1c` message must also carry a valid HEVC screen-video offer or the
  agent aborts both legs. So even an audio-only feature has to synthesize and send a
  video offer it then ignores — extra reverse-engineered surface for a subtype
  already marked experimental.
- **It is the least-settled subtype.** Everything here is measurement against one
  macOS build with no specification behind it, on the `ard-high-performance` path
  that AGENTS.md already tells readers to prefer widening `ard` over.

The shippable shape, then: a `--features apple-hp-audio` (default-off) build that
pulls in the fdk-aac decoder and the SRTP/RTP/RTCP receiver, a per-target opt-in key
that is refused at config parse unless the feature is compiled in (the same rule
shape as `audio` being refused on non-RDP targets), and — once decoded to PCM — the
existing `/ws/audio`, bridge, and Opus-or-passthrough path downstream, which is
protocol-agnostic and needs nothing new. The container and default release binaries
never compile the feature, so the Fraunhofer dependency and the experimental wire
stay out of them; an operator who wants Mac audio builds it in deliberately. The
[`tests/hp_audio_probe.py`](../tests/hp_audio_probe.py) probe is the reference for
the receive-and-decrypt half.

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
