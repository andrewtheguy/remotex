# The RDP client, written here

The gateway speaks RDP with its own client, `src/rdp_client/`, down to the wire
format. `rdp_client/proto/` encodes and decodes every PDU against [MS-RDPBCGR]
and its extensions; the modules above it own one thread per session, a complete
framebuffer painted from those decoders, and an event per damaged rectangle. The
engine that consumes those events — damage into tiles, `ClientMsg` into scancodes
— is `src/rdp.rs`, and the boundary between the two is the point of this document:
everything below it is protocol, everything above it is this gateway's.

The only thing under that boundary not written here is the CredSSP exchange
itself (`sspi`), because NLA is not optional on a current Windows host and NTLM is
the one mechanism a user name and a password can drive.

The target is a current Windows host and only that. Where [MS-RDPBCGR] is silent
or wrong about what such a host really does — and it is, in places, about both —
the reference is FreeRDP's `libfreerdp/core`, and the arbiter is a real host:
`tests/rdp_proto_probe.rs` and `tests/rdp_client_probe.rs` drive one named in the
operator's `tmp/test_uat.toml`. Several of the decisions below are marked as
measured rather than read, and each of those is a place where the specification
alone would have produced a client that connects and then quietly does nothing.

[MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c

## What it carries

The desktop, the pointer, keyboard, mouse, resize and the clipboard. No sound and
no touch: `audio = true` is refused on an rdp target at config parse, and touch is
announced only by a host that opens MS-RDPEI, which this client never asks for.
What each of the two would take is in [`roadmap.md`](roadmap.md).

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
and no multitransport is offered. The auto-detect PDUs a Windows host sends anyway
go unanswered, which it treats as a link it cannot measure.

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

Two are asked for, each by a key: `drdynvc` for `resize = true`, which is the
transport Display Control rides on, and `cliprdr` for `clipboard = true`. A
session that wants neither asks for no channel at all.

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
dropped in the two windows where the share is busy with itself, the capability
exchange and the wait for a Demand Active that precedes it. `connect::activate`
hands back what arrived on a channel and the session acts on it once the share is
live; the clipboard alone is answered in place, because a Format Data Request is a
remote application stopped inside its own paste and it has no idea a desktop is
being rebuilt. A server opens the clipboard as soon as the channel is up, which can
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
sends H.264. `THINCLIENT` is deliberately not set. The host confirms one set and
then draws.

Every PDU on the channel is wrapped in RDP 8 bulk compression (`proto/zgfx.rs`), a
port of FreeRDP's decoder: a fixed Huffman table over literals, matches into a
2.5 MB history shared by every PDU for the channel's life, and runs of unencoded
bytes. One wrapper may hold several RDPGFX PDUs, each eight-byte-headed with its
own length, and `proto/gfx.rs` decodes them in order. The client's own PDUs go back
uncompressed inside the same wrapper, which the specification permits.

The host does not paint the desktop; it paints *surfaces* it creates and sizes,
maps them onto the output at an origin, and brackets drawing in StartFrame and
EndFrame. `rdp_client/gfx.rs` keeps each surface's pixels and the rectangles drawn
into since the last frame, and at the EndFrame copies those rectangles of every
mapped surface into the framebuffer — the shape of FreeRDP's `gdi/gfx.c`. Each
EndFrame is acknowledged (`queueDepth` unavailable), which a Windows host requires
or it throttles and then stops; and each surfaces to the engine as `Event::Frame`
after the paints it covers, which is the frame boundary the engine's flush was
built to guess at. A monitor layout is answered by ResetGraphics, which resizes the
framebuffer and surfaces as `Event::Resize` — no reactivation, and the channels
untouched.

The decoders are landing in stages ([the plan](rdp-egfx-plan.md)). Today the
compositor paints uncompressed rectangles and the planar codec — the same
`proto/planar.rs` a bitmap update uses, with the rows the right way up — and counts
every other codec and every copy or cache command the host sends, saying each once
in the log and summarising all of them when the channel ends. A codec with no
decoder leaves its rectangle unpainted and the session running; a PDU that does not
decode ends the session, as any malformed PDU does. A host that draws entirely in
RemoteFX Progressive therefore paints nothing yet, and `egfx = false` is the way
back to a picture until that decoder lands.

### Bitmap updates

With `egfx = false` the pipeline is not advertised, so the server draws with bitmap
updates, decoded by the planar bitmap codec (`proto/planar.rs`). The client
announces no drawing orders either, so the path is bitmaps throughout. That is what
makes a resize a full reactivation, after which a Windows host re-renders the
desktop sharp. Every server that is not Windows takes this path whatever the key.

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

Under a plan that takes copies, each flush first searches the damage for regions
the client already holds elsewhere on its canvas (`src/copies.rs`, guacamole-
server's cell-hash search over this gateway's shadow): a scroll goes out as a few
`COPY` records instead of image bytes, and the tile pass carries only what the
copies did not — including repainting anything a copy got wrong, which is what
makes a wrong copy waste rather than corruption.

## The pointer

The pointer is not part of the framebuffer. RDP servers send the cursor's shape
rather than drawing it, and each shape goes to the browser as `cursor`, which
draws it on its own hardware pointer. A mouse move therefore costs the session
nothing at all, where compositing the pointer into the framebuffer put every one
of them through damage, the flush interval, an encode, the socket, a decode and a
paint. The server's own pointer *positions* are dropped: the browser's pointer is
already where the mouse is, and nothing here can move a hardware pointer.

## Resize and density

With `resize = true`, the Display Control Virtual Channel applies explicit
desktop-size requests, and also matches the client's display density: a monitor
layout carries `DesktopScaleFactor` beside the geometry, so a Retina client gets
twice the pixels with the host's UI drawn at 200% rather than the same UI
stretched. The opening handshake is always 1x; the client applies its screen
density after `connected`, so a Retina client costs a graphics reset on the default
pipeline path and a reactivation on the bitmap path. RDP reports no scale factor
back, so the density here is declared rather than measured. The layout
always says a monitor is upright: a window taller than it is wide is not a rotated
screen, and a server told otherwise turns the desktop on its side.

A size change that is *real* costs a graphics reset under the pipeline and a full
Deactivation-Reactivation Sequence without it; the client runs either and reports a
new desktop size. Asking twice for the same
size triggers one change, and a request equal to the current size never triggers
one. A layout is asked for on a bounded schedule rather than once, because a
Windows host discards one sent before the session it is starting has settled and
acknowledges nothing either way — measured against a Windows 11 host, a
byte-identical layout was discarded 400 ms after the server's own Display Control
capabilities PDU and honoured 6.7 s into the same session. The ladder lives in the
engine (`LAYOUT_RETRY_DELAYS`), because a retry needs a clock and a policy and the
client owns neither.

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
which by MS-RDPECLIP 3.1.5.2 settles the form of every list in either direction;
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

## What a host will not tell you

Three of the decisions above are measured, and they share a shape: the host does
not refuse, it simply stops answering. `CHANNEL_FLAG_SHOW_PROTOCOL` on the wrong
channel, a monitor layout sent too early, and a Format Data Request left
unanswered all look from here exactly like a working session in which nothing
happens. That is why the probes assert on what the *host* does — that it opens a
channel, takes a format list, rebuilds a desktop at the size that was asked for —
rather than on what this client sent.
