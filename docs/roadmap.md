# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### What the RDP client does not carry yet

The client carries the desktop, the pointer, keyboard, mouse, resize, the
clipboard, sound and the browser's camera. Touch was carried by the engine before it, from FreeRDP's
`rdpei` plugin, and is not carried *here*: `proto` is the gateway's own now, so it
is a channel to write rather than a dependency to configure, and it is refused
where it would otherwise build a control with nothing behind it, by having no key
at all — whether touch exists is the host's answer, and this client never asks.

Everything on either side of the channel is already written and shipped: the
browser's touch passthrough layer (`touchPassthrough.ts`), `ServerMsg::TouchReady`
and `ClientMsg::Touch` are protocol-agnostic. Nothing below the wire needs
designing for it.

The clipboard and sound were the other two of these and are done. MS-RDPECLIP is
`rdp_client/proto/cliprdr.rs`, the channel plumbing the static channels needed is
in `connect.rs` and `proto/channel.rs`, and the engine's half — advertise on
Ready, ask the moment the remote's format list arrives, answer every paste request
including with nothing, retry a `CB_RESPONSE_FAIL` on a bounded ladder — is
`ClipboardState` in `src/rdp.rs`. MS-RDPEA is `rdp_client/proto/rdpsnd.rs`, with
the device-redirection handshake a Windows host requires beside it in
`proto/rdpdr.rs`, and its buffers reach the same `AudioBridge` every other engine
feeds. What each took is recorded in
[The RDP client](rdp-client.md#the-clipboard-ms-rdpeclip) rather than here.

EGFX is in, as [The RDP client](rdp-client.md#the-graphics-pipeline-ms-rdpegfx)
describes; what is left of it
beyond the decoders is under
[Source payloads](#source-payloads-the-gateway-decodes-instead-of-forwarding)
rather than here, because that payoff is a transcode removed, not a control
restored.

#### Touch (MS-RDPEI)

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

#### Licensing on a Remote Desktop Session Host

The licensing step accepts exactly one PDU: an `ERROR_ALERT` carrying
`STATUS_VALID_CLIENT`, which is what every Windows host this client has been
pointed at sends. Anything else is refused by name in
`proto/license.rs`, and the connection ends before `DemandActive`.

A Session Host with the Remote Desktop Session Host role and per-device CAL
licensing does not send that. It opens a real exchange: `LICENSE_REQUEST`, the
client's new or upgrade licence request, the platform challenge and its response,
and the issued licence. This client would disconnect at the first of those. The
exchange is written in FreeRDP 3.30.0's `libfreerdp/core/license.c`, the version
`main` builds against, and a copy is in the local reference checkout under
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

### Raising quality above the dial

The stream's congestion loop can notice a backlog but never find headroom: it walks
the dial down when the outbound queue says the link is behind and back up to the
configured quality when it is not, and never past it. That is sound — exceeding the
operator's setting was never a goal — but it means a link with room to spare is never
discovered.

The existing `paintAck` feedback supplies the receiver's view of *queueing*: it
reports when a batch finished the client's ordered decode-and-draw pass, and the
adaptive loop subtracts the link's recent floor to detect falling behind. An empty
paint window still says only that the configured quality fits; it does not measure
how much more would fit. Going further therefore wants richer receiver feedback —
delivered bytes and arrival timing added to that contract, for example — plus an
explicit upper-bound policy. It is a separate feature whose value should be argued
from `video`'s measurements rather than assumed.

### The target's quality keys on wlshare's own stream

A browser that decodes 4:4:4 watching wlshare is sent wlshare's VP9 as it comes
([wlshare's stream, passed through](architecture.md#wlshares-stream-passed-through)),
and that stream is coded at wlshare's `vp9_quality` and `vp9_quality_min`: the
target's `video_quality`, `render_adaptive` and `render_adaptive_min` do not reach
it. The plan is one message of wlshare's VP9 encoding, client to server, naming the
dial's ceiling and floor, which the gateway sends from the target's keys when it
lists the encoding, so that the keys mean on a passed stream what they mean on one
coded here.

### How a passed-through stream is doing

A passed stream is coded and walked in wlshare, so the gateway knows each frame's
size and whether it is a keyframe and nothing about the quality it went out at: the
encode totals of such a session count its units, keyframes and bytes, and report no
round coarsened and a lowest quality of 100 whatever wlshare did. Reporting it wants wlshare to say what it coded each
frame at, which its desktop clients want for their own throughput readout too. It
is one change to the wire, to be made with the desktop clients' throughput support
rather than ahead of it.

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
([Apple's HEVC, passed through](architecture.md#apples-hevc-passed-through)) is
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
target without `hevc_passthrough`, whose VP9 the adaptive walk lowers; a passed
session is not switched to VP9 for it, though the gaps' switch would make that
possible. A brief stall stays what it is: the Mac's units predict from every one
before, so none can be dropped alone, and a full queue drops to the next IDR and
asks the Mac for one, which brings the picture back as one fresh frame rather than
a replay of the backlog.

### Source payloads the gateway decodes instead of forwarding

Three places where a remote could hand this gateway something closer to what the
browser needs, and it decodes or re-encodes instead. Each is real work with a real
payoff, and none of them is near-term — they are here so that "why not this one"
has an answer rather than being rediscovered.

- **RDP EGFX.** The RDP client carries the pipeline again — the channel, ZGFX,
  the surface compositor with its caches and copies, the frame marks, and the
  decoders a current Windows host draws with: ClearCodec with NSCodec inside it,
  RemoteFX Progressive, planar, uncompressed, and the H.264 it hands video to
  ([The RDP client](rdp-client.md#the-graphics-pipeline-ms-rdpegfx)
  describes each). Beyond the decoders lies AVC420
  pass-through — handing the host's H.264 to the browser
  rather than decoding it and encoding VP9. What makes that large is that it is a
  second graphics pipeline beside the one every engine shares, not an option on
  it, and that the host masks each picture by rectangles and mixes it with the
  other codecs on one surface, which a passed stream cannot show.
- **Tight/JPEG/H.264 VNC decode or pass-through.** Generic `vnc` advertises only
  the lossless standard encodings on purpose: Tight and TightPNG are vendor
  encodings, JPEG and H.264 are lossy, and advertising an encoding is a promise to
  decode it. Tight-family decoding, and handing a lossy source payload to the
  browser untouched, would remove upstream bytes and a transcode — for a target
  where the operator has already accepted lossy, the transcode is pure loss. The
  cost is a decoder this repo would then own.

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

### `THINCLIENT` in the graphics capability advertise

`caps_advertise` in `proto/gfx.rs` sends every version from 8 to 10.4, and 10.7, with the
small cache and leaves `RDPGFX_CAPS_FLAG_THINCLIENT` unset. A current Windows host, the only host this client targets, ignores the
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
shape as Windows Remote Desktop. A reconnect, a target switch and a browser
takeover all reclaim the slot in silence: they are the same session coming back,
whatever else has changed.
