# The browser's camera over VNC with wlshare

How the browser's camera reaches a wlroots-based Wayland desktop behind wlshare,
on the RFB connection the session already has, so a `vnc` target can take a
camera the way an RDP one does over MS-RDPECAM. RFB carries nothing from a client
but input and a clipboard, and no registered extension carries video that way, so
this is wlshare's third private extension, in the shape of the density and outputs
ones ([`wlshare-density.md`](wlshare-density.md),
[`wlshare-outputs.md`](wlshare-outputs.md)). It is discovered rather than
configured: the client lists a pseudo-encoding, a server that speaks it answers,
and one that does not says nothing and the browser's camera is never plugged.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which
decodes the H.264 with the system's libavcodec and offers it to the desktop as a
PipeWire `Video/Source` node — a camera applications reaching cameras through
PipeWire can open. How wlshare builds that node is in its own
[`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-camera-extension).

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
host = "127.0.0.1"
port = 5900
camera = true
```

`camera = true` is the whole of it, and it is what makes the gateway *ask*. It is
accepted on any plain `vnc` target and refused on both Apple subtypes, whose
Screen Sharing has no camera to offer. As on RDP it is capability only: the camera
is plugged when a browser enables it from the floating menu, per session and never
remembered, and unplugged when that socket closes, the session changes hands, or
the engine ends. wlshare's own `camera` key (default `true`) is the server's side
of the same switch.

## The wire

One private pseudo-encoding and one private message type, in both directions.

- **Pseudo-encoding** `0x574c5343`, the ASCII bytes `WLSC`, listed in
  `SetEncodings` beside the audio request and ahead of the density and outputs
  requests, which stay last.
- **Message type** `0xE2`. Every message is the type, an operation, two more bytes,
  and what the operation carries. Integers are big-endian.

| Direction | Operation | Bytes 2–3 | Then |
| --------- | --------- | --------- | ---- |
| client → server | 0, plug | padding | `u16` width, `u16` height, `u32` frame-rate numerator, `u32` denominator |
| client → server | 1, unplug | padding | nothing |
| client → server | 2, sample | flags (bit 0: keyframe), padding | `u32` length, one Annex B access unit |
| server → client | 0, available | padding | nothing |
| server → client | 1, start | padding | the plugged format, as a plug lays it out |
| server → client | 2, stop | padding | nothing |
| server → client | 3, keyframe | padding | nothing |

wlshare answers *every* `SetEncodings` that lists the encoding with *available*,
and only the first is news. A server operation this client does not know is fatal:
the messages share no length field, so one that cannot be measured leaves the
stream at an offset nothing recovers from.

## The gateway's half

`src/vnc_camera.rs` is the VNC engine's adapter to the camera socket's
`CameraBridge` — the same socket, bridge and browser encoder MS-RDPECAM uses, so
the browser cannot tell the two apart. The bridge's control feeds two queues, as
the RDP client's does: plug and unplug without limit, samples sixteen deep. A
sample that finds the queue full, or is past the 4 MiB wlshare accepts, is dropped
with every later one until a keyframe, and the browser is asked for it once per gap
— the dropped unit may have been the keyframe a stream starts on. Commands go
ahead of the samples queued behind them, so every plug and unplug starts a new
generation and a sample carries the one current when it was sent: the old camera's
samples still waiting when a replug is taken are dropped, never sent as the new
camera's.
The engine's loop writes what they carry; the read loop turns wlshare's start,
stop and keyframe into the bridge's signals, which reach the browser as
`cameraStart`, `cameraStop` and `cameraKeyframe`.

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
the gateway: a probe client speaking the wire above plugged 640x480 at 15/1 and
sent libx264 Constrained Baseline, and a PipeWire consumer linked to
`wlshare-camera-1`. The consumer negotiated I420 640x480 at 15/1 and received
whole pictures; wlshare sent start as it linked and stop as it left. The path
through the gateway and a browser, and an application such as a browser on the
wlroots desktop opening the camera, are checked by hand, not by the container
test in `tests/wlshare_e2e.rs`.
