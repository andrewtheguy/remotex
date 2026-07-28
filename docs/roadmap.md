# Roadmap

What is merely *designed* belongs in the architecture docs; what is *planned*
belongs here. A defect that has been fixed needs no entry anywhere — the commit
that fixed it, and the test that holds it fixed, are the record. The limitations
imposed on us from outside are recorded beside the mechanism they constrain, which
is the only place they can be read in context.

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
lever for a remote whose *changing* area is genuinely large — full-screen video, a
window dragged across a 2x panel — and the measurement it needs is that case:
encode time as a share of the frame budget while most of the screen is moving. No
workload here has pushed hard enough to produce it.

### H.264 through the tile format byte

VideoToolbox to encode, browser WebCodecs to decode, as a second payload kind
rather than a replacement. The `TILE` record's `format` byte is the seam it arrives
through, which is why the byte was kept when the codec collapsed to a single value
(`src/protocol.rs`).

It is behind everything else here because it costs more than a codec swap did: a
second decode path in *both* clients, and stream state — reference frames, keyframe
cadence, a decoder that cannot be handed tiles out of order — which the independent
tile protocol deliberately does not have. It is not an `rxa`-only item either: the
RDP and VNC engines would carry it too. The trigger is the same large moving area
the section above addresses far more cheaply, which is the order to try them in.

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

### Audio

No engine currently carries audio. Its transport, synchronization, and browser
playback design remain unspecified.


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
