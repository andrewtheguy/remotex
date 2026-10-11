# The browser's camera over VNC with wlshare

How the browser's camera reaches a wlroots-based Wayland desktop behind wlshare,
on the RFB connection the session already has, so a `wlshare` target can take a
camera the way an RDP one does over MS-RDPECAM. RFB carries nothing from a client
but input and a clipboard, and no registered extension carries video that way, so
this is one of wlshare's private extensions, in the shape of the density and outputs
ones ([`wlshare-density.md`](wlshare-density.md),
[`wlshare-outputs.md`](wlshare-outputs.md)). A `wlshare` target with
`camera = true` lists a pseudo-encoding and wlshare answers that it takes a
camera; until it answers, the browser's camera is never plugged.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which
decodes the H.264 with the system's libavcodec and offers it to the desktop as a
PipeWire `Video/Source` node that camera applications can open. How wlshare builds
that node is in its own
[`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-camera-extension).

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "wlshare"
host = "127.0.0.1"
port = 5900
camera = true
```

`subtype = "wlshare"` says the server is wlshare, and `camera = true` is what
makes the gateway list the extension to it. The key is accepted on a `wlshare`
target and refused on a plain `vnc` target and on both Apple subtypes, none of
which has anywhere to put a camera. As on RDP it is capability only: the camera
is plugged when a browser enables it from the floating menu, per session and never
remembered, and unplugged when that socket closes, the session changes hands, or
the engine ends. wlshare's own `camera` key (default `false`) is the server's side
of the same switch.

## The wire

The extension is wlshare's, and its messages and their layouts are in wlshare's
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-camera-extension):
the pseudo-encoding `WLSC` and message type `0xE2` in both directions. The
gateway lists `WLSC` in `SetEncodings` beside the audio request and ahead of the
density and outputs requests, which stay last.

wlshare answers *every* `SetEncodings` that lists the encoding with *available*,
and only the first is news. A server operation this client does not know is fatal:
the messages share no length field, so one that cannot be measured leaves the
stream at an offset nothing recovers from.

## The gateway's half

`src/vnc_camera.rs` adapts the camera socket's `CameraBridge` to the VNC engine.
It reuses the same browser-facing socket, bridge and encoder as the MS-RDPECAM
path; only the engine-side protocol differs.

Traffic reaches the engine through two queues:

1. Plug and unplug commands use an unbounded control queue so a device transition
   is not lost. Each command advances a generation number.
2. H.264 samples use a queue of sixteen entries. Each sample is tagged with the
   generation current when the browser submitted it.
3. The engine checks the control queue first. After it takes a new plug or unplug,
   it discards queued samples tagged with an older generation instead of sending
   an old stream under the new device state.
4. A sample larger than wlshare's 4 MiB limit, or one submitted while the sample
   queue is full, opens a gap. That sample and subsequent non-keyframes are
   dropped; the bridge asks the browser for a keyframe once for that gap, and new
   submissions resume at a keyframe. Samples already in the queue keep their
   order.

The engine loop writes plug, unplug and sample messages. Its read loop converts
wlshare's start, stop and keyframe requests into `cameraStart`, `cameraStop` and
`cameraKeyframe` signals for the browser.

What reaches the wire is `vnc_camera::Device`'s decision, taken under the uplink
so two decisions leave in the order they were made:

- A plug the browser makes before wlshare's answer is held, and goes out with the
  answer. A server that never answers is sent nothing at all — no plug, no
  samples — whatever the browser does, so a target with `camera = true` pointed
  at wayvnc or TigerVNC loses nothing but the camera.
- A plug with no area or no rate is never sent, as the RDP path refuses it too:
  wlshare takes one as a client that does not speak the extension and ends the
  session over it.
- An unplug goes out only for a camera that was plugged on the wire.
- A sample goes out only on a plugged camera the server knows about.

The browser encodes between `cameraStart` and `cameraStop` alone, and wlshare
sends start only while an application on the desktop has the camera open, so an
enabled camera on a desktop nobody is filming costs nothing past the plug.

## What is checked

Both halves' wire formats are unit tested against independent decoders — this
side's in `src/vnc_camera.rs`, wlshare's in `crates/wlshare-rfb/src/camera.rs` —
and so are the device's hold-until-answered rule, the queue's drop to a keyframe,
and the bridge keeping a plug made before the engine registers. wlshare's decoder
is tested against a libx264 stream.

The container test in `tests/wlshare_e2e.rs` follows the whole path but the page:
wlshare on a headless sway with PipeWire and WirePlumber, and the gateway, driven by
a client that opens the camera socket as the session starts, plugs 320x240 at 15/1,
and sends a libx264 Constrained Baseline fixture in real time. GStreamer's
`pipewiresrc` is the application: it asserts that wlshare lends the desktop a node,
that opening it sends `cameraStart` with the plugged format and leaving it sends
`cameraStop`, that the application negotiates raw pictures of that size, and that
closing the socket removes the node.

Measured 2026-09-14 against wlshare on a sway session with PipeWire 1.4.2, without
the gateway: a probe client speaking the extension plugged 640x480 at 15/1 and
sent libx264 Constrained Baseline, and a PipeWire consumer linked to
`wlshare-camera-1`. The consumer negotiated I420 640x480 at 15/1 and received
whole pictures; wlshare sent start as it linked and stop as it left. The path
through the gateway and a browser, and an application such as a browser on the
wlroots desktop opening the camera, are checked by hand, not by the container
test in `tests/wlshare_e2e.rs`.
