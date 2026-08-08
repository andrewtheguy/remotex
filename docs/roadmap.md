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
  more than one kind of content behind it.
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
- **Apple Adaptive media.** High Performance supplies its virtual display over zlib
  rectangles. Apple's HEVC/AAC-over-SRTP path is reverse-engineering work with no
  specification behind it, on the subtype that is already the least settled — see
  [`apple-vnc-889.md`](apple-vnc-889.md). Widening standard `ard`, or anything on
  the path both subtypes share, comes first.

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

### Companion Chrome extension

`remotex.app` (`apps/viewer`) has these two things today, because it is a window
this program controls. Most people are in a browser, and this is what would give
them the same two. Both were measured against stock Chrome plus a small MV3
extension — the spike is a proof of viability, not a design:

- **A clipboard that keeps syncing while the window is unfocused or minimized.**
  `navigator.clipboard.readText()` is refused unless the document is focused, which
  is why `pushBrowserClipboardOnFocus` in `useRemoteDesktop.ts` fires on focus and
  nowhere else. An extension *offscreen document* with reason `CLIPBOARD` polls the
  system clipboard regardless of focus. Confirmed on macOS in both directions:
  a copy made while Chrome was minimized reached the page, and a push made while
  Chrome was minimized reached the system pasteboard.
- **⌘W and ⌘Q reaching the guest.** In a regular tab Chrome reserves them and the
  page never sees a keydown, which is why `macKeys.ts` forwards Command as itself
  there rather than mapping the chord. Two ways out, both confirmed: in an
  **immersive** view — fullscreen plus `navigator.keyboard.lock(['KeyW','KeyQ'])` —
  both arrive as ordinary keydowns; and in a **macOS installed-app window** no keys
  are reserved at all, so the page sees every shortcut first and `preventDefault()`
  captures ⌘W/⌘Q windowed. Held Esc always escapes the lock, by design, and is the
  one chord a remote session can never have.

Only those two. Everything else the app owns is a menu bar standing in front of
this client's own controls, and a browser needs none of it — which is also why the
app hides the floating menu rather than duplicating what it does.

The shape of the work: a `window.postMessage` handshake the page uses to notice the
extension and degrade without it, `clipboard-changed` from the extension calling the
same `sendClipboard` path a focus push takes (the echo guards `lastFromRemoteRef` and
`lastToRemoteRef` already cover it), and the reverse direction replacing what
`mirrorRemoteClipboard` hands the app. The keyboard half wants the fuller Command
chord table `macKeys.ts` already selects for the app, gated on the extension being
present rather than on a build flag. Distribution is "Load unpacked" for personal
use; anything managed goes through `ExtensionSettings` with
`installation_mode: force_installed` and an `update_url` — a hosted update manifest
beside the `.crx`, or the Web Store's. A policy has no way to pin a `.crx` sitting
on the disk.

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
