# Remote audio

The first audio path is deliberately narrow: **sound from a Windows RDP target
to the browser through the remotex gateway**. RDP already redirects the sound to
its client; remotex requests that channel and exposes what arrives as an
ordinary live HTTP audio response.

Audio does not belong in the desktop WebSocket for this path. The browser already
has a streaming audio client:

```html
<audio controls src="/api/session/audio?session=…"></audio>
```

That leaves the gateway responsible for the RDP side and the HTTP response, and
leaves buffering, decoding, and playback to the browser. It avoids a new remotex
wire record, a protocol-version bump, WebCodecs, an `AudioWorklet`, and a decoder
or jitter buffer in either remotex client.

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
would otherwise interleave two streams into one WAV response with nothing to
report it.

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
inseparable.

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

RDPSND must be negotiated when the RDP connection is established. An HTTP
listener that appears later cannot add the channel to an existing connection,
so an audio-enabled RDP target requests redirection from the start and discards
audio while nobody is listening.

**Gateway to browser.** An authenticated streaming endpoint (`src/server.rs`):

```text
GET /api/session/audio?session=<claim token>
Content-Type: audio/wav
Cache-Control: no-store, no-transform
Accept-Ranges: none
X-Accel-Buffering: no
```

The claim token identifies the owner of the single active session, just as it
does for `/ws`; the login cookie authenticates the request. Two refusals, and they
mean different things to a client: a token that is not the current claim is `403`
and will never work, while a session with no audio *yet* — the picker, a target
without the flag, or a channel still negotiating — is `503` and worth asking about
again.

The endpoint waits (up to five seconds) for the negotiated PCM format, writes one
WAV header for it, and then writes the wave buffers as an open-ended response
without a `Content-Length`. Both size fields in the header are `0xFFFFFFFF`, the
convention for a stream whose length is not known. There is no recording and no
seekable history: a listener starts at live audio and receives only buffers that
arrive after it attaches, and a `Range` request is answered with the same stream
rather than a `206`.

An open-ended PCM/WAV response is the cheapest possible representation because
wrapping PCM requires a header, not an encoder — and it has been heard playing
progressively from a browser, so it stays (see below). Should some other required
browser or proxy turn out to buffer it to the end instead, the architecture does not
move: only this endpoint's media representation becomes a progressively playable
compressed stream, while the `<audio>` element and the RDP side remain unchanged.

**The SPA** offers an Audio row in the floating menu for a session whose
`connected` message carried `audio` (`frontend/src/AudioPanel.tsx`). It opens a
docked panel holding a native `<audio controls autoPlay>`. `autoPlay` is honest
because the panel is mounted by a click, and the native controls are the fallback
when browser policy refuses it anyway — they are also the reason to use them here
rather than a button of our own, since a stream that is playing, one that is
stalled, and one that never started look different in them and identical behind a
custom control. Closing the panel unmounts the element and so ends the response;
that is a real limitation, and the trade for not portalling one element through
two components in a proof of concept.

## Lifetime and backpressure

The audio response belongs to the claimed session, not merely to an authenticated
login. It ends immediately when:

- another client takes over the single session slot;
- the target changes or disconnects;
- the RDP engine ends;
- the owner logs out; or
- that listener's HTTP connection closes.

Two mechanisms cover those, and the split is worth knowing. The queue lives on the
session's engine slot, so `State::take_engine` ends the response on every path that
stops an engine — switch target, log out, heartbeat expiry, reattach-grace expiry,
engine death. A takeover is the case that needs its own line, because it *keeps the
engine running*: `SessionManager::claim` ends the previous listener explicitly,
since the desktop carries on for a browser that is not the one listening.

Neither mechanism is the `Arc` going out of scope. The engine holds a reference of
its own and would keep the stream open until it noticed its input channel close,
which is why both end the listener rather than relying on a drop.

There is one active session and therefore at most one live audio consumer. A
second request by the same owner replaces the first instead of creating a shared
stream.

Audio must not travel through the tile encoder or its queue. The RDPSND handler
feeds a small bounded audio queue owned by the session (`src/audio.rs`): a
`broadcast` channel of 64 buffers, around a second and a half. Every property of
that channel answers a requirement — `send` never awaits, so it cannot block the
RDP read loop; a full ring drops the *oldest* buffer, so a slow consumer loses old
audio rather than accumulating latency; a consumer that fell behind is told and
skips forward; and with nobody listening `send` simply fails, which is how an
audio-enabled target discards sound nobody asked for. An RDPSND wave confirmation
means the buffer was accepted or deliberately dropped, not that the browser's
speakers have physically played it.

Reverse proxies must pass the response through rather than buffer it.
`X-Accel-Buffering: no` is sent unconditionally because it is the one such header
that is inert everywhere it is not understood; the rest of the proxy configuration
is deployment-specific.

## What has been heard, and what has not

**The browser half is proven.** An open-ended `audio/wav` response *is* played
progressively rather than buffered to completion: a 440 Hz tone served from this
endpoint was audible, continuously and without stalling, in a browser on macOS on
2026-07-29. So the representation stands, and the fallback this design was hedging
against — swapping WAV for a progressively playable compressed stream — is not
needed. `Content-Length`-free, `0xFFFFFFFF`-sized streaming WAV is enough.

