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

### Passed streams that adapt, on Windows and the Mac

The goal is a gateway that re-encodes as VP9 only for the servers that are not
prioritized ([Other servers](architecture.md#other-servers-not-prioritized)) and
for a browser that cannot take what the remote sends. A wlshare target is there
already: its stream is always passed, and wlshare walks it by the browser's link.
So is Windows, in part: an RDP target with the pipeline on passes it in every
session, and VP9 is left to the bitmap path, `egfx = false` and a host that
answers with bitmap updates. Its pipeline follows the browser's link by the
host's own pacing, and in a session started with Adaptive (alpha) by the host's
rate control of its H.264, which rests on host behaviour the specification does
not promise
([An official way](#an-official-way-to-have-windows-rate-control-its-h264-over-tcp);
[A passed stream and the browser's link](architecture.md#a-passed-stream-and-the-browsers-link)).
The Mac is not. A Mac's passed HEVC is for a LAN and stays one: its rate control is the Mac's,
between 20 and 60 Mbit/s, and it is not to be made to hear the browser. A Mac on
a slow link is to be served by another passed stream instead. A passed stream has
to adapt as well as the gateway's VP9 before it can replace it. What is to be
explored:

- **Standard mode with Adaptive, passed, for a Mac on a slow link.** Standard
  mode (`ard`), on the Mac's physical displays or on the unofficial virtual
  display (`virtual_display = true`), is decoded here and always re-encoded
  today ([Tier 3](architecture.md#tier-3-a-macs-standard-screen-sharing)). Apple's
  viewer's **Adaptive** quality asks for the Mac's private encodings first, the
  8×8 DCT tiles of `0x3f3` and the 15-bit zlib of `0x3ea`, before zlib and ZRLE
  ([Apple's own framebuffer encodings](apple-vnc-889.md#apples-own-framebuffer-encodings)).
  Passed and decoded in the page, they are lossy encodings the Mac chooses
  itself, and RFB is pulled, one update request at a time, so a request held
  until the page has drawn the update before it paces the Mac by the browser's
  link, as wlshare's held fence does. That would make it the slow-link
  alternative to High Performance's HEVC: the virtual display where the session
  should follow the window, the physical displays otherwise. What is to be
  measured is whether the Mac chooses among the encodings, or refines its tiles
  less, when the link is slow, and how such a session compares on that link with
  ZRLE re-encoded as VP9.
- **More of the Windows host's own adapting.** A passed pipeline gets fewer
  frames from a page that composes slowly, and one step down in RemoteFX
  Progressive's first pass while the host's socket backs up. H.264 takes no step:
  its moving regions already go out at the host's coarsest quantizer, so on a
  slow link only its frame rate drops
  ([The connection sequence](rdp-client.md#the-connection-sequence)).
  Network auto-detect and `connectionType = 7` were tried over a constrained TCP
  link: the detector reported the constraint correctly, but the host made the
  same frame-rate reduction with or without it and did not change H.264
  quantizers. `AVC_THINCLIENT` was tried too; it moved the whole desktop to
  AVC444v2 but still adapted by rate. That narrows the missing
  Microsoft-client behavior. One pathway is now found: a client that takes the
  detection *and* claims reliable UDP, then turns down the host's offer and
  stays on TCP, gets rate-controlled H.264. It is not known to be the only one
  or the one Microsoft's clients use. At 1 Mbit/s
  that means coarser regions at 21 frames a second instead of 12.5, and over a
  LAN QP 22 throughout at 6.3 Mbit/s
  ([The connection sequence](rdp-client.md#the-connection-sequence)). Next:
  - Repeat the UDP offer with H.264 by region and with H.264 refused, to see
    whether RemoteFX Progressive and the mixed desktop adapt too.
  - A passed session's detection reports the page's link. Answered from the
    gateway's own socket, it reported the hop to the host: behind a page on
    1 Mbit/s the host was told 8 to 20 Mbit/s, drew LAN-grade 33 KB frames and
    fell to 3.7 frames a second. Answered at the pace the page composes (`Paced`
    in `session.rs`), the host was told about 1.2 Mbit/s. It then drew 17.7
    frames a second at 6.4 KB, mostly at QP 38, and behind a 4 Mbit/s page 20 a
    second at 20.6 KB at QP 22 and 30. All of it stays on TCP, so it works with a
    WebSocket ([The connection sequence](rdp-client.md#the-connection-sequence)).
    An adaptive session now takes both (`src/rdp.rs`); a lossless one takes
    neither. The picker offers the whole desktop in H.264 (`AVC_THINCLIENT`),
    the default under Adaptive, and H.264 by region, where the host is told the
    page's link but the desktop is mostly RemoteFX Progressive and the frame rate
    does not improve: 4.4 frames a second at 25.7 KB behind a 1 Mbit/s page,
    where the whole desktop drew 17.5 at 6.4 KB. Each round trip is held for
    the page's distance, the browser socket's smallest recent ping, idle link or
    not: behind a page 150 ms away the host then concluded 151 ms and drew 20
    frames a second instead of 10.5. The page's pace is read only while a frame
    has waited for it longer than its distance, so a distant page is not taken
    for a slow one: 150 ms away on an unlimited link it was reported at a median
    of 8.2 Mbit/s rather than 0.9 to 1.7. Nor are the frames on their way to
    it held as a backlog: there it got 30 frames a second, as near the host.
    Still to do: QA it from a browser on a really slow link.

  Measure each case against the same desktop on the bitmap path
  (`egfx = false`), re-encoded as VP9. A host that draws with bitmap updates
  could be passed too, for the page to decode.
- **What Microsoft's iOS client is sent on a slow link.** Against the same
  host, Microsoft's Remote Desktop client on iOS sees the whole screen soften on
  a slow link, as video does, where a passed pipeline keeps its text sharp and
  loses frames. The iPhone client was confirmed never to have UDP turned on, so
  its softening comes over TCP. One TCP pathway is found: the host
  rate-controls its H.264 for a client that takes network detection and claims
  UDP, even when it then refuses the UDP
  ([The connection sequence](rdp-client.md#the-connection-sequence)). Whether
  the iPhone client takes that pathway or another, such as drawing without the
  graphics pipeline, is not known. The host-side checks below are how to find
  out; the transport test among them is already answered (TCP).

  To be found out on the host, with the iPad on a slow link and a passed
  session on the same link beside it:
  - The `RemoteFX Graphics` counters (frame quality and frames skipped for
    insufficient network, client or server resources) and the `RemoteFX
    Network` counters (current TCP and UDP bandwidth and round trip), which say
    what the host is doing and which transport it measures.
  - The `RemoteDesktopServices-RdpCoreTS/Operational` event log, which records
    the transport, the graphics version and whether AVC and AVC444 are in use.
  - The iOS client's core data and capability advertise, read through a
    recording proxy such as `freerdp-proxy`: whether it claims UDP in
    `CS_MULTITRANSPORT` and network detection with UDP turned off, whether the
    host draws it with the graphics pipeline or with bitmap updates, and which
    codecs and quantizers the host answers with on the constrained link.

On Windows the pipeline is already passed in every session, with no VP9 for a
browser that cannot compose it. Once a passed stream for the Mac adapts as well
as VP9 on the links measured, it is offered first at the picker, and VP9 remains
for a browser that cannot take it.

### An official way to have Windows rate-control its H.264 over TCP

An adaptive session has the host rate-control its H.264 by taking the network
detection and claiming reliable UDP in `CS_MULTITRANSPORT`, then turning the
host's offer of UDP down (`E_ABORT`) and staying on TCP
([The connection sequence](rdp-client.md#the-connection-sequence)). It works
against the hosts measured, but it depends on host behaviour the specification
does not promise: nothing in MS-RDPBCGR or MS-RDPEMT says a claim of UDP that is
then refused changes how the host codes for TCP, and a Windows update may stop
doing it. What is to be found out is whether there is a cleaner or official way
to the same rate control:

- What Microsoft's clients actually send: mstsc, the iOS and macOS apps, and
  the web client over its gateway, read through a recording proxy — whether
  they claim UDP with UDP turned off, which connection type and detection
  results they state, and which graphics capability sets and flags they
  advertise.
- Whether a capability, a connection type or a detection result alone does it,
  without the claim of UDP.
- Whether a group policy or registry setting on the host does it, for an
  operator who can set one.
- What the RDP 10.x graphics capability sets (MS-RDPEGFX) and the MS-RDPBCGR and
  MS-RDPEMT text on network detection and multitransport say of it, read from
  the specifications rather than recalled.

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

None of it has been measured at 4K.

A slow link is not the passed HEVC's to answer. A browser on one is served by a
session started without the passthrough, whose VP9 the adaptive walk lowers, and
later by a passed Standard mode with Adaptive
([Passed streams that adapt](#passed-streams-that-adapt-on-windows-and-the-mac)); a passed
session is not switched to VP9 for it. A brief stall stays what it is: the Mac's
units predict from every one
before, so none can be dropped alone, and a full queue drops to the next IDR and
asks the Mac for one, which brings the picture back as one fresh frame rather than
a replay of the backlog.

### Strips in Safari's decoder

A passed High Performance stream comes in four strips, to the browser's own
`VideoDecoder` as to the page's software decoder, and the page places each
picture that decoder outputs by its strip's number
([Apple's media stream, passed through](architecture.md#apples-media-stream-passed-through)).
Chrome's decoder on a Mac takes such a stream, whose pictures are a quarter of
the display. Safari's has not been tried with one, on a Mac or an iPhone.

### The page's own FLAC decoder, heard in a browser

A session started with lossless sound is decoded by a decoder written in the
page's module (`frontend/wasm/flac`), which leaves a frame's samples in the
module's memory for the page to copy into its `AudioBuffer`
([Audio frames](architecture.md#audio-frames)). It is
held to libFLAC's frames sample for sample by its tests and its benchmark, under
Bun. It has not been played in a browser: a `wlshare` session and an RDP one,
in Chrome, Safari and Firefox, listened to, with the console read for a frame
refused.

### H.264 in the RDP graphics pipeline

A host draws with H.264 only on a passed pipeline, for the page to decode, in a
session started with the picker's Adaptive (alpha), over the whole desktop or
by region
([RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)).
What is not done:

- **The gateway does not decode it, and is not going to.** It composes no
  pipeline at all: every pipeline is passed to the page, and a Lossless session
  refuses H.264 with `AVC_DISABLED`. Decoding H.264 here only to encode VP9
  would be a second lossy pass. It is here so that "why not this one" has an
  answer rather than being rediscovered.
- **AVC444 against a host.** Both layouts are implemented and tested from the
  specification's tables. *H.264 full screen* sets `AVC_THINCLIENT`, and the
  host tried sent AVC444v2, luma views alone, over the whole desktop; *H.264
  region* does not, and the same host sent AVC420 by region. No host has been
  seen to send AVC444, or AVC444v2's chroma views.
- **Large video.** Every decoded picture is copied into the compositor's memory
  and converted there, which was measured at a 1280×800 desktop and not above,
  and from a hardware decoder that copy is a readback off the GPU. Presenting a
  decoded picture on the GPU, and reading it back only when a later command
  copies from it, is the step after that if a large one proves slow.

### Two streams for Standard's Combined Display

Standard's Combined Display over two screens is one framebuffer of both, which is
often past the video ceiling at factor 1.0 (5376×2287 over a 2x screen beside a
1x one), and then has no picture: the page offers one screen instead
([past the ceiling](architecture.md#past-the-ceiling)). The plan is to carry that
view as two streams, one per screen, each its own VP9 stream within the ceiling,
for the page to lay out by the mosaic's regions as it composes one framebuffer
today. How the Mac's rectangles split between the two, how the queue and the
paint window order two chains, and how each stream starts over are the work.

Two screens is the limit, as it is today: All Displays over three or more is held
with the notice whatever its size.

High Performance is not part of this. Its All Displays is not one framebuffer:
each of its virtual displays is a stream of its own already, the second shown in
a browser tab ([two virtual displays](#two-virtual-displays-past-alpha)).

### Two virtual displays past alpha

`virtual_displays = 2` on an `rdp` or `ard-high-performance` target is alpha
([Windows](guide.md#windows-rdp), [Macs](guide.md#macs-vnc-with-an-apple-subtype)).
What it lacks:

- **Telling the displays apart.** The session page's Info names both displays
  and links to the second, but nothing on the remote's screens says which
  display is which.
- **A tab left behind.** When the session's page goes — closed, reloading, or
  its socket dropped — the tab showing the second display carries on with no
  word of it: its input still reaches the remote through the reattach grace, and
  only the engine ending takes the display away. The tab's display socket already
  carries one thing that is the session's rather than the display's, the
  clipboard ([Clipboard](architecture.md#clipboard)). The plan is for it to carry
  whether the session's page is attached too, and for the tab to say so over its
  desktop until the page is back.

Fixing those does not take it out of alpha by itself. It also needs more testing
than it has had, on both kinds of target, before it is offered as anything else.

### A touchpad's scroll as a gesture on a Mac

A Mac is sent a scroll as a distance on both axes, with no gesture around it
([a Mac scrolls by a distance](apple-vnc-889.md#a-mac-scrolls-by-a-distance)).
Horizontal scrolling from a touchpad feels less fluid that way than vertical. A
two-finger swipe is rarely straight: on a Windows precision touchpad the browser
reports both axes in most swipes, as finely on one as on the other, and a Mac
holds a scroll to the axis it began on only within a gesture.

The plan is to send the gesture. The scroll message has a field for the scroll
phase and one for the momentum phase, which Apple's viewer fills from its own
trackpad. A browser's wheel events carry neither, so the gateway would open a
gesture at the first event of a glide, continue it while events keep coming and
end it once they stop. What the Mac does with a gesture it is sent that way, and
whether the phases alone make the horizontal axis feel right, are still to be
tested, on a physical Mac.

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

### A cheaper frame of lossless sound

Taken up if a page is ever short of the time. The page's FLAC decoder takes
some 58 thousand cycles for a frame of music, twenty milliseconds of it, which
is a thousandth of a core and less than libFLAC takes natively
([the benchmark](../frontend/wasm/flac/bench/README.md)). What is left is two
loops in which every sample waits on the one before, so neither is a loop SIMD
divides:

- **The residual's Rice codes**, half of a frame. Each is a count of zeros and a
  shift on a 64-bit window, and the window is taken once for two to eight of
  them. Three more shapes of that loop measured no faster, and are in the
  benchmark's README so that they are not tried again as they were.
- **The prediction**, a fifth. It is a function for each order to twelve, with
  the sample before held in a register, and has been looked at least: the sum
  over the samples before the last could be taken four at a step, and a
  predictor of two or three samples, which a quiet passage mostly has, could
  hold them all in registers.

Those shares are a profile's under Node, and the cycles are counted under Bun,
so a change is judged by `bench/run.sh` and not by the profile.

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

### Encodings other than ZRLE on a plain VNC target

A plain `vnc` target advertises ZRLE, and Raw beside it: CopyRect, zlib, Hextile
and RRE are for servers without ZRLE, Tight and TightPNG are vendor encodings,
JPEG and H.264 are lossy, and advertising an encoding is a promise to decode
it. Decoding the Tight family, or handing a
lossy payload to the browser untouched, would remove upstream bytes and a
transcode at the cost of a decoder this repo would then own. That work is for a
server outside the three the project prioritizes, which is reached through the
RFB baseline and nothing more
([Constraints](architecture.md#constraints)). A stream of its own passed
through is what a `wlshare` target has.

### `THINCLIENT` in the graphics capability advertise

`caps_advertise` in the graphics crate's `proto/gfx.rs` sends versions 8 and 10 with the small cache
and, on version 10, `AVC_DISABLED` — or, for an adaptive session, which takes H.264,
every set to 10.7 without it — and leaves `RDPGFX_CAPS_FLAG_THINCLIENT`
unset either way. A current Windows host, the only host this client targets, ignores the
flag; the hosts that acted on it, choosing the classic RemoteFX codec over the
progressive form, are not supported, so there is nothing for the flag to change.
The decision is recorded in
[The RDP client](rdp-client.md#the-graphics-pipeline-ms-rdpegfx).

### More than two virtual displays on a target

`virtual_displays` is held to two. A browser tab shows one display, and the
gateway encodes only the displays shown: the one on the session's page, and on
*All Displays* (alpha) the second in a tab of its own at `/display/2`. Each
further display is a desktop the host renders for nobody but a further tab, and
two is the most the picker, the input offset, the tab's feed and the span the host
builds have been checked with. RDP would allow sixteen in a row (MS-RDPBCGR 2.2.1.3.6). A Mac's High
Performance mode advertises how many virtual displays it will create, which is
two, holds a configuration naming more to two, and has a video leg for a second
display and no third, so two is the limit there whatever this gateway asked for.
What holds the ceiling is that nothing shows more: a person at
a browser looks at one screen, and a third display that is never on it is work
the host does for a menu entry. Raising it means first wanting the view that
would use it, such as the Mac's All Displays composed across virtual displays,
which is not planned either.

### Multiple sessions

Concurrent sessions, shared sessions and a session broker are not planned. One
gateway instance has one active session slot, as defined by the
[session constraints](architecture.md#sessions). This keeps engine ownership,
input state and per-session media choices attached to one browser claim.

A different browser can use **Take over** to evict the current holder. A reconnect
or target switch by the same browser reclaims the slot without a prompt. A forced
takeover ends the existing session and returns the new holder to the picker
(`src/session.rs`).
