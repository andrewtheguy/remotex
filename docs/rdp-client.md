# The RDP client, written here

The gateway speaks RDP with its own client, `src/rdp_client/`, down to the wire
format. `rdp_client/proto/` encodes and decodes every PDU against [MS-RDPBCGR]
and its extensions; the modules above it own one thread per session, a complete
framebuffer painted from those decoders, and an event per damaged rectangle. The
engine that consumes those events — damage into the video stream, `ClientMsg` into scancodes
— is `src/rdp.rs`, and the boundary between the two is the point of this document:
everything below it is protocol, everything above it is this gateway's.

What a host draws with is a crate of its own, `crates/remotex-rdp-graphics`: the
graphics pipeline's PDUs and bulk compression, the codecs, the compositor and the
framebuffer. It is this client's all the same — `rdp_client` names the framebuffer
and that part of the wire as its own — and is apart because the page runs it too,
built to WebAssembly, for a session started with the pipeline passed. It holds no
connection or clock, so it builds for `wasm32-unknown-unknown`, and the one thing
it does on threads is decode a Progressive region's tiles, side by side on rayon's
pool: the gateway's is the process's, and the page's is workers it starts. The files
[Graphics](#graphics) names are that crate's, under its `src/`.

The only thing under that boundary not written here is the CredSSP exchange
itself (`sspi`), because NLA is not optional on a current Windows host and NTLM is
the one mechanism a user name and a password can drive.

The target is the Remote Desktop server built into a current Windows and only
that: no xrdp or other RDP server's behavior, and no legacy fallbacks for older
hosts. Only what such a host was seen
to send is implemented, and anything else — a codec, subcodec or PDU this client
lacks — is refused by name in the log rather than guessed at, so it shows up as a
named refusal instead of a wrong picture. What "current" has been tested against
is Windows 10 and Windows 11. An older Windows and xrdp are not targets: one that
speaks what this client implements may happen to work, but that is neither
tested nor promised. Where [MS-RDPBCGR] is silent
or wrong about what such a host really does — and it is, in places, about both —
the reference is FreeRDP's `libfreerdp/core`, and the arbiter is a real host:
`tests/rdp_proto_probe.rs` and `tests/rdp_client_probe.rs` drive one named in the
operator's `tmp/test_uat.toml`. Several of the decisions below are marked as
measured rather than read, and each of those is a place where the specification
alone would have produced a client that connects and then quietly does nothing.

Every specification this client cites is kept, with its link and the revision
read, in [andrewtheguy/ms-rdp-specs](https://github.com/andrewtheguy/ms-rdp-specs).

[MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c

## What it carries

The desktop, the pointer, keyboard, mouse, resize, the clipboard, sound, and the
browser's camera and microphone going the other way, over one monitor or a row
of two ([Virtual displays](#virtual-displays-alpha)). No touch: it is announced
only by a host that opens MS-RDPEI, which this client never asks for. What it
would take is in [`roadmap.md`](roadmap.md).

## The connection sequence

One long sequence, every step of it a question the host answers before the next
may be asked: the security negotiation, TLS, the credentials, the channels, the
logon, the licence, the capability exchange, and the four PDUs that make the share
live. `rdp_client/connect.rs` is that order, written as one function on purpose —
the steps share nothing but the socket and what the last one said.

The security negotiation offers `HYBRID` and nothing else, so a server that cannot
do NLA is refused rather than logged on to some other way. The TLS certificate
chain and host name are **not** verified, and there is no certificate prompt or
pin. TLS therefore proves possession of the private key for the certificate the
endpoint presented; it does not establish that the endpoint is the host the
operator intended.

CredSSP supplies a separate protection. [MS-CSSP sections 1.5 and
3.1.5](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-CSSP.pdf)
explicitly allow deployments without a common CA root, then bind the authenticated
SPNEGO exchange to the public key used by TLS before the client delegates its
credentials. A relay onto a TLS session with a different public key fails that
binding. This does not protect a user who was directed to an unintended server
that can itself complete authentication, or a server whose key is compromised.
Targets therefore have to be identified by the operator's network and name
configuration, not by this client's certificate handling. `proto/tls.rs` records
the implementation checks.

The socket comes from `engine::tcp_connect`, so an RDP host that goes silent is
noticed on the same keepalive schedule as every other engine's.

`ConnectionType` is declared a LAN rather than detected: the socket is the hop
between the host and a gateway near it, which is not the link a person is on, and
neither the declaration nor the detection alone changes what the host draws
(below). A session requests neither an MCS message channel nor multitransport, and it does not
advertise network-characteristics detection. The detection is implemented
(`proto/autodetect.rs`) behind `Connect::autodetect`, which only
`tests/rdp_link_probe.rs` turns on, to measure what a host does with it: the client
then claims it in the core data, leaves `connectionType` for the host to detect,
asks for and joins the message channel, and answers each measurement with what
arrived on its own connection. Beside it, `Connect::multitransport`, also the
probe's alone, claims reliable UDP with Soft-Sync in `CS_MULTITRANSPORT`
([MS-RDPBCGR section
2.2.1.3.8](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf))
and answers each Initiate Multitransport Request with `E_ABORT` (2.2.15.2), so
the session stays on TCP (`proto/multitransport.rs`). This is a deliberate
protocol boundary, not a general claim about what Windows will send. In particular,
[MS-RDPBCGR section
2.2.1.3.2](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
says that advertising the graphics-pipeline flag also requires network-detection
support. The tested Windows 10 and 11 hosts accept the client's smaller
combination; an auto-detect PDU outside that observed traffic is unsupported.

A Windows host does draw the graphics pipeline for a slow link, but for the one it
finds for itself and not for the one it is told of, unless the client also offers
UDP (the last case below). Measured against a Windows 11
host without a GPU, with the pipeline passed and one full-screen moving picture at
1280×800, which over a LAN is about 34 frames a second at 114 KB each in RemoteFX
Progressive, and about 32 a second at 9 KB each where H.264 is taken:

- **Told, nothing changes.** A client that took the detection of [MS-RDPBCGR
  section 2.2.14](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
  and reported every bandwidth window as 2 Mbit/s, or as 200 kbit/s, was sent the
  same frames as one that declared a LAN, and so was one that took no detection
  and declared `connectionType` a modem or low-speed broadband. The host's
  detection over TCP was only a Bandwidth Measure Start and Stop around each few
  kilobytes it sent: no round-trip request, no connect-time phase, and never a
  Network Characteristics Result.
- **Acknowledged late, only the rate changes.** With each frame's acknowledgement
  held 200 ms the host drew 20 frames a second, and held 1.5 s about 3, every one
  still 114 KB.
- **Read slowly, the frames get smaller.** Behind a relay that read the host's
  socket at 8 Mbit/s the host drew 8 frames a second at 115 KB for about fifteen
  seconds and then 15.6 a second at 62 KB, and stayed there; at 2 Mbit/s it took
  the same one step, to 3.8 a second at 63 KB, and no second one in ninety
  seconds. Taking the detection as well changed neither the step nor when it came.
  H.264 took no step, and has none to take. Its regions state their quantization
  parameter ([MS-RDPEGFX section
  2.2.4.4.2](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPEGFX.pdf)),
  and what moves is sent at 41, the coarsest of the three the host uses, over a
  LAN already; 33 and 25 are what it refines a still region to. Read at 1 Mbit/s
  it drew 12 frames a second at 10 KB, and at 500 kbit/s under 3 at 23 KB, each
  carrying more of the motion, all still at 41.
- **H.264 for the whole desktop, only the rate changes.** With `AVC_THINCLIENT`
  ([The pipeline, passed on](#the-pipeline-passed-on)) a Windows 11 laptop drew a
  full-screen video at 1280×800 in AVC444v2 alone, luma views only, 32 frames a
  second at 9.6 KB and 2.4 Mbit/s, its regions at 22, 30 and 38. Behind a page on
  1 Mbit/s, and behind a relay reading its socket at 1 Mbit/s, it drew 13 frames a
  second at the same 9.6 KB and the same three quantizers. Taking the detection as
  well, answered with what arrived through the relay — 1,854 measurements over 45
  seconds, 1,001 kbit/s in all — left the quantizers unchanged. The detector did
  not cause a further change: with or without it, the constrained link made the
  host cut its output from 32 to 13 frames a second and hold the stream to the
  link. That is one of the [adaptive responses Microsoft
  documents](https://learn.microsoft.com/en-us/azure/virtual-desktop/rdp-bandwidth);
  a quality change, a frame-rate change, and a codec change are alternatives, not
  one required response. Nor is the absence of a Network Characteristics Result
  evidence of failure: [MS-RDPBCGR section
  1.3.9](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
  does not include that message in the continuous exchange over the main TCP
  connection.
- **Offered UDP and told, the quantizers move.** The same laptop, the same video,
  whole-desktop AVC444v2 and the detection, with the client also claiming UDP and
  turning down the one offer of it the host made, so that everything stayed on
  TCP. The host's detection changed at once: round-trip requests, and about a
  hundred Network Characteristics Results in 45 seconds, the first saying 512
  kbit/s and 400 ms before any measurement, which a client that did not claim UDP
  was never sent. Its encoder changed with it. Over the LAN every region went out
  at 22, in 26 KB frames, 30 a second at 6.3 Mbit/s, where a control run
  beside it without the claim was 22, 30 and 38 in 9.6 KB frames at 2.4 Mbit/s.
  Behind the relay at 1 Mbit/s it drew 21 frames a second at 5.6 KB, 76% of
  its regions at 38, where without the claim it drew 12.5 at 9.5 KB with 52% at
  38: a coarser picture at a usable rate.
  The claim without the detection changed nothing: 22, 30 and 38 at 2.4 Mbit/s
  over the LAN, and no results. So the host rate-controls its H.264 for a client
  that both takes the detection and offers UDP, without the UDP being used. This
  is one pathway, not shown to be the only one or the one Microsoft's clients
  take. Microsoft's iPhone client, with UDP off, softens over TCP by means not yet
  read ([Roadmap](roadmap.md#passed-streams-that-adapt-on-windows-and-the-mac)).
- **Offered UDP behind a slow page, the wrong link is reported.** Same again, with
  the host's link fast and a page on 1 Mbit/s behind the gateway instead, which
  is the passthrough case. Answered from what reached this client's socket, the
  host was told 8 to 20 Mbit/s: each measurement is a short burst that lands in
  the receive buffer before the page's pace holds the socket back. The host
  answered with the LAN's quality, regions at 22 and 30 in 33 KB frames, and so
  only 3.7 frames a second, a second or two behind the desktop. Without the claim,
  the same page got 13 frames a second at 9.6 KB. For a passed session the
  detection has to report the page's link, not the socket's.

The detector's and the link probe's earlier defects are fixed: the
connect-time count includes each payload's header, malformed requests are
refused, a continuous measurement counts inside the framing, `timeDelta` is at
least a millisecond, and a zero frame timestamp gives no lag. The measurements
above were repeated with the fixes. The detector and the claim of UDP stay out of
normal sessions until it is decided whose link they should report
([Roadmap](roadmap.md#passed-streams-that-adapt-on-windows-and-the-mac)).

So what makes this host spend fewer bytes on a Progressive frame is its own socket
backing up, and the regions say what it does then ([MS-RDPEGFX section
2.2.4.2.1.5](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPEGFX.pdf)):
the first pass of each tile goes out at the progressive stage of quality 25 where
over a LAN it is the stage of quality 50, under the same quantization tables. Ten
seconds after the socket is read freely again the host is back at 50. Through a
2 MiB receive buffer the same relay showed the host nothing, and its frames stayed
full size. A session that passes its pipeline is built on this ([The pipeline,
passed on](#the-pipeline-passed-on)).

## The two threads

```text
  caller  ──  Session, Input                  UnboundedReceiver<Event>  ──  caller
     │  command queue                                    ▲
     ▼                                                   │
  the "rdp" thread: a current-thread runtime, select! over the socket and the queue
```

Every session gets an OS thread of its own and keeps it until the session ends.
Decoding a desktop is real CPU work, and on its own thread it runs beside the
engine's encoding rather than in turns with it. Input goes onto a queue the thread
drains between PDUs, so nothing outside that thread ever touches the connection.
Dropping the `Session` asks the thread to disconnect and waits for it, bounded, so
a session that has gone out of scope has really stopped.

## Static virtual channels

Each is asked for by what the session needs: `drdynvc` for a session started with
resize, `egfx = true`, `camera = true`, or `microphone = true`, which is the transport Display Control,
the graphics pipeline, the camera's two channels, and the microphone's channel
ride on; `cliprdr` always, and `rdpsnd` with `rdpdr` for a session started with sound — see
[Sound](#sound-ms-rdpea).

`SC_NET` numbers the channels in the order `CS_NET` named them and says nothing
else about which is which, so `connect.rs` pairs the numbers back up with the
names and hands the session a `Joined` — a number, and the header flags that
channel's chunks wear.

Each channel's chunks carry the flags derived from its `CS_NET` options.
[MS-RDPBCGR sections 2.2.1.3.4.1 and
2.2.6.1.1](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPBCGR.pdf)
say that the server ignores `CHANNEL_OPTION_SHOW_PROTOCOL`, while
`CHANNEL_FLAG_SHOW_PROTOCOL` determines whether the channel header is delivered to
the application endpoint. The client nevertheless keeps the option and the chunk
flag paired. On the Windows host used for the protocol probe, omitting the flag
from `cliprdr` made the clipboard stop responding, while adding it to `drdynvc`
prevented Display Control from opening. `channel::SHOW_PROTOCOL` and the adjacent
tests record those two interoperability observations; they are not asserted as a
rule for every Windows release.

Static-channel PDUs can arrive while the main share is still completing its
capability exchange. `connect::activate` retains them and gives them to the live
session in arrival order. This matters for messages with no generic recovery
mechanism: for example, if the clipboard's opening Monitor Ready PDU were discarded,
the clipboard conversation would never begin. Later clipboard data requests can
still be retried; the invariant here is only that activation must not discard an
already-received channel PDU.

A channel PDU of any length travels either way. Half a megabyte of clipboard text
is a megabyte of UTF-16 against a 1600-byte chunk floor, so `proto/channel.rs`
splits an outbound PDU into chunks that each carry the whole length, and
reassembles an inbound one. An inbound PDU announcing more than 4 MiB
(`channel::MAX_PDU`) is read past and dropped rather than gathered: a novel on the
far end's clipboard then costs a report of its size instead of the session.

## Graphics

Two paths, chosen by the target's `egfx` key, which defaults to on.

### The graphics pipeline (MS-RDPEGFX)

The client sets `RNS_UD_CS_SUPPORT_DYNVC_GFX_PROTOCOL` in the GCC core data, and a
Windows host answers by opening `Microsoft::Windows::RDS::Graphics` over `drdynvc`
— the same dynamic channel transport Display Control rides, so `drdynvc` is asked
for when either key is on. The client accepts the channel and at once advertises
two capability sets (`proto/gfx.rs::caps_advertise`): version 8 with the small
cache, and version 10 with the small cache and `AVC_DISABLED`, so the host never
sends H.264. A host that may send it hands the parts of the desktop that move like
video to it, and a lossy video codec loses detail before the gateway ever encodes
the picture; the client takes the desktop as the pipeline's other codecs carry it
instead, and has no H.264 decoder. `THINCLIENT` is deliberately not set. The host
confirms one set and then draws.

One session advertises otherwise: a pipeline that is passed on with
`Connect::h264`, for a caller that decodes H.264 itself
([The pipeline, passed on](#the-pipeline-passed-on)).

Every PDU the host sends on the channel is wrapped in RDP 8 bulk compression
(`proto/zgfx.rs`), a port of FreeRDP's decoder: a fixed Huffman table over
literals, matches into a 2.5 MB history shared by every PDU for the channel's life,
and runs of unencoded bytes. One wrapper may hold several RDPGFX PDUs, each
eight-byte-headed with its own length, and `proto/gfx.rs` decodes them in order.
The client's own PDUs — the caps advertise, each frame acknowledgement — go out raw, as MS-RDPEGFX 2.1 specifies: "Client-to-server
graphics messages are not encapsulated within any external structure". A Windows
host reads the RDPGFX header straight off the channel and fails its graphics
subsystem when it finds a wrapper there instead.

The host does not paint the desktop; it paints *surfaces* it creates and sizes,
maps them onto the output at an origin, and brackets drawing in StartFrame and
EndFrame. `gfx.rs` keeps each surface's pixels and the rectangles drawn
into since the last frame, and at the EndFrame copies those rectangles of every
mapped surface into the framebuffer — the shape of FreeRDP's `gdi/gfx.c`. Each
EndFrame is acknowledged (`queueDepth` unavailable); on the probed Windows host,
withholding acknowledgements first throttled and then stopped drawing. After its
paint operations are applied, the EndFrame reaches the engine as `Event::Frame`,
the explicit boundary that replaces the engine's timer-based flush. A monitor
layout is answered by ResetGraphics, which resizes the framebuffer and surfaces
as `Event::Resize` without rebuilding the channels.

The support boundary is the set below, not every command or codec a current
Windows release could send. `tests/rdp_client_probe.rs` prints the codec and
command tally for the particular host and session under test, so a newly observed
kind is visible instead of being inferred from an earlier run.

| Graphics input | Implementation and observed boundary |
| --- | --- |
| Uncompressed and planar rectangles | Implemented. Planar uses the same `proto/planar.rs` decoder as bitmap updates. |
| ClearCodec | Residual, band and glyph layers are implemented with their caches, including raw, RLEX and NSCodec subcodecs (`proto/clear.rs`, `proto/nsc.rs`). These were the principal desktop updates on the probed Windows host. |
| RemoteFX Progressive | Reduce-extrapolate wavelets and RLGR1 are implemented in `proto/progressive.rs`. A region selecting the classic RemoteFX wavelet is rejected by name. |
| Surface commands | Surface-to-surface copy, surface-to-cache, cache-to-surface and solid fill are implemented. |
| Unknown or malformed input | A rectangle whose supported command cannot be decoded is left unchanged and logged. Invalid PDU framing ends the session; an unsupported codec or command is reported by name. |

ClearCodec draws over the surface's existing contents. Pixels not covered by a
layer remain unchanged, glyph cache entries capture the surface after their layers
are applied, and band columns can extend beside the nominal rectangle; all affected
pixels are included in the invalidation.

Progressive state is also retained per tile and surface between PDUs, including
coefficients and signs, so an upgrade pass refines the earlier pass. A region is
painted from every tile decoded since the graphics frame began and clipped to the
region's rectangles. [MS-RDPEGFX section
2.2.4.2.1.5](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPEGFX.pdf)
permits a region to refer to tiles carried by an earlier region or PDU in the same
frame. The probed Windows host uses that form when a moved or minimized window
reveals the desktop.

Surface-to-surface copy reads the complete source before writing, which preserves
overlapping scrolls. Cache commands retain rectangles by slot, and solid fills are
clipped to their surface. When the graphics channel ends, an `info` log records the
codecs and commands actually received during that session.

### The pipeline, passed on

`Connect::pass_graphics` — a session started with the passthrough — has the session hand the
pipeline's commands to its caller instead of composing them. The channel is
still this client's: the capability exchange and the bulk compression are as
above, since the history is the connection's, and so is writing each frame's
acknowledgement, which the host stops drawing without. `Graphics::passing` reads
the commands by their headers alone (`proto/gfx.rs::commands`) and decodes three:
the confirmation, the reset, whose size the session announces, and each
EndFrame. Everything the host sent goes out as `Event::Graphics`, whole PDUs in
order, cut where the session has something of its own to say — before the
confirmation and the reset, so `Event::FramesMarked` and `Event::Resize` reach
the caller ahead of the commands that follow them, and after each EndFrame, so
that a run names the frame it ends and an `Event::Frame` follows it. Nothing is
decoded, no surface is kept, and the session's framebuffer holds nothing of what
the pipeline draws.

What the session no longer knows is when a frame is finished, so the caller
says: `Input::frame_composed` once it has composed the run, and the host is
acknowledged then. The host paces itself by that. Measured against a Windows 11
host, it draws until eleven of its frames are unacknowledged and then one for
each acknowledgement, evenly, at whatever rate they come — a reported
`queueDepth` changes nothing — so a caller that composes more slowly than the
host draws sets the rate itself, and every frame it composes is one the host
drew for it. Acknowledged as each is composed, though, such a caller is left
eleven frames behind the desktop for as long as it is slower. So the session
keeps the host a few frames ahead of the caller instead (`PASSED_AHEAD`): while
more wait to be composed than that, the frames already composed go
unacknowledged, and the host's count fills with those; at that many, one is
acknowledged for each composed; below it, two; and once nothing waits,
everything is. A caller that keeps up is acknowledged at once, and no
acknowledgement outlasts the frames in front of it.

Late acknowledgements slow the host's frames and leave each one full size: a host
sends smaller frames only to a client its own socket backs up behind ([Opening a
session](#opening-a-session)), and a session that read the socket as it came would
hide a slow caller from it. So while two frames wait to be composed
(`PASSED_HELD`) the socket goes unread, through a receive buffer of 32 KB
(`PASSED_RECEIVE_BUFFER`), and what the host sends next stays at the host. The
host is then held by its socket, and `PASSED_AHEAD`, which two waiting never
reaches, has nothing to do. Everything else on the connection waits with the
frames while it is unread — the pointer, the sound, the clipboard — as it does
behind a slow link to any client.

Only frames of 32 KB or more are waited on that way (`PASSED_HELD_BYTES`). The
host stops at eleven unacknowledged frames whatever their size, so small ones
never fill its socket, and leaving it unread shows the host nothing and takes
`PASSED_AHEAD` away: with H.264 frames of 9 KB and a caller composing twenty a
second, a socket left at two waiting kept the caller half a second behind the
desktop, and read as it came, under `PASSED_AHEAD`, a sixth of one.

Measured against a Windows 11 host drawing one full-screen moving picture at
1280×800, with a caller that took the frames as a link of a given rate would
(`tests/rdp_link_probe.rs`):

| The caller's link | Socket read as it came | Socket left while two wait |
| --- | --- | --- |
| Unlimited | 34 frames/s of 114 KB, 20 ms behind | the same |
| 27 Mbit/s | 30 frames/s of 114 KB, 140 ms behind | 30 frames/s of 114 KB, 160 ms behind |
| 8 Mbit/s | 8.6 frames/s of 116 KB, 474 ms behind | 16 frames/s of 62 KB, 454 ms behind |
| 2 Mbit/s | 2.0 frames/s of 118 KB, 1.9 s behind | 4.0 frames/s of 63 KB, 2.0 s behind |

At 20 Mbit/s the host stayed at full-size frames either way. The buffer and the
count were chosen on that picture: at one frame waiting and 16 KB the 8 Mbit/s
caller was 394 ms behind at 15 frames a second, and the 27 Mbit/s caller lost a
seventh of its frames to the wait for each read; at 128 KB the 8 Mbit/s caller
was 640 ms behind. The receive buffer bounds what the host can send in one round
trip, so a gateway far from its host is limited by it: 32 KB carries the 31 Mbit/s
of that picture only while the round trip is under 8 ms. So the socket has that
buffer only while the host has a graphics pipeline open (`PassedSocket`): a host
that draws with bitmap updates instead, or has closed the pipeline, has no frames
to be held for, and keeps the buffer the socket came with.

With `Connect::h264` set to `H264::ByRegion` the session tells the host it takes
H.264, which nothing here decodes: the advertise is every set from version 8 to 10.4, and 10.7, none
with `AVC_DISABLED`. 10.5 and 10.6 are left out and 10.7 carries
`SCALEDMAP_DISABLE`, since from 10.5 a host may map a surface through the scaled
mappings, which this client ignores, and only 10.7 has a flag to say so. Version
10.1's capability data is sixteen reserved bytes where every other set's is four
bytes of flags, in the advertise and in a confirmation of it. `AVC_THINCLIENT`,
which asks for the whole desktop in AVC444, is set only by `H264::WholeDesktop`,
on every set from 10.3, and no session asks for that; `tests/rdp_link_probe.rs`
does with `REMOTEX_UAT_H264=whole`. Measured against a
Windows 11 host without a GPU: it confirms 10.7 and goes on drawing with
ClearCodec, Progressive and the caches, and draws what moves like video with
AVC420 in the same frames — a Main-profile stream for the surface, about fifteen
access units a second, Annex B with the parameter sets at each keyframe, each
unit behind a metablock naming the rectangles of the picture to show
(`proto/avc.rs`). With `AVC_THINCLIENT` the same host drew the whole desktop
with AVC444v2 and nothing else, every unit a luma view alone. That is not a finer
picture of what moves: its regions stated a quantization parameter of 38 while
moving and 30 and then 22 once still, three below the 41, 33 and 25 of the
AVC420 it draws otherwise, in frames of the same 9 KB, and the text of the
desktop goes through H.264 with them. Over a full-screen video it is the cheaper
of the two by far: 2.4 Mbit/s at 32 frames a second, where by region the same
video took 7.2 Mbit/s, 5.7 of it RemoteFX Progressive and 0.6 AVC420. On a slow
link it gives up frames and keeps its quantizers, unless the client takes the
network detection and offers UDP
([The connection sequence](#the-connection-sequence)).

The access units ride in the commands handed on, and the caller composes a run
that has any in three steps (`avc.rs`): `avc::scan` finds the units and the
surfaces whose streams have ended, the caller decodes each unit through its
surface's decoder and supplies the picture (`Compositor::supply`), and the run is
composed, painting the masked rectangles from those samples as full-range BT.709.
A unit whose picture was not supplied leaves its rectangles as they were.
`a_real_host_draws_video_with_h264` in `tests/rdp_client_probe.rs` checks what is
passed against a host playing a video.

`Compositor` (`compositor.rs`) is the other half: the same compositor, fed the
commands that were passed. The page's WebAssembly module is a binding around it
(`frontend/wasm/egfx`), and `tests/rdp_client_probe.rs` composes a real host's
passed pipeline with it. It is right only for a pipeline it has followed from
its first command, which is why `Event::FramesMarked` is where a caller starts
one.

### Bitmap updates

With `egfx = false` the pipeline is not advertised, so the server draws with bitmap
updates, decoded by the planar bitmap codec (`proto/planar.rs`). The client
announces no drawing orders either, so the path is bitmaps throughout. The desktop
keeps its opening size: a resize is the pipeline's graphics reset, Display Control is
not taken without it, and a target with `egfx = false` offers no resize at the
picker. Every server that is not Windows takes this path whatever the key.

That is a deliberate narrowing, not an oversight. MS-RDPEDISP 1.3 has a server
without the pipeline answer a monitor layout with a Deactivation-Reactivation
Sequence (MS-RDPBCGR 1.3.1.3), and the client does not implement it: rebuilding the
share mid-session meant holding channel PDUs until it was live again, answering the
clipboard in place meanwhile, and correcting Wave Confirm timestamps for the time
held, all for a resize the pipeline does with one graphics reset. MS-RDPBCGR also
allows the sequence when a logon is attached to an existing session. Windows does not
use it there on either graphics path, measured by reclaiming a session left at another
size, so a mid-session Deactivate All or second Demand Active ends the session with an
error naming it rather than being carried.

The path is 32 bits per pixel and nothing else: the planar codec is the only one
decoded, and the interleaved run-length coding a shallower session would use is
not. The depth is asked for in the GCC conference and in the Confirm Active, but
the server's listener has the last word, and a Demand Active naming any other depth
is refused during the capability exchange. Measured on Windows Server 2025
Datacenter: its `RDP-Tcp` listener limits colour depth to 16 bits out of the box,
so `egfx = false` against it ends with *a server Bitmap capability set carries a
colour depth 0x10*. The same host connects and paints under the pipeline, which
negotiates its own pixel formats and ignores that limit. A Windows workstation's
listener allows 32 bits and takes either path. Such a server is reached with
`egfx = true`, or with its "Limit maximum color depth" setting raised to 32 bits.

Bitmap updates carry no frame boundary, so the engine flushes damage on a guess:
the 16 ms coalescer (`DAMAGE_INTERVAL` in `src/rdp.rs`) reconstructs boundaries by
timing — a quiet screen's damage leaves on the spot, and everything within one
interval after it waits for the deadline, coalesced. Under the pipeline the
EndFrame is the flush signal and that interval demotes to a 100 ms safety net
(`FRAME_NET`) hung past any real frame.

### Either way

A server names the desktop, and the framebuffer is one allocation sized by what it
says, so a size past `bitmap::MAX_DESKTOP_BYTES` — named in a Demand Active, a
ResetGraphics or a CreateSurface — ends the session before anything is allocated
for it. One compressed rectangle is held to the same ceiling, before its planes
are.

## The pointer

The pointer is not part of the framebuffer. RDP servers send the cursor's shape
rather than drawing it, and each shape goes to the browser as `cursor`, which
draws it on its own hardware pointer. A mouse move therefore costs the session
nothing at all, where compositing the pointer into the framebuffer put every one
of them through damage, the flush interval, an encode, the socket, a decode and a
paint. The server's own pointer *positions* are dropped: the browser's pointer is
already where the mouse is, and nothing here can move a hardware pointer.

## Resize and density

In a session started with resize, the Display Control Virtual Channel applies explicit
desktop-size requests, and also matches the client's display density: a monitor
layout carries `DesktopScaleFactor` beside the geometry, so a Retina client gets
twice the pixels with the host's UI drawn at 200% rather than the same UI
stretched. The session also opens at that density: the core data's
`desktopScaleFactor` (with a `deviceScaleFactor` of 100) and a desktop in pixels at
it, taken from the screen the connect names. A later layout would not do: a Windows
host applies its size and its scale factor as two steps, and in between the desktop
is drawn at twice the pixels and 100%, every window at half its final size. The
opening density counts as applied only when the desktop comes back at the size that
asked for it; otherwise the session is 1x and the client's `hostDisplay` asks again
through Display Control. RDP reports no scale factor back, so the density here is
declared rather than measured.
Each monitor of a layout carries its own scale factor ([MS-RDPEDISP] 2.2.2.2.1,
`DISPLAYCONTROL_MONITOR_LAYOUT`), and a second monitor shown in a browser tab of
its own states the density of the screen that tab's window is on, with its
window's points in pixels at it. It states the first's until the tab reports one,
and again once *All Displays* is no longer chosen; a tab closing or reloading
changes nothing.
The layout always says a monitor is upright: a window taller than it is wide is not a rotated
screen, and a server told otherwise turns the desktop on its side.

A size change that is *real* costs a graphics reset, which the client reports as a
new desktop size. Asking twice for the same
size triggers one change, and a request equal to the current size never triggers
one. A layout is asked for on a bounded schedule rather than once, because a
Windows host discards one sent before the session it is starting has settled and
acknowledges nothing either way — measured against a Windows 11 host, a
byte-identical layout was discarded 400 ms after the server's own Display Control
capabilities PDU and honoured 6.7 s into the same session. The ladder lives in the
engine (`LAYOUT_RETRY_DELAYS`), because a retry needs a clock and a policy and the
client owns neither.

## Virtual displays (alpha)

`Connect::monitors`, from the target's `virtual_displays`, asks the host for that
many monitors of the session's size, the first the primary and the second against
the edge of it that `Connect::placement` names, the session's choice at the
picker: to its right or left, top-aligned, or above or below it, left-aligned.
Positions are relative to the primary's corner, which is always (0, 0)
([MS-RDPBCGR] 2.2.1.3.6.1, [MS-RDPEDISP] 2.2.2.2.1), so a monitor to its left or
above it has negative ones. It goes out twice. At connect, as
a `CS_MONITOR` block in the GCC conference ([MS-RDPBCGR] 2.2.1.3.6, `TS_UD_CS_MONITOR`)
beside the three blocks every connection sends, with the core data's desktop set
to the monitors' union; it is an extended block, so it is sent only to a server whose
X.224 Connection Confirm carried `EXTENDED_CLIENT_DATA_SUPPORTED`, and a server
without the flag is asked for one monitor and the one desktop it always gave. And
in every monitor layout a resizing session sends ([MS-RDPEDISP] 2.2.2.2), one
`DISPLAYCONTROL_MONITOR_LAYOUT` per monitor, the first flagged primary, all at one
scale factor and each at the size `Input::resize` names for it: the same for every
monitor unless a display shown in a tab of its own follows that tab's window, the
second placed against the first's edge as at connect. The server's `MaxNumMonitors` and area ceiling arrive in
its capabilities as before, and every layout is held to both: no more monitors
than the server lays out, and no more than fit its area together, since a layout
past either is one a conforming server ignores in silence, so monitors are dropped
from the right until the rest fit. A first monitor that does not fit alone is not
sent. `Event::ResizeReady` carries the area over every
monitor together.

The host answers with one desktop spanning the monitors: a Demand Active naming
the union at connect, and a `ResetGraphics` naming it, with its monitor
definitions, after each layout. The framebuffer is that span, and the host maps
one surface onto the output per monitor. The definitions say where each monitor
is (`arrangement` in the graphics crate's `proto/gfx.rs`): their edges are
relative to the primary's corner, and the output starts at the leftmost and
topmost of them, so `Event::Resize` carries each monitor's place in the output
and its size, the primary first, wherever the host put them — beside one
another or stacked, in whatever order it lists them. It carries the output as
one monitor when the definitions do not make it up: none or one, an empty one,
two that overlap, or a union that is not the output's size. At connect, where
there are no definitions, the caller reads the equal monitors and the placement
it asked for off the union's size. The client composes and reports the span as
it does any desktop. Which part of it a browser sees, and the pointer offset
into it, is the engine's (`View` in `src/rdp.rs`), and so is *All Displays*, which two
monitors start on and which shows the second column on a socket of its own (`Tab`): nothing of it reaches
the RDP client, which composes the one span either way. In a session that passes
the pipeline the span is composed in the browser instead, which is told the
column to show (`ServerMsg::GraphicsView`) and paints the second column's tab
from the same picture
([Two displays, one picture](architecture.md#rdps-graphics-pipeline-passed-through)).

Measured 2026-10-03 against a Windows 11 host, through `tests/ws_probe.py`: a
kept-size session asked for two 1440×900 monitors opened a 2880×900 desktop,
`CreateSurface 2, MapSurfaceToOutput 2`; a resizing session opened at two
1280×800 and a 1366×768 layout came back as one `ResetGraphics` of 2732×768 over
two monitors; and asked for 1280×800 beside 1024×700, it answered a 2304×800
`ResetGraphics` defining those two monitors, top-aligned. On 2026-10-03 the
second monitor placed left of, above and below the first came back the same way,
at connect and after a layout, and a position sent in the span's coordinates
landed on the monitor it was made on. Alpha: one host, and
nothing of the second display has been held
against Microsoft's own client.

## The clipboard (MS-RDPECLIP)

Three pieces, and the seam between them is a format id and a `Vec<u8>`:

- `proto/cliprdr.rs` is the wire — Monitor Ready, Clipboard Capabilities, Format
  List and its response, Format Data Request and Response — and decides nothing
  about what a clipboard holds.
- `src/rdp_clipboard.rs` is that decision: `CF_UNICODETEXT` alone, the UTF-16
  conversions, and the line endings each direction needs.
- `ClipboardState` in `src/rdp.rs` is the policy, and it is the same policy the
  VNC engines present to the browser.

Both directions are lazy on the wire. A copy announces *which formats* it can be
had in, and the bytes cost a second round trip that happens only when somebody
pastes:

```text
  remote copies  ──  Format List  ──▶  gateway asks at once  ──▶  browser sees it
  browser copies  ──  Format List  ──▶  the remote asks when a person pastes
```

The gateway hides the first half from the browser by asking the moment a format
list arrives, so a remote copy reaches the panel unprompted exactly as it does on
the two VNC engines. The second half it cannot hide, and must not get wrong: every
Format Data Request is answered, including with nothing, because a request left
unanswered is an application on the far end stopped inside its own paste handler —
on Windows a window that has stopped repainting rather than an error anybody sees.
A `CB_RESPONSE_FAIL`, which a Windows peer sends without saying why and often
answers a second ask for, is retried on a bounded ladder
(`CLIPBOARD_READ_RETRY_DELAYS`) rather than forwarded to the browser as empty
text, which would wipe the panel over a transient refusal.

Monitor Ready is what opens the channel, and this end says nothing on it before
answering that with its own capabilities: the two capability sets are what settle
the shape of every format list after them. The browser can be told the session is
up before the channel is, so a copy that arrives early is held — the most recent
one, since each advertisement replaces the last — and sent when the channel opens.

The client does not offer `CB_USE_LONG_FORMAT_NAMES`. Under [MS-RDPECLIP sections
2.2.2.1.1.1 and
2.2.3.1](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPECLIP.pdf),
that selects fixed 36-byte short-name entries in both directions. The parser also
has a compatibility heuristic for a peer that ignores the negotiation: a body
whose length is not divisible by 36 is attempted as variable-length, null-terminated
UTF-16 long-name entries. This is leniency, not an exhaustive identification; an
ambiguous body divisible by 36 is treated as short names. A truncated ID or an
unterminated long name is malformed and ends the session. The peer's capability
set is logged, but it does not change this policy.

Transfers are capped at 512 KiB (`protocol::MAX_CLIPBOARD_BYTES`) and refused
rather than truncated, in both directions — a partial paste cannot be told from a
whole one. A remote copy too large even to receive is reported as its size, which
is what the panel says instead of showing text that is not there.

`tests/rdp_client_probe.rs` drives the whole of it against a real host, including
the part no gateway can do for itself: it takes the remote clipboard over, has the
remote paste it and copy what it pasted, and compares the bytes that come back
with the bytes that went out.

The same round trip, one layer up — through a live gateway, in the messages a
browser actually exchanges — is `tests/ws_probe.py`:

```sh
uv run tests/ws_probe.py --port 52888 --target <rdp target> --user admin     --clipboard "probe — 画面 ☕" --key-delay 6     --chord MetaLeft+KeyR --chord ControlLeft+KeyA --chord ControlLeft+KeyV     --chord ControlLeft+KeyA --chord ControlLeft+KeyC --chord Escape
```

`--clipboard` fetches what the remote holds and then puts that text on it; the
chords open the remote's Run dialog, paste, copy the paste back and close it. What
comes back is a `clipboard` message the gateway pushed unprompted, carrying the
text that went out — the Fetch, the push, the paste and the copy, in one run.

## Camera (MS-RDPECAM)

**Experimental**, for the reason [Camera frames](architecture.md#camera-frames)
gives: no container host can be redirected to, so the channel is unit tested against
the specification's own examples and checked against a real host by the probe, and
the stream itself — an application on the host opening the camera — is checked by
hand.

`camera = true` gives the session a camera to offer, and asks for `drdynvc` when
nothing else did: MS-RDPECAM is two dynamic channels, and the host opens both. The
first, `RDCamera_Device_Enumerator`, a Windows host creates by itself for a client
with the dynamic channel transport. A Windows Server without the Remote Desktop
Session Host role never creates it, and Microsoft's own client gets no camera
against such a host either. On it the client asks for version 2, the host answers
with the highest it speaks that is not higher, and nothing more is said until the
browser enables its camera. That plug is what sends the Device Added Notification —
a display name, `Remotex Camera`, and the name of a device channel — and the host
then opens that channel by name. A version the client does not speak ends the
protocol on that channel, as MS-RDPECAM 3.2.5.2 requires.

`proto/rdpecam.rs` owns the protocol state. The state rules come from
[MS-RDPECAM section
3.1.1](https://github.com/andrewtheguy/ms-rdp-specs/blob/main/MS-RDPECAM.pdf):

| State | Requests and transitions |
| --- | --- |
| Deactivated | Only Activate Device is accepted. Other requests receive `NotInitialized`. |
| Activated | Each Activate increments a device-wide count. Start Streams enters Streaming; Deactivate decrements the count and returns to Deactivated only when it reaches zero. A Sample Request before Start Streams receives `InvalidRequest`. |
| Streaming | Sample Requests are accepted. Stop Streams returns to Activated. Deactivate stops the stream and decrements the activation count. |

Malformed and unknown requests receive `InvalidMessage`; failures of a Sample
Request use the Sample Error Response required for that request. Messages that are
responses rather than requests are discarded without a reply, avoiding an error
exchange with no terminating message. Versions 1 and 2 are implemented. Version 1
has no property requests; in version 2 this device reports no properties.

The implementation keeps one device state across every dynamic-channel instance
opened with the advertised device-channel name. Each request is answered on the
instance that carried it. Activations from all instances contribute to the one
count; closing an instance removes the activations it contributed and stops a stream
owned by that instance. Samples are returned on the instance that started the stream
or most recently requested a sample.

That multi-instance policy is based on a dated interoperability observation, not a
requirement that every host follow the same sequence. On 2026-09-13, the probed
Windows Enterprise host kept several instances open: Device Initialization used the
first, Device Control Initialization opened another before the first closed, and the
Camera app started a stream on one instance but requested samples on an unactivated
second instance. The app then tore the initial stream down, reopened the device and
ran a second stream. On that host without a GPU, requests also arrived more slowly
than the announced frame rate, exercising the queue's drop-to-keyframe policy.

The device is one color stream in one media type: H.264 at the geometry and rate the
browser's encoder announced, decoded on the host. The host chooses from that list of
one, so a Start Streams naming any other type is `InvalidMediaType`. The stream
description says the stream cannot be shared, as FreeRDP's client tells a Windows
host, because one encoder is behind it. Samples are one Annex B access unit each and
are never looked inside.

Samples are metered by the host: each Sample Request is owed one Sample Response, a
sample that arrives with nothing owed waits in a queue of eight, and past that the
queue is dropped whole and only a keyframe is taken until one comes — H.264 cannot
resume mid-GOP — with the browser asked for one once per gap. A new stream opens the
same way, on a keyframe. Unplugging answers every Sample Request still owed with an
error before the Device Removed Notification, so no request is left without its
response. A sample is far longer than one dynamic channel PDU, so it goes as a Data
First announcing its length and the Data PDUs after it (`dvc::pieces`, MS-RDPEDYC
2.2.3.1); every other message this client sends fits one.

The session thread runs all of it beside everything else on the connection, fed from
`rdp_client/camera.rs`: two queues, because a plug or an unplug must never be lost and
a late sample is worthless, so the device's commands queue without limit and samples
wait in a bounded queue that drops to a keyframe when it fills. The host's decisions
go to a `CameraSink` on that thread. `src/rdp_camera.rs` is the gateway's adapter: the
sink becomes `CameraBridge` signals, and the bridge's control becomes the session's
feed.

`tests/rdp_client_probe.rs` plugs a camera into a real host and, under
`REMOTEX_UAT_CAMERA=1`, asserts that the host agreed version 2 and opened the
announced device's channel; `RUST_LOG=remotex=debug` shows the host's device queries
and this end's answers.

## Microphone (MS-RDPEAI)

**Experimental.** `microphone = true` tells the logon that the client captures
audio, asks for `drdynvc` when nothing else did, and accepts the one dynamic
channel MS-RDPEAI uses, `AUDIO_INPUT`. A Windows host opens that channel only
once an application starts recording. The browser's microphone is enabled
separately by opening `/ws/mic`; closing that socket or ending the engine stops
the feed, and a new session starts with it off.

The host speaks first at every step. It sends its version and this end answers at
no higher than version 2. It lists the PCM formats it can record, and this end
offers exactly one 16-bit format it can produce: mono where possible, at 16 kHz
where offered, otherwise the lowest rate above 16 kHz or the highest below. Only
one- or two-channel rates from 8 to 48 kHz that form exact 20 ms groups from
48 kHz are candidates. When the host sends Open it names that format and the
number of frames each packet must contain; this end confirms it, replies success,
and gathers decoded PCM into packets of exactly that size. Each packet travels
as an Incoming Data PDU followed by a Data PDU, split across dynamic-channel
pieces where necessary.

The browser encodes mono Opus in voice mode at 16 kbit/s in 60 ms packets. The
gateway's `MicBridge` (`src/mic.rs`) decodes them at 48 kHz, emits 20 ms groups,
resamples and duplicates channels as the chosen host format requires, and hands
the PCM to `rdp_client/microphone.rs`. That session-side queue holds sixteen
groups and drops the oldest when full, because stale microphone audio is worse
than a gap. A host channel close or socket close flushes queued and partial
audio, so a later Open begins with a fresh stream. The protocol state machine is
`rdp_client/proto/rdpeai.rs`; `src/rdp_mic.rs` is its bridge adapter.

`tests/rdp_client_probe.rs` drives a real host's Recording panel in
`a_real_host_records_the_microphone`. With `REMOTEX_UAT_MICROPHONE=1` it asserts
version negotiation, then feeds a tone for the host's level meter while the
device is open. Like the camera probe it is ignored by default and does not
automatically inspect what the remote application heard.

## Sound (MS-RDPEA)

A session started with sound asks for two more static channels, `rdpsnd` and `rdpdr`, and leaves
`INFO_NOAUDIOPLAYBACK` out of the Client Info PDU. One started without it sets the
flag and names neither channel, so the host's own audio settings are not touched
and the session has no audio device to redirect. That is the whole of the choice:
redirect, or leave alone.

`rdpdr` is device redirection with no devices in it (`proto/rdpdr.rs`). A Windows
host redirects no sound to a client that did not name it — measured here as a
session numbered an `rdpsnd` channel the host never spoke on, and recorded in
FreeRDP as "rdpsnd requires rdpdr to be registered" — so the client walks the
channel's opening handshake, announce, name, capabilities and an empty device
list, sent again when the host says the user is logged on, and there is nothing
after it: no I/O request could arrive.

A current Windows host carries the conversation on a dynamic channel,
`AUDIO_PLAYBACK_DVC`, rather than the static one, and the client accepts it when
audio was asked for. `proto/rdpsnd.rs` is the conversation, fed whole PDUs from
whichever transport they arrive on. The host speaks first at every step: it sends
its format list, this end answers with 44.1 kHz 16-bit stereo PCM alone and asks
for high quality; the host sends a training probe, echoed back; then Wave2
buffers, each confirmed by block number and each handed to the engine's
`AudioSink` — from there to the same `AudioBridge` every other engine feeds, on the
client's thread, never through the event queue. Each confirm goes out once its
buffer is with the sink, its timestamp the host's plus the milliseconds that took,
as MS-RDPEA 3.2.5.2.1.6 has it. Close keeps the format: a Windows host sends its
format list once per channel and a Close after every stream.

The measured part: a Windows host negotiates nothing until something plays. A
session opened onto a quiet desktop shows the dynamic channel opened and not a byte
on it, for as long as the desktop stays quiet, and a probe that asserts on the
format list fails there through no fault in the client. `tests/rdp_client_probe.rs`
asserts the negotiation only under `REMOTEX_UAT_AUDIO=1`, which is the run's word
that a sound is playing on the remote; without it the counts are printed.

## What a host will not tell you

Four of the decisions above are measured, and they share a shape: the host does
not refuse, it simply stops answering. `CHANNEL_FLAG_SHOW_PROTOCOL` on the wrong
channel, a monitor layout sent too early, a Format Data Request left unanswered,
and `rdpsnd` named without `rdpdr` all look from here exactly like a working
session in which nothing happens. That is why the probes assert on what the *host* does — that it opens a
channel, takes a format list, resets the graphics to the size that was asked for —
rather than on what this client sent.