That was settled with a generated tone rather than a remote's audio, because the
two halves fail independently and at the time no server had redirected to this
gateway. The harness stays useful for exactly that reason — it exercises the
browser half with no RDP host involved.
`server::tests::serve_a_test_tone` is that harness: an `#[ignore]`d in-crate test
serving the real router — SPA, login, endpoint — in front of a scripted engine that
publishes the negotiated format and fills the queue in real time.

```sh
cargo test --lib serve_a_test_tone -- --ignored --nocapture
```

Two things it is worth knowing that harness gets right, because both were wrong
first and both would have been misread as flaws in the WAV path: it must publish a
format (or the endpoint honestly answers 503), and it must pace against a deadline
rather than sleeping a fixed interval (a fixed 20 ms sleep delivers ~2.5 s of audio
every 3 s, and the browser stutters on the underrun). It also answers
`ClientMsg::Refresh` by re-announcing the desktop size, as every real engine does —
without that, any browser but the first to attach waits forever for a desktop.

**The RDP negotiation is now proven too, and the two halves have been run
together.** A live Windows 11 host (`desktop-vnvgdaf`) redirects its audio to this
gateway, and it was *heard* on 2026-07-29 — the guest's own sound, played by the
SPA's `<audio>` panel in a browser, not a generated tone. The gateway log for that
session names every hop:

```text
INFO remotex::audio]     audio: negotiated 44100 Hz, 2 channel(s), 16-bit PCM
INFO remotex::rdp_audio] rdp: the remote is redirecting audio over the static
                         channel (32768 bytes in the first buffer)
INFO remotex::server]    audio: stream requested
INFO remotex::session]   session: audio listener attached
INFO remotex::server]    audio: streaming 44100 Hz PCM
```

The samples were separately measured arriving intact at a plain HTTP client, which
is what rules out a header with nothing behind it: 1.34 MB pulled from
`/api/session/audio` over 10 s decoded as 7.8 s of 44100 Hz 16-bit stereo with
peak 14728, RMS 1621 and 99.7% non-zero samples.

Under `RUST_LOG=…,ironrdp_rdpdr=trace,ironrdp_rdpsnd=debug` the negotiation itself
is legible: the `rdpdr` handshake completes (`ClientAnnounceReply`, client name,
capability response, `UserLoggedon`, no devices announced), then
`ServerAudioFormatPdu { version: V8, … }` arrives on the static channel, our single
PCM entry matches, and `Wave2` PDUs follow.

What the earlier silence was *not*, each ruled out with evidence before `rdpdr`
was found: audio policy or devices on the host (a Remote Audio endpoint exists and
its meter moves), `CHANNEL_OPTION_INITIALIZED` (patched into IronRDP locally — no
change), interference from the other channels (tested with clipboard and resize
both off), the drdynvc capability version (`V3`, same as FreeRDP's), and the Client
Info PDU flags (`NO_AUDIO_PLAYBACK` drops exactly when `audio = true`). All of
that was true and none of it mattered. The one thing not tested until last was the
extra channel FreeRDP announces, and that was the answer.

Two smaller things the delivered stream shows, neither a defect: the byte rate
runs under the format's 176400 B/s over a window that includes the attach (waves
start when the guest produces sound, not when the response opens), and the WAV
carries `0xFFFFFFFF` in both size fields with no `Content-Length`, as intended.

The measurement is a scratch script rather than a committed test, because it needs
that live host and a guest that happens to be making noise; the committed
coverage is the in-crate pair below.

The gateway's own half is proven without a cooperating server:
`rdp_audio::tests::a_server_speaking_rdpsnd_gets_its_audio_onto_a_listener` and its
`_the_audio_dvc_` twin drive real MS-RDPEA server PDUs through both transports and
assert the PCM comes out of the HTTP stream behind its WAV header.

**What has not been heard** is the dynamic transport. `AudioPlaybackDvc` has never
carried a byte from a real server — only from those in-crate PDUs — because the one
host available serves this gateway the static channel. It is written from
[MS-RDPEA] and reviewed against FreeRDP's client, which is not the same as having
run. A host that offers only `AUDIO_PLAYBACK_DVC` would be the thing to test it
with, and the first sign of trouble would be the `claim_transport` line naming the
dynamic transport followed by no first-buffer line.

## Scope

Audible sound from one Windows RDP target through an `<audio>` element loaded from
the gateway, surviving an ordinary page reconnect and stopping on target
disconnect or takeover.

It does not include:

- audio for VNC or `rxa` — `audio = true` is refused on those targets rather than
  accepted and left inert, because there is no audio channel behind either one:
  RFB has none, and the Mac agent captures no sound;
- audio/video synchronization;
- a selectable bitrate or codec;
- recording, seeking, or replay;
- mixing or more than one listener;
- audio records in the remotex WebSocket; or
- the macOS viewer.

The native viewer can consume the same authenticated endpoint with `AVPlayer`; it
does not need a second gateway transport. If the open-ended WAV representation
proves unsuitable for both clients, direct PCM playback in the viewer is a
fallback, not a reason to complicate the browser path.
