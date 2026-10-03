# Desktop audio over VNC with wlshare

How a wlroots-based Wayland desktop behind wlshare hands the gateway its sound,
on the RFB connection it already has, so a `wlshare` target plays through the
browser the way an RDP one does. Standard RFB carries pixels and a clipboard and
nothing else; this is wlshare's private audio extension, which carries the sound
as Opus, or as lossless FLAC on a target that asks for that, and borrows its
control messages from the QEMU Audio extension `rfbproto` registers. Either way
the gateway codes nothing: wlshare makes the stream the browser decodes, and the
gateway passes it as it came. It is announced the way the density extension is
([`wlshare-density.md`](wlshare-density.md)): a `wlshare` session started with
sound lists a pseudo-encoding, and wlshare announces that it speaks it
before anything is turned on.

Measured 2026-09-09 on `workstation-wsl`, a headless sway with one `HEADLESS-1`
output and PipeWire's own dummy sink, through a one-off WebSocket probe. The
maintained `tests/ws_probe.py --audio` probe exercises the current socket path.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), which,
while any client listens, has the desktop play into a PipeWire sink of its own
rather than the host's, so the host is silent the way a remote desktop's is. It
captures that sink's monitor — what the desktop is playing, whatever is playing
it — and sends it in the format the client asked for, coded as the client asked:
Opus packets, or FLAC frames. How it makes that sink and captures it is in its
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-audio-extension).

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "wlshare"
host = "127.0.0.1"
port = 5900
username = "me"
password = "…"
```

`audio_bitrate` and `audio_adaptive` mean on this target what they mean on an
`rdp` one, with wlshare's encoder in place of the gateway's: Opus at
`audio_bitrate`, walked down toward the floor sound-opus fixes while the
browser's link is behind. They tune a session started with its sound as Opus;
one started with it lossless is sent FLAC, which has no rate. The
format is chosen at the picker and is the gateway's to ask for on the wire;
wlshare has no key for it, and neither has the target.

`subtype = "wlshare"` says the server is wlshare, and Sound, chosen under the
target at the picker before Start as Opus or lossless, is what makes the gateway
list the extension to it. A session started with it off lists none, and the desktop keeps playing on
the host. The choice is offered on a `wlshare` target and on no other `vnc`
target: a plain one is read through the
RFB baseline, which carries no sound, `ard` carries none either, and
`ard-high-performance` takes its sound from the media stream. A `wlshare`
target pointed at a server that is not wlshare, or at a wlshare with its own
switch off, lists the pseudo-encoding, hears no announcement, and runs in
silence. QEMU's own audio extension, which carries raw samples, is never asked
for.

wlshare's own `audio` key (default `false`) is the server's side of the same
switch: with it off the extension is not announced, and a client that lists the
pseudo-encoding is told nothing.

## The wire

The extension is wlshare's, and its messages and their layouts are in wlshare's
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-audio-extension):
two pseudo-encodings, `WLSF` for the sound and `WLOP` for it as Opus, the QEMU
Audio extension's message type `255` for the controls, and message type `0xE4`
for a frame. What the gateway asks of it:

- **What it lists.** `WLSF`, and `WLOP` beside it unless the session was started
  with its sound lossless. QEMU's own pseudo-encoding, `-259`, is not listed:
  what it promises is raw samples.
- **The format.** Signed 16-bit, 2 channels, 48 000 Hz, which is Opus's own
  rate — wlshare codes Opus only at 8, 12, 16, 24 or 48 kHz — and one a browser
  plays as it is. So every frame holds **960** samples, 20 ms, and nothing is
  flipped, as an unsigned format's samples would be.
- **When it speaks.** Set-format, set-bitrate and enable go out once, when the
  announcement arrives, and a set-bitrate again each time the gateway's walk
  moves.
- **What it refuses.** Nothing else under type 255 is advertised by this client,
  and a submessage or operation it does not know is fatal: the QEMU submessages
  share no length field, so one that cannot be measured leaves the stream at an
  offset nothing recovers from. That includes QEMU's operation 2, raw data.
- **The headers nobody sends.** `OpusHead` is never on the wire: everything in it
  follows from the format the gateway set, with the encoder's lookahead, 312
  samples at 48 kHz, as the pre-skip, so the gateway states it to the browser
  itself (`vnc_audio::PASSED_OPUS`). FLAC's `STREAMINFO` is not sent either, and
  the page's decoder is held to the format and the 960-sample block.

An Opus packet is the stream the gateway's own encoder makes of an RDP host's
sound: both are [sound-opus](https://github.com/andrewtheguy/sound-opus), a
repository of its own that the gateway and wlshare each pin by release tag.

Opus is the one lossy step on the way to the browser, made once, by wlshare, and
a session started with lossless sound has none
([Lossless sound](architecture.md#lossless-sound)). As FLAC, music and speech
cost about two-thirds of their 1.5 Mbit/s PCM rate or less on the RFB
connection and on the browser's, and a desktop playing nothing, whose capture
still runs, a few bytes a frame; as Opus they cost the target's bitrate on both.

## What the gateway does with it

`src/vnc_audio.rs` is the wire and what the browser is told of each stream;
`src/vnc.rs` keeps the
extension's state per connection as `Audio`: `Off` where no sound was asked for
and on the Apple dialects, `Asked` from the handshake, `Announced` once the
rectangle has arrived and the stream has been turned on, and `Unanswered` once
pixels have arrived with no announcement in front of them — a server that
announces late is still taken.

- The announcement is answered after the update it arrived in, not inside it,
  so the enable goes out once however the update was framed.
- `begin` publishes the negotiated format on `AudioBridge` and `end` clears it,
  which leaves an open `/ws/audio` response filling with silence rather than
  ending. A desktop going quiet must not cost the listener its stream.
- Each frame between a begin and an end goes to the bridge as it came, one unit
  a frame (`AudioBridge::unit`), and from there to `/ws/audio` behind an
  `audioFormat` that says `passthrough` (`AudioListener::into_passed`): `opus`
  with the `OpusHead` above, which the browser's WebCodecs decoder takes, or
  `flac` for the page's own decoder. Nothing here decodes, checks or re-encodes
  a frame; the browser's decoder is what refuses a bad one. An empty frame, one
  past the audio socket's 16-bit packet length, or one outside a begin and an
  end is dropped with a warning.
- The Opus bitrate is the target's: `audio_bitrate` is named to wlshare with the
  enable, and where `audio_adaptive` is on, the walk that would move an encoder
  here — sound-opus's, the crate wlshare codes with too — moves wlshare's
  instead. The pump that feeds `/ws/audio` measures how
  long its sends block, as for any target, and each rate the walk arrives at
  goes through the bridge to the VNC engine, which sends it as a set-bitrate
  ([Audio frames](architecture.md#audio-frames)). A new listener's walk starts
  from the ceiling. Silence is not shed, since a passed stream has no samples
  here to tell it by.
- A listener that falls behind the queue loses its oldest units, as one of PCM
  does, but these were coded: the pump sends a gap frame
  ([Audio frames](architecture.md#audio-frames)) ahead of the next batch, and
  the player resets its Opus decoder there. FLAC frames decode alone, and its
  player takes no notice.
- A frame length past 64 KiB is read past rather than allocated: a frame is
  3840 bytes of samples before compression, and FLAC adds a few header bytes at
  worst, so anything larger is a server that has lost its framing.

So the gateway needs no codec for wlshare's sound.
wlshare's FLAC encoder is libFLAC, through
[sound-flac](https://github.com/andrewtheguy/sound-flac), which the gateway
pins too, for the FLAC it codes of an RDP host's sound.

Audio shares the TCP stream with the pixels, which is the one cost of carrying
it in band. wlshare drains its capture queue before every framebuffer update, so
sound is never held behind a ZRLE or VP9 frame it was ready before; the browser's
300 ms lead clamp absorbs what is left.

## Measured

These were taken while the RFB connection carried raw PCM and the gateway coded
the Opus. The stream that reaches the browser is the same one, now made by
wlshare with the same encoder at the same rate; neither leg has been measured
again on a live desktop since.

With a 6-second 440/660 Hz stereo tone playing into the default sink through
`pw-play`, then silence, on this host:

```
306 frames, 306 packets, 73746 bytes     tone:    241 bytes a packet
192 frames, 192 packets,   576 bytes     silence:   3 bytes a packet
```

And the control, wayvnc on another host, reached as a `wlshare` target and asked
for sound:

```
vnc: the server carries no audio; the session runs without sound
0 frames
```

The format is still announced on `/ws/audio` there — the gateway advertises one
format and writes the header before any remote channel is up — so a server
without the extension is silence measured in packets, not in the announcement.

Exercise the current path:

```sh
REMOTEX_PROBE_PASSWORD=… uv run tests/ws_probe.py \
  --port <gateway port> --target <name> --user <user> --seconds 8 --audio
# meanwhile, on the host
pw-play <some>.wav
```
