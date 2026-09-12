# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### What the RDP client does not carry yet

The client carries the desktop, the pointer, keyboard, mouse and resize. Sound,
the clipboard and touch were all carried by the engines before it, and none of
them was carried *here*: the clipboard and sound came from a library's channel
client — IronRDP's `cliprdr` and `rdpsnd`, then FreeRDP's — and touch from
FreeRDP's `rdpei` plugin. `proto` is the gateway's own now, so each of the three
is a channel to write rather than a dependency to configure, and each is refused
where it would otherwise build a control with nothing behind it: `audio = true`
and `clipboard = true` at config parse, touch by having no key at all — whether
touch exists is the host's answer, and this client never asks.

Everything on either side of all three channels is already written and shipped.
The browser's Clipboard panel, its touch passthrough layer (`touchPassthrough.ts`),
the `/ws/audio` socket, the audio queue and both its encoders are
protocol-agnostic; so are `ServerMsg::TouchReady`, `ClientMsg::Touch` and the
clipboard message pair. Nothing below the wire needs designing for any of them.

EGFX is the fourth thing this client does not carry, and it is under
[Source payloads](#source-payloads-the-gateway-decodes-instead-of-forwarding)
rather than here: its payoff is a transcode removed, not a control restored.

#### The clipboard (MS-RDPECLIP)

The nearest of the three, because only the wire is missing:

- `src/rdp_clipboard.rs` is the format decision and the `CF_UNICODETEXT`
  conversions either direction needs, with its tests, compiled and called by
  nothing.
- The engine's half of it was written for the FreeRDP era and removed with that
  engine — `git show 7d76b5d^:src/rdp.rs`. Advertise on Ready, ask the moment the
  remote's format list arrives so a remote copy reaches the browser unprompted as
  it does on the other two engines, answer every paste request including with
  nothing, and retry a `CB_RESPONSE_FAIL` on a bounded ladder. It wants
  re-targeting at the new client's event stream and command queue, not rewriting.

New here: Monitor Ready, Clipboard Capabilities, Format List and its response,
Format Data Request and Response. Short format names and the one text format are
the whole of what this gateway speaks, and there is no library to take the PDUs
from now that the client is its own.

Delayed rendering is what to verify against a real host rather than a container.
Both directions are lazy on the wire, and the bounded retry above is a scar from a
live Windows peer: xrdp's cliprdr is not a proxy for what a Windows paste handler
does, and a request left unanswered is a remote application frozen in its own
paste. `tests/rdp_client_probe.rs` against the Windows host in `tmp/test_uat.toml`
is where a round trip belongs.

#### Touch (MS-RDPEI)

The smallest, because MS-RDPEI is a *dynamic* channel and that transport is
already here: `proto/dvc.rs` carries Display Control over `drdynvc`, and the
session answers every Create Request it does not want with `NO_LISTENER`
(`session.rs`). Accepting a second name, the RDPEI PDUs — client ready, and a
touch event's contact frames — and the contact state machine are the work.

The rest is waiting for it. A host that opens the channel becomes
`Event::TouchReady`, which the engine forwards as `ServerMsg::TouchReady` and
re-sends to each client that attaches; the browser offers the passthrough toggle
only after that, and `rdp.rs` drops a `ClientMsg::Touch` today because no engine
can report one. Held contacts must be released when a client goes away, or the
remote keeps fingers down that no longer exist. A Windows host opens MS-RDPEI and
xrdp never does, which is the reason this stays an always-offered capability
rather than a key.

#### Sound (MS-RDPEA)

The largest of the three, and the least of a shared story with the other two. The
server chooses between two transports, so both have to exist: the static `rdpsnd`
channel, which also wants `rdpdr` registered beside it, and the dynamic
`AUDIO_PLAYBACK_DVC`. Over them: version and the server's format list, the client's
answer, training and its confirm, Wave and Wave2 with their confirms, quality mode,
volume and pitch. Both earlier engines took these PDUs from a library, so all of it
is new here even though `src/rdp_audio.rs` has twice been written.

What does not change is everything downstream. One format is advertised —
`PCM_CD_QUALITY`, 44.1 kHz 16-bit stereo — and `AudioBridge`, the Opus and
passthrough encoders, the socket and the claim it is bound to stay exactly as the
VNC engines leave them. Two rules come with the bridge: a wave buffer must never
block the decode loop (`AudioBridge::wave` drops its oldest rather than waiting,
the same bargain the damage path makes), and the Client Info PDU has to stop saying
`INFO_NOAUDIOPLAYBACK` — with that flag set the session has no audio device to
redirect at all.

#### The channel plumbing the first of them pays for

The clipboard and `rdpsnd`'s static transport both need a second *static* virtual
channel, which this client has never had:

- `CS_NET` asks for `drdynvc` alone and `connect.rs` takes the first number the
  server hands back as it. A second channel makes that resolution by name, and
  gives the session a second reassembler, a dispatch arm and a channel id on the
  way out.
- `proto/channel.rs` writes one chunk and refuses a payload that would need a
  second, because everything this client sends on a channel is a sixty-four byte
  monitor layout. Half a megabyte of clipboard text is a megabyte of UTF-16
  against a 1600-byte floor, so the splitter deliberately left out has to exist.

Inbound reassembly of any length is already there, and so is the Virtual Channel
capability set. Touch needs none of this.

### Render dial — what the region streams do not decide yet

`render_motion = true` ships: the motion detection chooses the regions,
a video stream carries each one, and the still codecs carry everything else. Three
of its numbers are policy that was chosen to be legible rather than measured, and the
measurements are what should settle them:

- **`MAX_STREAMS` is four**, and the merge that keeps the count under it is judged by
  one ratio (`MERGE_WASTE`). A desktop with five genuinely independent moving regions
  is not obviously a desktop where merging beats dropping the smallest to stills.
- **`RETUNE` is 500 ms and `STREAM_IDLE` 1 s**, and a retune costs a keyframe only
  when the region it wants no longer fits inside the rectangle its stream already
  has. Shrinking is free and so is any change of shape that stays inside it; what
  pays is growing, and a region that ended and came back. The measurement is a
  damage tape (`src/tape.rs`: record a session with `REMOTEX_MOTION_TAPE=<path>`)
  replayed through the detector, the regions and the encoder at each pair of values
  (`encode::tests::replay_a_motion_tape`, `--release`), so every row of a table is
  the same pixels under a different number. Two tapes exist on the 64-point grid,
  both on a 1920×1080 Windows RDP desktop with base `png` and stream quality 40:
  40 s of a video playing in a 1331×749 window, and 44 s of a document paged with
  PageDown and PageUp tapped six times a second (`tests/ws_probe.py --page`), whose
  moving region is a steady 768×896 body. At 500/1000 the video costs 12 keyframes
  and 238 KB of a 2453 KB stream (9.7%) with 9.2 MB of lossless tiles beside it; the
  scroll 11 keyframes and 166 KB of 5522 KB (3%) with 15.7 MB of tiles. Before the
  tapes, at 500/500, they cost 26 keyframes, 18% and 33.8 MB, and 13 keyframes, 5%
  and 91 MB. Three mechanisms made up the difference, and each is worth knowing
  because each is the kind of thing that comes back:
  - *The cleanup tick expired a playing stream.* A stream's "last moving" stamp is
    refreshed only by a retune, every 500–533 ms at frame boundaries, and the 250 ms
    cleanup tick ends a stream idle for `STREAM_IDLE`. With the two numbers equal, a
    tick landing between a retune plus 500 ms and the next retune ended a stream
    whose region was still moving, and the retune milliseconds later rebuilt it with
    a keyframe — every 5–6 s on both tapes, at the beat of the two clocks, and the
    whole of the scroll's keyframe waste. A second's `STREAM_IDLE` costs 4% more
    lossily carried cells on the scroll and 13% on the video, not the 76% the old
    grid measured, because a 64-point region holds little that is not moving. A stamp
    refreshed by the damage path when a covered cell is seen moving would remove the
    race whatever the two numbers are; the values do it for now.
  - *Nothing streamed for a second after the video qualified.* The retune interval
    was measured from the last retune whether or not anything was live, so a region
    qualifying just after one waited for the next — and on the video tape a 500 ms
    gap in the frame boundaries, the engine stalled behind the PNG encode of those
    very frames, let the detector forget and cost a second interval. 23 MB of the
    session's 24 MB of tiles were the two seconds before the first keyframe. The
    interval now holds only while a stream is live.
  - *A split band re-sent every still cell in the report's box.* The scroll's box
    ran from the text column to the scrollbar, and the cells between went out as PNG
    at the frame rate, changed or not — some 67 MB of the 86 MB the replay encoded,
    for cells the tape saw change three times in 44 s. On the wire most of it had
    become cache references; all of it had been encoded. A split band now sends only
    the quiet cells that changed.
  What is left is measured, and not taken or not yet:
  - *The box grows a column per retune.* A video's edge cells cross the churn
    threshold later than its middle, so after every scene change the wanted box
    widens by one cell at successive retunes — 960, 1024, 1088, 1216, 1280, 1344,
    1408 wide — and each step is a restart, six of the video's twelve keyframes.
    `RETUNE` does little to it because the ramp is paced by the detector. A ring of
    one still cell round every built stream was tried on the tapes: on the video,
    keyframes 11 → 9 and decoder builds 6 → 3 for 20% more lossily carried cells,
    15% more keyframe bytes and 2% fewer tiles; two rings, 8 keyframes for 39% more
    cells. Not worth it. What would pay is the detector admitting an edge cell sooner
    when its neighbours already stream.
  - *`STREAM_IDLE` at 2 s* costs the video nothing more and takes the scroll's tiles
    from 15.7 MB to 9.4 for 12% more lossily carried cells, because a run of paging
    pauses between PageDown and PageUp; a paused video would sharpen a second later.
    Not taken.
  - *The detector's own latency* — a cell needs `CHURN_MOVING` of `CHURN_SLOT`,
    400 ms, before it moves — is what remains of the startup cost: the video's
    frames in that time are most of its 9.2 MB of tiles, the scroll's 5 MB of 15.7.
  - *`RETUNE` longer than `STREAM_IDLE` is ruinous*: the stream expires before it may
    be rebuilt and the content goes to the still codec meanwhile — 44 MB of tiles at
    1000/500 for the video against 9 at 500/1000, 28 against 15.7 for the scroll.
    Whatever the values, keep `STREAM_IDLE` the larger.
  The tile column is the encode, before the wire's tile cache: the scroll session
  encoded 87 MB of tiles and sent 25 MB of them. The older figures were on the 320×64
  grid and are kept for the shape of the question: 25 s of a pointer swept in a
  circle on a 1280×800 RDP desktop cost 12 keyframes and 38 KB of a 140 KB stream
  (27%), and replaying 98 retunes of a real 1920×1080 scroll with `STREAM_IDLE` at
  1 s instead of 500 ms took the client's decoder builds from 37 to 31 for 76% more
  streamed cells. A pointer sweep over RDP moves nothing — the cursor is drawn by the
  client — so the pointer case is no longer a content kind worth taping there.
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

- **RDP EGFX.** The RDP client takes plain bitmap updates and does not advertise
  MS-RDPEGFX at all, so a host's surface commands, RemoteFX Progressive and H.264
  are all unused. Carrying the pipeline would buy cheaper resizes and, beyond it,
  AVC420 pass-through — handing the host's H.264 to the browser rather than
  decoding it and encoding VP9. What makes that large is that it is a second
  graphics pipeline beside the one every engine shares, not an option on it.
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

### A virtual-display remote session for sway

Console-style remote control of a physical sway machine, the way Apple's High
Performance mode and the Windows console session work: every physical display
is folded into one resizable headless output for the length of the session, the
gateway renders it at the browser's density, and the person at the keyboard
takes control back through a virtual console switch. The wire half has shipped
in wlshare, one private pseudo-encoding and one message type each way,
documented in [`wlshare-density.md`](wlshare-density.md). What remains is
the session daemon on the sway host, a controller speaking sway IPC beside
wlshare. A stage 1 prototype of it was measured on macintel: stock sway
1.10 gives the resizable headless output beside the live panel and the restore
holds, but wayvnc 0.9.1 crashes on half the connects while the output is created
beside it. That crash is the risk now, and stage 2 starts with its backtrace.

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
