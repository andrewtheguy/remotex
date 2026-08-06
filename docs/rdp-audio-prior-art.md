# Other implementations of remote desktop audio

Projects that carry desktop sound over RDP, collected while chasing a stutter
observed in remotex but not in the official Microsoft client. The investigation
changed remotex's transport; the result is recorded at the end. Each entry says
what it implements and why it is worth reading; none of them has been read in
depth except Guacamole.

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

## What this investigation changed

A stutter was observed during movie playback on a 2.5 GbE link. Microsoft's own
client did not reproduce it, at full picture quality, on the same host. The
investigation ruled out:

- **Bitrate.** Guacamole sends uncompressed audio at 1.41 Mbit/s and is clean.
- **Encoding cost.** HE-AAC at 64 kbps was built and made no difference, which is
  what `audio_codec = "pcm"` replaced.
- **Video encoding blocking the read loop.** All tile and frame encoding runs in
  `spawn_blocking` (`src/encode.rs`).

The remaining structural difference was a loss path in the gateway. The audio
pump awaited the same bounded outbound queue as video frames — four deep under
`render_type = "video"` — and a pump stalled behind picture traffic stopped
draining `AudioBridge`, whose bounded queue then dropped old wave buffers.

That shared path no longer exists. Audio has its own `/ws/audio` WebSocket and its
own outbound queue; opening the socket is the subscription and closing it stops
sound. `AudioBridge` can still drop its oldest buffers if the audio consumer itself
falls behind, but video backlog can no longer cause that lag. See the audio section
of [`architecture.md`](architecture.md).

If a stutter recurs, the useful measurement remains audio seconds delivered ÷
wall-clock elapsed. Anything below 1.0 means sound is still being discarded before
it reaches the browser, which would sound identical on every codec.

## The resize that took the sound with it

A second investigation, 2026-08-05: after an RDP resize — manual or window-driven —
sound stopped and never came back. The gateway side was ruled out first: an RDP
resize is a Display Control request, not a reconnect, and nothing on that path
touches `arm_audio`, `evict_audio`, or the `AudioBridge`, which is a broadcast that
cannot be used up. The kill was in FreeRDP's `rdpsnd` and the wrapper's device
claim, together.

The chain, confirmed in freerdp-3.30.0 source and fixed in `libfreerdp-prebuilt`:

1. A real size change costs a Deactivation-Reactivation Sequence, and the server
   closes and reopens the audio channel across it. Observed directly: one
   `freerdp-e2e` session against the Windows 11 test host logged
   `[dynamic] Loaded rust backend for rdpsnd` twice, two seconds apart.
2. `rdpsnd_on_close` frees the device and nulls `rdpsnd->device`, but never resets
   the plugin's `isOpen` or `wCurrentFormatNo`. Nothing does — the `TRUE` in
   `rdpsnd_ensure_device_is_open` is the only write that flag has.
3. The reloaded channel mints a new device, and the first wave finds `isOpen` still
   true and the format number unchanged — with exactly one advertised format it is
   always 0 — so `rdpsnd_ensure_device_is_open` skips `Open` entirely and calls
   `Play` on a device that owns nothing.
4. The wrapper's `play` refused any device that had not claimed the bridge through
   `Open`. `Play`'s return value is a latency, not a status, so rdpsnd logged
   nothing, and every wave buffer was discarded for the rest of the session. The
   log signature is exact: `audio: the remote closed its audio channel`, a
   re-negotiated format, and then never again `audio: rdpsnd playing`.

The fix is in the wrapper, where the claim lives: `play` adopts a *vacant* claim,
because a vacant claim now means a device rdpsnd will never open. Only a vacant
one — a device that lost the claim to a live winner is refused exactly as before,
which is what keeps two transports from interleaving into one sink. The format is
not in doubt on adoption: a wave's format number indexes the client format list
that same device built through `FormatSupported`, which accepts nothing but the
format it was asked for. Upstream's half of the bug — `rdpsnd_on_close` leaving
`isOpen` set — belongs in a FreeRDP issue.
