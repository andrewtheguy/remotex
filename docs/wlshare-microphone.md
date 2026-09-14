# The browser's microphone over VNC with wlshare

How the browser's microphone reaches a wlroots-based Wayland desktop behind
wlshare, on the RFB connection the session already has, so a `vnc` target can take
a microphone the way an RDP one does over MS-RDPEAI. The QEMU Audio extension
carries sound from the server only, so this is wlshare's fourth private extension,
the camera's twin ([`wlshare-camera.md`](wlshare-camera.md)). It is discovered
rather than configured: the client lists a pseudo-encoding, a server that speaks it
answers, and one that does not says nothing and the browser's microphone is never
plugged.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which offers
the PCM to the desktop as a PipeWire `Audio/Source` node, "wlshare remote
microphone", that applications record from like any other. How wlshare builds that
node is in its own
[`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-microphone-extension).

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
host = "127.0.0.1"
port = 5900
microphone = true
```

`microphone = true` is the whole of it, and it is what makes the gateway *ask*. It
is accepted on any plain `vnc` target and refused on both Apple subtypes. As on RDP
it is capability only: the microphone is plugged when a browser enables it from the
floating menu, per session and never remembered, and unplugged when that socket
closes, the session changes hands, or the engine ends. wlshare's own `microphone`
key (default `true`) is the server's side of the same switch.

## The wire

One private pseudo-encoding and one private message type, in both directions.

- **Pseudo-encoding** `0x574c534d`, the ASCII bytes `WLSM`, listed in
  `SetEncodings` after the camera request and ahead of the density and outputs
  requests, which stay last.
- **Message type** `0xE3`. Every message is the type, an operation, two more bytes,
  and what the operation carries. Integers are big-endian.

| Direction | Operation | Bytes 2–3 | Then |
| --------- | --------- | --------- | ---- |
| client → server | 0, plug | padding | nothing |
| client → server | 1, unplug | padding | nothing |
| client → server | 2, sample | padding | `u32` length, interleaved signed 16-bit little-endian PCM |
| server → client | 0, available | padding | nothing |
| server → client | 1, start | padding | `u16` channels, `u16` padding, `u32` frequency |
| server → client | 2, stop | padding | nothing |

wlshare answers *every* `SetEncodings` that lists the encoding with *available*,
and only the first is news. The format is the server's to name, as a host's is over
RDP; wlshare names mono at 48 kHz, which is the rate the gateway's Opus decodes at,
so the bridge resamples nothing. wlshare ends the session over a sample past
256 KiB or one that is not whole frames of that format. A server operation this
client does not know is fatal: the messages share no length field.

## The gateway's half

`src/vnc_mic.rs` is the VNC engine's adapter to the mic socket's `MicBridge` — the
same socket, bridge, Opus decoder and browser encoder MS-RDPEAI uses, so the
browser cannot tell the two apart. The bridge's control is told of the socket
attaching and closing as a plug and an unplug, which RDP's adapter has no use for:
a Windows host's recording device is there for the whole session, where wlshare's
is made by the plug. Plugs and unplugs queue without limit; PCM waits sixteen
buffers deep, dropping the oldest, and an unplug or the bridge's close drops what
waits. The engine's loop writes what they carry; the read loop turns wlshare's
start and stop into the bridge's open and close, which reach the browser as
`micOpen` and `micClose`.

What reaches the wire is `vnc_mic::Device`'s decision, taken under the uplink so two
decisions leave in the order they were made:

- A plug the browser makes before wlshare's answer is held, and goes out with the
  answer. A server that never answers is sent nothing at all.
- An unplug goes out only for a microphone that was plugged on the wire.
- A start is heard only for a microphone plugged on the wire, so one already in
  flight when the browser unplugged opens nothing.
- PCM goes out only between a start and a stop.
- Unplugging or replugging a microphone something records from closes the
  recording at the bridge, since wlshare sends no stop for a node it has removed;
  the close is told under the device's lock, so a start for the same device is
  never told after it.

## What is checked

Both halves' wire formats are unit tested against independent decoders — this
side's in `src/vnc_mic.rs`, wlshare's in `crates/wlshare-rfb/src/microphone.rs` —
and so are the device's rules, the queues, and the bridge keeping a plug made
before the engine registers.

The container test in `tests/wlshare_e2e.rs` follows the whole path but the page:
wlshare on a headless sway with PipeWire and WirePlumber, and the gateway, driven by
a client that opens the mic socket as the session starts and sends 60 ms Opus
packets of a 440 Hz tone. It asserts that wlshare lends the desktop a node, that
`pw-record` linking to it sends `micOpen` and leaving it sends `micClose`, that what
was recorded is loudest at 440 Hz, and that closing the socket removes the node.

Measured 2026-09-14 against wlshare on a labwc session with PipeWire 1.4.2,
without the gateway: a probe client speaking the wire above plugged a microphone
and sent a 440 Hz tone while `pw-record --target wlshare-microphone-1` recorded.
wlshare sent start as the recorder linked and stop as it left, and the recording
held the tone without gaps past the node's 60 ms prefill. The path through the
gateway and a browser is checked by hand.
