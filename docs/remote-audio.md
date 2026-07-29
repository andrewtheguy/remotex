# Remote audio

remotex carries no audio, in any engine. There is no record for it on the wire and
no playback in either client, and that is a decision rather than an omission: the
one case that wanted it is already covered by software that exists, and building it
here would be four greenfield pieces at once for a want that a second window
answers.

This document is the record of that decision and of the design it displaces, so
that revisiting it later starts from what was already worked out rather than from
scratch.

## The case it was for

A **Windows** desktop reached over RDP, whose sound has no way to a Mac. macOS has
AirPlay both ways between Apple devices and Windows has nothing equivalent, so
audio playing on that desktop is audio that stays there.

A Mac guest never needed remotex for this. `rxa`'s Macs are VMs here, and a VM's
audio is the host application's to route — enabling it in UTM is one setting and
does not involve remotex at all. VNC has nothing to carry either way: RFB has no
audio in the protocol.

So this was only ever about Windows over RDP, and about one client.

## What covers it instead

Four routes, cheapest first. Each of them leaves remotex alone.

- **Microsoft's own RDP client.** Audio redirection is part of RDP and the official
  macOS client implements it, so a session opened there has sound with nothing
  installed on the Windows side. The cost is using a second application for the
  sessions where audio matters, which is a smaller cost than the feature.
- **Sunshine on the Windows box with Moonlight on the Mac** (or Parsec). Hardware
  H.264/HEVC video *and* audio, built for low latency. Worth naming because it
  covers the case that motivated both this and
  [Smoother video from an RDP guest](roadmap.md#smoother-video-from-an-rdp-guest) —
  a video stream by design will beat a screen-update protocol at playing video,
  however much the tile path improves.
- **[Scream](https://github.com/duncanthrax/scream) as an audio-only bridge.** A
  virtual sound card for Windows that publishes what plays through it as a raw PCM
  multicast stream (`239.255.77.77:4010`). Receivers ship for Linux and Windows;
  macOS has no official one, but [Roar](https://github.com/tyllj/Roar) is a
  Scream-compatible service for it. This is the closest in spirit to what was
  wanted — remotex stays the desktop and sound simply arrives beside it — and the
  least travelled.
- **[TuneBlade](http://www.tuneblade.com/) into the Mac's AirPlay Receiver.**
  Literally AirPlay for Windows: a tray application that streams system audio to
  AirPlay targets, with a real-time mode. **Verify before relying on it**: its
  documented targets are Apple TV, AirPort Express and Shairport-style receivers,
  which is AirPlay 1, while a Mac's own AirPlay Receiver is AirPlay 2 with pairing.
  Whether Apple's receiver accepts a third-party sender is the open question, and it
  is a five-minute test rather than an argument.

## The design, if it is ever built

Two hops, and deliberately two different answers: **uncompressed from the RDP
server, compressed to the client.** That way the chain has exactly one lossy step
and it is ours, at a bitrate we choose, rather than whatever a Windows server
happened to negotiate.

**Server to gateway.** `ironrdp` already has the channel behind its `rdpsnd` feature
(MS-RDPEA): implement `RdpsndClientHandler` — `get_formats() -> &[AudioFormat]` to
say what we accept, then `wave(format_no, ts, data)` per buffer — and the crate
intersects our list with the server's, so nothing we did not ask for arrives.
Advertising `WAVE_FORMAT_PCM` alone is therefore the whole of the quality decision:
a server with ADPCM or MP3 on offer cannot choose them, and no upstream encoder ever
touches the audio. It costs about 1.4 Mbit/s at 44.1 kHz stereo 16-bit, paid on the
gateway-to-host link rather than the one to the client.

**Gateway to the macOS viewer.** Opus, because macOS decodes it without help:
`kAudioFormatOpus` is in `CoreAudioTypes`, so `AVAudioConverter` handles it and the
viewer bundles no decoder. The encoder would be libopus on the gateway — another
vendored C dependency in a tree that already builds several — where AAC would mean
an encoder with licensing baggage on the Linux side for a format macOS is no better
at receiving.

The two hops have to agree on a sample rate, and that decides whether a resampler
joins them. libopus takes 8, 12, 16, 24 or 48 kHz and nothing else, while a server
left to itself will offer 44.1 — so **advertise the PCM format at 48 kHz** and the
conversion never has to exist. What to do when a server refuses that rate is the
first thing to settle, since the fallback is either a resampler or no audio.

Four things neither hop gives us:

- **Audio has to be opt-in per client.** The SPA's batch parser discards a whole
  frame when it meets a record op it does not know, so a gateway that simply starts
  sending audio records blanks the browser. A client asks for audio and gets none
  until it does — which is also the mute control, and the negotiation the wire
  otherwise has no room for. It is still a wire change: `PROTOCOL_VERSION` 5, which
  the viewer's `ProductInfo` pins itself to.
- **It must not queue behind tile encode.** Audio is paced by a clock and tiles are
  paced by damage, and a large repaint spends tens of milliseconds encoding on the
  very task that would carry audio through `frame_tx`. One queue for both makes
  every repaint an audible gap.
- **A jitter buffer in the client.** Nothing about a WebSocket delivers at the rate
  a sound card consumes, and `wave`'s `ts` is the server's clock, not ours. In the
  viewer, playback is an `AVAudioPlayerNode` fed from that buffer.
- **A decision about what happens with a client that cannot play it attached.** The
  engine is carrying audio either way; the choices are to not ask the server for it
  until someone can play it, or to keep pulling it and drop it.

### The browser is the harder half

Not "the same work again". The gateway half above is shared, but the client half is
not, and the browser's version is the more awkward one:

- **Decode.** The viewer gets Opus from `CoreAudioTypes` for free. In a browser, raw
  Opus packets are not playable through `decodeAudioData`, which wants a container
  (Ogg or WebM) rather than bare packets — so it means WebCodecs `AudioDecoder`.
  That is fine in Chrome; **Safari's support for `AudioDecoder` specifically is
  unverified here**, and on a Mac Safari is a likely client.
- **Playback.** `AudioContext` starts suspended until a user gesture, which the SPA
  has one of already, and a jitter buffer that does not glitch means an
  `AudioWorklet` with a ring buffer rather than a chain of `AudioBufferSourceNode`s.

### What remotex would add that the routes above do not

Audio inside the one session — arriving over the same socket, following a target
switch, subject to the same takeover — and audio in the browser at all. Neither is
needed today, which is the whole reason this is a document rather than a plan.

`rxa` would be a separate piece of work again, and a cheaper one: ScreenCaptureKit
can capture audio alongside video (`capturesAudio`), so a Mac guest's sound never
needs a channel of its own. It is also the case that wants this least, since UTM
already routes it.
