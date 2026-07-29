# Remote audio

The first audio path is deliberately narrow: **sound from a Windows RDP target
to the browser through the remotex gateway**. RDP already redirects the sound to
its client; remotex needs to request that channel and expose what arrives as an
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

## First proof of concept

**RDP to gateway.** Enable IronRDP's `rdpsnd` feature and register its static
channel when an RDP engine is created. The handler advertises PCM and receives
the negotiated format plus each redirected wave buffer. PCM is the right first
input because it is the one [RDPSND audio
format](https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpea/30a6cc00-31c4-4e15-9aa4-95a5c5074697)
both clients and servers are required to support; accepting a compressed RDP
format would only make the PoC depend on what one Windows version happens to
offer.

RDPSND must be negotiated when the RDP connection is established. An HTTP
listener that appears later cannot add the channel to an existing connection,
so an audio-enabled RDP target requests redirection from the start and discards
audio while nobody is listening.

**Gateway to browser.** Add an authenticated streaming endpoint, for example:

```text
GET /api/session/audio?session=<claim token>
Content-Type: audio/wav
Cache-Control: no-store
```

The claim token identifies the owner of the single active session, just as it
does for `/ws`; the login cookie authenticates the request. The endpoint waits
for the negotiated PCM format, writes one WAV header for that format, and then
writes the wave buffers as an open-ended response without a `Content-Length`.
There is no recording and no seekable history. A listener starts at live audio
and receives only buffers that arrive after it attaches.

An open-ended PCM/WAV response is the cheapest possible PoC because wrapping PCM
requires a header, not an encoder. The first experiment is therefore to verify
that the browsers and reverse proxy used with remotex begin playing that response
progressively instead of waiting for it to end. If any required browser does not,
the architecture stays the same: the endpoint changes its representation to a
progressively playable compressed stream, while the `<audio>` element and the
RDP side remain unchanged.

The SPA creates the element after the desktop connects and calls `play()` from a
user action. Autoplay can still be rejected by browser policy, so a visible
Play/Unmute action is the fallback rather than an audio pipeline of our own.

## Lifetime and backpressure

The audio response belongs to the claimed session, not merely to an authenticated
login. It ends immediately when:

- another client takes over the single session slot;
- the target changes or disconnects;
- the RDP engine ends;
- the owner logs out; or
- that listener's HTTP connection closes.

There is one active session and therefore at most one live audio consumer. A
second request by the same owner replaces the first instead of creating a shared
stream.

Audio must not travel through the tile encoder or its queue. The RDPSND handler
feeds a small bounded audio queue owned by the session. A slow or absent HTTP
consumer causes old audio to be dropped; it never blocks the RDP read loop and
never builds an ever-growing delay. An RDPSND wave confirmation means the buffer
was accepted or deliberately dropped, not that the browser's speakers have
physically played it.

Reverse proxies must pass the response through rather than buffer it. The
endpoint should also disable content transformation and caching; exact proxy
headers are deployment-specific and belong with the implementation once the PoC
has selected its media representation.

## PoC boundary

The proof is complete when sound played in one Windows RDP target is audible from
an `<audio>` element loaded from the gateway, survives an ordinary page reconnect,
and stops on target disconnect or takeover.

It does not include:

- audio for VNC or `rxa`;
- audio/video synchronization;
- a selectable bitrate or codec;
- recording, seeking, or replay;
- mixing or more than one listener;
- audio records in the remotex WebSocket; or
- the macOS viewer.

Once the browser path works, the native viewer can consume the same authenticated
endpoint with `AVPlayer`; it does not need a second gateway transport. If the
open-ended WAV representation proves unsuitable for both clients, direct PCM
playback in the viewer is a fallback, not a reason to complicate the browser PoC
up front.
