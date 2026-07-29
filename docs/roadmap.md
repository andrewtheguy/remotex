# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

## Planned

### Smoother video from an RDP guest

For both clients: the browser and the macOS viewer. The case is a **browser on the
guest playing a video in a window** — ordinary screen content, no cooperating
application. That rules out the redirection routes before anyone reaches for them:
multimedia redirection needs a player that hands its stream to the RDP stack, and a
browser compositing video into its own window is not that. The pixels arrive as
graphics updates like everything else, which is why this is a question about our own
path rather than about a channel we have not turned on.

That path is worst in three places at once. Every frame changes every pixel of the
video's rectangle, so the shadow copy has nothing to trim out of it; `Rect::bands()`
splits the rectangle into 64-row strips — seventeen of them if it fills a 1080p
screen — and `send_tiles` WebP-encodes each strip **synchronously on the RDP read
loop**. At that encoder's measured cost (`WEBP_LOSSLESS_METHOD`: about 60µs fixed
plus ~17ns a pixel) a full 1080p frame is 30–40 ms of encoding, so it caps out near
25–30 fps before the network is involved, with input waiting behind it; a windowed
video pays the same rate per pixel.

Then each client decodes those strips one at a time, and in both cases on purpose:
the SPA chains every message through one promise so draws land in arrival order, and
the viewer's `TileDecoder` is an actor for the same reason. Tiles overwrite their
rectangles with no delta state, so arrival order is correctness rather than
tidiness — which is why the fix on the client side is fewer or cheaper payloads per
frame, not more concurrency.

Three levers, cheapest first. The first two are gateway-side, so both clients get
them at once, and they are worth measuring before the third is committed to:

- **Lossy WebP in the gateway.** `encode_webp` is lossless with no choice today,
  while the macOS agent classifies per tile. Video is exactly the content the lossy
  branch exists for, and both configs are already measured.
- **Encoding off the read loop**, which is
  [Encoder parallelism](#encoder-parallelism) and its ordering hazard. A video region
  makes that hazard the *normal* case rather than an edge one — the same pixels are
  dirty every frame — so a pool needs ordering and not merely threads.
- **H.264 passed straight through.** A Windows server already encodes the screen as
  H.264 over the graphics pipeline, and `ironrdp-egfx` implements that channel with
  `AVC420`/`AVC444` and a `decode` module of *traits*: it delegates decoding rather
  than doing it. A passthrough implementation of that trait would carry the server's
  own bitstream to the clients, and the gateway would stop decoding and re-encoding
  video at all. The `TILE` record's `format` byte is the seam it arrives through,
  which is why the byte was kept when the codec collapsed to one value
  (`src/protocol.rs`). Each client already has somewhere to put the result: the
  browser has WebCodecs `VideoDecoder`, and in the viewer VideoToolbox decodes to a
  `CVPixelBuffer` that `CVMetalTextureCache` hands to the same Metal texture
  `FramebufferRenderer` blits — so the second path ends at the decoder rather than at
  the renderer.

The third lever is last because of what it costs: `egfx` is a channel the gateway
does not negotiate at all today, H.264 brings stream state the independent tile
protocol deliberately does not have — reference frames, keyframe cadence, a decoder
that cannot be handed tiles out of order — and it is a second decode path in *each*
client rather than one shared piece of work. `rxa`'s equivalent, encoding with
VideoToolbox on the Mac, is separate again.

## Deferred pending measurements

### Downscaled capture

A display is captured at its full pixel size, so a Retina panel hands the encoder
four times the pixels of the same desktop at 1x. `SCStreamConfiguration`'s `width`
and `height` can ask ScreenCaptureKit for fewer, trading sharpness for encode time
and bytes. Nothing already in place substitutes for it: the frame rate is already
capped, `VirtualDisplay::set_size` changes a desktop's *resolution* rather than
sampling it and only for a display of our own, and per-cell change detection and
the tile cache answer pixels that did *not* change.

That last one is why this waits rather than ships. It leaves downscaling as the
lever for a remote whose *changing* area is genuinely large, which is the same case
the video section above is about — but this one costs sharpness everywhere rather
than only where the picture is moving, so it comes after those levers, not before
them. The measurement it needs is that case: encode time as a share of the frame
budget while most of the screen is moving. It is also `rxa`-only, since a Linux or
Windows box cannot be told what size to capture itself at.

### Application-level liveness for VNC

Socket keepalive bounds host death and network partition
([`architecture.md`](architecture.md)), but it is answered by the peer's kernel,
so for RDP and VNC a server that is hung while still on the network reads as an
idle desktop, with no error and no return to the picker — RXA's ping/pong already
closes that half for the agent. RFB has exactly one message a conformant
server must answer regardless of change: a **non-incremental**
`FramebufferUpdateRequest`. A 1×1 one at the origin is therefore a ~10-byte probe,
and `update_request` already builds the shape — it would need the region
parameterised.

Three things to measure before committing to it. There is no correlation nonce, so
liveness could only mean "some update arrived within the deadline". Servers union
pending requests, and while libvncserver and TigerVNC add a non-incremental region
to the modified region (so only the 1×1 comes back), a server that instead applies
the flag to the whole union would retransmit the entire framebuffer every probe —
4 MB at 1280×800 raw. And each probe pushes a 1×1 tile through to the client.

RDP has no equivalent to offer. Its Heartbeat PDU is server-to-client only and
undecoded by `ironrdp-pdu`; Refresh Rect would force a round trip but may not be
sent unless the server advertised `refreshRectSupport`, which IronRDP's
`ConnectionResult` does not expose. That needs an upstream change first.

### Capture-stream linger

Keeping `SCStream` alive briefly after a gateway disconnect could avoid capture
teardown during a network blip. It is worthwhile only if stream restart time is
material relative to the gateway's one-second minimum reconnect backoff.
Implementing it would move capture ownership from the session task to
agent-level state.

### Encoder parallelism

A worker pool can complete tiles out of order. When consecutive frames update
the same region, a late tile from the older frame could overwrite newer pixels.
Keep ordered single-worker encoding unless measurements justify adding explicit
ordering.

### macOS login-window service

The `SMAppService` LaunchAgent runs only in the signed-in user's Aqua session, so
the agent stops at logout and cannot be reached from the macOS login screen.

The shape of an answer is established: RealVNC's
[Service Mode](https://help.realvnc.com/hc/en-us/articles/360002253238-Understanding-RealVNC-Server-Modes)
and RustDesk's installed-service mode both provide login-window access, by
installing launch components that declare the `LoginWindow` session type
alongside `Aqua` instead of relying on per-user `SMAppService`.

Nothing past that shape is settled, and none of it should be designed here first.
What has to be measured on a real Mac: how the single listener is held across the
login transition and fast user switching, where a config and private key readable by both
the configured user and the UID 0 login-window process should live, and whether
Screen Recording and Accessibility grants reach the signed app in the
`LoginWindow` session at all.

FileVault is the one boundary none of it crosses: no remote-access process runs
before pre-boot disk unlock.


## Not planned

### Audio

No engine carries it, and the case that wanted it — a Windows desktop whose sound
has no route to a Mac — is answered by software that already exists, from the
official RDP client's own audio redirection down to an AirPlay sender on the Windows
side. [`remote-audio.md`](remote-audio.md) records those routes and keeps the design
that was worked out for carrying it ourselves, for whenever one of them stops being
enough.

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
