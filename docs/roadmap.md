# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*,
or under consideration, belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### What the RDP client does not carry yet

The client carries the desktop, the pointer, keyboard, mouse, resize, the
clipboard, sound, and the browser's camera and microphone. Touch is not carried,
and is [under consideration](#touch-on-an-rdp-target-ms-rdpei) rather than
planned.

#### Licensing on a Remote Desktop Session Host

The licensing step accepts exactly one PDU: an `ERROR_ALERT` carrying
`STATUS_VALID_CLIENT`, which is what every Windows host this client has been
pointed at sends. Anything else is refused by name in
`proto/license.rs`, and the connection ends before `DemandActive`.

A Session Host with the Remote Desktop Session Host role and per-device CAL
licensing does not send that. It opens a real exchange: `LICENSE_REQUEST`, the
client's new or upgrade licence request, the platform challenge and its response,
and the issued licence. This client would disconnect at the first of those.
FreeRDP's `libfreerdp/core/license.c` is an implementation to compare while
writing the exchange; a temporary reference checkout belongs under
`tmp/references/`.

How many real deployments this reaches is not known, and no host in use has shown
it. What would settle it is one connection to an RDSH configured for per-device
CALs; the work is only worth taking once a host that needs it turns up.

#### Verifying the server's certificate chain

The TLS handshake accepts any certificate, for the session only, and verifies the
handshake signature alone (`proto/tls.rs`). What stands in for the chain is
CredSSP: the credential exchange is bound to the public key of the certificate
that terminated this very handshake, so an interceptor holding a certificate of
its own cannot complete it. That argument is spelled out in the module's header
and holds only because plain TLS is never offered.

What it does not give an operator is a way to say *which* host they expect. A
Windows host's default listener certificate is self-signed and regenerated when
the machine is renamed, so a public CA chain is the wrong check for most targets;
a per-target pin — the certificate's public key or its SHA-256 fingerprint in the
target's configuration, compared against what the handshake presented — is the
one that fits. Absent a pin, today's behavior stands.

### The first keyframe on a slow link

Every VP9 stream governed by the target's dial starts there, and its first
keyframe is the whole desktop at that quality: 400 KB for a 1080p desktop at 90,
two seconds on a 2 Mbit/s link before the first paint, and the largest lag any
session on such a link ever shows.
The walk cannot know the link before the first frame has crossed it, so the
keyframe holds the verdicts for two seconds instead of misreading its own queue
as the link ([the codec](architecture.md#the-codec)); a stream that started below
the dial and climbed, as the walk climbs from a settle, would paint sooner on a
slow link at the cost of a coarser first second on a fast one, which the settle
sharpens within half a second of the desktop going quiet. Measured before it is
chosen: the cost on the LAN, where most sessions run, against the gain on the link
it is for.

### How wlshare's passed-through stream is doing

wlshare's passed stream is coded and walked there, so the gateway knows each frame's
size and whether it is a keyframe and nothing about the quality it went out at: the
encode totals of such a session count its units, keyframes and bytes, and report no
round coarsened and a lowest quality of 100 whatever wlshare did. Reporting it
wants wlshare to say what it coded each frame at: one change to the wire, made
in wlshare and read here.

### Apple's passed HEVC at 4K

A High Performance stream is offered Apple's bitrate entries and reported on as
Apple's viewer reports, so the Mac's rate controller walks it between 20 and
60 Mbit/s by the delay between the Mac and the gateway
([Rate control](apple-vnc-889.md#rate-control)).

High Performance is not made for a slow link. Apple's
[guide](https://support.apple.com/guide/remote-desktop/use-high-performance-screen-sharing-apdf8e09f5a9/mac)
asks for high bandwidth and consistently low latency, recommends a wired
connection, and names 75 Mbit/s for a single 4K display, its virtual display's
largest (3840×2160, within the gateway's ceiling). So what a passed stream
([Apple's media stream, passed through](architecture.md#apples-media-stream-passed-through)) is
adapted for next is resolution: carrying a 4K display, not a link that cannot
carry the stream.

- **60 Hz.** A virtual display refreshed 60 times a second, which a passed stream
  can carry since nothing here decodes it.
- **The level the browser is asked about.** The page asks its decoder about level
  5.0 (`L150`), macwork's stream at 1600×1000, and 5.0 carries 4K at 30 pictures a
  second. The Mac does not name the least level that fits, though: macvm's stream
  announced 5.1 (`L153`) at 2880×1800 and 30 Hz, and 4K at 60 needs 5.1 anyway. So
  the question is to name the level the Mac announces, measured in Chrome and
  Safari, rather than the one macwork's first capture did.
- **The browser's queue in the reported delay.** The delay reported to the Mac
  ends at the gateway's socket. For a passed stream, whose picture nothing here
  re-encodes, the delay that matters runs on to the browser, which the gateway
  knows from its paint window (`src/feedback.rs`). Adding it would let the Mac's
  controller answer a browser link that falls behind, which today drops to the
  next IDR instead.

None of it has been measured at 4K.

A slow link is not a passed stream's to answer. A browser on one is served by a
session started without the passthrough, whose VP9 the adaptive walk lowers; a passed
session is not switched to VP9 for it. A brief stall stays what it is: the Mac's
units predict from every one
before, so none can be dropped alone, and a full queue drops to the next IDR and
asks the Mac for one, which brings the picture back as one fresh frame rather than
a replay of the backlog.

### H.264 in the RDP graphics pipeline

A host draws with H.264 only on a passed pipeline, for the page to decode, behind
a target's experimental `egfx_h264` key
([RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)).
What is not done:

- **The gateway does not decode it, and is not going to.** A pipeline composed
  here refuses H.264 with `AVC_DISABLED` on purpose: a lossy video codec loses
  detail before the gateway ever encodes the picture, and decoding it only to
  encode VP9 is a second lossy pass. It is here so that "why not this one" has an
  answer rather than being rediscovered.
- **A choice at the picker.** It is a config key while it is experimental. Once
  it is not, whether a session takes a lossy source is the kind of thing the
  picker asks.
- **AVC444 against a host.** Both layouts are implemented and tested from the
  specification's tables. The one host tried sent AVC420 by region, and with
  `AVC_THINCLIENT`, which this client does not set, luma views alone.
- **Large video.** Every decoded picture is copied into the compositor's memory
  and converted there, which was measured at a 1280×800 desktop and not above,
  and from a hardware decoder that copy is a readback off the GPU. Presenting a
  decoded picture on the GPU, and reading it back only when a later command
  copies from it, is the step after that if a large one proves slow.

### Two streams for Apple's All Displays

Standard's All Displays over two screens is one framebuffer of both, which is
often past the video ceiling at factor 1.0 (5376×2287 over a 2x screen beside a
1x one), and then has no picture: the page offers one screen instead
([past the ceiling](architecture.md#past-the-ceiling)). The plan is to carry that
view as two streams, one per screen, each its own VP9 stream within the ceiling,
for the page to lay out by the mosaic's regions as it composes one framebuffer
today. How the Mac's rectangles split between the two, how the queue and the
paint window order two chains, and how each stream starts over are the work.

Two screens is the limit, as it is today: All Displays over three or more is held
with the notice whatever its size.

## Under consideration

### Touch on an RDP target (MS-RDPEI)

Taken up if the need for it shows. The RDP client does not carry touch: it is a
channel to write, and it has no config key, since whether touch exists is the
host's answer and this client never asks.

Everything on either side of the channel is already written and shipped: the
browser's touch passthrough layer (`touchPassthrough.ts`), `ServerMsg::TouchReady`
and `ClientMsg::Touch` are protocol-agnostic. Nothing below the wire needs
designing for it.

MS-RDPEI is a *dynamic* channel and that transport is already here:
`proto/dvc.rs` carries Display Control over `drdynvc`, and the session answers
every Create Request it does not want with `NO_LISTENER` (`session.rs`). Accepting a second name, the RDPEI PDUs — client ready, and a
touch event's contact frames — and the contact state machine are the work.

The rest is waiting for it. A host that opens the channel becomes
`Event::TouchReady`, which the engine forwards as `ServerMsg::TouchReady` and
re-sends to each client that attaches; the browser offers the passthrough toggle
only after that, and `rdp.rs` drops a `ClientMsg::Touch` today because no engine
can report one. Held contacts must be released when a client goes away, or the
remote keeps fingers down that no longer exist. A Windows host opens MS-RDPEI and
xrdp never does, which is the reason this stays an always-offered capability
rather than a key.

### Not forwarding silence in a passed sound stream

A quiet RDP host sends no sound, so its browser receives no packets until
something plays. The two passed streams do not behave that way: a High
Performance Mac and a `wlshare` target go on sending units while nothing plays,
and the gateway forwards each one (`AudioListener::into_passed` in
`src/audio.rs`). Holding the silent ones back would make all three alike.

- **The Mac's AAC-ELD.** Silence can be told without a decoder. A quiet Mac sends
  one unit a hundred times a second, the four bytes `00 68 34 00`: `max_sfb` 0,
  so no band carries a coefficient, and a `global_gain` for each channel. A unit
  whose `max_sfb` is 0, or whose every section names the zero codebook, is
  silence by its header, read before anything Huffman-coded. That was captured
  on macvm; a physical Mac has not been.
- **wlshare's sound.** wlshare holds the PCM before it codes it, so the silence is
  its to withhold rather than the gateway's to detect in Opus or FLAC.

What it saves is small: the Mac's silence is 400 bytes a second of payload
against about 320 kbit/s while it plays. What it costs is on the page, which
must take a stop in the units as silence rather than as a stream that fell
behind, and in a few silent units still forwarded after the sound ends, since
AAC-ELD's window overlaps the frames before it. Whether the framing around each
unit makes the saving worth that has not been measured.

## Not planned

### Tight, JPEG and H.264 on a plain VNC target

A plain `vnc` target advertises only the lossless standard encodings: Tight and
TightPNG are vendor encodings, JPEG and H.264 are lossy, and advertising an
encoding is a promise to decode it. Decoding the Tight family, or handing a
lossy payload to the browser untouched, would remove upstream bytes and a
transcode at the cost of a decoder this repo would then own. That work is for a
server outside the three the project prioritizes, which is reached through the
RFB baseline and nothing more
([Constraints](architecture.md#constraints)). A stream of its own passed
through is what a `wlshare` target has.

### `THINCLIENT` in the graphics capability advertise

`caps_advertise` in the graphics crate's `proto/gfx.rs` sends versions 8 and 10 with the small cache
and, on version 10, `AVC_DISABLED` — or, for a passed pipeline that takes H.264,
every set to 10.7 without it — and leaves `RDPGFX_CAPS_FLAG_THINCLIENT`
unset either way. A current Windows host, the only host this client targets, ignores the
flag; the hosts that acted on it, choosing the classic RemoteFX codec over the
progressive form, are not supported, so there is nothing for the flag to change.
The decision is recorded in
[The RDP client](rdp-client.md#the-graphics-pipeline-ms-rdpegfx).

### Multiple sessions

**Concurrent sessions, shared sessions, and a session broker are outside the
product model.** This is one user's program, and that is not a limitation waiting
to be lifted.

There is one active session slot: one active session per gateway instance,
permanently. A new browser takes over and evicts the previous holder
(`src/session.rs`), which a client offers with a Take over button — the same
shape as Windows Remote Desktop. A reconnect and a target switch reclaim the
slot in silence: they are the same browser coming back. A takeover ends the
session it found and starts at the picker.
