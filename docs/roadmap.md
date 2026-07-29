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

### Prompter audio from an RDP guest

The RDP-to-browser path is done: MS-RDPEA redirection over both of its channels (plus
the `rdpdr` advertisement Windows requires before it will redirect anything), Opus
frames on the session's own WebSocket, and a browser that decodes them with WebCodecs
and schedules every buffer itself. A live Windows 11 target's own sound has been heard
through it. The mechanism, its lifecycle and that evidence are recorded in
[`remote-audio.md`](remote-audio.md), which is where they belong.

Every open question the design named is now closed, including the one that reversed
it. A browser does play an open-ended response progressively, and a real host does
redirect to this gateway — but **the schedule of an `<audio>` element belongs to the
browser and can never be reclaimed**, so a delay it accumulated stayed for the
session. Guacamole's `RawAudioPlayer` bounds latency in one line, in `sync()`:

```js
// guacamole-common-js/modules/AudioPlayer.js
nextPacketTime = Math.min(nextPacketTime, now + maxLatency);   // maxLatency = 0.3
```

That reframed the problem — not a codec, not a container, but **owning the playback
clock** — and owning it means the bytes have to arrive as bytes, which is what put
audio on the WebSocket and deleted the endpoint, the Ogg container and the
silence keepalive together. The one thing not copied is Guacamole's wire: it sends raw
PCM (`audio/L16`) in-band, spending the ~176 kB/s that Opus takes to about 10, so
keeping Opus and decoding it with WebCodecs holds both properties where each design
had only one.

What is **not** settled is promptness against the live host, and it is the first of the
two things planned here. A live desktop was heard a couple of seconds behind itself
under the old design, with the gateway measured out of it as a cause; the ceiling is
aimed at the remaining suspect and has not yet been listened to
([`remote-audio.md`](remote-audio.md)). If it is still late with no trims recurring,
the delay is Windows' own capture path and the next step is an A/B against `freerdp`
rather than any further change here.

Two of Guacamole's other choices are worth recording, one of which this now shares:

- **It loads `rdpdr` and `rdpsnd` together**, unconditionally, whenever printing,
  drive redirection *or* audio is enabled. A second independent client therefore
  never ships `rdpsnd` without `rdpdr` — which is why nothing documents that Windows
  needs it, and why reading another client's source is what found it here.
- **Audio is negotiated with the client rather than configured.** Both protocols call
  one `guac_audio_stream_alloc`, which picks an encoder from the mimetypes the
  connected user declared — the owner's first, then any user's. A stream whose
  encoder stayed `NULL` is still returned and then silently does nothing, since
  `guac_audio_stream_write_pcm` and `_flush` both check the encoder before calling
  it; the protocols' "Sound disabled" log covers the stream failing to allocate at
  all. **Ours is now half of that**: the target's config flag still decides whether a
  queue exists at all and the `connected` message reports it, but nothing is *sent*
  until a client asks. Which turned out to matter for a reason Guacamole does not
  have — it is what let audio frames join a wire the macOS viewer already speaks
  without bumping the version number the viewer matches on.

**Then the same RDP audio in the macOS viewer, and deliberately second.** It could once
have pointed `AVPlayer` at an HTTP endpoint unchanged; Ogg took that away (AVFoundation
has no Ogg demuxer) and audio frames on the WebSocket take it further — there is no
response left to point anything at. So viewer audio means a representation of its own
from the same queue: Opus in CAF or fragmented MP4, both of which AVFoundation reads,
or decoding Opus in the viewer. A second representation, not a second transport, and
not difficult.

The order was the argument, not the difficulty, and it has already paid: the browser
path has since moved from an open-ended WAV to Ogg/Opus to raw Opus frames on the
socket, and anything the viewer had been built against first would have been a
representation on its way out. What it copies now is a settled *timing* model rather
than an unsettled one — and note the deletion this leaves it, since the frames it would
need are opt-in: a viewer that never sends the `audio` message needed no protocol
version bump and no rebuild.

**Until that work starts, the viewer is not a constraint on the browser path.** It has
no audio at all — and now not even a wire it could receive audio on — so no change to
the encoder, the frames or the control needs checking against it, and none should be
held up for it. Being planned is not the same as being a dependency.

Audio for `rxa` and VNC, and a microphone going the other way, are
[not planned](#audio-for-rxa-and-vnc-and-a-microphone).

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

### Audio for `rxa` and VNC, and a microphone

Remote audio is an **RDP** feature here, to both clients
([`remote-audio.md`](remote-audio.md)). The other two protocols are not a matter of
priority — each would be a different feature wearing the same name:

- **`rxa`.** Capturing the Mac's own output is a feature of the agent, not of this
  path, and the agent does not do it.
- **VNC.** RFB carries no audio, and the interesting part is that Guacamole does not
  extend it either: guacd opens a **separate PulseAudio connection** to a sound
  server on the remote host (`audio-servername`, compile-time optional behind
  `--with-pulse`) and feeds the same audio stream its RDP path uses. So the option
  this roadmap once wrote off as "RFB would need an extension both ends invented" is
  real and out-of-band — it is just a different product, requiring a sound server
  configured and reachable on every target.

A microphone at the browser reaching the remote is also not planned, and has never
been designed.
