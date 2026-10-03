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
chain is *not* verified; what is verified is the handshake signature, which is
what makes the chain's absence defensible — CredSSP binds the server's TLS public
key into the credential exchange, so an interceptor without the private key cannot
complete the handshake it would need to replay the credentials. See
`proto/tls.rs`, which spells out the attack that argument rests on.

The socket comes from `engine::tcp_connect`, so an RDP host that goes silent is
noticed on the same keepalive schedule as every other engine's.

`ConnectionType` is declared a LAN rather than probed, because a server's own
estimate of the hop between it and a gateway beside it throttled updates badly,
and no multitransport is offered. The client requests no MCS message channel and
does not advertise network-characteristics detection, so a conforming host has no
channel on which to send auto-detect PDUs. Whether a current Windows host sends
any despite that remains a live-host question in
[`rdp-spec-audit.md`](rdp-spec-audit.md#to-settle-against-a-host).

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

Those flags are the measured part. Each channel's chunks wear what its own
`CS_NET` options asked for, and `CHANNEL_FLAG_SHOW_PROTOCOL` is the one that
matters: a Windows host reads **nothing at all** off a chunk that disagrees with
the channel it arrived on, and reports nothing about having ignored it. The
clipboard needs the flag on every chunk and the dynamic channel must not carry it;
set it on both and Display Control never opens, set it on neither and the
clipboard never answers. `channel::SHOW_PROTOCOL` records both failures.

A channel belongs to the connection, not to the share, and nothing on one can be
asked for again — there is no repaint for a clipboard. So nothing on a channel is
dropped while the share is busy with itself in the capability exchange.
`connect::activate`
hands back what arrived on a channel and the session acts on it once the share is
live. A server opens the clipboard as soon as the channel is up, which can
be mid-finalization, and a Monitor Ready read past there is a session whose
clipboard never starts.

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
EndFrame is acknowledged (`queueDepth` unavailable), which a Windows host requires
or it throttles and then stops; and each surfaces to the engine as `Event::Frame`
after the paints it covers, which is the frame boundary the engine's flush was
built to guess at. A monitor layout is answered by ResetGraphics, which resizes the
framebuffer and surfaces as `Event::Resize`, the channels untouched.

The compositor decodes everything a current Windows host sends, measured against
one with `tests/rdp_client_probe.rs`, which prints the host's codec and command
tally. Uncompressed rectangles and
the planar codec — the same `proto/planar.rs` a bitmap update uses, with the rows
the right way up. ClearCodec (`proto/clear.rs`), the desktop's primary codec here:
its residual, band and glyph layers with their caches, and all three subcodecs,
raw, RLEX and NSCodec (`proto/nsc.rs`), the last carrying most pictures and
anti-aliased text. A ClearCodec rectangle is decoded onto the surface over what it
already holds, as FreeRDP decodes it: the host leaves pixels no layer paints to the
surface underneath, stores a glyph as the surface looks once its layers are done,
and places band columns that can land beside the rectangle — so every one of those
is invalidated with it. RemoteFX Progressive (`proto/progressive.rs`), which arrives on
WireToSurface2 with its own rectangles: the decoder keeps every tile of every
surface between PDUs — its coefficients and their signs — so an upgrade pass adds
bits to what a first pass left, and only the reduce-extrapolate wavelet and RLGR1 a
modern host uses are implemented; a region asking for the classic RemoteFX wavelet
is refused by name. A region is painted from every tile decoded since its graphics
frame began, clipped to its rectangles, not from its own tiles alone: [MS-RDPEGFX]
2.2.4.2.1.5 lets a region's rectangles be covered by tiles an earlier region of the
frame carried, in an earlier PDU even, and a Windows host relies on it when a
minimized or dragged window uncovers the desktop. The copies and caches that make a desktop cheap are acted on:
SurfaceToSurface reads its source whole before writing so a scroll over itself does
not smear, SurfaceToCache and CacheToSurface keep rectangles by slot, SolidFill
clips to the surface. A rectangle that will not decode is left unpainted with a
warning and the session runs on, since the host draws it again; a PDU whose framing
is wrong ends the session, as any malformed PDU does. The channel says which codecs
and commands it carried when it ends, at `info`.

### The pipeline, passed on

**Beta**, for the reason
[RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)
gives. `Connect::pass_graphics` — a session started with the passthrough — has the session hand the
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

With `Connect::h264` the session tells the host it takes H.264, which nothing
here decodes: the advertise is every set from version 8 to 10.4, and 10.7, none
with `AVC_DISABLED`. 10.5 and 10.6 are left out and 10.7 carries
`SCALEDMAP_DISABLE`, since from 10.5 a host may map a surface through the scaled
mappings, which this client ignores, and only 10.7 has a flag to say so. Version
10.1's capability data is sixteen reserved bytes where every other set's is four
bytes of flags, in the advertise and in a confirmation of it. `AVC_THINCLIENT`,
which asks for the whole desktop in AVC444, is not set. Measured against a
Windows 11 host without a GPU: it confirms 10.7 and goes on drawing with
ClearCodec, Progressive and the caches, and draws what moves like video with
AVC420 in the same frames — a Main-profile stream for the surface, about fifteen
access units a second, Annex B with the parameter sets at each keyframe, each
unit behind a metablock naming the rectangles of the picture to show
(`proto/avc.rs`). With `AVC_THINCLIENT` the same host drew the whole desktop
with AVC444v2 and nothing else, every unit a luma view alone.

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
many monitors of the session's size in a row, the primary at the left and each
next one starting at the last one's right edge. It goes out twice. At connect, as
a `CS_MONITOR` block in the GCC conference ([MS-RDPBCGR] 2.2.1.3.6, `TS_UD_CS_MONITOR`)
beside the three blocks every connection sends, with the core data's desktop set
to the row's union; it is an extended block, so it is sent only to a server whose
X.224 Connection Confirm carried `EXTENDED_CLIENT_DATA_SUPPORTED`, and a server
without the flag is asked for one monitor and the one desktop it always gave. And
in every monitor layout a resizing session sends ([MS-RDPEDISP] 2.2.2.2), one
`DISPLAYCONTROL_MONITOR_LAYOUT` per monitor, the first flagged primary, all at one
scale factor and each at the size `Input::resize` names for it: the same for every
monitor unless a display shown in a tab of its own follows that tab's window, top
at 0 and each starting at the last one's right edge. The server's `MaxNumMonitors` and area ceiling arrive in
its capabilities as before, and every layout is held to both: no more monitors
than the server lays out, and no more than fit its area together, since a layout
past either is one a conforming server ignores in silence, so monitors are dropped
from the right until the rest fit. A first monitor that does not fit alone is not
sent. `Event::ResizeReady` carries the area over every
monitor together.

The host answers with one desktop spanning the row: a Demand Active naming the
union at connect, and a `ResetGraphics` naming it, with its monitor definitions,
after each layout. The framebuffer is that span, and the host maps one surface
onto the output per monitor. The definitions are read against the row asked for
(`row` in the graphics crate's `proto/gfx.rs`): `Event::Resize` carries each
monitor's size when they are a row like the one asked for — top-aligned, each
starting where the last ended, the row as wide as the output and as tall as its
tallest — and the output's one size when the host laid the session out any other
way, stacked, offset or out of order, so a caller that shows one column never
cuts a layout it does not know the shape of. At connect the caller reads the
equal row off the size. The client composes and reports the span as
it does any desktop. Which part of it a browser sees, and the pointer offset
into it, is the engine's (`View` in `src/rdp.rs`), and so is *All Displays*, which
shows the second column on a socket of its own (`Tab`): nothing of it reaches
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
`ResetGraphics` defining those two monitors, top-aligned. Alpha: one host, and
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

Short format names are what is spoken. `CB_USE_LONG_FORMAT_NAMES` is not offered,
which by MS-RDPECLIP 2.2.2.1.1.1 settles the form of every list in either direction;
a server that ignores that is still read, because a list that is not a whole
number of short entries can only be long ones. Nothing branches on what the
server's own capabilities say: they are read for the log, where a server that
wants something else is a line rather than a mystery.

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

`proto/rdpecam.rs` is the whole conversation, and a pure one: every host message and
every browser action goes in, and what to send and what it meant comes out. The
device keeps the three states of MS-RDPECAM 3.1.1 — Deactivated, Activated with a
count of Activate requests not yet matched by a Deactivate, and Streaming — and
answers every request the state does not take with the error the specification
names: `NotInitialized` before activation, `InvalidRequest` for a Sample Request with
no stream running, `InvalidMessage` for anything malformed or of an unknown kind, and
a Sample Request's failures in the Sample Error Response it is always owed. A response
from the host is never answered. Version 1 is spoken too, because a client must
speak every version below the one it claims: under it the property requests do not
exist, and under version 2 this device has no properties to report.

The measured part: a host opens the device channel more than once, under the one
name, and keeps every instance open. Against a Windows Enterprise host the Device
Initialization sequence runs on the first instance, and before that instance is
deactivated or closed a second is opened for the Device Control Initialization
sequence. A client that took the second opening for a replacement would leave the
first unanswered, and a stream started on it would never start. The device is still
one device, so its state is kept once, as MS-RDPECAM 3.1.1 describes it and as
FreeRDP's client keeps it: every request is answered on the instance it came in on,
the Activate requests of every instance nest into one count, a Deactivate on any of
them ends a stream, and samples go out on the instance that started the stream or last
asked for one. An instance that closes gives back the activations it made, and the
stream if the stream was its.

The Windows Camera app leans on exactly that. It starts the stream on one instance and
asks for samples on another it opens straight after, which it never activates: a
client keeping a state per instance would answer those requests `NotInitialized`, and
the stream would start and never carry a frame. The same app tears its first stream
down a few seconds in — Deactivate, and every instance closed — then opens the device
again and starts a second one that runs until the app closes. It also asks for samples
more slowly than the rate the device announced, on a host without a GPU, so the queue
fills and a second's worth of stream is dropped to the next keyframe at a time.

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
