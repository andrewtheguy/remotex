# Remote audio

The first audio path is deliberately narrow: **sound from a Windows RDP target
to the browser through the remotex gateway**. RDP already redirects the sound to
its client; remotex requests that channel, re-encodes what arrives as Opus, and
sends it as audio frames on the session's own WebSocket, where the browser decodes
and schedules it.

> **The design is settled; what is left is a deployment constraint.** Sound arrives, in
> the right format, in stereo, starting and stopping on its own, and **the latency the
> previous design was called experimental for is largely gone**: the live Windows target
> was heard through the in-band path on 2026-07-29, and the couple of seconds of delay
> it used to carry is much smaller. That is the schedule ceiling working, and it settles
> the question in
> [Latency, and what has been ruled out](#latency-and-what-has-been-ruled-out) — the
> delay *was* browser-side, held where nothing could see it, exactly where the old
> design had no mechanism to reach.
>
> **Opus-only survived contact with Safari**, which was the risk this took: it plays in
> Chrome, in macOS 26 Safari, and on a real iPhone. It **did not work in the iOS
> Simulator**, and that is the whole of what is known — the cause has not been looked
> into, so nothing here should be read as blaming the simulator or clearing this path.
>
> Two other things are open, and neither is about a browser refusing the codec. The
> origin is one: WebCodecs is **secure-context only**, so a client reaching this gateway
> at a LAN address over plain HTTP has no decoder at all. Every run above went through
> loopback, so serving audio to a phone on a real network needs TLS and has not been
> exercised. The other is the dynamic MS-RDPEA transport, which has still never carried
> a byte from a real server.
>
> **This is a browser feature, and changing it needs no check against the macOS
> viewer.** The viewer has no audio at all — no decoder, no setting, and now not even
> a wire it could receive audio on — so there is nothing there for a change here to
> break, and asking whether one still suits the viewer is friction bought for nothing.
> Viewer audio is planned ([`roadmap.md`](roadmap.md)) as a *second* representation
> from the same queue; when it exists it will bring its own tests, and until then this
> path answers only to a browser.

## Why the browser owns the schedule

This began the other way round, and the reversal is the most important thing in this
document.

The original design handed the browser a live HTTP response and let it play:

```html
<audio autoplay src="/api/session/audio?session=…"></audio>
```

That is genuinely less code — no wire record, no decoder, no jitter buffer — and it
gives away the one thing that turned out to matter. **A media element's schedule
belongs to the browser and cannot be reclaimed.** It resumes where it stopped and
never skips forward, so whatever it fell behind by during start-up buffering, or one
hiccup, or one suspended laptop lid, it stayed behind by for the rest of the session.
Two mechanisms existed only to work around that, and both were palliative: the
gateway trickled silence *below* real time so a listener could drain back toward live,
and the panel nudged `playbackRate` up when a standing buffer appeared (which was
never once observed to happen).

Apache Guacamole solves it structurally, and its client is where to look —
`guacamole-common-js/modules/AudioPlayer.js`, in `RawAudioPlayer.sync()`:

```js
nextPacketTime = Math.min(nextPacketTime, now + maxLatency);   // maxLatency = 0.3
```

Every packet is scheduled explicitly through Web Audio, on a timeline the *client*
holds, and that one line bounds the latency by throwing the excess away. So the fix
was never a codec or a container: it was owning the playback clock. Doing that means
the bytes have to arrive as bytes rather than as a media source, which is what put
audio on the WebSocket and deleted the endpoint, the container and the keepalive
together.

What is **not** copied from Guacamole is its wire: guacd sends raw PCM
(`audio/L16;rate=…,channels=…`) in-band as base64 blobs, spending the ~176 kB/s that
Opus takes to about 10. Keeping Opus and decoding it with WebCodecs keeps both
properties at once, where Guacamole has only the bounded latency and the old design
here had only the bandwidth.

## What is implemented

**RDP to gateway.** An RDP target with `audio = true` registers MS-RDPEA and
MS-RDPEFS at connect (`src/rdp.rs`, `src/rdp_audio.rs`), advertises PCM, and
forwards each redirected wave buffer. PCM is the right input because it is the one [RDPSND audio
format](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/30a6cc00-31c4-4e15-9aa4-95a5c5074697)
both clients and servers are required to support; accepting a compressed RDP
format would only make this depend on what one Windows version happens to offer.
A Windows host given the choice picks AAC, which is exactly the dependency worth
refusing.

**Two channels carry it, and the server picks.** MS-RDPEA defines both a *static*
virtual channel `rdpsnd` and a *dynamic* one, `AUDIO_PLAYBACK_DVC`. IronRDP
implements only the static one, so remotex registers that plus its own
`AudioPlaybackDvc` — the same conversation over `DvcProcessor`, since `Rdpsnd`'s
state machine answers in `SvcMessage`s and cannot be reused. Both feed one queue
and `AudioBridge::claim_transport` decides which one owns it; a server driving both
would otherwise interleave two streams into one listener with nothing to report it.

**A third channel is what actually unlocked it: `rdpdr`.** Registering the audio
channels is not enough. Against a live Windows 11 host, `rdpsnd` plus
`AUDIO_PLAYBACK_DVC` plus `enable_audio_playback` produced a completely silent
session — the server accepted `rdpsnd`, never sent a byte on it, and opened no
dynamic audio channel either, while MS-RDPECLIP handshook normally on the same
connection. Windows gates audio redirection on *device* redirection (MS-RDPEFS)
being advertised. Advertising `rdpdr` with no devices and an inert backend
(`NoopRdpdrBackend`) made the same host start redirecting immediately, over the
static channel, with no other change.

That is behaviour rather than specification, and the two written sources point in
opposite directions, which is why it took a live test to settle:

- FreeRDP forces device redirection on whenever audio playback is on —
  "rdpsnd requires rdpdr to be registered", `client/common/cmdline.c` — so its
  `/sound` run advertises `rdpdr` without being asked to;
- [MS-RDPEFS] Appendix A\<1\> states only the converse: without `RDPSND`
  advertised, the server issues nothing on `RDPDR`.

Reading FreeRDP's source is what found it. No gateway-side logging could have,
because "the channel is open and quiet" and "the remote is playing nothing" look
identical from this end. IronRDP's own `Rdpdr` doc comment says the same thing
from the other direction, so both reference implementations treat the pair as
inseparable. Guacamole is a third: it loads `rdpdr` and `rdpsnd` together,
unconditionally, whenever printing, drive redirection *or* audio is enabled — so no
independent client ever ships one without the other, which is why nothing documents
the dependency.

The transport the server then chooses is genuinely its own: this host serves
remotex over static `rdpsnd`, and serves FreeRDP — which advertises `rdpsnd` both
statically and dynamically — over `AUDIO_PLAYBACK_DVC`, naming it in its log
(`[dynamic] Loaded mac backend for rdpsnd`, then `[dynamic] Server Audio
Formats`). Hence both halves stay registered here.

Exactly **one** format is advertised — 44100 Hz, 16-bit stereo — and the second
reason for that is not about codecs at all. A wave buffer names its format by
index into the list the *client* advertised, and IronRDP is loose about which list
the index refers to: it advertises the intersection of our formats and the
server's collected through a `HashSet`, whose order is undefined, while its
`get_format` resolves an index against the *server's* list. With one advertised
format the intersection holds at most one entry and the index cannot be misread.
FreeRDP's trace against the same host confirms the client-list reading —
`wFormatNo: 3 [WAVE_FORMAT_AAC_MS]` indexes *its* advertised list, not the
server's.

The cost is that a server offering no matching format redirects nothing, which the
log makes legible: with one format the negotiated line appears and the
first-buffer line never does; with none matching, neither does.

`enable_audio_playback` on the connector config matters as much as the channel.
Left false, IronRDP sets `NO_AUDIO_PLAYBACK` in the Client Info PDU and the server
redirects nothing however carefully RDPSND was negotiated.

RDPSND must be negotiated when the RDP connection is established. A browser that
asks for audio later cannot add the channel to an existing connection, so an
audio-enabled RDP target requests redirection from the start and discards audio
while nobody is subscribed.

**Gateway to browser: audio frames on the session's WebSocket, and only for a client
that asks.** There is no audio endpoint. A browser sends

```json
{"type": "audio", "enabled": true}
```

and the gateway answers with one text control message describing the stream,

```json
{"type":"audioFormat","codec":"opus","sampleRate":48000,"channels":2,"head":"T3B1c0hlYWQBAjgBRKwAAAAAAA=="}
```

followed by binary frames of kind `0x03`, each carrying one wave buffer's worth of
Opus packets:

```text
offset 0: u8  frame kind, always 0x03 (audio)
offset 1: u8  flags, always 0
offset 2: u16 packet count
offset 4: packets, each u16 length | length bytes
```

Four things about that shape are decisions rather than defaults:

- **`head` is `OpusHead`** (RFC 7845 §5.1), base64 for the same reason the cursor's
  PNG is: a text frame cannot carry bytes, and 19 bytes once a session is not worth a
  second binary frame kind. It is also exactly what WebCodecs takes as an
  `AudioDecoderConfig.description`, and it is what carries the encoder's pre-skip so a
  decoder discards its own delay instead of playing it as leading silence. It arrives
  **before** the first packet, on the same ordered channel, because a decoder
  configured afterwards has already thrown away the audio it was meant to decode.
- **`sampleRate` is 48000, not the 44100 the remote negotiated.** libopus encodes at
  48 kHz and nothing else; the resampling is `src/opus_stream.rs`'s business.
- **Packets are length-prefixed and counted.** An Opus packet does not carry its own
  size and one frame holds nine or ten of them, so lengths are necessary; the count
  makes a truncated frame detectable rather than a complete shorter one. The header is
  deliberately the same four bytes as the tile batch frame, so a reader of one finds
  nothing surprising in the other.
- **One frame per wave buffer**, not per packet. The tested host's 32 KiB buffers
  become 9 or 10 packets of 20 ms, so this is ~5 WebSocket messages a second rather
  than 50.

**Audio is opt-in, and that is what keeps the macOS viewer working.** The viewer
compares `protocolVersion` for **equality** (`GatewayClient.swift`) and ships as its
own DMG, so bumping that number would cost every installed copy a reinstall. Nothing
but a browser sends `{"type":"audio"}`, so a viewer's socket carries exactly the bytes
it carried before audio existed — there is no new wire for it to refuse, and
`PROTOCOL_VERSION` stays at 4. Guacamole arranges the same thing from the client end:
its client declares the audio mimetypes it can decode, and one that declares none gets
a stream that carries nothing.

**Audio does not wait behind pixels.** One socket carries both, so `src/wire.rs` gives
audio the one exemption in that file: an audio frame is emitted immediately and does
*not* flush a tile batch that is still filling. A control message flushes because a
resize invalidates the tiles before it; sound has no such relationship with pixels,
and a full repaint draining ahead of it is exactly when a client-side scheduler
starves. What remains is that a batch already being written delays audio by up to
`MAX_BATCH_BYTES` (256 KiB) — real on a slow uplink, and the accepted cost of one
socket. Guacamole has the same property.

**A quiet remote sends nothing at all, and getting there took two designs.** The tested
Windows host sends no `ServerAudioFormatPdu` while nothing is playing: the channel
opens, stays quiet, and closes again. So "no audio yet" and "no audio ever" are
indistinguishable from this end, and the first design had to paper over it — an
`<audio>` element whose stream goes dry *stalls*, so the gateway filled the gaps with
encoded silence, trickling at a fifth of real time because silence that kept pace with
the clock made every hiccup permanent. All of that is gone. A Web Audio schedule simply
has a gap in it, and the client resumes at its own cushion when packets return, so
**quiet costs zero bytes** where the keepalive cost 0.09 kB/s. The WebSocket's own ping
keeps the connection alive through a long silence, which an HTTP body could not do
without sending audio.

FreeRDP makes the same call one layer down, which is worth knowing before treating a
server's `Close` as a teardown: `rdpsnd_recv_close_pdu` only logs, deliberately
leaving the local audio device open, and `rdpsnd_ensure_device_is_open` reopens it on
the next wave.

The cost of all this is diagnostic, and it is real: a target whose host offers no
compatible format sounds exactly like one that is merely quiet. The gateway log is
where they differ — `audio: negotiated …` when the channel comes up, the
`no … PCM audio format` warning when the host offers nothing usable, and a line at
every subscription saying whether the channel is up at that moment.

**Why Opus, and why there is no container.** The response began as raw PCM in an
open-ended WAV, chosen because wrapping PCM needs a header rather than an encoder, and
kept until it had answered the question the design was built to ask: *does a browser
play an open-ended response progressively?* It does. But PCM at the negotiated format
is **176 400 B/s — about 1.4 Mbit/s** — which is wasteful on a LAN and unusable on
anything slower. Opus at 96 kbps carries the same sound for about a fifteenth of that.

Ogg came next, because a container was needed to hand a *media element* something
demuxable, and Ogg is the streamable one: a page can be flushed as soon as a packet
exists. With the element gone the container has no remaining purpose — a container
exists to delimit and describe packets, the WebSocket already delimits them, and
`audioFormat` describes them — so `src/opus_stream.rs` now hands back bare packets and
the `ogg` dependency is gone. What survived is `OpusHead`, the 44100→48000 resampling
libopus forces, and a fresh encoder per listener.

**The browser half** (`frontend/src/audioPlayer.ts`, `audioSchedule.ts`) is
Guacamole's `RawAudioPlayer` with three deliberate differences:

- **A cushion on a fresh start.** Guacamole schedules at `max(now, nextPacketTime)`,
  which is zero lead, so the very next jitter is another gap. The tested host paces
  itself to real time but not to a clock — inter-arrival measured 169–200 ms around a
  186 ms buffer — so `START_LEAD_S = 0.1` costs a tenth of a second and buys out the
  ordinary case. It applies again after every underrun, which is what makes a remote
  going quiet and coming back sound like a gap rather than a stumble.
- **A ceiling of `MAX_LEAD_S = 0.3`**, which is Guacamole's `maxLatency` unchanged.
- **A trim, not an overlap.** Guacamole's clamp moves the next start earlier while
  audio is still scheduled past it, so the two mix — which is why it needs
  `splitAudioPacket`'s quietest-point search to hide the seam. Here the already-scheduled
  tail is stopped at the new start and the front of the arriving buffer is skipped by
  the same amount, so there is no seam to hide: `source.start(when, offset)` takes the
  skip directly, with no copying and no resampling. Discarding audio to stay near live
  is the same choice the gateway's queue already makes when a consumer falls behind.

The arithmetic is a pure function with its own unit tests (`bun test src` in
`frontend/`), which is the only part of a player a test can reach: the properties
pinned are that nothing is ever scheduled in the past or past the ceiling, that a trim
never exceeds the buffer, and that the timeline never moves backwards — swept across
leads from −5 s to +60 s.

**WebCodecs is secure-context only, and that is a real deployment constraint.**
`AudioDecoder` is simply *undefined* on `http://` to anything but localhost, where the
`<audio>` element it replaces played a plain-HTTP response from any origin. So a
gateway reached over LAN HTTP has no audio until it is behind TLS. The SPA says which
of the two things is wrong rather than blaming the browser (`audioUnavailable` in
`audioPlayer.ts`), because "this browser cannot decode Opus" would send someone
hunting through browser versions for a problem that is the URL.

**Opus only, with no fallback in either direction.** A browser whose `AudioDecoder`
will not take Opus gets no audio and a line saying so. There is no raw-PCM path to
fall back to and the WAV representation Opus replaced is gone, deliberately: a
fallback would keep a second encoder, a second frame kind and a second set of failure
modes alive for a user who may not exist. That makes browser support something to
**test** — `server::tests::serve_a_test_tone`, on each browser that matters — rather
than something to hedge.

**The SPA** offers an Audio row in the floating menu for a session whose `connected`
message carried `audio`, and the row's button **is** the control: Enable audio /
Disable audio (`frontend/src/FloatingMenu.tsx`).

There is no audio panel any more. The native `<audio>` controls went first — a
scrubber and an elapsed time describe a recording that can be returned to, and their
Pause did not pause the remote, it dropped the listener behind live for the rest of
the session — and the panel that held them went with the element. What a docked panel
was left asking was one question, "am I listening to this", and it spent a strip of
the desktop to ask it.

Two things follow from the button rather than the panel, and both are improvements:
sound survives closing the drawer, where closing the panel used to stop it; and the
press is the *user gesture the whole path needs*. Enabling audio creates the
`AudioContext`, and a context created inside a click is one a browser will let play,
where one created any other way is suspended — on iOS with no way back. The old
element had to argue for `autoPlay` and then report a refusal whose cause was
invisible. The trade is that in-page volume went with the native controls, leaving the
system's own.

Nothing reports whether sound is *arriving*. The one thing the row says, when there is
something to say, is that this browser cannot play it — a missing decoder or an
insecure origin. "The remote is quiet" is not reportable, because from the gateway's
end a quiet remote and one that will never redirect are the same thing; the log is
where they differ.

## Lifetime and backpressure

Audio belongs to one **attachment** — one browser's socket — and that is the
simplification the move to the WebSocket bought. It ends when:

- that browser disables it;
- another client takes over the single session slot;
- the same browser reattaches (a reconnect starts with audio off);
- the target changes or disconnects;
- the RDP engine ends;
- the owner logs out; or
- the socket closes.

One mechanism covers all of them: the session slot holds the forwarding task's handle
and aborts it (`State::stop_audio`). What used to need two — a `oneshot` inside the
bridge for the cases where the engine kept running, plus the engine's own teardown —
is now one, because the pump's lifetime is the attachment's.

Aborting the task rather than dropping the queue is not incidental. The engine holds
its own `Arc` on the bridge and only releases it when it notices its input channel
closed, so "the bridge was dropped" happens some time *after* the desktop is gone.
Ending the reader is immediate. It also matters that the handle exists at all: a pump
parked in `recv()` on a quiet desktop would otherwise sit there until the remote's
next wave buffer, which may be never.

Authorisation went the same way. The endpoint needed the claim token in a URL because
an HTTP request arrives with nothing else to identify it; a message on an attached
socket is already authorised twice over — the login cookie was checked before the
upgrade and the claim token on attach — so being the current attachment *is* the
permission. There is no second request left to refuse, and `AudioError`,
`AppError::Unavailable` and the `403`/`503` pair went with it.

There is one active session and therefore at most one audio consumer. Enabling twice
replaces the subscription rather than adding one, which the session tests assert on
the queue's subscriber count.

Audio must not travel through the tile encoder or its queue. The RDPSND handler feeds
a bounded audio queue owned by the session (`src/audio.rs`): a `broadcast` channel of
64 buffers. That is **11.8 seconds** — the tested host sends 32 KiB, 186 ms, per
buffer — so the drop rule is not what bounds latency here, and it has never had to be:
299 consecutive buffers arrived with the queue at zero. Every property of that channel
answers a requirement: `send` never awaits, so it cannot block the RDP read loop; a
full ring drops the *oldest* buffer, so a slow consumer loses old audio rather than
accumulating latency; a consumer that fell behind is told and skips forward; and with
nobody subscribed `send` simply fails, which is how an audio-enabled target discards
sound nobody asked for. An RDPSND wave confirmation means the buffer was accepted or
deliberately dropped, not that the browser's speakers have physically played it.

Backpressure to the browser is the pump awaiting its send on the attachment's channel.
A browser that cannot keep up stops the pump reading, and the queue then drops its
oldest buffers — the same skip-forward choice the client's ceiling makes at the other
end of the same path.

## Latency, and what has been ruled out

**A live desktop was heard a couple of seconds behind itself**, under the `<audio>`
design. This is the open question in this path and the reason to call it experimental.
What follows is what has been measured rather than what has been theorised, because
several plausible theories have already been wrong.

**The gateway is not adding it.** Per-buffer instrumentation (`RUST_LOG=remotex=debug`,
the `audio: wave …` line) against the live target, across a 55-second run with music
playing:

```
299 buffers, all 32768 bytes, max queued: 0
content produced 55340 ms over 55360 ms of wall clock  → ratio 0.9996
inter-arrival: min 169 ms, median 189 ms, max 200 ms
frame steps other than 9/10: []
```

Each line closes one theory:

- **No backlog in the queue.** `max queued: 0`, every time, so the 11.8 s of depth
  above was never touched.
- **The host paces itself.** One 186 ms buffer every ~189 ms, and content out matching
  the clock to 0.04%. FreeRDP's `rdpsnd_detect_overrun` exists because "older windows
  RDP servers do not limit the send buffer" and dump audio faster than real time; this
  host does not, so there is nothing for us to drop.
- **The keepalive was never landing in real audio.** Every step was 9 or 10 frames —
  the arithmetic of a 32 KiB buffer — with no silence batch anywhere. So filler was not
  the cause of the occasional stutter either, and its deletion is not a fix for this.

What the gateway contributes is therefore about **200 ms**: one wave buffer, plus at
most one 20 ms Opus frame held for the next one.

**The browser's buffered range was not adding it either.** Reading
`buffered.end - currentTime` on screen while the sound was late showed it **near
zero** — the element was already at the live edge of what it had been sent.

So what was left, and untested: **Windows' own capture path before rdpsnd sends**, and
whatever a browser holds *outside* the buffered range, where nothing could see it.

**It was the second one, and it is now gone by construction.** The live target was
heard in Chrome on 2026-07-29 through this path and the delay is **much smaller** —
which is the answer to the question this document had been carrying, and it arrived by
elimination rather than by instrumentation, because the suspect was in the one place
nothing could measure. A media element's own buffering was holding the audio, and the
old design had no mechanism to reach it: `buffered` showed nothing because what it held
was not in `buffered`, and `playbackRate` could not trim a buffer it could not see.

The schedule is the client's now, and it cannot leave the range the arithmetic defines:
a fresh start is `now + 0.1 s` and nothing is ever scheduled past `now + 0.3 s`. Where
the old design's answer to a delay was "it stays", this one's is to skip forward and
log it — the SPA warns when it trims, and a recurring trim is the ceiling doing its
job.

That leaves **Windows' own capture path** as whatever delay is still audible, and it is
the one thing no change here can help. The experiment for it is unchanged and still
unrun: an A/B against `freerdp` playing the same desktop's audio, because if it is
equally late, the remaining latency was never ours. Worth doing only if what is left
turns out to be worth chasing — the reason to run it before was that two seconds was
too much to accept, and that reason is now weaker.

## What has been heard, and what has not

**The gateway's own half is proven without a cooperating server.**
`rdp_audio::tests::a_server_speaking_rdpsnd_gets_its_audio_onto_a_listener` and its
`_the_audio_dvc_` twin drive real MS-RDPEA server PDUs through both transports and
assert Opus packets come out of a listener. They cannot compare the bytes they sent
against the bytes that came out, so where a buffer must be *ignored* — a second
transport, an unadvertised format index — they send a different number of frames on
each path and read the packet count. The encoding is checked separately, by
`opus_stream`, which decodes what it encoded with libopus.

**The independent check changed shape, and that is worth knowing.** With Ogg there was
a third-party demuxer to appeal to: `ffprobe` reported `Audio: opus, 48000 Hz, stereo`,
a 5.99 s duration for six seconds of audio (so the granule positions were right, not
merely present), and `start 0.006500` — the 312-sample pre-skip being honoured. Without
a container there is nothing for `ffprobe` to read, so the outside readers are now
libopus in the Rust tests and the browser's own `AudioDecoder`. The tone harness is
therefore load-bearing in a way it was not before.

**The whole gateway path, verified over a raw WebSocket** on 2026-07-29, against the
tone harness, with a client that is not the SPA (`audio_ws_probe.py`, a scratch script):
log in, claim, attach, connect, `{"type":"audio","enabled":true}`, and read frames.

```text
audioFormat: codec=opus rate=48000 ch=2 head=b'OpusHead' len=19 pre_skip=312
audio frame 1: 1 packets [353]
audio frame 2: 1 packets [258]
…
audio frames:   749 carrying 749 packets
audio bytes:    185003 (7400 B/s over 25s)
frames after disable: 0
```

Four findings in that:

- **The pre-skip is real** (312 samples), so the header is built rather than stubbed.
- **Packets settle at ~241 bytes per 20 ms**, which is the 96 kbps this is configured
  for, and the first packets are larger while libopus's rate controller settles.
- **7.4 kB/s across a 50% duty cycle** — the harness plays five seconds and rests
  five — so ~14 kB/s while sounding and **nothing at all** while quiet, which is the
  keepalive's deletion showing up as an absence in the trace: three separate five-second
  windows with no frame in them.
- **Disabling stops it.** Not one audio frame after `{"enabled":false}`.

One packet per frame there, rather than the nine or ten a live host produces, because
the harness feeds 20 ms buffers where RDP delivers 32 KiB ones. That is the harness's
cadence, not the design's.

**A browser's decoder accepts this exact configuration.** A headless Chromium reported
`isConfigSupported` true for `{codec:"opus", sampleRate:48000, numberOfChannels:2,
description:<the OpusHead above>}` — and, on the same run, **no `AudioDecoder` at all**
on `about:blank`, which is how the secure-context requirement above was found rather
than discovered in the field.

**Stereo has been confirmed by ear**, which is the part no test can claim: a
hard-panned file played on the live Windows target on 2026-07-29 arrived correctly
separated in the browser — left on the left, right on the right, no collapse toward
the centre. Worth doing by ear rather than only in code, because a channel *swap*
passes every assertion in the suite; only a listener who knows which side the source is
playing can catch it. One false alarm from that session is worth keeping, because it
will recur: a live capture's two channels decoded with an L/R correlation of exactly
1.0000, which is what blended channels would look like and was in fact a dual-mono
source. Only a hard-panned signal tells those apart, which is what
`opus_stream::tests::a_hard_panned_signal_still_has_two_channels_after_a_round_trip`
sends through the whole path for exactly this reason.

**The RDP negotiation is proven, and the two halves have been run together** — under
the previous representation. A live Windows 11 host (`desktop-vnvgdaf`) redirects its
audio to this gateway, and it was *heard* on 2026-07-29: the guest's own sound, in a
browser, not a generated tone. 12 s pulled while the guest played music came to 125 KB
— **10.4 kB/s against PCM's 176**, a seventeenth — and the decode was real audio
rather than a header with nothing behind it: peak 16112, RMS 2175, 99.8% non-zero
samples. Under `RUST_LOG=…,ironrdp_rdpdr=trace,ironrdp_rdpsnd=debug` the negotiation
itself is legible: the `rdpdr` handshake completes (`ClientAnnounceReply`, client name,
capability response, `UserLoggedon`, no devices announced), then
`ServerAudioFormatPdu { version: V8, … }` arrives on the static channel, our single PCM
entry matches, and `Wave2` PDUs follow.

What the earlier silence was *not*, each ruled out with evidence before `rdpdr` was
found: audio policy or devices on the host (a Remote Audio endpoint exists and its
meter moves), `CHANNEL_OPTION_INITIALIZED` (patched into IronRDP locally — no change),
interference from the other channels (tested with clipboard and resize both off), the
drdynvc capability version (`V3`, same as FreeRDP's), and the Client Info PDU flags
(`NO_AUDIO_PLAYBACK` drops exactly when `audio = true`). All of that was true and none
of it mattered. The one thing not tested until last was the extra channel FreeRDP
announces, and that was the answer.

**And this representation has been heard from the live host**, which is the test that
matters and the one no amount of the above substitutes for: the guest's own sound, in
Chrome, on 2026-07-29, with the delay **much smaller** than the couple of seconds the
`<audio>` path carried. Every hop in this document ran at once — MS-RDPEA to the queue,
the encoder to Opus packets, audio frames on the socket the desktop was already using,
WebCodecs, and a schedule under a ceiling.

**Safari plays it too, which is what makes Opus-only defensible rather than a gamble.**
macOS 26 Safari and a real iPhone both played the same stream on 2026-07-29. That was
the open risk in this design — there is no fallback representation, so a WebKit refusal
would have meant no sound at all on Apple platforms — and it is now the answer rather
than an assumption. It also means the decision *not* to build a raw-PCM fallback
speculatively was the right one: it would have been a second encoder, a second frame
kind and a second set of failure modes maintained for a browser that turned out not to
need it.

**It did not work in the iOS Simulator**, and that is deliberately all this says. The
cause has not been investigated: a simulator without the codec, a simulator without a
working audio output, and a fault in this path would all present the same way from
here, and guessing between them in a document is how a wrong explanation outlives the
observation. So: verified on devices, unexplained in the simulator, and worth ten
minutes with the console before anyone concludes which.

**Every one of those runs reached the gateway over loopback**, including the iPhone's,
so none of them tested the secure-context rule — they were all exempt from it. A client
on a real network over plain HTTP still has no `AudioDecoder`, and serving audio to one
therefore still needs TLS. Untested, and the first thing to suspect when a phone that
worked at a desk stops working on the wifi.

**Nor has the dynamic transport.** `AudioPlaybackDvc` has never carried a byte from a
real server — only from in-crate PDUs — because the one host available serves this
gateway the static channel. It is written from [MS-RDPEA] and reviewed against
FreeRDP's client, which is not the same as having run. The first sign of trouble would
be the `claim_transport` line naming the dynamic transport followed by no first-buffer
line.

**The tone harness is how the browser half is checked**, and it needs no Windows host:
an `#[ignore]`d in-crate test serving the real router — SPA, login, `/ws` — in front of
a scripted engine that fills the queue in real time.

```sh
cargo test --lib serve_a_test_tone -- --ignored --nocapture
```

It plays five seconds of tone and then goes quiet for five, publishing and clearing the
format around the gaps the way a real host's channel opening and closing does. That is
what makes it QA for the behaviour rather than only for the codec: press **Enable
audio** *during a quiet phase*, close the drawer, and touch nothing — the tone must
arrive on its own, go away, and come back. A line under the button instead means this
browser has no decoder for it, which is the one answer this design does not work
around.

One other thing it is worth knowing the harness gets right, because it was wrong first
and would have been misread as a flaw in the transport: it paces against a deadline
rather than sleeping a fixed interval (a fixed 20 ms sleep delivers ~2.5 s of audio
every 3 s, and a browser stutters on the underrun). It also answers `ClientMsg::Refresh`
by re-announcing the desktop size, as every real engine does — without that, any
browser but the first to attach waits forever for a desktop.

## Scope

Audible sound from one Windows RDP target to a browser, on the session's own
WebSocket, surviving an ordinary page reconnect (with audio off, to be re-enabled by a
click) and stopping on target disconnect or takeover.

It does not include:

- audio for VNC or `rxa` — `audio = true` is refused on those targets rather than
  accepted and left inert, because there is no audio channel behind either one:
  RFB has none, and the Mac agent captures no sound;
- audio/video synchronization;
- a selectable bitrate or codec, or any fallback representation;
- recording, seeking, or replay;
- mixing, in-page volume, or more than one listener;
- audio over plain HTTP to anything but localhost, which WebCodecs forbids; or
- the macOS viewer.

**The viewer cannot share this path, and that is a cost of the design rather than an
oversight.** It could once have pointed `AVPlayer` at the WAV endpoint; Ogg took that
away (AVFoundation has no Ogg demuxer), and audio frames on the WebSocket take it
further — there is no HTTP response left to point anything at, and the viewer does not
send the message that would start one. Giving the viewer sound therefore means a
representation of its own from the same queue: Opus in CAF or fragmented MP4, both of
which AVFoundation reads, or decoding Opus in the viewer. That is planned and second
([`roadmap.md`](roadmap.md)), and the ordering is deliberate — the browser is where
this is being listened to, so it is where the timing model should be settled before a
second client inherits it.
