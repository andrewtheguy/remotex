# Desktop audio over VNC with wlshare

How a wlroots-based Wayland desktop behind wlshare hands the gateway its sound,
on the RFB connection it already has, so a `vnc` target plays through the
browser the way an RDP one does. Standard RFB carries pixels and a clipboard and
nothing else; this is wlshare's private audio extension, which carries the sound
as lossless FLAC and borrows its control messages from the QEMU Audio extension
`rfbproto` registers. It is discovered rather than configured, exactly as the density extension is
([`wlshare-density.md`](wlshare-density.md)): the client lists a pseudo-encoding,
a server that speaks it announces so, and one that does not says nothing and the
session runs in silence.

Measured 2026-09-09 on `workstation-wsl`, a headless sway with one `HEADLESS-1`
output and PipeWire's own dummy sink, through `tmp/audio_ws_probe.py`.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which
captures the **default sink's monitor** from PipeWire — what the desktop is
playing, whatever is playing it — and sends it as FLAC frames of the format the
client asked for.

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
servers have to give. So does QEMU, whose own audio extension carries raw
samples and is not asked for. On the gateway side `audio = true` is now accepted on any
plain `vnc` target; it stays refused on Apple's standard Screen Sharing, which
carries no sound and speaks no audio extension, and on High Performance it
still means Apple's media stream and still needs the `apple-hp-audio` build.

wlshare's own `audio` key (default `true`) is the server's side of the same
switch: with it off the extension is not announced, and a client that lists the
pseudo-encoding is told nothing.

## The wire

One private pseudo-encoding and one private message type, beside the QEMU Audio
extension's message type for everything else.

- **Pseudo-encoding** `0x574c5346`, `WLSF` in ASCII, listed in the client's
  `SetEncodings` beside the standard ones. A server that does not know it
  ignores it, as RFB requires. QEMU's own pseudo-encoding, `-259`, is not
  listed: what it promises is raw samples.
- **Message type** `255` with **submessage** `1`, the QEMU extensions' shared
  type, for the client's set-format, enable and disable and the server's begin
  and end. Nothing else under type 255 is advertised by this client, and a
  submessage or operation it does not know is fatal: the QEMU submessages share
  no length field, so one that cannot be measured leaves the stream at an
  offset nothing recovers from. That includes QEMU's operation 2, raw data.
- **Message type** `0xE4`, server → client, for the sound itself: one FLAC frame
  a message.

### Server → client: the announcement

An **empty pseudo-rectangle** of encoding `WLSF` inside a `FramebufferUpdate` —
the only way support is announced, the same shape ExtendedDesktopSize uses.
wlshare sends it as its own update, ahead of any pixels, on the first
`SetEncodings` that lists the encoding.

| Offset | Type | Field |
|---|---|---|
| 0 | U16 | x, 0 |
| 2 | U16 | y, 0 |
| 4 | U16 | width, 0 |
| 6 | U16 | height, 0 |
| 8 | S32 | encoding, `0x574c5346` |

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
| 4 | U8 | sample format (set format only): 0 U8, 1 S8, 2 U16, **3 S16** |
| 5 | U8 | channels, 1 or 2 |
| 6 | U32 | frequency, 8 000 to 96 000 in wlshare |

Four bytes for an enable or a disable, ten for a set-format. The gateway sends
set-format then enable, once, when the announcement arrives. QEMU's 32-bit
formats, 4 and 5, are refused by wlshare, since FLAC stores at most 24 bits.

### Server → client: begin, end

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `255` |
| 1 | U8 | `1` |
| 2 | U16 | operation: 0 end, 1 begin |

### Server → client: a FLAC frame

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE4` |
| 1 | U8[3] | padding |
| 4 | U32 | length of the frame |
| 8 | U8[] | one FLAC frame |

Sent between a begin and an end. Every frame holds exactly `frequency / 50`
frames of samples — 20 ms, **960** at the gateway's 48 kHz — in FLAC's
fixed-blocking mode, numbered from zero at each begin. The FLAC stream header,
`STREAMINFO`, is never sent: everything in it follows from the format the client
set and that block size, so the client builds it (`vnc_audio::streaminfo`). An
unsigned format has the top bit of every sample flipped before it is encoded,
mapping it onto the signed range with silence on zero, and flipped back after;
the gateway asks for a signed one, so it never flips. Decoded samples are
interleaved, little-endian, and bit for bit what wlshare captured.

FLAC is lossless, so the gateway's Opus encode stays the only lossy step on the
way to the browser, and passthrough stays exactly the desktop's samples. On the
RFB connection music and speech cost about two-thirds of their 1.5 Mbit/s PCM
rate or less, and a desktop playing nothing, whose capture still runs, a few
bytes a frame.

## What the gateway does with it

`src/vnc_audio.rs` is the wire and the decoder; `src/vnc.rs` keeps the
extension's state per connection as `Audio`: `Off` where no sound was asked for
and on the Apple dialects, `Asked` from the handshake, `Announced` once the
rectangle has arrived and the stream has been turned on, and `Unanswered` once
pixels have arrived with no announcement in front of them — a server that
announces late is still taken.

- The announcement is answered after the update it arrived in, not inside it,
  so the enable goes out once however the update was framed.
- `begin` makes a fresh FLAC decoder and publishes the negotiated format on
  `AudioBridge`; `end` drops the decoder and clears the format, which leaves an
  open `/ws/audio` response filling with silence rather than ending. A desktop
  going quiet must not cost the listener its stream.
- Each frame is decoded by symphonia's FLAC decoder into interleaved
  little-endian 16-bit stereo, which is what was asked for and what the queue
  takes, and goes to the bridge as one wave buffer. From there the path is every
  target's: the queue, the Opus or passthrough encoder, `/ws/audio`
  ([Audio frames](architecture.md#audio-frames)).
- A frame that does not decode, or does not hold exactly 960 stereo frames, is
  dropped with a warning: each FLAC frame decodes on its own, so it costs its
  20 ms and nothing after it. So is a frame outside a begin and an end.
- A frame length past 64 KiB is read past rather than allocated: a frame is
  3840 bytes of samples before compression, and FLAC adds a few header bytes at
  worst, so anything larger is a server that has lost its framing.

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
for 20 ms buffers. The process callback runs on that capture's own loop thread
rather than on the graph's real-time one — `RT_PROCESS` is deliberately not set,
since the callback encodes, allocates, takes a mutex and wakes a task, none of
which is real-time safe: on the data thread it could stall the whole audio graph
and give every application on the host an xrun. It encodes with `flacenc` there,
off the session's task, and queues each finished frame in a sixteen-deep queue,
dropping the oldest when a client cannot keep up — a dropped frame is a 20 ms
hole, and a stalled capture callback is worse. A set-format on a running stream
restarts the capture in the new format.

PipeWire honours its own quantum before settling on the requested one, so the
first buffers of a session are often smaller than 20 ms — 512 frames where 960
were asked for, measured. The encoder keeps what does not fill a frame for the
next buffer, so every frame on the wire is exactly 20 ms.

A headless session still has a sink to capture: PipeWire's Dummy Output is one,
and no `null-sink` needs configuring.

## Measured

These were taken while the RFB connection carried raw PCM, before FLAC replaced
it. What reaches the browser is unchanged, since FLAC is lossless and the gateway
hands the bridge the same samples; the RFB leg's own rate has not been measured
on a live desktop.

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
