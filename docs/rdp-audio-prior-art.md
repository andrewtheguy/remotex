# Other implementations of remote desktop audio

Projects that carry desktop sound over RDP, collected while chasing a stutter
remotex has and the official Microsoft client does not (see the open question at
the end). Each entry says what it implements and why it is worth reading; none of
them has been read in depth except Guacamole.

The useful split is which side of the wire a project sits on. remotex receives
`rdpsnd` and plays it, so the **client** entries are the ones doing the same job
and making the same decisions — when to play a buffer, what to do when one is
late, what to do when the queue is behind. The server entries are the other end
of the same `[MS-RDPEA]` contract and produce the stream rather than schedule it.

Confidence is marked. "Confirmed" means the audio claim was checked against
source or project documentation, not inferred from a project's reputation.

## Client side — receives and plays audio

### FreeRDP — confirmed

The reference implementation, and the largest body of prior art on this problem.
`rdpsnd` for output and `audin` for microphone; uncompressed PCM needs no extra
library, and compressed formats come through FFmpeg. PulseAudio is detected
first, then ALSA.

- Source: [`channels/rdpsnd/client/rdpsnd_main.c`](https://github.com/FreeRDP/FreeRDP/blob/master/channels/rdpsnd/client/rdpsnd_main.c)
- [Audio guide](https://mintlify.wiki/freerdp/freerdp/guides/audio) ·
  [Multimedia Redirection wiki](https://github.com/FreeRDP/FreeRDP/wiki/Multimedia-Redirection)

**Read this one first.** It is the only project here with both a mature
client-side scheduler and a public issue history about audio timing, including
[#6798, "Audio redirection latency is not stable and high"](https://github.com/FreeRDP/FreeRDP/issues/6798)
— which is remotex's symptom, described by someone else's client. Whatever the
answer turns out to be, it has probably been argued about there.

### IronRDP — confirmed

Pure Rust, and the stack remotex already builds on. It ships `ironrdp-rdpsnd` as
a channel crate behind an `rdpsnd` feature; remotex does not use it and
implements `src/rdp_audio.rs` instead. That makes it the one like-for-like
comparison in the same language — the same protocol, the same crate ecosystem,
a different set of choices.

- [github.com/Devolutions/IronRDP](https://github.com/Devolutions/IronRDP) ·
  [`ironrdp-rdpsnd`](https://crates.io/crates/ironrdp-rdpsnd)

### Remmina — confirmed, but derivative

GTK client whose RDP support is FreeRDP with a settings layer over it, so its
audio behaviour is FreeRDP's. Worth reading for how the options are presented to
a user, not as an independent implementation.

- [github.com/FreeRDP/Remmina](https://github.com/FreeRDP/Remmina)

## Browser gateways — the same shape as remotex

### Apache Guacamole — confirmed, and measured

The closest thing to a control experiment, and the only project here that has
actually been tested against the same Windows host over the same link.

It ships **one** audio encoder, `src/libguac/raw_encoder.c`, emitting
`audio/L16;rate=44100,channels=2` — uncompressed PCM at 1.41 Mbit/s, flushed once
per wave PDU. No Opus, no Vorbis, no AAC anywhere in 1.6. The browser side plays
it through Web Audio with no WebCodecs involved, so it needs no secure context.

**It does not stutter.** That measurement is what ruled bandwidth out as the
cause of remotex's problem, and what `audio_codec = "pcm"` was built from.

- [guacamole-server](https://github.com/apache/guacamole-server) ·
  [guacamole-client](https://github.com/apache/guacamole-client)

### Myrtille — confirmed

HTTP(S) gateway to RDP and SSH in .NET/C#, streaming display *and* audio to a
browser with no plugin. Apart from Guacamole it is the only browser-based RDP
gateway found that claims audio at all, which makes it the closest architectural
sibling remotex has.

- [github.com/cedrozor/myrtille](https://github.com/cedrozor/myrtille) ·
  [myrtille.io](https://www.myrtille.io/)

## Server side — produces audio

The other end of the contract. Less directly useful for a scheduling question,
but this is what a gateway is reading from when the remote is Linux rather than
Windows.

### xrdp — confirmed

Implements `[MS-RDPEA]` output redirection over PulseAudio, and `[MS-RDPEAI]`
input, deliberately interoperable with Microsoft's own client and FreeRDP. The
PulseAudio dependency is now a live problem for it: distributions moving to
PipeWire broke the existing modules.

- [github.com/neutrinolabs/xrdp](https://github.com/neutrinolabs/xrdp) ·
  [pulseaudio-modules](https://github.com/neutrinolabs/pulseaudio-modules)
- [Audio redirection overview](https://deepwiki.com/neutrinolabs/xrdp/5.3-audio-redirection)

### ogon — confirmed as a project, audio unverified

RDP server and session manager for Linux desktops in C/C++, supporting modern RDP
extensions and device redirections. Whether audio is among them was not checked.

- [github.com/ogon-project/ogon](https://github.com/ogon-project/ogon)

## Exists, audio not verified

Listed so they are not searched for twice, not as recommendations.

- **PyRDP** — [github.com/GoSecure/pyrdp](https://github.com/GoSecure/pyrdp).
  RDP monster-in-the-middle that records sessions and replays them. Whether the
  replay carries audio was not confirmed. If it does it is an unusual angle worth
  the detour: a recorder has to parse the channel correctly without ever playing
  it, so its parser is separable from any scheduling policy.
- **rdesktop** — has carried a sound channel historically, did not surface in
  these searches, and is largely superseded by FreeRDP.
- **gnome-remote-desktop** — GNOME's RDP server; audio is believed to go through
  PipeWire. Unconfirmed.

## The open question this was collected for

remotex's RDP audio stutters during movie playback on a 2.5 GbE link. Microsoft's
own client does not, at full picture quality, on the same host. Ruled out so far:

- **Bitrate.** Guacamole sends 22× the bytes at 1.41 Mbit/s and is clean.
- **Encoding cost.** HE-AAC at 64 kbps was built and made no difference, which is
  what `audio_codec = "pcm"` replaced.
- **Video encoding blocking the read loop.** All tile and frame encoding runs in
  `spawn_blocking` (`src/encode.rs`).

Still standing, and the reason the client-side entries above are the interesting
ones: remotex can *lose* audio in the gateway in a way guacd structurally cannot.
`AudioBridge` drops its oldest buffers when the pump falls behind
(`src/audio.rs`), and the pump awaits the same bounded queue video frames use —
four deep under `render_type = "video"` (`src/session.rs`). Guacamole has no such
queue and never discards a wave buffer.

The measurement that would settle it is audio seconds delivered ÷ wall-clock
elapsed. Guacamole is 1.0 by construction; anything below 1.0 here means the
gateway is deleting sound before it reaches a browser, which would sound
identical on every codec.
