# Desktop audio over VNC with wlshare

How a wlroots-based Wayland desktop behind wlshare hands the gateway its sound,
on the RFB connection it already has, so a `vnc` target plays through the
browser the way an RDP one does. Standard RFB carries pixels and a clipboard and
nothing else; this is the one audio extension `rfbproto` registers, spoken by
QEMU as a server and gtk-vnc as a client, and now by wlshare and this gateway.
It is discovered rather than configured, exactly as the density extension is
([`wlshare-density.md`](wlshare-density.md)): the client lists a pseudo-encoding,
a server that speaks it announces so, and one that does not says nothing and the
session runs in silence.

Measured 2026-09-09 on `workstation-wsl`, a headless sway with one `HEADLESS-1`
output and PipeWire's own dummy sink, through `tmp/audio_ws_probe.py`.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which
captures the **default sink's monitor** from PipeWire — what the desktop is
playing, whatever is playing it — and sends it in the format the client asked
for.

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
host = "127.0.0.1"
port = 5900
username = "me"
password = "…"
audio = true
```

`audio = true` is the whole of it, and it is what makes the gateway *ask*.
Nothing names the server or the extension: a target that asks and connects to
wayvnc, TigerVNC or x11vnc gets a desktop and no sound, which is what those
servers have to give. On the gateway side `audio = true` is now accepted on any
plain `vnc` target; it stays refused on Apple's standard Screen Sharing, which
carries no sound and speaks no audio extension, and on High Performance it
still means Apple's media stream and still needs the `apple-hp-audio` build.

wlshare's own `audio` key (default `true`) is the server's side of the same
switch: with it off the extension is not announced, and a client that lists the
pseudo-encoding is told nothing.

## The wire

One registered pseudo-encoding and one registered message type, in both
directions. `rfbproto` calls it the QEMU Audio extension.

- **Pseudo-encoding** `-259`, listed in the client's `SetEncodings` beside the
  standard ones. A server that does not know it ignores it, as RFB requires.
- **Message type** `255` with **submessage** `1`, the QEMU extensions' shared
  type. Nothing else under type 255 is advertised by this client, and a
  submessage it does not know is fatal: the QEMU submessages share no length
  field, so one that cannot be measured leaves the stream at an offset nothing
  recovers from.
- **Samples** are little-endian. The specification says nothing about the byte
  order of a sample wider than eight bits; QEMU writes host-native samples and
  gtk-vnc reads little-endian ones. Host-native is safe because every host this
  runs on is little-endian, which is what gtk-vnc already expects.

### Server → client: the announcement

An **empty pseudo-rectangle** of encoding `-259` inside a `FramebufferUpdate` —
the only way support is announced, the same shape ExtendedDesktopSize uses.
wlshare sends it as its own update, ahead of any pixels, on the first
`SetEncodings` that lists the encoding.

| Offset | Type | Field |
|---|---|---|
| 0 | U16 | x, 0 |
| 2 | U16 | y, 0 |
| 4 | U16 | width, 0 |
| 6 | U16 | height, 0 |
| 8 | S32 | encoding, `-259` |

### Client → server: set format, enable, disable

The format is the client's to choose — the server converts whatever the desktop
plays into it — so there is nothing to negotiate. This gateway asks for
**signed 16-bit, 2 channels, 48 000 Hz**, which is Opus's own rate and the
passthrough encoder's PCM, and so needs no resampler on either path.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation: 0 enable, 1 disable, 2 set format |
| 4 | U8 | sample format (set format only): 0 U8, 1 S8, 2 U16, **3 S16**, 4 U32, 5 S32 |
| 5 | U8 | channels, 1 or 2 |
| 6 | U32 | frequency |

Four bytes for an enable or a disable, ten for a set-format. The gateway sends
set-format then enable, once, when the announcement arrives.

### Server → client: begin, data, end

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation: 0 end, 1 begin, 2 data |
| 4 | U32 | length of the samples (data only) |
| 8 | U8[] | samples, interleaved, in the client's format |

## What the gateway does with it

`src/vnc_qemu_audio.rs` is the wire; `src/vnc.rs` keeps the extension's state
per connection as `Audio`: `Off` where no sound was asked for and on the Apple
dialects, `Asked` from the handshake, `Announced` once the rectangle has
arrived and the stream has been turned on, and `Unanswered` once pixels have
arrived with no announcement in front of them — a server that announces late is
still taken.

- The announcement is answered after the update it arrived in, not inside it,
  so the enable goes out once however the update was framed.
- `begin` publishes the negotiated format on `AudioBridge`; `end` clears it,
  which leaves an open `/ws/audio` response filling with silence rather than
  ending. A desktop going quiet must not cost the listener its stream.
- Each `data` message's samples go to the bridge as they arrived — interleaved
  little-endian 16-bit stereo is what was asked for and what the queue takes,
  so nothing is copied or converted between the socket and the encoder. From
  there the path is every target's: the queue, the Opus or passthrough encoder,
  `/ws/audio` ([Audio frames](architecture.md#audio-frames)).
- A data length past a megabyte is read past rather than allocated: a buffer is
  20 ms, and a second of this format is 192 000 bytes, so anything larger is a
  server that has lost its framing.

Audio shares the TCP stream with the pixels, which is the one cost of carrying
it in band. wlshare drains its capture queue before every framebuffer update, so
sound is never held behind a ZRLE frame it was ready before; the browser's
300 ms lead clamp absorbs what is left.

## What wlshare does

`crates/wlshare/src/audio.rs` starts one PipeWire capture per client that
enables audio, on a thread of its own, and stops it on a disable or when the
client goes. The stream is a `Stream/Input/Audio` node with
`stream.capture.sink = "true"`, which is what makes PipeWire connect it to the
**default sink's monitor** rather than to a microphone, and `node.latency` asks
for 20 ms buffers. The process callback runs on PipeWire's real-time thread and
does nothing but copy whole frames into a sixteen-deep queue, dropping the
oldest when a client cannot keep up — a dropped buffer is a hole, and a stalled
capture callback is worse. A set-format on a running stream restarts the capture
in the new format.

PipeWire honours its own quantum before settling on the requested one, so the
first buffers of a session are often smaller than 20 ms — 512 frames where 960
were asked for, measured. Nothing downstream cares: every buffer is a whole
number of frames and the encoder cuts its own packets.

A headless session still has a sink to capture: PipeWire's Dummy Output is one,
and no `null-sink` needs configuring.

## Measured

With a 6-second 440/660 Hz stereo tone playing into the default sink through
`pw-play`, a passthrough target on this host:

```
format: {"type":"audioFormat","codec":"pcm-s16le","sampleRate":48000,"channels":2,"packetFrames":0,"head":""}
551 frames, 551 packets, 1128448 bytes = 5.88s of 48000 Hz audio, peak 12000
```

The same target under Opus, tone playing and then silent:

```
306 frames, 306 packets, 73746 bytes     tone:    241 bytes a packet
192 frames, 192 packets,   576 bytes     silence:   3 bytes a packet
```

And the control, wayvnc on another host with `audio = true` asked of it:

```
vnc: the server carries no audio; the session runs without sound
0 frames
```

The format is still announced on `/ws/audio` there — the gateway advertises one
format and writes the header before any remote channel is up — so a server
without the extension is silence measured in packets, not in the announcement.

Reproduce it:

```sh
REMOTEX_PROBE_PASSWORD=… uv run tmp/audio_ws_probe.py \
  --port <gateway port> --target <name> --user <user> --seconds 8
# meanwhile, on the host
pw-play <some>.wav
```
