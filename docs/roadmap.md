# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*,
or under consideration, belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

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

### Two virtual displays past alpha

`virtual_displays = 2` on an `rdp` target is alpha
([Windows](guide.md#windows-rdp)). What it lacks:

- **A tab left behind.** When the session's page goes — closed, reloading, or
  its socket dropped — the tab showing the second display carries on with no
  word of it: its input still reaches the remote through the reattach grace, and
  only the engine ending takes the display away. The tab's display socket already
  carries one thing that is the session's rather than the display's, the
  clipboard ([Clipboard](architecture.md#clipboard)). The plan is for it to carry
  whether the session's page is attached too, and for the tab to say so over its
  desktop until the page is back.

Fixing that does not take it out of alpha by itself. It also needs more testing
than it has had before it is offered as anything else.

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

### Verifying an RDP host's certificate

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

### Licensing on a Remote Desktop Session Host

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

### The pipeline compressed again for the browser

Taken up if a pipeline's bytes to the browser come to matter, a Lossless host
over anything short of a LAN first. A passed pipeline reaches the browser in the
host's own bulk compression
([RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)),
whose history is 2.5 MB. Unwrapped and compressed again with zstd at level 1,
with a 32 MB window and long-distance matching, captures of a Windows 11 host
under the Lossless policy at 3456×1802 (2026-10-15) came out smaller than the
host's compression on every one: by 21% on a still desktop's opening paint, 25%
on a desktop busy about half the time, 19% with a video in a window, and 66% on
scrolling, where a scrolled page is mostly rows the host sent a frame or two
earlier, megabytes back. Compressing ran at 280 to 580 MB/s on one core,
against at most 25 MB/s from the host. The cost is a C library in the gateway,
which the packaging rules and the fallback build have to take, a zstd decoder in
the page's WebAssembly module, and 32 MB of window in the page per pipeline; a
64 MB window gained only on scrolling, from 6.15 to 7.36 times smaller than
unwrapped. The record and its
acknowledgement would stay as they are: one stream per pipeline, started at
`graphicsStart` and flushed at each record, in place of the host's.

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

The rest is waiting for it. A host that opens the channel is to be forwarded as
`ServerMsg::TouchReady` and re-sent to each client that attaches; the browser offers the passthrough toggle
only after that, and `rdp.rs` drops a `ClientMsg::Touch` today because no engine
can report one. Held contacts must be released when a client goes away, or the
remote keeps fingers down that no longer exist. A Windows host opens MS-RDPEI and
xrdp never does, which is the reason this stays an always-offered capability
rather than a key.

## Not planned

### Decoding an RDP host's H.264 at the gateway

The gateway composes no graphics pipeline: every pipeline is passed to the
page, and a High quality session refuses H.264 with `AVC_DISABLED`
([RDP's graphics pipeline, passed through](architecture.md#rdps-graphics-pipeline-passed-through)).
Decoding H.264 here only to send it as tiles would be a second lossy pass, of a
stream the page already decodes.

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

The gateway does decode zlib, since a Standard Mac's Full quality is RFB's zlib
asked for ahead of ZRLE as Apple's viewer asks
([Full quality's zlib](apple-vnc-889.md#full-qualitys-zlib)); the decoder sits
beside ZRLE's and Raw's (`vnc_encodings`, `Payload::Zlib`). Listing it on a plain
target would cost only the entry and a test, and is still not planned: every
VNC server in use has ZRLE and prefers it when a client lists it first, so the
listing would be read by no server this project meets, and would be a path the
end-to-end tests against a real server never exercise.

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
