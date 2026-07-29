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

That path is worst in three places at once, one of which has been dealt with. Every
frame changes every pixel of the video's rectangle, so the shadow copy has nothing to
trim out of it; `Rect::bands()` splits the rectangle into 64-row strips — seventeen of
them if it fills a 1080p screen — and each strip is WebP-encoded at a cost of 60µs plus
~17ns a pixel (`WEBP_LOSSLESS_METHOD`), so a full 1080p frame is 30–40 ms of
encoding; a windowed video pays the same rate per pixel. `encode.rs` took that off the
RDP read loop and compresses up to `ENCODE_DEPTH` bands at once, which roughly halved
a measured 1280x800 repaint and took the read loop's wait for the encoder to zero —
that second number was the input latency. What it did not do is make the work smaller,
so the frame rate is still bounded by how much of it there is.

Then each client decodes those strips one at a time, and in both cases on purpose:
the SPA chains every message through one promise so draws land in arrival order, and
the viewer's `TileDecoder` is an actor for the same reason. Tiles overwrite their
rectangles with no delta state, so arrival order is correctness rather than
tidiness — which is why the fix on the client side is fewer or cheaper payloads per
frame, not more concurrency.

Two levers left. The first is gateway-side, so both clients get it at once, and it is
worth measuring before the second is committed to:

- **Lossy WebP in the gateway.** `encode_webp` is lossless with no choice today, while
  the macOS agent classifies per tile and sends photographic content down a lossy
  branch. Video is exactly what that branch exists for. The cost is that a desktop is
  mostly text, so this cannot be a blanket switch — it needs the agent's classifier, or
  something like it, on this side of the wire.

  **Spending more encoder effort instead is not an alternative here, and the reason is
  worth keeping.** `m2 q50` is lossless, so it costs no fidelity — but at 320x64 it is
  7.7x the CPU of the shipping `m0 q20` (3.1ms against 402µs) for only 10–17% fewer
  bytes, and a video's bands are wider still. Moving encoding off the read loop did not
  make that cheap, it only stopped it blocking input; the CPU is unchanged and
  `ENCODE_DEPTH` can only overlap it up to the core count. On the largest shapes it
  would *raise* the per-repaint wall clock, which is the frame rate this item is about.
  Higher effort pays only below roughly 512 pixels, where it is nearly free (1.45x the
  time for 30% fewer bytes at 16x16) — a **size-tiered** effort, and a general
  small-rect win rather than anything to do with video. See `WEBP_LOSSLESS_METHOD`.
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

H.264 is last because of what it costs: `egfx` is a channel the gateway
does not negotiate at all today, H.264 brings stream state the independent tile
protocol deliberately does not have — reference frames, keyframe cadence, a decoder
that cannot be handed tiles out of order — and it is a second decode path in *each*
client rather than one shared piece of work. `rxa`'s equivalent, encoding with
VideoToolbox on the Mac, is separate again.

### A remote size that is chosen rather than derived

A client on a real desktop works its size out from its own window, and mobile is
where that stops being possible: a portrait phone's window asks for a tall, narrow
desktop no desktop OS lays out well, and rotating it asks for a different one. So a
pinch-zoom client derives nothing. A tablet asks for its own landscape dimensions off
`screen`, and a phone sends `ClientMsg::DefaultSize` — a request carrying no size at
all, which each engine answers with the target's configured `width`/`height`, or with
the point size the `rxa` agent created its display at.

Those are defensible guesses, not answers. An iPad's landscape shape is a
tablet's, not a resolution anybody picked for the desktop behind it, and the
`width`/`height` default of 1280x800 is the size that was reasonable for RDP's
connect. The person looking at the desktop knows what they want it to be, and
nothing anywhere asks them — the phone/tablet split in `useRemoteDesktop.ts` exists
only because the client has to guess, and a chosen size would delete it.

A chosen size needs nothing new on the wire: `ClientMsg::Viewport` already carries an
arbitrary size and all three engines already act on it. `DefaultSize` is not a step
towards it and does not grow into it — deferring to whatever the far side calls its
default is the opposite of naming a resolution, and it remains what a phone sends
whatever this adds beside it.

What a chosen size revises is the stance recorded on `Viewport`, that a remote's
resolution belongs to the machine running it. That is true of a Mac's own panel —
which is why `rxa` resizes only a display the agent made, and that stays true — and
false of a headless VNC server or a display created for one client, whose size exists
only for whoever is connected.

What is not settled, and should not be decided here first:

- **Where the choice lives.** Per target in the config is the least mechanism and
  survives a reconnect; per session in the floating menu is where somebody
  actually wants it while looking at a desktop that is the wrong size; both, with
  the config as the default the menu starts from, is the likely answer and the most
  state.
- **What the list contains.** A fixed set of common resolutions is trivial and will
  be wrong for somebody's ultrawide. The guest's own modes are the honest list, and
  only `rxa` can produce one — RFB and RDP have no way to enumerate what the far
  side would accept, so a VNC or RDP target can only be asked and told after the
  fact whether it worked. `AgentMsg::Displays` is the seam that would carry the Mac's.
- **What a phone does with it.** Possibly nothing: pinch zoom over a desktop-shaped
  remote is already how a phone reads one, and a chosen size is a desktop and tablet
  feature that phones inherit only through the config. Worth resisting the symmetry
  if the menu would be unusable at that size.

### RDP audio through a gateway live stream

The browser is already an audio-stream client, so the first audio slice does not
need to become part of remotex's desktop protocol. The RDP engine requests
RDPSND audio redirection, the gateway exposes the claimed session through an
authenticated live HTTP endpoint, and the SPA points a basic `<audio>` element
at it.

The first PoC wraps negotiated PCM in an open-ended WAV response. Its purpose is
to prove progressive playback through the browsers and reverse proxy used with
remotex. If that representation is not consumed progressively, only the HTTP
media representation changes to a compressed stream; the RDP channel, endpoint,
and `<audio>` integration remain the path. [`remote-audio.md`](remote-audio.md)
holds the boundary and lifecycle details.

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
them. It is also `rxa`-only, since a Linux or Windows box cannot be told what size
to capture itself at.

The measurement it was waiting on is now taken, by
[`tests/rxa_repaint_probe.rs`](../tests/rxa_repaint_probe.rs), and it moved this
further back rather than nearer: encoding a 3200x2000 repaint's 320 cells eight at a
time instead of one took the frame rate from 7.3 to 28.8 a second, and the
*redundancy* with it — every frame had been a full repaint, because one that could not
finish in time made capture ask for another. `ENCODE_WIDTH`
(`crates/rxa-agent/src/session.rs`) is the parallelism that was varied to get those
numbers, and carries the table.

Downscaling reaches the same encode cost from the other side and by a similar factor,
but the factor is a different kind of quantity: halving each axis is a **quarter of
the pixels to encode**, which is a reduction in work rather than a throughput or
frame-rate figure, and nothing here has benchmarked what it would actually deliver.
Nor is it the same trade — those pixels are given up permanently, where parallelism
spent cores that were idle. So what is left for this lever is the case where the cores
really are gone, which is a smaller Mac than any measurement here has been run on.

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

### Multiple sessions

remotex permanently has one active session slot. A new browser takes over and
evicts the previous holder. Concurrent sessions, shared sessions, and a session
broker are outside the product model.
