# Architecture

remotex is a single-user gateway for RDP and VNC targets, including Macs reached
through their built-in Screen Sharing service. A Rust backend owns the remote
protocol session and exposes one common HTTP/WebSocket interface to the React SPA,
which is the only client.

## Data path

```text
browser SPA over loopback or the network
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary video batches
   │  /ws/audio: the audio format, then binary audio frames
   │  /ws/camera: the camera format and H.264 samples up, start/stop down
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through the built-in client
                                         └─ built-in RFB client (3.8 or Apple 003.889)
```

RDP and VNC frames are decoded in the gateway and sent as one VP9 stream of the
whole desktop, at the quality and chroma the target's render plan resolves to. A
Mac is reached
over Apple's own RFB 003.889 with Apple Remote Desktop authentication, as
Apple's viewer reaches it: in Screen Sharing's Standard mode with `subtype = "ard"`,
or in High Performance with `ard-high-performance` (a virtual display, with its
picture and sound over the Mac's media stream, as Apple's viewer takes them). Remote audio is either encoded as
Opus or passed through as PCM and sent on `/ws/audio`, never on the picture queue.
The browser's camera goes the other way on `/ws/camera`: browser-encoded H.264,
passed through to an RDP host over MS-RDPECAM, or to wlshare over its camera
extension on a generic VNC target. The redirection is experimental —
see [Camera frames](#camera-frames).

## Constraints

- There is one active session slot per gateway instance. A new client may force a
  takeover and evict the previous holder; concurrent and shared sessions are not
  supported.
- Remote credentials remain in the server-side TOML configuration.
- Clients speak only the remotex protocol and never implement RDP or RFB.
- Protocol engines prefer broadly supported baseline features over
  server-specific behavior.

## Backend

| Module | Responsibility |
|---|---|
| `server.rs`, `auth.rs` | HTTP routes, SPA serving, login sessions |
| `session.rs` | target selection, takeover, detach, and reattach |
| `ws.rs`, `protocol.rs`, `wire.rs` | WebSocket bridge and client wire format |
| `rdp.rs` | RDP engine: damage, input, cursor, resize, clipboard, over `rdp_client` |
| `rdp_client/` | the RDP client, protocol and all: `proto/` is the wire format, the rest is the session, framebuffer and input queue |
| `rdp_clipboard.rs` | `CF_UNICODETEXT` and the line endings either direction needs |
| `vnc.rs` | RFB connection, framebuffer, input, cursor, clipboard, resize |
| `vnc_apple_media.rs` | High Performance's media stream: the offer, SRTP, HEVC depacketizing and decoding, and the sound's receiver |
| `aac_eld.rs` | the AAC-ELD decoder for that stream's sound |
| `shadow.rs` | change detection: what the client already has |
| `encode.rs`, `stream.rs`, `video.rs` | the ordered, paced, congestion-aware stream: its mirror, its rounds, and the picture limits |
| `vp9.rs` | libvpx — the video codec |
| `audio.rs`, `opus_stream.rs`, `pcm48.rs`, `pcm_stream.rs` | PCM queue, Opus encoding or PCM passthrough, resampling |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine consumes `ClientMsg` input and emits the same `ServerMsg` stream.
RDP and VNC pass dirty pixels through the ordered encoder before reaching that
boundary.

Ordering is a correctness requirement throughout the frame path. Every access unit
is a change from the one before it, and a resize changes the picture that follows.
The encoding and outbound queues therefore keep access units, resizes, and cursor
updates in source order even though each encode runs off the engine's own task.

### The video stream

Every target reaches the browser the same way: the whole framebuffer as one
inter-frame VP9 stream, for the whole session.

> **There is no tile transport any more.** Earlier releases also sent each changed
> region as an independent PNG or WebP still (`render_type = "tiles"`, with
> `render_subtype`, `image_quality`, a per-tile photographic classifier, a
> `render_motion` switch that streamed only the moving regions, a slot cache,
> `COPY` records and the `render_grid_debug` overlay). All of it was removed after
> **v0.0.253**; `git checkout v0.0.253` recovers it, including the classifier's
> research notes in `docs/still-image-classification-research.md`.

A target's stream keys are per target, and every one has a default:

- `video_quality` (1–100, default 90) is the ceiling the stream holds to.
- `render_chroma` (`"auto"`, the default, or `"420"` / `"444"`) is how much colour
  the stream carries per pixel. A target that writes nothing resolves it per
  browser; the two fixed answers are selections no decoder can overrule. See
  [the codec](#the-codec) for why it, and not the quality, is where a desktop
  stream's picture goes, and [choosing a chroma](#choosing-a-chroma) for when to
  take the decision away from the browser.
- `render_adaptive` (on unless a target writes `false`) lets the quality track the
  measured link down to `render_adaptive_min` (default 20, or the dial itself where
  that is lower) — see [what the link will bear](#choosing-a-chroma) for the signal
  and the walk. Turned off, the walk is the pressure-only one floored at 1.

The engines never see the config keys. They collapse to one `RenderPlan`
(`quality`, `adaptive`, `chroma`) at the config boundary in
`TargetConfig::render_plan`, which reaches the encoder through the engine-agnostic
`VideoSink` in `src/encode.rs`:

```text
video_quality / render_chroma / render_adaptive*
  → TargetConfig::render_plan(browser chroma) → RenderPlan → vnc::run / rdp::run
  → VideoSink::new(engine, frame_tx, plan) → DesktopStream (src/stream.rs) → vp9::Stream
```

Every size an engine asks a remote for is held under the stream's picture ceiling
(`video::fit_ceiling`: a long side of 3840 and a short side of 2400), and a pinned
`width`/`height` past it is refused at config load.

Five rules hold the stream up, and each is a rule somewhere:

- **`Shadow::accept` is a promise.** It records source pixels as delivered the
  moment it accepts them, and nothing re-sends them, so the encoder may never drop a
  frame (`rc_dropframe_thresh = 0`) and an encode that yields no bitstream leaves the
  stream dirty for the next frame to carry rather than clearing it. The shadow earns
  its keep all the same: RDP repaints regions that did not change, and a VNC server
  re-sends unchanged pixels, and those never reach the mirror at all.
- **The stream is fed rectangles, not frames.** `VideoSink::damage` is called once
  per damage *rectangle*, and VNC's pixels can only be cropped out of the rect just
  decoded, so the stream keeps its own whole-framebuffer RGB copy — the mirror — to
  blit into. `VideoSink::frame` encodes it, called at RDP's outputs-loop end (and its
  `Refresh` arm, which `continue`s past that) and at VNC's `FramebufferUpdate` end.
  It is a no-op when nothing was blitted, because RDP's loop turns once per PDU and
  most redraw nothing. VNC's CopyRect is read back out of the shadow as pixels.
- **A frame boundary is a proposal, not a frame rate.** Those boundaries occur at
  whatever rate the remote reports damage — 126 a second, measured, on a busy RDP
  desktop against a 30 Hz stream and a 60 Hz screen — and every one of them used to
  cost a full encode, which is how a session carrying under 800 kbit/s spent 88% of
  itself inside the encoder. `VIDEO_FRAME_INTERVAL` caps it at one access unit per
  33 ms; damage in between accumulates in the mirror and rides the next one, which
  is cheaper than coding the same movement four times over. A forced keyframe skips
  the cap, because a repaint, reattach, takeover or resize is a client with nothing
  on screen. And because a deferral leaves pixels the shadow has already promised,
  `VideoSink::due_at` tells the engines when to come back for them whether or not
  more damage arrives — RDP in a `select!` arm beside its layout retry, VNC raced
  against its next message read, at a message boundary so a flush cannot split a
  `FramebufferUpdate`.
- **The encode is pipelined, and serial.** A round takes the mirror and the encoder
  to a blocking worker; the spare mirror takes the blits meanwhile and the order task
  puts the round back when it lands, signalling `round_returned` if pixels arrived
  while it was away. At most one round is out, because two encoders on one chain of
  frames would decode wrongly. A resize while a round is out drops that round on its
  return (its epoch is stale), and the pixels the new desktop blitted meanwhile are
  owed a stream of their own.
- **The picture may be a pixel larger than the desktop.** The 4:2:0 conversion
  needs even sides — VP9 itself does not, nor 4:4:4, and both are held to them
  anyway — so the mirror is padded up with its edge repeated (black would be a seam
  the encoder paid for every frame). The record header carries the *true* desktop
  size and the client crops.

`video_quality` maps to a constant quantizer: the dial spans 63 → 8 of VP9's own
0–63 (the floor is where screen content goes visually lossless — mapping past it
would give a dial whose top third did nothing but spend bandwidth). The quantizer
never leaves the codec module —
the dial is what everything above it speaks. A constant quantizer *is* variable
bitrate — bits go where the
picture needs them, so a motionless desktop costs almost nothing.

**Above the middle of that dial the loss you can see is not the quantizer's; it is
the chroma sampling's.** VP9 profile 0 carries one colour sample per 2×2 pixels,
and a one-pixel coloured glyph stem on a dark terminal shares its sample with three
pixels of background: the luma survives, the colour comes back at a quarter of its
saturation, and no quantizer can restore what was averaged before the encoder saw
it. Measured 2026-09-01 on 1280×800 of rendered text — coloured monospace on a dark
pane, black and coloured proportional on a light one, one-pixel rules, a gradient —
encoded and decoded through the same libvpx:

| stream                       | keyframe | inter frame | PSNR    | worst pixel |
|------------------------------|----------|-------------|---------|-------------|
| 4:2:0, quality 100 (q 8)     | 408 KB   | 52 KB       | 28.4 dB | 136         |
| 4:2:0, lossless (q 0)        | 640 KB   | 122 KB      | 28.5 dB | 135         |
| 4:2:0 conversion, no codec   | —        | —           | 28.5 dB | 135         |
| 4:4:4, quality 100 (q 8)     | 558 KB   | 48 KB       | 42.8 dB | 33          |
| 4:4:4, lossless (q 0)        | 912 KB   | 70 KB       | 49.5 dB | 4           |

Every 4:2:0 row is the conversion's own floor; speed, loop filter, tuning and
adaptive quantization moved nothing either. `render_chroma = "444"` selects VP9
profile 1 — a colour sample per pixel, the same quantizer, a keyframe a third
larger, inter frames no larger, about a third more encode time — and the codec
string it announces is `vp09.01.…` instead of `vp09.00.…`. The cost is the
decoder: no hardware VP9 decoder takes profile 1, so it always decodes in software
— Chromium does — and a browser with no software VP9 at all, which is iOS and
iPadOS, refuses the configuration by name the way it would refuse any other.
`a_444_stream_keeps_the_colour_420_averages_away` in `src/vp9.rs` is the round
trip that pins the difference, through the archive's own decoder.

### Choosing a chroma

The key takes three answers: the default resolves per browser, and the other two
are decisions no browser can overrule.

**`"auto"` — 4:4:4 where the decoder takes profile 1, 4:2:0 where it does not.**
The default, and what a target that writes no chroma gets: every browser is sent
the most colour its own decoder takes, and no target is written down twice under
two names to serve a desktop and an iPad. The page asks
its own `VideoDecoder` once, at load, about the profile 1 configuration the gateway
would announce (`frontend/src/videoChroma.ts`), and states the answer as
`chroma=444|420` on every session socket it opens. `render_plan` resolves the key
against it; nothing else reads it.

The answer rides the socket URL rather than a message because of *when* it is
needed: a takeover reconnects a still-selected target at attach
([session lifecycle](#session-lifecycle)), before the new browser has sent
anything, and that engine must be built for the browser that took over rather than
the one that left. It is held on the attachment (`ClientSlot`) and read by both
engine starts — including the one reattachment that would otherwise resume a
running engine, which compares the plan the returning browser resolves to against
the plan that is running and rebuilds when they differ.

This is **selection, never refusal**, which is the distinction the removed probe
lacked. Only a definite `supported === false` gives up the colour; a "yes", an
answer with no verdict, and an `isConfigSupported` that throws all read as 4:4:4,
and a browser that answered wrongly still ends where every browser ends, at its own
decoder's refusal by name. One question at page load, no round trip in front of a
session, and no path where the gateway turns a client away on the strength of a
probe.

**`"444"` — profile 1 for every browser, refusals included.** Set it to hold a
fleet to one bitstream, or to pin one side of a measurement; an iPhone or iPad
watching the target is sent a stream it rejects by name. Losing the hardware
decoder on the browsers that do take it is a smaller loss than it reads: the
GPU-process decoder is the one that goes quiet under stream churn, and software
libvpx is what answers every chunk (`frontend/src/videoDecoder.ts`). What it costs
is CPU on the client, roughly twice the samples per frame.

**`"420"` — profile 0 for every browser.** What every stream was before this key
existed, and a selection now rather than a default: a decoder that would have taken
profile 1 is sent the subsampled stream anyway. Right for a fleet that must stay on
a hardware decoder, or where the target is photographic rather than text and the
chroma buys nothing.

Set nothing and every browser gets what it can decode. The two fixed answers are
for when the bitstream, not the picture, is the thing being held
still.

The keyframe header also *says* the conversion is BT.601 studio swing
(`VP9E_SET_COLOR_SPACE` / `VP9E_SET_COLOR_RANGE`). libvpx writes *unknown* unless
told, and a decoder given unknown guesses — Chromium picks BT.709 for anything HD —
so a 1080p desktop was converted with one matrix and displayed with another, every
saturated colour a little off. Nothing on the wire carries it; the decoder reads it
from the bitstream.

The dial is a **ceiling**, and that framing is what makes adaptation tractable here.
`Congestion` in `src/encode.rs` watches one local signal — how long queueing an
access unit blocked — and walks the 1–100 dial down towards 1 when the link is behind,
back up towards the configured quality when it is not; never past it. It moves the
dial rather than a quantizer because a quantizer is the codec module's own scale
and never leaves it. What TCP hides is
*headroom*, and this never needs headroom, because exceeding the operator's setting
was never a goal. "Am I behind?" is the whole question, and the outbound queue
answers it. Quality moves through `Stream::set_quality`, which re-tunes the running
encoder rather than rebuilding it: a rebuild would force a keyframe per adjustment,
spending a few hundred KB exactly when bytes are scarce.

`render_adaptive` gives the same walk a second signal and an operator's
floor, on every target that has not turned it off. The signal is the client's own lag: the paint window already tracks how
long the oldest unacknowledged batch has been owed, and `LinkFeedback`
(`src/feedback.rs`) publishes that age minus a baseline — the smallest recent
end-to-end time, so distance never reads as queueing; RustDesk and Guacamole
both make the same subtraction. Sixty milliseconds of queueing lag counts as
a behind frame even when nothing local blocked, which is exactly the case the
paint window measured a VP9 attachment falling 222 ms behind at 7 batches in
flight while every queue stayed shallow. The walk's floor moves from 1 to
`render_adaptive_min`. Under `render_adaptive = false` the walk is pressure-only.

The walk only runs when a round is taken, and a round is only taken when something
changed, so a desktop that stops moving right after the link coarsened it would keep
that picture, and the walk would stay below the dial, until something changed again.
The order task's settle tick is what comes back for it. Once the stream has been
idle `SETTLE_IDLE` since a round that went out below the
dial, and on a `render_adaptive` target the lag has cleared, it takes the dial back
and marks the unchanged mirror dirty. The engine encodes that as one inter frame.
libvpx codes the residual of unchanged blocks at the finer quantizer, so the frame
sharpens the whole desktop without a keyframe; the vp9 test
`a_finer_quantizer_sharpens_an_unchanged_picture_without_a_keyframe` guards that. A
stream that went out at the dial owes nothing and sends nothing when it goes quiet.

That signal only works because those queues are shallow. One message is a whole
frame, and a deep queue at each of two hops in series is seconds of buffered
picture, so `FRAME_BUFFER` is 4 at both.

Shallow in messages is not shallow in time, and time is what a person at the
keyboard feels: a message is whatever a frame compressed to, so on a link that
slows down the counts bound nothing. Against a throttled link and a busy desktop
the queues held 30 MB, which at 4 Mbit/s is a picture 63 s behind its desktop —
input reaches the remote and its effect arrives a minute later, which reads as a
session that stopped responding until a fresh engine throws the queues away. So
the path is bounded in bytes as well. `QUEUE_BUDGET` in `src/encode.rs` (512 KiB,
two full batches) is taken by the *engine* before a round is encoded, at the size of
the last round, which the order task settles once the size is known, and the share
travels inside the unit (`Held` in `src/protocol.rs`) so that every way out of the
queues returns it: dropped while nobody is attached, left in a channel an ended
engine took with it — or delivered. With the budget spent the
engine waits and stops reading its remote, which is the backpressure the message
counts were meant to be. The order task never waits on it, because everything
queued behind the order task holds a share only the order task can move. The wait
also counts towards the walk's blocked time, which is where a link that is behind
holds the engine instead of at a full queue.

One thing beside the engine touches the budget: a socket replaced by its own browser's next attach gives everything back at once. Such a
socket is usually one parked on a link that stopped, the engine lives on into the
replacement, and eviction queued behind the socket's events would leave the shares
— and the pump, waiting on that full channel — held until its heartbeat ran out. So
that signal travels beside the events (`Attachment::superseded`) and the outbound
task races it: the queue is dropped, the shares of batches in flight are let go,
and the close goes out last.

**The client decodes it with WebCodecs** `VideoDecoder`, reached through
`frontend/src/videoDecoder.ts` and driven from `framePainter.ts` — the batch loop,
which replaces the decoder when the stream restarts on a different size. **Which decoder is the platform's choice**: the
configuration states no `hardwareAcceleration`. A `prefer-software` hint was tried
and removed, because it bought one platform's decoder at most — WebKit honours it
only on macOS (the clause routing it to a local software decoder is compiled
`#if PLATFORM(MAC)`), Firefox disregards it, and iOS has no software VP9 decoder to
route to at all, so VP9 there is VideoToolbox or nothing (measured against WebKit
main, 2026-08-21). What makes a hardware decoder safe is the gateway rather than a
hint: the stream is rebuilt only by a resize, so decode sessions are not churned.
Verified by fast touchscreen scrolling on a Mac and an iPad with no decode errors. The stall backstop in `createVideoStream` — silence where a decode
error belongs, then a failed end-of-stream flush, which is how a GPU-process decoder
fails — stands whatever ends up decoding. That whole loop — parse, decode, paint, and the
decoders with it — runs in a dedicated worker drawing on an `OffscreenCanvas`
(`desktopPainterWorker.ts`, handled from the page by `desktopPainter.ts`); each
binary frame is transferred there, not copied. What that boundary buys is narrower
than it looks — `VideoDecoder` was never doing its work on the main thread anyway — and is mostly presentation: a transferred canvas commits
from the worker, so a frame reaches the screen without the thread carrying input and
React being scheduled for it.

A browser without `VideoDecoder` never reaches this code — the preflight gate turns
it away before React mounts (`preflight.ts`). What survives is the narrower failure:
a decoder that exists and refuses this *configuration*, which no keyframe repairs.
That is *said* rather than logged, because the stream is all a target sends and the
alternative is a desktop that never paints and never explains itself: a banner that stays up, naming the configuration the browser
would not take.

#### The codec

Video is **VP9 only** (`src/vp9.rs`), and there is no codec key. VP9 is
BSD-3-Clause with a patent grant and present in every browser build, the ones that
carry no proprietary codecs included. On synthetic screen content at 1080p and
quality 60 it encodes a frame in **4.7 ms** at **18 KB** — measure with
`cargo test --release measure_the_encoder -- --ignored --nocapture`; a debug build
reports nonsense, because the RGB→YUV conversion it also times is Rust — the `yuv`
crate's, on the AVX2 or NEON path the machine has — and runs an order of magnitude
slower unoptimised. The conversion is the one part of an encode this gateway owns,
and the scalar loop that came before the crate was two fifths of a 1080p encode on
a six-core host, its 4:2:0 averaging the slower of its two paths; the crate's takes
a third of that time at either chroma.

Nothing downstream of `TargetConfig::render_plan` names a codec: `encode.rs`,
`stream.rs` and the wire carry access units, a keyframe bit and a configuration
string, and `vp9.rs` is reachable only from `stream.rs`.

**The browser is asked one question, and never asked to justify itself.** The
client used to probe for the codec: `/api/config` published the gateway's ordered
codecs with a WebCodecs string for each, the client asked
`VideoDecoder.isConfigSupported` about them before login, and `ClientMsg::Connect`
carried the accepted names for `connect` to pick from. It worked, and it was
removed. It put a round trip and a decoder query in front of every video session;
`isConfigSupported` is not reliable enough on the same browser twice to build a
refusal on; and because the refusal was phrased as "this browser accepted neither",
any fault anywhere near the path — a serde field-name mismatch, for one — surfaced
as an accusation against the browser and sent the reader to the wrong half of the
system.

What survives of asking is one question with no power to refuse: how much colour
this decoder takes, for `render_chroma = "auto"` to resolve against
([choosing a chroma](#choosing-a-chroma)). It selects between two streams the
gateway is willing to send, both of them VP9; it never decides whether a session
may happen. A wrong answer costs a picture, not a desktop.

The refusal itself stays where it always was: one honest failure at the client's own
decoder. The gateway announces the configuration in `ServerMsg::VideoFormat` before
the stream's first unit, `VideoDecoder.configure` accepts it or refuses it, and a
refusal is reported by name — "this browser cannot decode the video this target
sends" — with the configuration string beside it.

## Session lifecycle

Authentication and desktop ownership are separate:

1. `POST /api/auth/login` creates the login cookie.
2. `POST /api/session` claims the single slot. A conflicting claim returns
   `409` unless the request reclaims its token or forces takeover.
3. `/ws?session=<token>&chroma=420|444` attaches to the slot and reports either
   the target picker or the current connected target. `chroma` is required and
   names the most colour this browser's video decoder takes; see
   [Choosing a chroma](#choosing-a-chroma). The media sockets carry the token
   alone.
4. `connect` starts the selected engine. `disconnect` stops it and returns to
   the picker.
5. Losing the WebSocket detaches the client. The engine remains available for a
   60-second reattach grace period while frames are discarded.
6. Logging out ends the login and session immediately, closes the engine, and
   releases the claim.

Every `connect` first ends any running engine, including one already connected
to the same target, and the next engine is not spawned until that process exits;
`ENGINE_EXIT_GRACE` bounds the wait. Switching targets and logging out likewise
end the engine outright. The sole resume is the owning browser reattaching to
the same target after its session socket drops, and it resumes only while the
running engine is still the one that reattachment resolves to: a reload re-runs
the chroma question, and an `"auto"` target whose browser comes back with a
different answer is rebuilt rather than resumed, because the stream that is
running is one that browser has just said it cannot decode. Opening size, density, display
selection, and connection state do not carry into any other session.

Any claim by a different browser — a forced takeover, or a plain claim while
nobody is attached — closes the previous WebSocket and its engine but
preserves the selected target: the new claimant's attach reconnects that
target for its own screen and chroma (both named on the `/ws` URL), so a desktop
opened for one display never carries its size, density, or colour over to a
different device.
Only the owner reclaiming its token resumes the running engine, with a
full-repaint request instead of a reconnect.

Login tokens are held in memory with sliding expiry and delivered through an
`HttpOnly`, `SameSite=Strict` cookie. The cookie is marked `Secure` when
`x-forwarded-proto` reports HTTPS. Restarting the gateway invalidates all
logins.

## Client protocol

`src/protocol.rs` and `frontend/src/protocol.ts` define the client contract.
`GET /api/config` publishes the deployment branding before authentication —
the display name, and whether `GET /api/logo` (equally public) serves an icon
the page then sets as its favicon — and whether the `[airplay]` table is set,
which the page prints as `(airplay)` after its own version wherever it shows
one. There
is no client/server version negotiation: the gateway serves the matching SPA from
the same build, and no second client is supported.

Control and input messages are tagged JSON. Server messages cover picker and
connected state, desktop size, display selection, cursor shape, clipboard,
audio format, and errors. The `connected` message includes `resize`,
`clipboard`, and `audio` capability flags so clients expose only supported
controls.

It also carries two things a client cannot work out and nothing else reveals:
`render`, the resolved render dial, and `subtype`, the target's `ard` or
`ard-high-performance` where it has one. The
last is there because `protocol` is not an answer on VNC — a plain server and a
Mac on either subtype all say `vnc`, and they differ in whether resize is
offered, where the picture and sound come from, and whether the path beneath is
the reverse-engineered one (a display list is no longer the difference: wlshare sends
one over a plain `vnc` target). Both appear on the client's session card, which
`frontend/src/connectionLabel.ts` words, beside the video decoder's configuration
(`mediaLabel.ts`).

`GET /api/targets` carries `subtype` too, so the picker names it one step
earlier — the difference between two Macs in that list is a choice being made,
not something to discover after connecting. The row uses the config spelling
alone (`VNC · ard · 192.0.2.10:5900`); the card, which describes one target and
has the room, spells it out.

### Image batches

Screen updates use little-endian binary frames:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | u32 sequence | records

VIDEO    op 0x03: u8 flags | u16 w | u16 h | u32 len | payload[len]
```

`VIDEO` is the only record. One frame carries every unit ready at once, so a backlog
does not cost one WebSocket event per unit. Receivers reject unknown operations and
truncated records, and reject a nonzero frame flags byte. A `VIDEO` record's own
flags byte is `0x01` for a keyframe and nothing else — any other bit is rejected
the same way.

`sequence` starts at one and increases for the lifetime of one session-socket
attachment. After the paint worker has finished the batch's ordered
parse/decode/draw pass, the client sends `paintAck` with that sequence plus its
worker queue and draw times. `ws.rs` consumes this transport feedback rather than
forwarding it to the remote engine, and logs those measurements with the
attachment totals. A socket generation travels through the worker so a late
completion from a dead attachment cannot acknowledge a new one. This is the
measurement contract for application-level backpressure, and the gateway acts on
it three times: the paint window in `ws.rs` holds the next batch when too many are
owed or the oldest is owed too long, on a `render_adaptive` target the same
measurement — published through `LinkFeedback` — moves a stream's quality before
the window ever parks, and it decides when a batch's share of `QUEUE_BUDGET` (see
[choosing a chroma](#choosing-a-chroma), where the queues are sized) goes back to
the engine.
Nothing is dropped in any of them; an access unit's dependency order is untouched.

The window is pacing and must not be a way to wedge a session, so a batch parked
behind a client that stays silent is eventually sent anyway — but only a client
that *holds* what it owes can be called silent. Every ping carries the sequence of
the last batch written before it, the socket is ordered, and a browser echoes a
ping's payload from its network stack, so a pong is proof of receipt up to that
batch. The half-second grace runs from that proof, and a parked wait sends one ping
of its own rather than waiting out a heartbeat interval for it. Until the proof
arrives the silence is the link's, and it is waited out however long it lasts: a
grace timed from when the batch parked sends two batches a second into a link
carrying one every two, and the kernel's send buffer becomes the backlog the window
exists to prevent.

The same distinction settles the budget. A client whose acknowledgments arrive
about as fast as its distance allows — the oldest batch owed or the last round
trip, less the fastest round trip the socket has shown, within 100 ms — gets a
batch's share back at the write, and its flight is the window's to bound: holding
it to receipt instead capped an unthrottled attachment 100 ms away at 22 Mbit/s
that otherwise carried 65. A client that is behind keeps the share with the batch
until its acknowledgment or a pong says it arrived, because there a written batch
has only moved from a queue into the send buffer. Measured with an incompressible
12 Mbit/s of damage, that holds the picture 0.6 s behind at 4 Mbit/s and 4 s at
1 Mbit/s (23 s without), and the link's return to full speed is immediate.

`VIDEO` carries one VP9 access unit of the desktop. Its keyframe bit comes from the
encoder rather than from parsing the payload — VP9 carries no parameter sets to read
one out of. `(w, h)` is the desktop's true size, and the decoded picture may exceed
it by a pixel on either axis (see [the video stream](#the-video-stream)); a size that
differs from the last unit's is a stream that started over, preceded by a fresh
`videoFormat`.

### Audio frames

Remote audio is opt-in — `audio = true` on an `rdp` or a plain `vnc` target,
and the gateway-wide `[airplay]` table for every target of either Apple subtype,
whose sound arrives at the gateway's AirPlay speaker
([A Mac's sound over AirPlay](airplay-audio.md)) — and it has a socket of its
own. **Opening
`/ws/audio?session=<token>` is the subscription** — there is no message that turns
sound on, and closing the socket is the only way to stop.

The separation is the point. Sound and pictures used to share the session socket and
the bounded queue behind it, which is four frames deep; an audio pump waiting behind
a video backlog stops draining the bridge, and what the bridge then drops is wave
buffers. A lost wave buffer is a hole. The dedicated socket removes that picture-induced loss
path entirely.

The socket is bound to the *claim*, not to an attachment, so it survives a session
socket reconnecting and a target switch: the gateway re-announces the format when it
arms the next engine. It ends when the claim does — a takeover, or a log out — and is
superseded by a newer audio socket on the same claim. Its refusals mirror the session
socket's: 401 before the upgrade without a login, close code 4000 for a token that is
not the current claim, 4001 on eviction.

The gateway answers with `audioFormat` — the codec string, the decoder
configuration, and the samples in one packet — followed by binary frames:

```text
u8 kind = 0x03 | u8 flags = 0 | u16 packet count
repeated: u16 packet length | packet bytes
```

There is no codec byte in the binary frame; the codec is named once, out of
band, in `audioFormat`. Two options exist, chosen per target by `audio_codec`:

| `audio_codec` | `codec` | bitrate | `sampleRate` | `packetFrames` | `head` |
|---|---|---|---|---|---|
| `opus` (default) | `opus` | `audio_bitrate`, default 96 kbit/s, walking down to `audio_adaptive_min`, default 32 | 48 000 | 960 (20 ms) | `OpusHead` |
| `pcm` | `pcm-s16le` | 1.41 Mbps at 44.1 kHz, 1.54 at 48 | the source's: 44 100 for RDP and AirPlay, 48 000 for wlshare | 0 (self-describing) | empty |

Opus is encoded in `Application::Audio` mode under constrained VBR, set
explicitly in `src/opus_stream.rs`: `audio_bitrate` is the average the encoder
holds to, silence costs a few bytes a packet and a loud passage a little more
than the number, and the running rate stays close enough to it that the
configured kbit/s is what the link sees. There is no FEC and no DTX — the socket
is TCP, nothing is lost, and the adaptive walk already sheds silence.

The rate is a per-target key and the audio dial's `video_quality`: a ceiling the
link may fall below, on by default like `render_adaptive`, with
`audio_adaptive = false` holding the rate whatever the link does.
`AudioCongestion` (`src/audio.rs`) lives beside the pump's send. The audio
socket's queue is deliberately two deep; two consecutive sends that each wait at
least 20 ms are a behind verdict. The walk moves the encoder's bitrate down by a
third toward `audio_adaptive_min`, and back up by an eighth after sustained
clear sends. The change reaches the live encoder through `OPUS_SET_BITRATE`;
packets stay 20 ms and independently decodable, so nothing is re-announced.
While the link is *behind*, wave buffers that are pure silence are shed before
the encoder instead of queued — silence is the one content whose loss cannot be
heard, the client just receives no packets for a while (what a quiet remote
already produces), and the backlog drains by exactly that much. All three keys
are Opus-only and refused beside `pcm`, which has no encoder to tune; the floor
is also refused beside `audio_adaptive = false`, and the default floor is held
to a lower `audio_bitrate` rather than refused.

`pcm` is passthrough: the remote's wave buffer becomes one packet, byte for
byte, with no encoder in the gateway and no decoder in the client. `pcm-s16le`
is deliberately not a WebCodecs codec string — the packets are interleaved
signed 16-bit little-endian samples, which is what an `AudioBuffer` holds
already, so the client builds one directly and schedules it on the same path an
Opus packet reaches after decoding. That makes it the only option whose packets
reach no decoder at all — which is a property of the path, not a compatibility
escape hatch: the client refuses to start without WebCodecs either way.

It also makes it the only option whose `sampleRate` follows the source: 44.1 kHz
from an RDP host or a Mac, 48 kHz from wlshare. An
`AudioBuffer` carries its own rate, so a context built at 48 kHz before the
format arrived simply resamples on playback, exactly as the OS mixer would for
any buffer that is not at the device's rate.

The bandwidth is the whole of the trade: 1.41 Mbit/s at 44.1 kHz, 1.54 at 48, is
fifteen times Opus or more, and
is a local-network proposition only. It is not a quality argument — Opus at 96
kbps is well clear of audible loss on this material. Guacamole carries desktop
audio this way and only this way (its single encoder emits
`audio/L16;rate=44100,channels=2`), which is where the option came from.

The RDP engine carries sound over MS-RDPEA (`rdp_client/proto/rdpsnd.rs`).
`audio = true` names the `rdpsnd` and `rdpdr` static channels and leaves
`INFO_NOAUDIOPLAYBACK` out of the Client Info PDU; the host opens
`AUDIO_PLAYBACK_DVC`, negotiates 44.1 kHz 16-bit stereo PCM the moment something
plays, and every Wave2 buffer reaches `AudioBridge` from the client's own thread,
never through the event queue. `audio` absent or false sets the flag and names
neither channel, so the host's audio settings are left exactly as they were and the
session has no audio device at all.

Two rules of the Windows host, both measured and neither in the specification, and
each has been rediscovered the hard way more than once:

- **A quiet host negotiates nothing.** Windows sends its format list only once
  something is playing on the remote. Until then the sound channel is open and
  silent, the gateway logs "arming audio, the remote's audio channel is not up
  yet", and the browser's Audio button has nothing to play. A session with no
  sound is not, by that alone, a session with anything wrong; start a sound on the
  remote before deciding the client is broken; `tests/rdp_client_probe.rs` asserts
  the negotiation only under `REMOTEX_UAT_AUDIO=1`, which says one is playing.
- **No `rdpdr`, no sound.** A host redirects no audio to a client that named
  `rdpsnd` without also naming the device-redirection channel, even with no device
  to redirect. `rdp_client/proto/rdpdr.rs` is that channel's opening handshake and
  nothing after it, and it exists for this reason alone.

The queue never blocks an engine's read loop. `AudioBridge` retains sixteen remote
wave buffers (about three seconds at the measured Windows cadence) and drops the
oldest when a listener falls behind; no receiver means audio is discarded. Between
the bridge and the socket sits a second, shallower two-buffer FIFO
(`AUDIO_SOCKET_BUFFER`) whose only job is to absorb a socket write in flight.
Losses belong at the bridge, which keeps sound that is still live, rather than in
that FIFO, which would deliver stale audio faithfully.

The client owns its playback schedule. It starts at the current audio playhead
with no added cushion and clamps accumulated lead to 300 ms, trimming the front
of an incoming buffer instead of turning temporary jitter into lasting latency. What
reaches that schedule differs by codec, and only there. The client does not decode
anything itself: an *encoded* stream goes to WebCodecs, so a codec a browser will
not take surfaces as a decoder error naming it rather than as silence. A
`pcm-s16le` stream reaches no decoder at all; the client turns the packet into an
`AudioBuffer` and schedules it directly.

An **`ard-high-performance`** engine always carries sound, from the Mac's media
stream: AAC-ELD at 48 kHz stereo over SRTP, authenticated and decrypted per
packet, decoded by Fraunhofer's decoder on a thread of its own (`src/aac_eld.rs`)
and handed to the bridge as 16-bit PCM, 20 ms at a time. The format is announced
when the decoder opens and withdrawn when the receiver ends. The target takes no
`audio` key: the Mac refuses the picture without the sound, and mutes its own
output while it streams. See
[The media stream](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).

An audio-enabled **`ard`** engine receives no sound from its Screen Sharing
connection: Standard has no measured audio path. The session attaches
the engine's bridge to the gateway's AirPlay speaker instead (`src/airplay/`), a gateway-wide AirPlay 1
receiver that the Mac picks from its Sound menu once and keeps. What reaches the
bridge is the Apple Lossless the Mac streams, decoded to 44.1 kHz 16-bit stereo
in 24 ms wave buffers, from whichever Mac is playing to the speaker while that
session runs. The speaker holds the bridge weakly, so the engine ending is what
detaches it. It asks every sender for the `[airplay]` password, since it answers
the whole LAN. See [A Mac's sound over AirPlay](airplay-audio.md).

An audio-enabled **generic VNC** engine has no channel to negotiate either. It
lists wlshare's audio pseudo-encoding, and a server that speaks it announces so
with an empty rectangle, at which point the gateway names the format it wants —
48 kHz, 16-bit stereo, little-endian, so under `pcm` the packets are 48 kHz
rather than 44.1, and `audioFormat` says so — and turns the stream on; the sound then
arrives as FLAC frames on the RFB connection itself, and each is decoded into
exactly the samples wlshare captured, in the format the queue takes. A server
that announces nothing gives a desktop and no sound, which is the whole of the
failure mode: asking costs such a session nothing. The extension is wlshare's
own, its control messages borrowed from `rfbproto`'s QEMU Audio extension — see
[`wlshare-audio.md`](wlshare-audio.md).

A quiet remote and one that never negotiates audio are indistinguishable to the
client, so detailed negotiation status remains in the gateway log.

### Camera frames

**Experimental, for lack of tests.** The camera is the one path this gateway
ships without automated coverage of the redirection itself. Its socket rules and
message encodings are unit tested like everything else here — the claim and
engine binding, the eviction, the byte-for-byte control frames — and so is the
MS-RDPECAM wire the RDP client speaks, against the specification's own examples.
`tests/rdp_client_probe.rs` checks against a real host that the camera is
negotiated and its device opened, and its `a_real_host_streams_the_camera` opens
the host's Camera app and carries H.264 frames from a file to it. Like every real-host
probe it is ignored by default, and it checks that the host started the stream
and took samples, not the pixels the host displays. The dummy RDP server the
container tests drive offers no camera at all. The displayed picture is verified
by hand against a Windows host, and a change here needs a hand check.

The browser's camera goes the other way, on a third socket, to an RDP target or a
generic VNC target that opted in with `camera = true` (refused on both Apple
subtypes at parse time: Screen Sharing has nowhere to put a camera). **Opening
`/ws/camera?session=<token>` is the enable** — explicit, per session, and never a
remembered preference, unlike audio's "sound by default". Its refusals add one
code to the family: 401 before the upgrade, 4000 for a stale token, 4001 on
eviction, and **4002** when the running target carries no camera (or no engine is
running at all). Where the audio socket is bound to the claim alone and survives a
target switch, the camera socket is bound to the claim *and the engine*: every
engine end and every claim change closes it, so the next session always starts
with the camera off. Closing it — either side — unplugs the virtual device from
the remote.

The socket's first message is `cameraFormat`, naming the H.264 the browser's
`VideoEncoder` is configured for (geometry and a rational frame rate); its arrival
is what announces the device to the host. Binary frames follow, one encoded access
unit each:

```text
u8 kind = 0x04 | u8 flags (bit 0: keyframe) | the Annex B access unit
```

Downstream the gateway relays the host's decisions as `cameraStart` (with the
confirmed format), `cameraStop`, and `cameraKeyframe`; the browser encodes only
between start and stop, restarting at an IDR, and honors a keyframe request on the
next frame. Streaming begins when an application on the host opens the camera,
which is the host's move alone — an enabled camera on an idle desktop sends
nothing.

The host also decides whether redirection exists at all, before any client
message: the enumeration channel is created by the server, and a **Windows Server
without the Remote Desktop Session Host role never creates it**. Microsoft's own
client gets no camera against such a host either, so an enabled camera there is an
announcement nobody asks about: the socket stays open and `cameraStart` never
comes. Installing the role on the host is what turns the channel on:

```powershell
Install-WindowsFeature RDS-RD-Server -IncludeAllSubFeature -Restart
```

Windows 11 creates the channel and installs the redirected device as a real
camera, enumerable by every capture application, for exactly as long as the camera
socket holds it plugged.

The gateway never transcodes — the PCM-passthrough bargain in the other
direction. The browser encodes Annex B Constrained Baseline H.264
(`frontend/src/cameraSender.ts`) from a capture asked for at 640 pixels wide and at
most 15 frames a second — a host without a GPU was measured taking samples below 30,
and a faster camera only fills the queue until it drops to a keyframe — the host's
own camera stack decodes it, and the
gateway advertises exactly one media type: the announced geometry. There is no
codec key beside `camera`, and a browser that cannot encode H.264 reports that by
name instead of falling back.

The channel is the RDP client's own: `src/rdp_client/proto/rdpecam.rs` speaks
MS-RDPECAM on the session thread, and `src/rdp_camera.rs` adapts it to the
gateway's `CameraBridge` (`src/camera.rs`), which is all the session layer sees.
How the client negotiates the version, announces the device, answers the host's
queries and meters samples against the host's requests is in
[The RDP client](rdp-client.md#camera-ms-rdpecam).

On a generic VNC target the camera goes to wlshare instead, over its private
camera extension, and wlshare makes it a PipeWire camera on the wlroots desktop.
`src/vnc_camera.rs` asks for the extension beside the density and outputs
requests, holds the browser's plug until wlshare says it takes a camera, and
relays wlshare's start, stop and keyframe as the same bridge signals, so the
socket and the browser behave as they do against RDP. A server that never answers
is sent nothing and the enabled camera is never started, as on a Windows Server
without the RDSH role. See
[The browser's camera over VNC with wlshare](wlshare-camera.md).

The browser's microphone follows the same path on a generic VNC target:
`src/vnc_mic.rs` asks for wlshare's microphone extension, plugs the microphone
when the mic socket attaches and unplugs it when the socket closes, relays
wlshare's start and stop as the bridge's open and close, and sends the bridge's
decoded PCM between them. See
[The browser's microphone over VNC with wlshare](wlshare-microphone.md).

### Display geometry

Client JSON messages cover pointer, wheel, keyboard, clipboard, display
selection, viewport size, refresh, and session control. Pointer
motion is coalesced while the socket has queued bytes; any non-motion input
flushes the latest held position first.

Pointer clients present every remote pixel at 100%, scrolling when the desktop
is larger than the window. The sole presentation-scale exception is a mobile
client marked `HostDisplay::fit`, gated by `CAN_PINCH_ZOOM`, which starts
fit-to-width and layers pinch zoom over it. Lack of remote resize support never
permits a pointer client to scale the canvas to fit.

`ClientMsg::Viewport` is the window's CSS size in points, including immediately
after a connect when no remote scale has been announced. `ServerMsg::Resize`
reports framebuffer pixels and the remote density; the browser presents it at
`w / scale` by `h / scale` CSS pixels. The scale is never a fit factor.

A target's `resize` means the window drives the remote's size, continuously and
on every engine alike: an engine that has it applies every `viewport` it is
sent — in points, the window's CSS pixels, rendered at the engine's own density —
an engine without it drops them all, and the client sends them exactly
when `connected` said `resize` — on every window change, with no toggle, no
manual button and no remembered preference beside it. Standard `ard` rejects
`resize` at config parse because it shares physical displays.

The opening size is one rule for every engine that can ask for one: the pinned
`width`/`height` when the config sets both, else the full resolution of the
client's own screen — carried in the `connect` message so it exists before the
engine's handshake — else `DEFAULT_SIZE`, 1440×900 points. That default is
sized for what an unasked-for session costs at 2x: a HiDPI client renders it at
twice the points, so 1920×1080 would mean capturing, scaling and encoding 4K
every frame, where 1440×900 comes to 2880×1800. An operator who wants the larger
desk pins `width`/`height`. A mobile `HostDisplay::fit` client has no screen
suitable for laying out a desktop, so it uses the pinned size or that default
while its density still counts. See
`TargetConfig::opening_size`; `width` and `height` remain options because whether
the operator specified them is meaningful.

A pin is spent whether or not `resize` is granted, because the two answer
different questions: the pin is the size the session *opens* at, and `resize` is
whether the browser window drives it afterwards. RDP connects at the pin, High
Performance builds its virtual display at it, and generic VNC asks for it with a
single `SetDesktopSize` as soon as the server declares support — seeded into the
same held-request slot a viewport report uses, so it goes out on the first
`ExtendedDesktopSize` rect and no earlier. A pinned target without `resize` stays
at the pin on all three. With `resize`, RDP and High Performance open at the pin
and then follow the window, because they state a size at connect and no report
can precede that; generic VNC cannot state one until the server declares support,
by which time the browser — which reports its window as soon as `connected`
reaches it — has superseded the held pin with the size it actually wants, so the
session opens at the window and the pin is left answering a later default-size
request. That supersede is the same rule any stale hold gets: a replay must never
ask for a desktop the window has already left. Standard `ard` is the only engine
with nothing to spend a pin on, and there `width`/`height` only answer a later
default-size request.

What is engine-specific is the mechanism:

| Engine | With `resize` |
|---|---|
| Generic VNC | applies a requested size, on servers accepting SetDesktopSize |
| Apple Standard VNC | rejects `resize`: it shares physical displays |
| Apple High Performance VNC | applies dynamic-resolution sizes within its fixed 3840×2160 backing ceiling |
| RDP | applies a requested size, and the client's reported display density |

Every desktop is also held under the gateway's 3840-pixel long
side by 2400-pixel short side ceiling at the negotiated density. RDP opening and
layout sizes and generic VNC `SetDesktopSize` requests all pass through
`video::fit_ceiling`; High Performance separately keeps its native 3840×2160
backing ceiling. This changes what the remote is asked to render, not how the
browser scales it: a 5K window receives at most a 3840×2400 desktop at 100%, with
the remainder bare. A pinned size already
over the ceiling at 1x is rejected during config parsing; a physical or
non-resizable remote may still reach the encoder's refusal because the gateway
cannot ask it for a smaller desktop.

High Performance paces what the window asks for. A second
`SetDisplayConfiguration` overlapping the first, or a region of the old size
served just after a change shrinks the display, crashes the Mac's agent. So a viewport or density report
waits for a second of quiet, and the newest size is the one that goes. Only one
change is out at a time, with no timeout short of 30 seconds. It is sent at the
end of an update, just after the automatic-update region is re-armed to one pixel,
and pixel polling holds to that pixel until the answering layout. A layout that
changes nothing, such as the Mac's repeat of its opening one, is not an answer.
From the first report until the
answering layout has held still for half a second, the gateway sends `resizing`
with `active: true`, and the page covers the desktop with a dimmed, blurred
"Resizing…" as Apple's client does, instead of showing each intermediate mode. A
session opens covered. The display it connects to is the Mac's own, and the
virtual display and then the window's size follow. The cover takes no input and leaves the menu reachable, and a
browser that reattaches mid-resize is told again. No other engine sends
`resizing`. The measurements are in
[Resizing a High Performance display](apple-vnc-889.md#resizing-a-high-performance-display-as-measured).

`hostDisplay` reports the screen the client's window is on — its full resolution
and its density. Mid-session only the density is acted on, and only with
`resize`: RDP quantizes it to 1x or 2x at a midpoint (and opens at it, from the
screen `connect` names), a High Performance virtual
display re-renders the same points at it; the resulting density travels back as
the `scale` on `resize`, and clients present the framebuffer at `pixels / scale`.
Other engines ignore the message. Generic VNC, whose wire carries no density, is
presented at 1x and takes no density from the client; see
[HiDPI over generic VNC](generic-vnc-hidpi.md). wlshare is the one generic
server that reports a scale, over a private extension every generic session asks
for and discovers by the answer: the label follows its reports and the client's
density is declared to it, never applied by the gateway itself; see
[Pixel density over VNC with wlshare](wlshare-density.md).

A client shows the display picker exactly when the target sends it a
`ServerMsg::Displays`, and hides it otherwise. The VNC engine sends one on both
Apple subtypes and against wlshare: it parses an `AppleDisplayLayout`, or
wlshare's `OutputList`, into a `displays` message and acts on a `selectDisplay`
by asking that remote for that screen. RDP exposes a single framebuffer spanning
every remote screen and has nothing to enumerate, and so does any other generic
VNC server, so neither sends the message and the picker stays hidden there.

Where the list is sent, the checkmark moves only when the remote comes back naming
the screen it is now sending — never on the click. On a Mac the engine prepends an
*All Displays* entry of its own so a client that picks a screen can get back; see
[`apple-vnc-889.md`](apple-vnc-889.md). wlshare captures one output at a time and
has no combined view to offer, so its list is the compositor's outputs and nothing
else; see [Switching outputs over VNC with wlshare](wlshare-outputs.md).

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine.

### Clipboard

Clipboard support is a per-target opt-in available on all engines. The backend
holds the latest remote value and its observed change time:

- generic VNC forwards and buffers `ServerCutText` or Extended Clipboard data;
- both Apple VNC subtypes read and write the Mac's native compressed pasteboard;
  while a fetch is pending, the normal framebuffer cycle finishes its one
  outstanding response and pauses before requesting another, leaving the ordered
  server stream free to deliver the pasteboard reply;
- RDP opens MS-RDPECLIP and carries `CF_UNICODETEXT` alone. Both directions of
  that protocol are lazy — a copy announces *which formats* it can be had in, and
  the bytes cost a second round trip — so the gateway asks the moment the remote's
  format list arrives, which is what makes a remote copy reach the browser
  unprompted as it does on the other two engines. See
  [The clipboard](rdp-client.md#the-clipboard-ms-rdpeclip).

Clients may request the current value after attaching, since they may have
missed earlier pushes. Replies to that explicit request are marked separately
from unsolicited changes. Only unsolicited changes are eligible for automatic
remote-to-local synchronization; an explicit fetch fills the UI until the user
chooses Copy.

Transfers are capped at 512 KiB and refused rather than truncated. Browser
clipboard integration is best effort because Safari's permission rules, and an
unfocused tab, may prevent automatic access.

### Liveness

The gateway sends a WebSocket ping every five seconds. Browsers answer at the
protocol layer, independent of application timers. About 60 seconds with nothing
at all from the browser ends the engine; an orderly close starts a fresh 60-second
reattach window. Any frame counts, not a pong alone: a ping queues behind every
batch already written, so on a slow link the pong is the last thing to come back,
while the acknowledgment for each batch that did arrive says the same thing sooner.
On the session socket a ping's payload is the sequence of the last screen batch
written before it, which is what makes its pong a receipt (see
[Image batches](#image-batches)).

All remote sockets use `TCP_NODELAY`, a 20-second connect budget, a 30-second
handshake budget, and TCP keepalive. Linux also uses `TCP_USER_TIMEOUT` to bound
unacknowledged writes. These checks prove only that the peer's kernel responds.
RDP and RFB have no portable application ping.

Browser-facing sockets get `TCP_NODELAY` too — `NodelayListener` in
`src/server.rs` sets it on every accepted connection, in both the served and
embedded shapes. Those sockets feed an ack-gated paint window, and a segment
Nagle holds back is that window stalled for a round trip.

## Engines

### RDP

The protocol is the gateway's own client, `src/rdp_client/`, down to the wire
format: `rdp_client/proto/` encodes and decodes every PDU against [MS-RDPBCGR],
and `rdp_client/` owns one thread per session, a complete framebuffer painted from
those decoders, and an event per damaged rectangle. The engine (`src/rdp.rs`)
compares those rectangles with a shadow of pixels already sent, splits the
remainder into bands, and encodes off the event loop. Input is mapped from DOM
codes to scancodes and queued to the client's thread as fast-path events.

The client carries the desktop, the pointer, keyboard, mouse, resize, the
clipboard and sound, and no touch: touch is announced only by a host that opens
MS-RDPEI, which this client never asks for. What it would take is in
[`roadmap.md`](roadmap.md).

Static virtual channels are asked for by key: `drdynvc` for `resize = true` or the
default `egfx = true`, `cliprdr` for `clipboard = true`, and `rdpsnd` with `rdpdr`
for `audio = true`.
Under the Graphics Pipeline (MS-RDPEGFX) the server draws through surfaces on a
dynamic channel, marks every frame's end — which is the engine's flush signal, with
the 16 ms coalescer demoted to a 100 ms safety net — and answers a monitor layout
with a graphics reset. Its decoders cover what a current Windows host draws with —
ClearCodec and the NSCodec inside it, RemoteFX Progressive, planar, uncompressed —
and its compositor carries the copies and caches between them, so the desktop is
lit and sharp; a rectangle that will not decode is left for the host to draw again,
not made the end of the session. `egfx = false` is the bitmap path: the
server draws with bitmap updates, damage is flushed on the 16 ms guess because those
carry no frame boundary, and the desktop keeps its opening size — `resize = true` is
refused beside it, because an RDP resize is the pipeline's graphics reset.
On either path the pointer travels as its own shape rather than in the framebuffer.

Read [The RDP client, written here](rdp-client.md) for the whole of it: the
connection sequence, the channels and the chunk flags a Windows host silently
requires, the codec and damage path, resize and density, the clipboard, and sound.

[MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c

### VNC

The built-in client speaks two dialects, chosen by the target's `subtype`, that
share everything below the handshake — one read loop, one input path, one video
path. Both force the same 32-bit true-color BGRX pixel format rather than
negotiating one, and use the same shadow and encoder path as RDP. `src/vnc_encodings.rs`
decodes whichever encoding a server picks into the packed RGB888 the shadow and the
mirror take, so nothing above it knows which was chosen.

**RFB 3.8** is used by generic `vnc`. It supports None, classic VNC
authentication and RealVNC's RSA-AES security types (5 and 129), plus the
Cursor pseudo-encoding and Cursor With Alpha — the same shape
with its alpha, so a shadow and antialiased edges survive where Cursor's 1-bit
mask cuts them away. Only its Raw form is read, which is what TigerVNC, QEMU and
wlshare send; QEMU's pixels are in its native `B, G, R, A` rather than the
spec's `R, G, B, A`, which a greyscale guest pointer does not show. Plain VNC carries
`vnc_password` for classic `VncAuth`, and `username` and `password` for RSA-AES —
the account a server such as wayvnc (`enable_auth`) or RealVNC checks — taking
whichever the server offers and the encrypted one when it offers both.
`src/vnc_rsa_aes.rs` is that exchange and the AES-EAX framed transport every byte
of such a session then rides in, exposed to the engine the way Apple's record
layer is: an `AsyncRead` and a per-message sink. The server's RSA key is logged
by fingerprint, not verified.

Apple Standard mode maps X11 modifiers by its own table. Measured on macOS 26,
`Alt_L`/`Alt_R` and `Super_L`/`Super_R` all arrive as Command,
`Meta_L`/`Meta_R` arrive as the corresponding Option key, and `Mode_switch` and
`ISO_Level3_Shift` do nothing. The engine therefore sends a keyboard's Alt codes
as Meta on a Mac (`keymap::apple_keysym`) and leaves Windows keys as Super; which
physical key a browser calls `AltLeft` is settled before that, in the page, which
is the only end that knows what the host keyboard is. The
server also drops pointer and key input during the first seconds of a session;
`tests/ws_probe.py --key` waits eight seconds before injecting for that reason.

Generic `vnc` advertises the standard lossless encodings in preference order —
CopyRect, ZRLE, zlib, Hextile, RRE, Raw — and a server encodes with the first it
supports, so a modern one settles on ZRLE and uses CopyRect for scrolls and window
moves. Tight, TightPNG, JPEG and H.264 are deliberately absent: vendor or lossy,
and this gateway re-encodes every frame for the browser anyway. CopyRect names a
source region rather than carrying pixels, so its source is read back out of the
shadow — the VNC link still carries no pixels for a scroll — and a source the
shadow does not know costs one non-incremental repaint rather than an invented
picture.

Generic `vnc` asked for `audio` also advertises wlshare's **audio**
pseudo-encoding (`WLSF`). `src/vnc_audio.rs` is that wire: the server announces
support with an empty rectangle of the encoding, the client answers with the
QEMU Audio extension's set-format and enable, and the server sends QEMU's begin,
a run of FLAC frames in message `0xE4`, and QEMU's end. Discovery works the way the density extension's does, and a server that
never announces leaves the session silent rather than failing it. See
[`wlshare-audio.md`](wlshare-audio.md).

Generic `vnc` also advertises **ContinuousUpdates** and **Fence**, which go
together. A server that supports the first answers the `SetEncodings` carrying it
with an `EndOfContinuousUpdates` message — the only way it is ever announced — and
the client then asks for the whole desktop and stops polling: updates arrive as
the screen changes rather than one per request, which takes a round trip out of
every frame. Non-incremental requests are unaffected and still go where they went,
because a repaint no amount of waiting for damage will produce is exactly what a
reattach, a resize and an unknown CopyRect source need; a resize also re-sends the
enable, since the region is part of the request. What that removes is this
engine's only pacing, which is what Fence restores: the server sends a marker down
the stream and asks for it back, and the read loop echoes it immediately, so its
congestion control can measure this end. A server offering neither is unaffected —
it says nothing and the polling loop never stops. The Apple subtypes are not
offered either: their encoding lists are measured exact, and adding to one costs
the display layout.

The client advertises DesktopSize and ExtendedDesktopSize on every generic
target, so a server can always say its size changed; `resize = true` decides only
whether the window asks it to change, with `SetDesktopSize`. Generic VNC clipboard support uses Extended Clipboard when the server
advertises it and falls back to Latin-1 `ServerCutText` otherwise. Both Apple
subtypes negotiate Apple's display metadata and native pasteboard instead, and ask
for zlib in their first `SetEncodings`.

**Apple Standard mode remains fixed-size.** It rejects `resize = true`, shares the
Mac's physical displays and never sends a viewport size or `SetDesktopSize`.
Density is handled by the Mac instead: from each `AppleDisplayLayout`, the gateway
reads the displays' native densities and the viewer scale already applied. It sends
`SetServerScaling` so the selected display, or All Displays over screens of one
density, matches the browser display's density. The answering layout is
authoritative. Its pixels pass through unchanged, and its effective density
(`native density × viewer scale`) is the `Resize.scale`.

All Displays over screens of *different* densities is the one view no factor can
render. There the gateway asks for 1.0, as Apple's viewer does, and sends a
`ServerMsg::Mosaic` ahead of the `Resize`: each screen's rectangle in the
framebuffer and in points. The paint worker keeps the framebuffer off screen and
draws every screen at its points at the browser's own density, and the page maps
pointer positions back through the same regions (`frontend/src/mosaic.ts`). It is
the only place the browser rescales remote pixels. See
[Apple RFB 003.889, as measured](apple-vnc-889.md#all-displays-over-mixed-densities).

**RFB 003.889** is Apple's own protocol revision, and both Apple subtypes speak
it, as Apple's viewer answers every Mac before choosing a mode after ServerInit.
None of it is documented by Apple, so every claim in this section is measurement
or a reading of Apple's binaries rather than specification, holding for the Macs
in [apple-vnc-889.md](apple-vnc-889.md) rather than for the protocol. The
dynamic-resolution path behind `resize = true` remains reverse engineered. It
authenticates with Apple's Diffie-Hellman security, type 30, with the macOS
account's username and password: named, the connection shares that user's screen,
where an anonymous one lands at a separate login-window session. It then differs
from RFB 3.8 in three places and nowhere else: the version banner, the `0x81`
ClientInit byte (the enhanced ServerInit, without the session-select exchange
`0x40` asks for), and a cleartext `SetEncryption` prelude after which every byte in
both directions rides inside an AES-128-CBC record layer keyed by a rekey message
the server delivers, of all places, inside a framebuffer rectangle. Apple's viewer
asks for that layer only under a preference that is off by default; remotex always
does. `src/vnc_record.rs` is that transport, exposed to the rest of the engine as
an ordinary `AsyncRead` and a per-message sink; `src/vnc_apple.rs` is the message
and payload layer above it. The Mac reads the pointer mask positionally on this
revision, so right and middle swap bits for both subtypes.

**High Performance mode is a virtual-display mode.** The gateway sends
`SetDisplayConfiguration` (`0x1d`) during setup, with one mode built from the
pinned `width` and `height` when both are set, or from the connecting client's
screen resolution otherwise, at that screen's density. The mode sits under the
native descriptor's fixed 3840×2160 backing ceiling. Once connected, the remote
Mac's physical displays are disabled and all of its windows are placed on that
virtual display. Apple's
official macOS Screen Sharing client can choose up to two virtual displays, while
Remotex always requests one. The full descriptor enables dynamic resolution on
every fresh session. With `resize = true`, the window continuously drives the
virtual display through Apple's dynamic-resolution feature: later viewport reports
resend the same full descriptor with the requested mode, and the Mac's answering
display layout sets the actual framebuffer geometry. There is no client-side
resize mode or one-shot button. The Mac supplies that virtual display the way
it does to Apple's viewer: as HEVC over its media stream, offered once the display has
settled and decoded in the gateway by FFmpeg's libavcodec (`src/vnc_apple_media.rs`), in a
gateway built with the `apple-hp-media` feature. Zlib rectangles carry the
picture until the stream delivers and across every display change. A stream the
Mac refuses, that brings no picture or that stops ends the session, as it ends
Apple's viewer's. While it runs, polling holds to one pixel, which still brings
cursor shapes and layouts. Apple's virtual-display-count and
resolution-preset controls remain unimplemented.

The wire constraints remain load-bearing: `SetEncodings` must list both
`DisplayInfo` (`0x44d`) and the layout (`0x451`), in any order, or the Mac reports
no layout; and a layout's `u16` length counts the bytes after itself, with a `u16`
display count ahead of the records. The byte layouts and protocol corrections are
in [`apple-vnc-889.md`](apple-vnc-889.md) — read that before touching this path.

Deliberately absent: Apple's own still-image codecs. The media stream's sound
leg comes with its picture on `ard-high-performance`; the other two subtypes'
sound arrives over AirPlay ([A Mac's sound over AirPlay](airplay-audio.md)). The
transport's measurements are in
[Apple RFB 003.889](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).
The native Apple pasteboard works on every
subtype; 003.889 enables monitoring before the rekey and carries the fetch and
data messages inside its encrypted record layer. See [`roadmap.md`](roadmap.md).

## Clients

### Browser SPA

The React SPA has login, target picker, and remote desktop states. It decodes
the desktop's video stream onto a canvas, applies incoming frames serially, and overlays mouse,
keyboard, touch, clipboard, display, and audio controls.

**It refuses to start without a secure context and both WebCodecs decoders**
(`preflight.ts`, before React mounts), and that refusal is what lets the rest of the
client be simple: nothing downstream tests for either again or carries a fallback for
its absence. `navigator.clipboard`, `navigator.keyboard` and WebCodecs itself all
require a secure context; the gateway speaks plain HTTP and has no TLS listener, so
one comes from how the page is reached — loopback (`localhost`, `127.0.0.1`, `[::1]`,
any `.localhost` label), or a TLS-terminating reverse proxy. A LAN address over
plain `http://` is the case this refuses,
by name. `VideoDecoder` and `AudioDecoder` are asked for together rather than either
alone, because audio is a target's choice and every target streams video: a
browser with one and not the other would play some targets' sound and not others, which is the
half-working session the gate exists to prevent. What remains reportable mid-session
is a *codec* a decoder refuses, which is a different sentence and arrives from the
decoder itself.

There are two ways for this page to be given the six Command chords a browser
otherwise keeps — ⌘W, ⌘T, ⌘N, ⌘L, ⌘O, ⌘R. A **Chrome app window** (`appWindow.ts`:
*Install page as app…*, or `--app=`) reserves no keys at all, so they arrive as
ordinary keydowns and `preventDefault` is the whole of it; that is the configuration
the client is meant to be run in. A plain tab gets the same from **immersive full
screen** plus `navigator.keyboard.lock` (`fullscreen.ts`, `keyboardLock.ts`), which
asks for every key rather than a list: ⌘Q, and the keys no window of any kind is
otherwise given — the Super key, Alt+Tab — so Super+E reaches the guest instead of
opening a local file manager over it. The lock is not a control of its own; it follows
the full screen, and the full screen is the menu button.

Which full screen is the whole of it, and the distinction is invisible from the
outside. Chromium activates a lock in `WebContentsImpl::RequestKeyboardLock` only
while `IsFullscreenForTabOrPending` holds — *element* full screen, entered through
`requestFullscreen()`, which is what **Menu → Immersive full screen** calls on
`documentElement`. Chrome's own full screen (the ⛶ beside the zoom row, or F11) hides
the frame and nothing else: `document.fullscreenElement` stays null, no lock is
activated, and the host keeps every key it reserves behind a remote desktop that fills
the screen. A page cannot promote one into the other, so the client offers its own
button and `keyboardLock.ts` deliberately does not watch `(display-mode: fullscreen)`
— arming on that took a lock Chromium never made active, which is the failure it
looked like a fix for. The way out of the mode is that button again, or holding
Escape, which is Chromium's own exit from a locked full screen.

The Command translation table itself is always complete and never changes with
fullscreen. App windows therefore send every chord in windowed and fullscreen use
alike, while a normal windowed tab remains subject to the shortcuts Chrome consumes
before the page sees them — and an app window still needs immersive full screen for
the Super key, which no browser window is handed without the lock. The window kind moves in one
direction only: *Install page as app…* reparents the live document into the new window
instead of reloading it, so `appWindow.ts` latches its answer true and notifies rather
than answering once at load — and full screen, which reports `display-mode: fullscreen`
and would otherwise unmake an app window mid-session, is what the latch defends
against. A close chord the page never sees — and Alt+F4, which no window catches —
ends the session without asking: the client raises no leave-site dialog, because a
dialog on every deliberate window close is worse than the session it saves.

A key held on the remote is let go by the page while the page is there to do it. A
page that goes away cannot: its keyups die with its socket, and the page that comes
back holds nothing of its own. So the session layer follows every key, button and
touch contact it forwards to the engine, and releases whatever is still down when the
attached browser leaves — a detach, an attachment superseded by a reload, a claim
evicting the socket — and before any engine ends. Otherwise a reattach resumed an
engine still holding a Control nobody was pressing. While it is there, the page
hears of a release two ways: the key's `keyup`, or the overlay's `blur`, which sweeps
everything still down. The local system can withhold both for a modifier — a chord it
keeps for itself, such as a window manager's move-window drag or a screenshot
shortcut, swallows the `keyup` without ever taking focus — so `heldModifiers.ts`
follows the physical modifiers and checks them against the modifier state every later
key, mouse and wheel event carries. One the event reports as up is released before
that event is forwarded, through the Command translator like the `keyup` it stands in
for. The translator reads the same flags for itself, because it can hold keys for a
Command the page never saw go down — ⌘-Tab into the window, then ⌘V with Command still
held — and so one `heldModifiers.ts` cannot lapse: any event reporting Command up
ends what the translator held under it, the synthetic Control of a mapped chord
included, without the bare tap a seen release would send. The soft keyboard's sticky
modifiers are outside it: the page holds those, and no event's flags know them.

The canvas is presented at the remote's point size, derived from framebuffer
pixels and remote scale. Desktop clients scroll when necessary. Touch clients
use fit-to-width presentation, pinch zoom, pan, a virtual cursor, and
multi-finger gestures without changing framebuffer coordinates.

That touch layer is a trackpad, and there is a second one that is a touchscreen.
When an engine's host opens a touch channel (MS-RDPEI on RDP), the gateway says
`touchReady`; a touch-capable client then shows a **Touchscreen** switch
(remembered, off by default) that forwards fingers as `touch` contacts — down,
move, up, cancel, in framebuffer pixels, named by small slot ids — instead of
interpreting them, and the guest recognises the gestures itself. A reattach
re-announces it. No engine opens one today — the RDP client does not offer
MS-RDPEI and VNC has no touch — so no session sends `touchReady` and the switch
stays hidden. See `frontend/src/touchPassthrough.ts`.

On a Mac host connected to a non-Mac remote, selected Command shortcuts are
translated to Control. A Mac-keyboard toggle disables translation, and the
gateway's `remoteOs` message suppresses it for Mac remotes.

The mirror of that is a keyboard with no Command key of its own. A PC keyboard
reaches a Mac the way the same keyboard plugged into one does — Windows key
Command, Alt keys Option — which leaves Command behind the key a Windows host
guards hardest: it keeps Super+C for itself, beside Super+L and the rest of the
Super chords it reserves, so the chord never becomes a key event the page can
forward and copy cannot be typed at the Mac at all. On a non-Mac host driving a
Mac remote the page therefore sends the left Alt key as the left Command and
both Windows keys as the right, which is RealVNC's default from a PC keyboard,
and leaves the right Alt key as Option
(`frontend/src/altAsCommand.ts`). Held keys follow the code that went out, as
they do for a translated Command chord, so a release lifts the Command rather
than the Alt. The left Option key is what it costs, and the soft keyboard still
carries it: those chords are sent by code and never pass through the
substitution. Neither side of this is a preference.

While the floating menu has something over the desktop — its drawer, or the one
modal card that opens from it and leaves the drawer standing — the desktop is
**view-only**. No input listener is attached at all (`useRemoteDesktop.ts`), which
is what gives the page back the chords the surface would otherwise take: ⌘C and
Ctrl+C among them, so the text on a card can be copied. The automatic clipboard
sync stands down with them, in both directions — a remote copy arriving behind the
card is not mirrored onto the browser's clipboard, and the browser's is not pushed
to the remote — because for as long as the menu is up that clipboard holds what was
copied off this page rather than anything the remote sent. The surface keeps
painting, under a dimmed layer that says which of the two it is doing. Every way
back out hands the keyboard to the surface as it goes, because the key listeners
live there: the ✕, the chord that hides the menu, a drawer button that closed the
drawer behind it. The soft keyboard is the one control in this menu that is itself
keyboard input, so a key pressed there takes the drawer down as it sends — the
label never stands over a remote being typed on.

Each tab stores its claim token in `sessionStorage`, allowing reconnects to
reclaim the same slot. Busy and evicted states require explicit takeover or
reclaim actions.

### Local multi-instance control plane

`remotex tui --port <port>` is the native local control plane. It discovers one
instance per immediate subdirectory, creates and edits the same serverless
`remotex.toml` format the former Electron viewer used, and starts, stops or
restarts each gateway from its own list. `remotex.localhost:<port>` is a landing
page; `<instance>.remotex.localhost:<port>` is that instance's browser origin.

```text
browser: <instance>.remotex.localhost:<port>
                    │ Host-routed HTTP and WebSockets
                    ▼
             TUI master process
                    │ <instance>/gateway.sock
                    ▼
       hidden serve-embedded subprocess
```

The master is the only TCP listener, and it takes its port the way `serve` takes
its address: `DEFAULT_PORT` (52380, the one `[server].listen` defaults to —
they are two ways to serve, never two servers) unless `--port` or
`REMOTEX_TUI_PORT` overrides it. Both loopbacks are bound through the same
`server::bind_all` a served gateway uses, so the policy is one implementation: a
family this host does not have is a warning, and a port already in use is fatal
on either of them, because a browser picks the family and a master left holding
`[::1]:<port>` would keep routing to its own workers. Nothing asks the kernel
for a port — an ephemeral one is a control plane nobody can be told how to
reach, and `SharedPort::bind` refuses `0` on every path, tests included.

Each hidden worker binds
`<instance>/gateway.sock` at mode `0600`, prints one JSON readiness line —
`{"socket","token"}` — after binding, reads only that instance's
`remotex.toml`, and stops when its parent's stdin closes (`src/embedded.rs`,
`Audience::Embedded`). The master seeds the token as a host-only HttpOnly
`remotex_session` cookie before proxying the browser to the child. Raw connection
proxying preserves both ordinary HTTP and WebSocket upgrades without another
gateway protocol implementation.

The entire substrate is behind the default `embedded-gateway` Cargo feature:
the module, token authentication, config audience, CLI commands, and their
`check-config --embedded` validation mode compile out together. Native packages
retain it. Container artifacts are built separately with
`--no-default-features --features airplay`; the build script and Dockerfile reject a
binary that exposes any embedded CLI surface.

There is still no separate native client: every instance is the same SPA loaded
by Chrome or Edge from its subdomain. The TUI is process and configuration
control, not another remote-desktop implementation.

## Configuration and testing

Configuration is one TOML file with `[server]` and `[[targets]]` sections.
Protocol-specific fields are validated at startup, including mutually exclusive
credential fields and unsupported feature combinations.

A gateway needs a target to offer and a credential to guard it, and is told where
to listen. `remotex check-config` applies those rules to a file — or to text on
stdin, which is what an unsaved edit is — without starting anything.

Where it listens is one key, `[server].listen`, and the one setting a deployment
can give from outside the file: `--listen`, or `REMOTEX_LISTEN` for a container
that has an environment but no argv to edit. An override replaces the address
whole rather than either half of it, so the running address is always the one
somebody wrote in one place.

It takes two forms. `host:port` is the one a browser can reach; every address the
host resolves to is bound, and the IPv6 wildcard `[::]` is bound as two sockets,
`[::]` for IPv6 and `0.0.0.0` beside it, on every platform — rather than as one
dual-stack socket, which on Windows is IPv6 only by default and, made dual-stack,
still binds beside another process's `0.0.0.0` on the same port. `unix:<path>`
binds a socket instead, for a gateway that only ever answers a reverse proxy on
the same machine: the socket is created `0660` so the filesystem decides who may
connect, a leftover from a killed gateway is taken over on the next start, one
that something is still serving refuses the start, and the file is removed when
the gateway stops. No client addresses that form directly — the page reaches its
gateway over one HTTP origin and two WebSockets, all of which need a host and a
port, so whatever terminates the proxy is what a browser talks to. An embedded
gateway is that arrangement in one process tree: the worker listens on
`<instance>/gateway.sock` and never on TCP, and the thing terminating the proxy
is the TUI master, which the browser reaches over loopback TCP and which
forwards each connection to that socket.

`[branding]` is a top-level table rather than `[server]` keys: it names the
deployment rather than the server, and one value with two spellings is one of
them going stale. There is one place to write it and no second spelling. `text`
is the display name; `logo` is the image the gateway serves at `GET /api/logo`
and the page sets as its tab icon.

A logo is written either as a path to an image file or as a `data:` URL holding
the image itself, and the value decides which — nothing else begins `data:`, so
one key covers both and no config can set two. Either way the content type is
settled at resolution, from the extension or from the URL's own media type
against the same closed list of what a tab can show; a `data:` URL is decoded
there too, so `check-config` refuses an icon the browser would have. A file is
then read per request, which is what lets an operator swap the image without a
restart; an inline one is held in the resolved config as `Bytes`, cheap to clone
with the state around it.

`[meter]` is top-level for the same reason and records the throughput of the
browser's four WebSockets in an SQLite database, one row per target, socket and
timeframe, so targets can be compared, and measures the rate they move at.
`enabled` is what turns it on and a table must carry it, so the file says which
it is rather than leaving it to be inferred from the table's presence: a table
can then hold its settings while it is off, and the other keys are checked as
written either way, so `check-config` refuses an unusable database before it is
ever switched on. The
database is `meter.sqlite3` in the
gateway's state directory unless `database` names another, and a relative
`database` is taken from there as well: the installation's state directory beside
its config and web paths when `serve` reads the installed config, the config
file's own directory when `--config` names one, and the instance directory under
`remotex tui`. Each socket counts the frames it writes and
reads — text and binary frames with their headers, never the heartbeat's pings and
pongs, so an idle connection records nothing — into the counters of the target
the session has selected at that moment, which the session manager publishes in
an atomic beside its state so a frame never takes the session lock; bytes moved
on the picker count under no target. Once a second the counters are taken as a
sample: what moved in that second, over the seconds since the last sample, is the
rate right now, and it is added to the open timeframe, which also keeps the
busiest second per direction it has seen and the second itself where it moved
anything. A sample the runtime delayed is that rate in each of the seconds behind
it rather than in one of them: what moved over five seconds is drawn as five
seconds at a fifth of it, not as one busy second beside four the graph would read
as idle. Every minute the open timeframe is closed and each target's socket that
moved data gains a row holding its bytes, its peaks and its seconds. The second is
the meter's resolution and the minute is only how often it reaches the disk;
neither is a configuration key. The seconds travel as one blob of unsigned LEB128
triples — the second of the timeframe it is, counted from the second the timeframe
begins, then the sent rate and the received rate — and a second that
moved nothing is not among them, so a socket busy through a whole minute costs
some four hundred bytes and one that moved in three of its seconds costs a dozen.
A trickle that rounds to nothing a second is in the row's bytes but is no second
of the graph's. The rows wait in memory for a separate writer task, so
a slow write or SQLite's busy wait never delays a sample; the writer adds them
and deletes the rows past `max_records` per target and socket oldest first, all
in one transaction, so a crash leaves the timeframe written or not at all. The
default keeps a week of minutes per target and socket. It is
best effort by design: the open timeframe dies with the process, and a failed
write is retried at the next close. A file that is not this gateway's throughput
database — not SQLite, SQLite without its application id, or another schema
version — is refused at startup before anything is written to it. The page reads
two things and draws one graph from them. `GET /api/throughput/live` is the last
sample, per target and socket, which the "Throughput" view polls once a second
while it is open and not paused, one poll out at a time, keeps for the last five
minutes, and shows the way a network meter does: the rate now over a graph of the
chosen range, one per direction on its own scale, narrowed to a target or a socket
by its filters. Four grid lines carry the scale and its rates, and a dashed line in
the direction's own colour crosses the plot at the range's average rate — its bytes
over the seconds something moved in, so an idle stretch does not pull it down — the
same average the tile beside the graph names, so the line carries no rate of its
own. The tile names the busiest second of the range too, but the graph does not mark
it: a line at the peak lets one busy second set the graph's mark, where a line at
the average shows that second as the outlier it is. The scale fits what is drawn —
the highest step, or the average line where a step's quiet seconds put that above
every step — and not the busiest second: second by second the two are one number,
but a recorded step is an average over its timeframe and the busiest second inside
it stands above that, and a scale fitted to it would flatten the graph under a
number it does not draw. A range that moved nothing gets no line, since one at zero
only traces the axis.
`GET /api/throughput?within=<seconds>` is the recorded rows, counted
back from the gateway's clock the rows were stamped with rather than the
browser's, with the gateway's clock at the read and the open timeframe as it
stands; `?from=<unix>&to=<unix>` reads a range that names its own ends instead,
and a query that carries both forms, or ends where it begins, is refused as the
nonsense it is. The range is one select — the last 60 seconds up to the last 30
days, everything kept, any whole number of seconds, minutes, hours or days, or
"Between…", two times typed in the browser's own zone and sent as the seconds
they come to. A range that ends now decides which of the two sources the graph is
drawn from: one no longer than the five
minutes of samples kept is drawn from them second by second, with a gap for a
second not read, its right edge following the gateway's clock rather than the last
sample so a failing poll leaves gaps. A longer one, and every range between two
times however short — the seconds kept are the view's own, not any clock's — is
drawn from the recorded rows. A read whose range is an hour or less is answered
with the seconds that moved in it, says so in the read itself rather than leaving
the page to guess from rows a quiet range has none of, and is drawn a point per
second, or per as many seconds as keep the graph within 600 points, each the average over its own
length with the quiet seconds counted as the zeroes they are; a read that is
answered without them is drawn
a point per timeframe instead: the average over one, or over as many as keep the
graph within 600 points, a row's bytes shared between the points it overlaps and a
timeframe with no row drawn as zero. The rows are read when the range is chosen and
again every ten seconds while the read carries the seconds, or as each timeframe
closes while it does not, since a graph of timeframe averages has nothing new to say until
one of them ends. The peak beside the rate now, which the graph's scale and its
dashed line both sit at, is the busiest second any one row in the range carries.
A range between two times is drawn between them, and stops at the read where it
reaches past it, since nothing is recorded ahead of the clock; its axis stands at
the times themselves where the others stand at how far back they reach. That read
takes the open timeframe and the rows still waiting for the writer together under
one lock before it queries the database, then counts a row the database has
meanwhile once, from the database, so a timeframe closed during the read is never
missing from it nor counted twice. Both are behind the login, and `/api/config` says whether there is a
database to offer. The gateway stores bytes, peaks and times; the page divides
for an average where it is given no seconds, and shows every rate in decimal bits
per second, as a network meter does. What an
engine exchanges with its remote is a different link and is not counted.

Unit tests cover protocol parsing, configuration, authentication, key mapping,
audio, and engine helpers. Tests under `tests/` exercise HTTP/WebSocket session
flow and protocol engines. Containerized dummy servers cover RDP and VNC.

Stable headless browser tests under
[`tests/playwright`](../tests/playwright/README.md) cover deterministic DOM,
control-plane, HTTP, and WebSocket behavior. Rendering races and timing
measurements remain in raw-protocol and container tests.
