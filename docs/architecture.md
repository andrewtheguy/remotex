# Architecture

remotex is a single-user gateway for RDP and VNC targets, including Macs reached
through their built-in Screen Sharing service. A Rust backend owns the remote
protocol session and exposes one common HTTP/WebSocket interface to the React SPA,
which is the only client. [Using remotex](guide.md) is the operator's view of
the same system.

## Data path

```text
browser SPA over loopback or the network
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input, binary picture batches
   │  /ws/audio: the audio format, then binary audio frames
   │  /ws/camera: the camera format and H.264 samples up, start/stop down
   │  /ws/mic: Opus microphone packets up, open/close down
   ▼
axum server ── single session slot ── protocol engine
                                         ├─ RDP through the built-in client
                                         └─ built-in RFB client (3.8 or Apple 003.889)
```

Ordinary RDP and VNC source frames are decoded in the gateway and sent as one VP9
stream of the whole desktop, at the quality and chroma the target's render plan
resolves to. wlshare, reached as `subtype = "wlshare"`, instead codes that
resolved VP9 stream itself for the gateway to pass through unchanged. A
VNC desktop too large for that stream, in a session that does not resize it, has no
picture: the session stays up and the page offers the remote's displays — see
[past the ceiling](#past-the-ceiling). A
Mac is reached
over Apple's own RFB 003.889 with Apple Remote Desktop authentication, as
Apple's viewer reaches it: in Screen Sharing's Standard mode with `subtype = "ard"`,
or in High Performance with `ard-high-performance` (a virtual display, with its
picture and sound over the Mac's media stream, as Apple's viewer takes them),
whose AAC-ELD sound every session passes to the browser, and whose HEVC a session
started with the passthrough passes too rather than re-encoding it — see
[Apple's media stream, passed through](#apples-media-stream-passed-through). An RDP
session started with its passthrough is not decoded here either: the host's
graphics pipeline is passed on for the browser to compose, which is in beta — see
[RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through).
How a session's desktop is sized, whether it takes the remote's sound and whether
it passes the remote's stream are chosen at the picker before it starts — see
[What a session is started with](#what-a-session-is-started-with). Remote audio is encoded as
Opus, save the Mac's passed AAC-ELD, wlshare's own Opus, passed too, and a target's lossless FLAC
([Lossless sound](#lossless-sound)), and sent on `/ws/audio`, never on the picture queue.
The browser's camera goes the other way on `/ws/camera`: browser-encoded H.264,
passed through to an RDP host over MS-RDPECAM, or to wlshare over its camera
extension on a `wlshare` target. Its microphone uses `/ws/mic`: browser-encoded
Opus decoded to the 16-bit PCM an RDP host or wlshare records. Both redirections
are experimental — see [Camera frames](#camera-frames) and
[Microphone frames](#microphone-frames).

## Constraints

The rules every change keeps, by area. [AGENTS.md](../AGENTS.md) names each
area and points here; read the area's section before changing what it covers.

- Remote credentials remain in the server-side TOML configuration.
- Clients speak only the remotex protocol and never implement RDP or RFB.
- Protocol engines speak each protocol's baseline to every server, and the
  project prioritizes three: Windows' own Remote Desktop, macOS's built-in Screen
  Sharing and wlshare. Behavior specific to one of them is welcome, and when work
  for them and for other servers competes, they come first.

### Server tiers

The three prioritized servers are ranked in tiers by how seamlessly each
integrates with remotex and with its host's operating system: how its displays
are chosen and resized, whether it carries sound and the browser's camera and
microphone, and how its picture reaches the browser. The ranking is not the
order work is done in: Windows and macOS are the common use case, so testing
and optimization prioritize them.

| Tier | Server | Target | Displays | Sound | Camera and microphone | Picture |
|---|---|---|---|---|---|---|
| 1 | wlshare on Linux | `vnc`, `subtype = "wlshare"` | the compositor's outputs, switched from the picker; a headless one follows the window at its density | Opus or FLAC | yes, experimental | its own VP9, passed through, adapting to the browser's link |
| 2 | Windows 10 and 11's Remote Desktop | `rdp` | one desktop spanning the host's screens, following the window at its density; or up to two virtual displays, switched from the picker or, on *All Displays*, the second shown in a browser tab of its own (alpha) | Opus or FLAC | yes, experimental | VP9 from the gateway, or the graphics pipeline passed through (beta) |
| 2 | macOS Screen Sharing, High Performance | `vnc`, `subtype = "ard-high-performance"` | one virtual display, following the window at its density; or two, switched from the picker or, on *All Displays*, the second shown in a browser tab of its own (alpha) | AAC-ELD, always | no | VP9 from the gateway, or the Mac's HEVC passed through |
| 3 | macOS Screen Sharing, Standard | `vnc`, `subtype = "ard"`, the unofficial `virtual_display = true` included | the Mac's physical displays, one or all, at their own size and density; the unofficial virtual display follows the window | none | no | VP9 from the gateway |
| baseline | any other VNC server | `vnc` | one framebuffer at the size the server says, at 1x | none | no | VP9 from the gateway |

The three native servers have these in common:

- **The desktop outlives the viewer.** A Windows host holds a disconnected
  session for the next logon, and a Mac's or a wlshare desktop keeps running
  with nobody watching. A browser coming back from a reload or a dropped
  connection resumes where it was. A different browser, a phone picking up what
  a desktop started, say, starts at the picker and chooses its own size, sound
  and passthrough; starting the target returns it to the same desktop, with its
  windows as they were left.
- **HiDPI and Retina.** Each renders at the pixel density of the browser's
  screen, or says which density its pixels are, so the desktop is sharp on a
  Retina display and is shown at its true size.
- **The pointer travels apart from the picture.** It arrives as its own shape
  and the browser wears it on its own pointer, so it moves with the hand rather
  than a network round trip behind it.

#### Tier 1: wlshare on Linux

[wlshare](https://github.com/andrewtheguy/wlshare), this project's own VNC
server for wlroots-based Wayland desktops, is the ideal. It codes the desktop as
VP9 itself, at the quality and chroma the target asks for, and walks that quality
by the browser's link, so the gateway passes its stream through untouched and
the session still adapts to a slow link. Because wlshare is ours, what RFB
lacks is added to it as an extension: pixel density, switching outputs,
sound, and the browser's camera and microphone. It is a `vnc` target with
`subtype = "wlshare"`, which is what makes the gateway list those extensions and
ask for the stream. See
[wlshare's VP9 and the `wlshare` subtype](#wlshares-vp9-and-the-wlshare-subtype).

#### Tier 2: modern Windows' Remote Desktop, and a Mac's High Performance Screen Sharing

The host's own stream, passed through to the browser for a LAN:

- **Modern Windows' own Remote Desktop server**, in a session started with the
  passthrough: the host's graphics pipeline (MS-RDPEGFX), composed in the browser
  by the gateway's own compositor built to WebAssembly, which takes nearly all of
  the picture's work off the gateway. It is in **beta**: run against one
  Windows 11 host, with sound and the clipboard beside it, and not yet with the
  camera or the microphone. A target with `egfx_h264 = true`, more experimental
  still, lets the host draw video with H.264 on that pipeline, which the browser
  decodes; without the key what is passed is lossless. See
  [RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through).
- **macOS Screen Sharing's High Performance mode** (`ard-high-performance`), in a
  session started with the passthrough: the Mac's HEVC picture, to a browser
  that decodes it (Chrome and Safari; not Firefox). Its AAC-ELD sound is passed
  in every session. See
  [Apple's media stream, passed through](#apples-media-stream-passed-through).

The passthrough is a choice made at the picker, greyed for a browser that cannot
take the stream. A passed stream does not adapt to a slow link: the Mac's own
rate control keeps it between 20 and 60 Mbit/s, and a Windows host's pipeline is
sent as drawn. A session started without it is the gateway's VP9, which does
adapt and is the answer for a slow link. VP9 also serves a browser that cannot
decode the Mac's stream, and a Windows host that draws with plain bitmap updates
rather than the pipeline. On RDP the passthrough is the only way past VP9:
without it the gateway composes the host's pipeline, or takes its bitmap updates,
and encodes the picture as VP9.

#### Tier 3: a Mac's Standard Screen Sharing

Screen Sharing's Standard mode (`ard`, including the unofficial
`virtual_display = true`) is decoded in the gateway and encoded as VP9, adapting
to the link. Nothing of it is passed through.

#### Other servers, not prioritized

Every other VNC server is a plain `vnc` target, reached through the RFB baseline
and always encoded as VP9 in the gateway, at 1x and without sound. The gateway
reads the standard lossless encodings, ZRLE first, then zlib, Hextile, RRE and
Raw, with CopyRect beside them; Tight and the other vendor or lossy ones are not
listed. A wlshare server behind a plain target is read the same way: its
fallback for ordinary VNC clients. Plain VNC stays supported, and is worked on
as needed rather than ahead of the tiers.

Another RDP server, an older Windows or xrdp say, may happen to work if it
speaks what the client implements ([The RDP client](rdp-client.md)), but it is
not a target: it is not tested against. Its picture follows Windows' rule,
encoded as VP9 in the gateway unless the session was started with the pipeline
passed.

### The client and its bundle

- There is one client: the browser SPA, whether in a browser or installed as a
  Chrome or Edge app. The user may at times start an experimental native client
  to try out a use case; one stays maintained only while its benefits justify a
  separate client.
- There is one frontend build, compiled from Cargo's `OUT_DIR` into the gateway
  binary (`src/assets.rs`) and served from its origin root. A standalone build
  and the release artifact use `frontend/dist`; a Cargo build produces the same
  bundle privately or stages that artifact. Do not add a web root, a
  `static_dir`, or any run-time path the SPA is read from. Every URL the page
  uses goes through `frontend/src/gateway.ts`.
- The bundle holds two WebAssembly modules, each a directory of its own under
  `frontend/wasm` and built by the frontend's build for
  `wasm32-unknown-unknown`. `frontend/wasm/egfx` is the compositor, around
  `crates/remotex-rdp-graphics`, built with threads by the dated nightly that
  directory's `rust-toolchain.toml` pins. The pin is that module's alone: the
  gateway and the graphics crate build on stable. Its threads share a memory, so
  every file `src/assets.rs` serves carries the two cross-origin isolation
  headers; keep them, and load nothing from another origin.
  `frontend/wasm/flac` is the lossless sound's decoder, the page's
  alone: the gateway's FLAC is libFLAC and shares no code with it, so it has no
  crate under `crates/`. It has no threads, needs no shared memory, and builds
  on stable. Do not fold one module into the other, or add a third without a
  stream that needs it.
- The only versioned client asset read at run time is the BETA software
  HEVC decoder's release archive, found in the data directory or named by `[hevc_wasm]`: read once at start-up, refused unless
  it is the release `src/hevc_wasm.rs` pins by SHA-256, and served from memory at
  `/hevc/`. An operator-configured path in `[branding].logo` is read per request
  and is not part of the client bundle. Do not widen the decoder input to another
  file, an unpinned archive, or a directory, and do not turn the logo path into a
  web root.
- The page requires a secure context plus `VideoDecoder` and `AudioDecoder` and
  refuses startup in `frontend/src/preflight.ts` without them. Do not add
  fallback browser paths.

### Sessions

- One gateway has one active session slot; concurrent, shared and multiple
  sessions are out of scope. A new client may force a takeover and evict the
  previous holder, which ends the session: a browser never inherits one it did
  not start, and lands on the picker. Every `connect` starts from scratch after
  the previous engine exits; only the owning browser's reattach to the same
  target resumes an engine. Preserve the takeover and fresh-session behavior in
  [Session lifecycle](#session-lifecycle).
- A session's displays may be shown in more than one tab of the browser that
  holds it, and in no other browser: each display rides a display socket of its
  own, let in by the login cookie the session was claimed under and never by a
  token. Only *All Displays* over two virtual displays does this, on an RDP target
  or a High Performance Mac, for the second display at
  `/display/2` (alpha). That is one session shown twice, not a shared one; do not
  let a display socket in for any other login, or hand a tab the claim's token.
- Size, sound and passthrough are chosen under the target at the picker and
  carried by `connect`; they are not config keys, and the gateway holds the
  session to them. The size a session will have is shown before Start: a size
  the desktop keeps, the target's `size` or the default, or the window's.
  Which of the three a target shows is its type's to say
  (`TargetConfig::offers`): one it does not offer has no row and is refused in a
  `connect`, and one it offers that cannot be had is greyed with the reason. Do
  not add a config key that makes one of these choices, a session-time control
  that changes one, or a default the gateway applies for a browser that named
  none. Choices are made by the browser that will see them: a takeover lands on
  the picker, and so does the owner coming back unable to take the session's
  passthrough. Do not hand a browser a session with choices it did not make,
  rebuild one with other choices for it, or send it a stream to fail in its
  decoder. See
  [What a session is started with](#what-a-session-is-started-with).

### Input and display

- Touch has two mutually exclusive layers: `touchGestures.ts` treats fingers as
  a trackpad and interprets gestures in the page; `touchPassthrough.ts` forwards
  uninterpreted MS-RDPEI contacts when an RDP host reports `touchReady`. Never add
  gesture recognition to touch passthrough. See
  [Browser SPA](#browser-spa).
- The display picker is the remote's list and the remote's checkmark: engines
  fill it, and the browser holds no display state of its own. Never move the
  checkmark on the click or add a client-side selection. An engine with nothing
  to choose between sends no list and the panel stays hidden. See
  [Switching outputs over VNC with wlshare](wlshare-outputs.md).
- Pointer clients present the remote desktop at 100%; oversized desktops scroll.
  Do not add fit-to-window, zoom-to-fit, or viewport-derived scaling. Mobile,
  gated by `CAN_PINCH_ZOOM`, is the sole fit-to-width/pinch-zoom exception.
- The soft keyboard has one press engine (`softKeyPress.ts`), listening on the
  key area, for every layout; a key is data (`softKeyboard.ts`) and never has a
  handler of its own. A phone — a touch screen of a phone's size — docks it and
  insets the canvas; every other client floats it. See
  [Browser SPA](#browser-spa).
- Neither the gateway nor the browser rescales what a remote sends: frames are
  presented at `w / scale` by the density the remote confirmed. When the size or
  density is wrong for the browser, ask the remote to render the right one and
  output its answer as is. The sole exception is Apple Standard's All Displays
  over screens of different densities: the gateway sends a `mosaic` and the
  browser composes each screen at its points (`frontend/src/mosaic.ts`). Do not
  extend it to another engine, view or density.
- `ClientMsg::Viewport` is in CSS points. `ServerMsg::Resize.scale` is remote
  pixel density, not a fit factor. A session started with resize has the window
  continuously driving the remote size, and any other keeps the size it opened
  at; the choice is made once, at the picker, so do not add a resize toggle to
  the session, and do not offer the window to a plain `vnc` target, whose answer
  to a size is not known before it is dialled. Density is the wire's word
  alone, and a plain `vnc` target, or a `wlshare` one whose density request goes
  unanswered, is presented at 1x: do not add a client-side density control, and never label a framebuffer with a density the
  server has not confirmed. Read
  [Display geometry](#display-geometry),
  [HiDPI over standard RFB](standard-rfb-hidpi.md) and
  [Pixel density over VNC with wlshare](wlshare-density.md) before
  changing geometry.
- Read [Apple RFB 003.889, as measured](apple-vnc-889.md) before changing
  either Apple Screen Sharing subtype. Treat High Performance behavior as
  reverse-engineered measurements, not a specification.

### Media paths

#### One encoder, or passed untouched

A session's picture reaches the browser one of two ways, both ordinary:
- **Encoded here** as VP9 by the gateway's one video encoder, the
  `screen-vp9` crate wlshare codes its own stream with, pinned by release tag
  in `Cargo.toml`. A libvpx setting, the conversion in front of it, the codec
  string or the quality walk changes in the screen-vp9 repository and reaches
  here as a pin bump.
- **Passed untouched**, as the remote made it, for the browser to decode or
  compose: today wlshare's VP9 on a `wlshare` target, and in a session started
  with the target's passthrough a High Performance Mac's HEVC or an RDP host's
  graphics pipeline, each with its rule below. Another stream the user
  asks to pass joins them with a rule of its own; do not refuse it on this
  rule's account.

The gateway keeps to one encoder: whatever it transcodes goes to VP9, and a
second encoder (H.264, AV1 or any other), a codec probe, or a codec key that
selects one is not added as a side effect of other work.

#### The browser's four answers

The browser is asked four questions, each once at page load and stated on the
session socket: which VP9 profile its decoder takes (for
`render_chroma = "auto"`), whether it decodes a High Performance Mac's HEVC,
whether it composes an RDP host's graphics pipeline, and whether it decodes the
H.264 such a pipeline may carry. The
gateway *selects* a chroma on the first and never refuses a client for it. The
last refuses nobody either: it decides only whether a passed pipeline's host is
told it may draw with H.264, on a target whose `egfx_h264` key allows it. The
other two say which passthrough the browser can take. They grey the choice at
the picker, where a browser that says no starts the target encoded here; they
refuse a `connect` that asks for the passthrough all the same; and they end the
session of an owner that comes back answering no, which lands on the picker
([Session lifecycle](#session-lifecycle)). No other browser's answers meet a
running session: a takeover ends it first. The one target
they can leave unstartable is a High Performance Mac on a gateway whose host
lacks FFmpeg, which can only pass the picture. Do not grow them into
a wider capability negotiation, and do not let either rebuild a session with
choices nobody made.
Preserve the announced configuration and color-space behavior in
[The codec](#the-codec) and
[Choosing a chroma](#choosing-a-chroma).

#### A desktop past the ceiling

A desktop past the video ceiling has no picture, and nor has a Mac's All
Displays over more than two screens, whatever its size. On VNC started without
resize the session stays up and the page says so, offering the remote's displays; every
other source ends on the ceiling's refusal. Do not carry such a desktop some
other way, in the gateway or the page: rectangles as images, a scaled or a
cropped picture. Two streams for Apple's All Displays are the planned way, in
[the roadmap](roadmap.md#two-streams-for-apples-all-displays). See
[Past the ceiling](#past-the-ceiling).

#### wlshare's VP9 and the `wlshare` subtype

`subtype = "wlshare"` on a `vnc` target says the server is wlshare, and is the
one thing that lists any of wlshare's extensions to it: its VP9 encoding, the
density and output-list requests, the audio extension in a session started with
sound, and with their keys the camera and microphone extensions. It is also what
sends a glide's scroll as wlshare's distance message rather than as wheel-button
notches. A plain `vnc` target lists none of them and scrolls by the notch,
whatever server
answers: a wlshare behind one is read through the RFB baseline, ZRLE encoded
here at 1x on the output it opened with, which is the fallback wlshare keeps for
ordinary VNC clients. Do not list a wlshare extension on a plain target, or
detect wlshare on one.

A `wlshare` target lists the VP9 encoding for every browser, with the plan's
chroma, dial and walk as pseudo-encodings beside it, and passes its frames
untouched; the stream is the subtype's picture, with no key and no choice at
the picker beside it. A server that ignores the listing is encoded here from
ZRLE. Do not transcode a passed frame, pass one at a chroma other than the
plan's, ask wlshare for the plan with a client message, or add a key or a choice
that selects the stream. See
[wlshare's stream, passed through](#wlshares-stream-passed-through).

#### Remote audio

- Whether a session takes the remote's sound, and as Opus or lossless, is
  chosen at the picker, on the targets that offer it: `rdp` and `wlshare`. A
  session started without it asks
  the remote for none, so the host keeps playing where it did. The session's
  audio button reads Mute and Unmute because the browser's subscription is all
  it changes; do not make it a way to start or stop the remote's sound.
- Remote audio uses its own `/ws/audio` socket and queue; opening the socket is
  the subscription. Do not put audio on the session socket. It is Opus encoded
  here, save a High Performance Mac's AAC-ELD, which every session passes as it
  came and the gateway never decodes; do not add a decoder for the Mac's sound.
  A `wlshare` target's is always passed too: wlshare codes it, as Opus with the
  encoder the gateway codes an RDP host's with (`sound-opus`) at the rate the
  target's audio keys and their walk arrive at, or as FLAC in a lossless session.
  Do not decode or re-encode wlshare's sound here, and do not give wlshare a
  codec key of its own: the format is the gateway's to ask for.
  Preserve claim-bound eviction and the source-format/resampling
  boundaries in [Audio frames](#audio-frames).
- The sound's format is part of that choice, Opus or lossless, and not a config
  key. Lossless is FLAC: a `wlshare` target is asked for FLAC frames,
  passed as they came like its Opus, and an `rdp` target's PCM is coded as FLAC
  here by libFLAC; the page decodes either
  in its own WebAssembly module (`frontend/wasm/flac`), never through WebCodecs.
  No other target offers it: `ard-high-performance` passes the Mac's
  AAC-ELD and nothing else, and `ard` and a plain `vnc` target carry no sound.
  Do not transcode a passed frame, add a third format, give the FLAC a rate
  to walk, or add a key that selects the format. See
  [Lossless sound](#lossless-sound).
- VNC audio is wlshare's audio extension (Opus packets or FLAC frames, with
  the QEMU Audio extension's control messages), on a `wlshare` target: a session started with
  sound makes the gateway list it, and a server that never announces it leaves
  the session silent rather than failing it. A plain `vnc` target carries no
  sound and offers none. Do not take raw PCM from the RFB connection or add a
  third codec to it. See
  [Desktop audio over VNC with wlshare](wlshare-audio.md).
- `ard` carries no sound and offers none: the Mac's sound keeps playing where
  the Mac sends it. Do not add an AirPlay receiver or any other sound path for
  it to the gateway.

#### Apple High Performance

- `ard-high-performance` takes the Mac's picture and sound together from High
  Performance's media stream (`src/vnc_apple_media.rs`): HEVC and AAC-ELD over
  SRTP, every packet authenticated before it is decrypted and every report sent
  as SRTCP. The Mac refuses one leg without the other, so a session always
  carries sound and the picker offers no choice of it. The sound is passed to
  the browser as the Mac's AAC-ELD units in every session, so the gateway has
  no decoder for it. The picture's decoder, FFmpeg's libavcodec, is the
  system's shared library, loaded when it is needed, or on Windows from the
  folder `[hp_decoders]` names, loaded at start-up, so published release
  artifacts do not link it; only the non-default `apple-hp-media-static` feature
  links a static archive instead. A gateway whose host lacks it can only pass
  the picture: `/api/targets` says so, the picker shows the passthrough as
  chosen, and a session started without it all the same ends before it dials
  the Mac.
  ZRLE is stepped over by its length, never inflated or shown; the
  browser remains covered until the media stream sends the display's first picture.
  A stream that fails ends the session: do not add a subtype
  without the stream or a fallback to zlib, combinations Apple's viewer never
  offers. The Mac's own controller sets the rate from the offer's bitrate
  entries and the gateway's rate reports; do not cap the offer or add a key that
  turns the reports off. See
  [The media stream](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).
- `virtual_display = true` on `ard` is the one unofficial combination: Standard
  mode's ZRLE session on the virtual display High Performance asks for, resized
  the same way, with no stream and no sound. It is `ard`'s key alone, refused on
  every other target, and everything but the display follows `ard`. Call it
  unofficial wherever it is named, and tested with macOS 26 only; do not present
  it as a mode of Apple's viewer or grow it into a third subtype.
- The passthrough on `ard-high-performance` passes the Mac's picture
  unaltered, for a LAN: HEVC access units on the display socket. It is offered
  at the picker to a browser that said it decodes the HEVC; a session started
  without it is sent VP9. The sound is not part of the choice: AAC-ELD units go
  on `/ws/audio` either way. Decoded or passed, the
  stream is the whole picture: the Mac's ZRLE rectangles are never shown, and
  the browser stays behind its resize notice until the stream's first picture,
  at connect and across every display change. A PLI is a passed stream's
  repaint. The page answers for the
  sound by decoding one of the Mac's units in each form `isConfigSupported`
  accepts, since it accepts forms that do not decode, and plays in the form that
  decoded (`frontend/src/appleMedia.ts`). Keep the choice to that stream. See
  [Apple's media stream, passed through](#apples-media-stream-passed-through).

#### RDP's graphics pipeline

The passthrough on `rdp` passes the host's graphics pipeline (MS-RDPEGFX) to
the browser, for a LAN: its commands out of their bulk compression, never
altered, as `GRAPHICS` records on the display socket behind a `graphicsStart`;
the gateway neither composes nor encodes them. It is offered at the picker by a
target with the pipeline on, to a page that said it composes one: a
cross-origin isolated page, for the compositor's threads, with a WebGL 2 canvas
to present on (`frontend/src/rdpGraphics.ts`). The page
composes with the gateway's own compositor, `crates/remotex-rdp-graphics`,
bound to WebAssembly by `frontend/wasm/egfx`: keep that crate building for
`wasm32-unknown-unknown`. One decoder and compositor is the gateway's rule; do
not write a second one there. The page, which already carries a software HEVC
decoder of its own, may decode, compose or present the pipeline its own way
(on the GPU, say) where that brings a measured gain. The host answers a
repaint out of its caches, so a reattach starts such a session over; do not
resume one on a repaint. A host that draws with bitmap updates is encoded here
as VP9. Call it beta wherever it is named to an operator.

H.264 stays refused in the capability advertise of every pipeline the gateway
composes: a host would hand the parts of the desktop that move like video to a
lossy codec before the gateway encodes the picture, and the gateway has no
decoder for it. Do not give it one. A passed pipeline may carry it, behind the
target's experimental `egfx_h264` key and only to a page that said it decodes
it: the access units ride inside the pipeline's commands, the page decodes them
with the browser's `VideoDecoder`, and the compositor paints the pictures. It is
a target key and not a row at the picker while it is experimental; it adds no
wire format, no record and no second stream. See
[RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through).

#### Camera and microphone

- Browser camera redirection is MS-RDPECAM on RDP and wlshare's camera extension
  on a `wlshare` target, H.264-only, and never transcoded by the gateway. It uses its own
  `/ws/camera` socket, is explicit per session, and is bound to both claim and
  engine. See [Camera frames](#camera-frames) and
  [The browser's camera over VNC with wlshare](wlshare-camera.md).
- Browser microphone redirection is MS-RDPEAI on RDP and wlshare's microphone
  extension on a `wlshare` target: low-bitrate mono Opus from the browser, decoded here
  to the 16-bit PCM the host records in. It uses its own `/ws/mic` socket under
  the camera socket's rules (explicit per session, refused with `4002` when the
  target carries no microphone, closed with the engine) and is never put on the
  session socket. See [Camera frames](#camera-frames) and
  [The browser's microphone over VNC with wlshare](wlshare-microphone.md).

## Backend

| Module | Responsibility |
|---|---|
| `server.rs`, `auth.rs` | HTTP routes, SPA serving, login sessions |
| `session.rs` | target selection, takeover, detach, and reattach |
| `ws.rs`, `protocol.rs`, `wire.rs` | WebSocket bridge and client wire format |
| `rdp.rs` | RDP engine: damage, input, cursor, resize, clipboard, over `rdp_client` |
| `rdp_client/` | the RDP client, protocol and all: `proto/` is the wire format, the rest is the session and input queue |
| `crates/remotex-rdp-graphics/` | the RDP client's graphics, a crate of its own: the codecs, the graphics pipeline's compositor and the framebuffer, which the page's WebAssembly module runs too (`frontend/wasm/egfx`) |
| `rdp_clipboard.rs` | `CF_UNICODETEXT` and the line endings either direction needs |
| `vnc.rs` | RFB connection, framebuffer, input, cursor, clipboard, resize |
| `vnc_apple_media.rs` | High Performance's media stream: the offer, SRTP, HEVC depacketizing and decoding, and the sound's receiver |
| `aac_eld.rs` | what that stream's AAC-ELD sound is, for the browser that decodes it |
| `camera.rs`, `mic.rs` | browser camera and microphone bridges into the active engine |
| `shadow.rs` | change detection: what the client already has |
| `encode.rs`, `stream.rs`, `video.rs` | the ordered, paced, congestion-aware stream: its mirror, its rounds, and the picture limits |
| `vp9.rs` | the VP9 stream over the mirror, coded by the `screen-vp9` crate wlshare shares — the one place libvpx is spoken to for either side |
| `audio.rs`, `opus_stream.rs`, `pcm48.rs` | PCM queue, Opus encoding by the `sound-opus` crate wlshare codes its own sound with, resampling, the FLAC coding of a lossless target's PCM, and the passing of a remote's own stream |
| `frontend/wasm/flac/` | the page's FLAC decoder for a session's lossless sound, a WebAssembly module of its own |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine consumes `ClientMsg` input and emits the same `ServerMsg` stream.
Where the gateway owns the encode, RDP and VNC pass dirty pixels through the
ordered encoder before reaching that boundary.

Ordering is a correctness requirement throughout the frame path. Every access unit
is a change from the one before it, and a resize changes the picture that follows.
The encoding and outbound queues therefore keep access units, resizes, and cursor
updates in source order even though each encode runs off the engine's own task.

### The video stream

Each target has one full-desktop picture path. Ordinarily it is one inter-frame
VP9 stream, encoded by the gateway or made by wlshare and passed through. Two
paths, chosen at the picker, keep another representation the remote made: an
`ard-high-performance` session can pass the Mac's HEVC,
and an RDP session can pass the host's graphics pipeline for the browser to
compose. A VNC desktop past the stream's picture ceiling in a session started
without resize has no picture until the remote sends a smaller one — see
[past the ceiling](#past-the-ceiling).

A target's stream keys are per target, and every one has a default:

- `video_quality` (1–100, default 90) is the ceiling the stream holds to.
- `render_chroma` (`"auto"`, the default, or `"420"` / `"444"`) is how much colour
  the stream carries per pixel. A target that writes nothing resolves it per
  browser; the two fixed answers are selections no decoder can overrule. See
  [the codec](#the-codec) for why it, and not the quality, is where a desktop
  stream's picture goes, and [choosing a chroma](#choosing-a-chroma) for when to
  take the decision away from the browser.
- `render_adaptive` (on unless a target writes `false`) lets VP9 encoded in the
  gateway track the measured link — see
  [what the link will bear](#choosing-a-chroma) for the signal and the walk.
  There is no floor key: the walk's floor is a constant of the encoder's, where
  it hands off from quality to frame rate as WebRTC's quality scaler does at its
  own quantizer threshold, and the settle sharpens a quiet desktop back at the
  dial. Turned off, the walk is the pressure-only one.

None of them reaches a passed stream: a session started with the passthrough
sends the Mac's HEVC, or the RDP host's pipeline, as it came.

The engines never see the config keys. They and the session's choices collapse
to one `RenderPlan` (`quality`, `adaptive`, `chroma`, `apple_media`,
`rdp_graphics`, `rdp_h264`) at the config boundary in
`TargetConfig::render_plan`, which reaches the encoder through the engine-agnostic
`VideoSink` in `src/encode.rs`:

```text
video_quality / render_chroma / render_adaptive, and the session's choices
  → TargetConfig::render_plan(choices, browser decoders) → RenderPlan → vnc::run / rdp::run
  → VideoSink::new(engine, frame_tx, plan, feedback, oversize)
  → DesktopStream (src/stream.rs) → vp9::Stream
```

Every size an engine asks a remote for is held under the stream's picture ceiling
(`video::fit_ceiling`: a long side of 3840 and a short side of 2400), and a
configured `size` past it is refused at config load. A remote the gateway cannot
size can still answer past it; what happens then is
[past the ceiling](#past-the-ceiling).

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
  desktop against a 30 Hz stream and a 60 Hz screen — and a full encode at every
  one of them had a session carrying under 800 kbit/s spend 88% of itself inside
  the encoder. `VIDEO_FRAME_INTERVAL` caps it at one access unit per
  33 ms; damage in between accumulates in the mirror and rides the next one, which
  is cheaper than coding the same movement four times over. A forced keyframe skips
  the cap, because a repaint, reattach or resize is a client with nothing
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
decoder: no browser's hardware VP9 path takes profile 1 — Intel's media engines
from Ice Lake on decode it, but Chromium's D3D11 and VA-API decoders advertise
profiles 0 and 2 only — so it always decodes in software, and a browser with no software VP9 at all, which is iOS and
iPadOS, refuses the configuration by name the way it would refuse any other.
`a_444_stream_keeps_the_colour_420_averages_away` in `screen-vp9` is the round
trip that pins the difference, through the archive's own decoder.

#### Past the ceiling

A desktop past the picture ceiling is one a video stream will not encode
(`video::check_picture`). The gateway sizes every desktop it asks for under it, so
only a remote it cannot size gets there: a Mac in Standard mode on All Displays —
5376×2287 over a 2x screen beside a 1x one, measured — or a plain or wlshare VNC
server whose desktop is simply that large.

Such a desktop has no picture. `Oversize` in `src/encode.rs` is the source's side
of it, fixed when the engine starts:

| Source | `Oversize` | Past the ceiling |
|---|---|---|
| VNC (plain, wlshare, Apple Standard) started without resize | `Hold` | the session stays up without a picture |
| VNC started with resize, Apple High Performance, RDP | `Refuse` | the session ends: "a video stream will not encode a W×H picture" |

Resize refuses because it is the gateway sizing the remote: every size it asks
for is under the ceiling, and a remote that answers past it has refused what it
was asked. High Performance's virtual display is held under the ceiling.

A `Hold` source holds one more view whatever its size: a Mac's All Displays over
more than two screens (`vnc_apple::MAX_COMBINED_SCREENS`). More than two is an edge
case on Standard, and composing them is too much for a browser to draw. The engine
tells the sink from each layout, ahead of the `Resize` it brings
(`VideoSink::hold_screens`).

A `Hold` source is decided at each `Resize`. Held, the sink drops every rectangle
and passed frame and builds no stream, and past the ceiling wlshare's VP9 comes off
the encoding list, so wlshare codes nothing that would not be sent. After every
`Resize` of a `Hold` source the gateway sends `oversize` with its `cause`
(`"size"`, `"screens"`, or `null` for a picture), a reattach included. While there
is a cause the page covers the desktop with a notice under the menu, takes no
input, and says which: the desktop's size, or more screens than one view shows.
It offers a button for each of the remote's displays but the one being sent, since
choosing one is how a Mac on All Displays gets back; with no list, it says that
nothing can be shown until the remote's desktop is smaller. A choice is a
`selectDisplay` like the menu's, and the notice comes down only at a `Resize`
without a cause. The remote repaints that desktop in full, as after any resize,
and its stream starts from an announcement and a keyframe.

#### wlshare's stream, passed through

wlshare has a VP9 encoding of its own, `WLSV` (`0x574c5356`), made for its desktop
clients and for this gateway: every update one rectangle over the whole desktop, a
`u32` length and one frame of a single stream. That stream is the one this gateway
would encode from the same pixels — coded by the same `screen-vp9` crate at the
same speed, screen tuning and dial, 8-bit at either chroma, BT.601 at studio swing
declared in its keyframes, so the two are one stream by construction — so on a
`wlshare` target the gateway lists it for every browser, tells wlshare what the
plan resolved to, and each frame goes to the browser as it came: no ZRLE on either
side, and no encode here. A plain target never lists it, so the same server sends
it ZRLE, which is encoded here, as is what any server that ignores the listing
sends.

- **Listed for every browser, with the plan beside it.** The encoding goes at
  the head of a `wlshare` target's `SetEncodings` for a desktop within the ceiling,
  and next to it, as pseudo-encodings the way Tight's quality levels ride the same
  list, what wlshare is to code: `WLQ` plus the target's `video_quality` as the
  ceiling wlshare's walk never goes above, `WLS0` for a plan whose chroma resolved
  to 4:2:0, and `WLSD` for one without `render_adaptive`, which holds the dial
  there rather than walking it on the browser's lag. So a browser whose decoder
  takes only profile 0 is passed a 4:2:0 stream rather than one encoded here, and
  the target's keys mean on a passed stream what they mean on one coded here.
  Pseudo-encodings rather than a client message because a server that is not
  wlshare ignores an encoding it does not know where a message it does not know
  ends the connection, and because they ride the list that names the encoding, so
  wlshare's first frame is already the plan's. The plan is fixed for an engine, and
  a reload that resolves otherwise rebuilds the engine
  ([choosing a chroma](#choosing-a-chroma)), so a session never changes carriage or
  chroma mid-stream. wlshare announces nothing: it sends the encoding in place of
  ZRLE.
- **Passed as it came** (`VideoSink::pass`). The frame's opening bits are read for
  its profile, which must be the plan's chroma's — a server that did not code what
  it was asked is refused by name rather than handed to a decoder configured for
  the other — and whether it is a keyframe; its size is held to the ceiling a
  stream encoded here is; the configuration announced ahead of it is the plan's
  chroma's string for its size, its level figured at 60 frames a second, wlshare's
  default `max_fps`, since wlshare rather than the gateway paces it. Its bytes take their share
  of `QUEUE_BUDGET` and go out in order with the messages around them. The mirror,
  the rounds, the interval, the quality walk and the settle do not run: wlshare
  paces, codes, walks and settles its stream itself.
- **A restart waits for a keyframe.** A reattach and a resize reset the
  render as always. On a reattach the full update the engine asks for
  is what makes wlshare send a keyframe; after a resize it asks for none, because
  wlshare starts the stream at the new size with one, and a full request would have
  it send a second that no shadow is there to skip. Until the keyframe arrives the
  frames still coded against the old picture are dropped, and it goes out behind a
  fresh `VideoFormat`.
- **The fence carries the browser's queue.** wlshare keeps one frame in flight
  and walks its quality by each fence's round trip, less the shortest it has seen.
  Echoed at once, as for any other server, it would time the hop to this gateway
  alone, which is never behind; while the picture is the passed stream each echo is
  instead held for the queue ahead of the frame on the browser's link
  (`VideoSink::fence_hold`): how long the oldest owed batch has waited beyond the
  link's ping round trip, when that batch was written before the frame was handed
  over, and nothing when the oldest owed is the frame's own — its transmission is
  the link working, not the link behind. Judged by the batch's time rather than
  by how many are owed, because the frame joins the paint window's count from the
  socket's task, after its fence has already been read. So a frame alone brings
  the next at once, and from then on each echo
  carries what is queued ahead of the frame it follows, which wlshare paces itself
  to. Two holds were measured and refused: one that ran to everything queued having
  given its budget back put every frame's own transmission into wlshare's floor,
  since in lock-step every frame is one size and takes the same time, and the walk
  never moved, at 8 frames a second a quarter of a second behind on 5 Mbit/s; one
  that ran to the frame's own delivery beyond the distance read that transmission
  as lag instead and walked to the floor for a third of the link. Echoes wait in one
  queue, in order, raced against the next server message rather than in front of
  it — except behind a fence asking for BlockAfter, where the reading waits for the
  echo — and none waits longer than 500 ms (`FENCE_HOLD_LIMIT`), the grace a window
  that is not drawing gets: such a window acknowledges nothing, the wait its oldest
  batch shows only grows, and wlshare, which sends nothing until the echo, would
  otherwise stop with it.
- **Past the ceiling it is off the list.** A frame is the whole desktop, which past
  the ceiling is not video: a desktop past it is not listed the encoding, and one
  that a resize takes there has it taken off the list, which wlshare answers with
  the whole desktop in ZRLE — [held](#past-the-ceiling) in a session started
  without resize, where a frame already on its way is dropped, and refused by the
  ceiling in one started with it. Back within the ceiling, the encoding is listed again and wlshare
  starts over at a keyframe.

#### Apple's media stream, passed through

`ard-high-performance` decodes the Mac's HEVC here and encodes every picture again
as VP9. A session started with the passthrough is sent the HEVC instead, as
the Mac sent it, and the gateway neither decodes nor encodes a picture
of the stream. The sound is passed in every session, started with the passthrough
or not: the gateway has no decoder for the Mac's AAC-ELD, and the browser has.
The passthrough is for a LAN. High Performance always enables its
own rate controller, between 20 and 60 Mbit/s
([Rate control](apple-vnc-889.md#rate-control)); this is automatic media-stream
behavior, not Standard mode's **Adaptive** quality choice. The gateway offers
Apple's bitrate entries and sends the rate reports Apple's viewer sends, every
50 ms, with the one-way delay of the picture packets reaching it, so the Mac
walks its encoder between those bounds by the link between it and the gateway,
decoded or passed. That is the only walk a passed stream has. Unlike wlshare's
stream, which walks its quality by the fence round trip to the browser, the
delay the Mac hears ends at the gateway: nothing reports the browser's queue
back to it ([roadmap](roadmap.md#apples-passed-hevc-at-4k)).

Three controls with similar names therefore remain separate:

| Control | Selection | What it reaches |
|---|---|---|
| Standard **Adaptive** / **Full** | Apple's viewer, in Standard mode only | Which RFB framebuffer encodings the viewer asks for |
| High Performance rate controller | Always enabled by the Mac's video profile; no UI choice | The Mac's HEVC encoder, within its fixed 20–60 Mbit/s range, by the gateway's reports |
| `render_adaptive` | A remotex target key, on by default | VP9 encoded in the gateway, including every picture after local HEVC decoding; it does not reach passed HEVC |

- **The browser says whether it can.** The page asks once, at load (`frontend/src/appleMedia.ts`),
  and states the answer as `apple_media=true|false` on every session socket, beside
  its chroma. The picker offers the passthrough to a page that said yes and greys
  it for one that said no, and the gateway holds both to it
  ([What a session is started with](#what-a-session-is-started-with)). For the picture it asks its `VideoDecoder` about the configuration
  macwork's stream announces, `hev1.4.10.L150.BE.8`. For the sound it decodes one of
  the Mac's own units, because no question answers it: Chrome and Safari both
  refuse `mp4a.40.39`, both decode AAC-ELD as `mp4a.40.2`, and they need the
  AudioSpecificConfig in different forms — Chrome as it is, Safari inside an MPEG-4
  ES_Descriptor — while both browsers' `isConfigSupported` say yes to the form they
  cannot decode ([The sound](apple-vnc-889.md#the-sound)). The page asks
  `isConfigSupported` about the bare form, then the ES_Descriptor, decodes the unit
  in each it says yes to, and keeps the first that produced sound. The picture
  answer alone controls the passthrough: a definite native "yes", or the software
  decoder below, offers it. If the native question says no, gives no verdict or
  throws, and no software decoder is served, the picture stays on VP9. The sound
  probe starts at load but is not part of that choice. If neither form produces
  sound, the picture still plays and the Audio row names the failure. Measured,
  Chrome and Safari's native decoders, desktop and mobile, decode the stream
  picture for picture and unit for unit; Firefox's native decoders take neither.
  Chrome on Windows decodes HEVC only in hardware, through D3D11. `FFmpegVideoDecoder`
  refuses HEVC, so there is no software fallback. It decodes this stream only where
  the GPU driver reports HEVC Range Extensions 8-bit 4:4:4 as a decoder profile. An
  Intel UHD Graphics 630 does not: Chrome 153 answered no, and each attempt ended
  when the SPS turned out to be 4:4:4, in `kDecoderUnsupportedCodec` (the profile
  missing from the device's list), so the "no" was right.
  A browser that decodes the sound but not the picture, as that one did, loses
  nothing by being sent both re-encoded.
- **BETA: the picture in software.** A gateway that has its release
  archive serves [hevc-wasm](https://github.com/andrewtheguy/hevc-wasm),
  libavcodec's HEVC decoder compiled to WebAssembly with SIMD128 and slice threads,
  at `/hevc/`; its threads share their memory through the cross-origin isolation
  every gateway serves the page with (`src/assets.rs`). No build holds the
  decoder, for the licence reason that keeps libavcodec out of every artifact:
  the operator downloads the
  release archive from the private `andrewtheguy/hevc-wasm-archives` into
  `share/remotex` in the gateway's release tree, beside `share/doc/remotex`,
  where the gateway looks for it and, finding it, serves it with nothing
  configured; `[hevc_wasm].archive` names a file kept elsewhere (`config::data_dir`):
  the archive is pinned to the binary's version, so it is kept with the binary
  rather than in the state directory, which outlives versions. The
  gateway reads it once at start-up and refuses to start unless it is the release `src/hevc_wasm.rs` pins by SHA-256, since the page's
  worker calls that build's exports. Where the browser's
  `VideoDecoder` refuses the picture, the page, if isolated, asks the gateway for
  the decoder and, served it, running shared-memory SIMD WebAssembly and holding
  a WebGL 2 canvas that takes the stream's primaries to present on, decodes
  the picture with it in a worker of its own. That browser answers yes when its
  `AudioDecoder` decodes the sound, so Chrome on a GPU without HEVC Range
  Extensions is passed the whole stream: its own decoder for the sound, the page's
  for the picture. The software decoder is shaped as a `VideoDecoder`, so the paint
  worker's stream runs it as it runs the browser's (`frontend/src/videoDecoder.ts`).
  What it outputs is not a `VideoFrame` but the picture's three planes where the
  decoder left them, in the module's shared memory: the paint worker uploads each
  as a texture from there and one draw converts them, on the WebGL canvas the
  page lays over the desktop's, the one a passed graphics pipeline is shown on
  (`frontend/src/hevcPicture.ts`, `glPicture.ts`). The canvas is given the
  stream's primaries, Display P3, so the browser takes the picture to the display
  as it takes a video frame. A session sent gateway-encoded VP9 paints the desktop's
  own canvas instead; a media-stream session never switches between the two. The
  decoder reuses a picture's memory from its next unit on, so it starts that unit
  only once the paint worker has released the picture. A `VideoFrame` over the
  planes was a copy of the picture, and drawing it on the desktop's canvas had
  the GPU convert and copy it again: on an Intel UHD 630, with a synthetic 4:4:4
  stream at 2880×1800 and twenty pictures a second, that was 29% of the GPU
  against 12%, and 6.2 ms of the two workers' time a picture against 1.8, beside
  the 23 ms across six threads that decoding one took.
  `?hevc_decoder=software` in the page's URL takes it even where the browser's own
  would do. Measured against macvm at 2880×1800 on an M2 Max, a picture took 9 ms
  across eight threads, 44 on one.
  `render_plan` sets `apple_media` for a target with the key and a browser that said
  yes; any other browser is sent VP9 exactly as without the key. The plan
  is fixed for an engine: a reload that no longer decodes the stream ends the
  session and lands on the picker ([Session lifecycle](#session-lifecycle)).
- **What passes** (`VideoSink::pass_hevc`). The receiver reassembles access units as
  it always does, and hands them to the read loop in order rather than to the
  decoder thread. Each goes out as Annex B, a keyframe where it holds an IRAP
  picture, behind a `VideoFormat` whose configuration string and size are read from
  the stream's own sequence parameter set (`parse_sps` in
  `src/vnc_apple_media.rs`, per ISO/IEC 14496-15 Annex E: `hev1` for parameter sets
  in band). Its size is held to the ceiling a stream encoded here is, and its bytes
  take their share of `QUEUE_BUDGET` like any access unit. Every picture the Mac
  sends goes out, up to the virtual display's 30 a second. The offer and the rate
  reports are a decoded session's: the quality is what that session receives.
- **A restart is an IDR from the Mac.** A reattach and the browser's
  own decoder failing each reset the render, and the gateway asks the Mac for an
  IDR with a PLI, which it answers within tens of milliseconds; until the IDR
  arrives the units still predicted from the old picture are dropped. A unit the
  read loop drops — one of another display, or one that comes while a resize
  holds the display — restarts the chain the same way, and a unit held back for a
  keyframe asks the Mac for one, since a still screen would never bring one unasked.
  A link that cannot carry the stream fills the receiver's queue of 15 units, half
  a second of the display's refresh; a full queue drops to the next keyframe, as
  the decoder's queue does.
- **The gaps show nothing.** ZRLE never carries the picture of a session with a
  media stream, passed or decoded: its rectangles are decoded only to keep the
  deflate stream in step and reach neither the shadow nor the encoder, so a passed
  session builds no encoder at all, and nothing is encoded here — a repaint is the
  Mac's IDR, not a VP9 keyframe. Before the stream is up, across every display
  change and across a stream the Mac restarts on its own, the browser stays behind
  its resize notice, which the gateway sends down behind the stream's first unit of
  the display (`VideoSink::uncover`), never ahead of it, and not while a resize in
  progress has covered the browser for the display it brings. The stream coming
  back starts at an IDR, announced again by its `VideoFormat`.
- **The dial does not reach it.** `video_quality`, `render_chroma` and the adaptive
  walk govern only VP9: the whole picture of a browser that says no.
  `render_adaptive` neither enables nor disables the Mac's separate, always-on
  High Performance controller. The Opus keys have no sound to reach and are
  refused on the target.
- **The sound always passes, on `/ws/audio`.** With or without the passthrough,
  the receiver hands each authenticated, decrypted AAC-ELD unit to the
  session's audio bridge as it came (`AudioBridge::unit`), and the audio socket,
  armed for the target's type, sends it on with no encoder behind it
  (`AudioListener::into_passed`): an `audioFormat` of `mp4a.40.39`, 48 kHz stereo,
  480 frames a packet and the AudioSpecificConfig as `head`, then the units, each a
  packet in the ordinary audio frame. The player configures its decoder in the form
  the page found at load. Claim-bound eviction, the queue and its dropping of the
  oldest unit are the Opus path's; there is no bitrate to walk and no silence to
  shed. A muted browser has no audio socket, so the units go nowhere and cost
  the gateway nothing: there is no decoder to run for them. A browser that
  decodes neither form of the configuration plays the session without sound
  and says so under Audio; Chrome and Safari decode it, and Firefox 153 on
  Linux accepted the configuration and produced no sound.
- **Without the decoder.** The picture's decoder is the host's FFmpeg,
  loaded when it is needed: when a session without the passthrough starts,
  and when `/api/targets` lists a High Performance target, which is how the
  picker knows. On a host without it the picture can only be passed, so the
  picker shows the passthrough as chosen, and for a browser that cannot take the
  HEVC Start is greyed with the reason: that is the one target the browser's
  answer leaves unstartable. A `connect` that asks for the picture decoded all the
  same is told so by the engine, naming the library, before it dials the Mac.

#### RDP's graphics pipeline, passed through

An `rdp` target composes the host's graphics pipeline here — every codec, the
surfaces and the caches — and encodes the picture that results as VP9. In a
session started with the passthrough it does neither: the pipeline's commands go to the
browser as the host sent them, and the page composes them. It is for a LAN. What
it saves is the gateway's work, which for a desktop in use was measured at about
three quarters VP9 encoding and a quarter decoding; passing the commands leaves
the gateway the connection, the channel's bulk compression and the frame
acknowledgements, a few percent of a core whatever the desktop is doing. What it
costs is the browser's work, and the quality walk: what the host draws with is
sent as it is, so `video_quality`, `render_chroma` and `render_adaptive` reach
nothing of it.

**Beta.** The compositor the page runs is the gateway's own, unit tested
as it is there, and the module built from it is tested as the page loads it.
What is passed is checked against a real host: `tests/rdp_client_probe.rs`
composes a passed pipeline beside the session that passed it, and
`tests/playwright/egfx-passthrough.spec.ts` reads the display socket of a
headless browser composing one. That host is one Windows 11 machine, used with
sound and the clipboard beside it; the camera and the microphone beside it
have not been tried, and no container stands in for a host that draws through
the pipeline.

- **The page says whether it can.** The page composes with WebAssembly on
  threads that share a memory, and presents on a WebGL 2 canvas. A page that is
  not cross-origin isolated has no shared memory, which is what a proxy that
  drops the gateway's two headers leaves, and one without WebGL 2 has nowhere to
  present. So the page asks itself once (`frontend/src/rdpGraphics.ts`) and states
  `rdp_graphics=true|false` on its session socket; the picker greys the choice
  for a page that said no, and `render_plan` sets `rdp_graphics` from the
  session's choice. Only an `rdp` target with its pipeline on offers it.
- **What passes** (`VideoSink::pass_graphics`). The RDP client still owns the
  channel: it answers the capability exchange, unwraps the bulk compression —
  whose history is the connection's — and writes every frame's acknowledgement
  (`Graphics::passing` in `crates/remotex-rdp-graphics/src/gfx.rs`). It decodes
  nothing. What it unwrapped goes to the engine as `Event::Graphics`: whole
  `RDPGFX` PDUs, headers and all, in order, each run ending at a frame's end or
  where the host's own packet did, and naming the frame it ends. The engine
  queues each as a `GRAPHICS` record, which takes its share of `QUEUE_BUDGET`
  like an access unit.
- **H.264, where the target's key allows it** (`egfx_h264 = true`,
  EXPERIMENTAL). Without the key the host is told its client takes none, passed
  or composed, and every pipeline is lossless. With it, a passed session whose
  page said it decodes H.264 advertises the capability sets that take it
  (`caps_advertise` in the graphics crate's `proto/gfx.rs`), and a Windows host
  then draws what moves like video with AVC420, in the same frames as the other
  codecs: one H.264 stream for each surface, each access unit behind a mask of
  the rectangles it shows. The gateway does nothing with them: they are bytes in
  the commands it passes. The page says whether it can on its session socket,
  `rdp_h264=true|false` (`frontend/src/rdpH264.ts`), and finds out by doing it
  when it loads: three access units of a stream of its own, shaped like a Windows
  host's, go through the decoders a session uses, and each has to give its
  picture before the next unit is handed over, laid out as the compositor reads
  it, and copy into shared memory. A decoder that holds a picture back for the
  units after it gives none in time, which is a no: a run cannot wait on units
  the host has not sent. A page that says no is not turned away; its host is told
  to send none.

  A run that carries H.264 is composed in three steps, and its batch is
  acknowledged after the third, so the host is still paced by what the page has
  drawn. The compositor scans the run for its access units (`avc::scan`). Each is
  decoded by its surface's own `VideoDecoder` (`frontend/src/egfxVideo.ts`), as
  the host sent it — Annex B, parameter sets inline, the codec string made from
  the stream's own profile and level — and its picture awaited: the decoder is
  asked for low latency, and the page's answer is that it then gives one picture
  for one unit. The part of the picture the
  unit's mask shows is copied into the compositor's memory, and the run is then
  composed, painting those rectangles from the samples
  (`crates/remotex-rdp-graphics/src/avc.rs`). The conversion to RGB is the
  compositor's, full-range BT.709 as MS-RDPEGFX has it, and never the browser's:
  measured, Chrome labels this stream BT.601 from its software decoder and
  limited-range BT.709 from a hardware one. Which decoder is the browser's
  choice, as it is for the desktop's own stream ([The codec](#the-codec)): the
  configuration states no `hardwareAcceleration`. A hardware decoder's picture
  is read back from the GPU to be composed: measured on an Intel GPU at
  1280×800, that copy took 6 to 9 ms where the decode itself took half a
  millisecond. A decoder
  that fails or gives no picture ends the pipeline, as a command that does not
  decode does: the host sends no keyframe on request.

  Checked against one Windows 11 host without a GPU, whose stream is Main
  profile, AVC420 by region. AVC444 and AVC444v2, which a host policy selects for
  the whole desktop, are implemented from the specification and tested against
  pictures built from its tables; no host has been seen to send both of their
  views.
- **The page paces the host.** A frame is acknowledged to the host when the
  page has composed it, not when the gateway read it. Nothing between the host
  and the page can drop a frame — every command is state the next one draws
  against — so a page that composes more slowly than the host draws has to be
  what the host hears from, and the host paces itself by its acknowledgements as
  it does for a client that decodes for itself. The run that ends a frame ends
  its batch (`wire.rs`), so the page draws and shows one frame per batch and its
  `paintAck` is that frame's; the acknowledgement rides the run and then its
  batch as the queue budget does (`Painted` in `protocol.rs`), said when the
  batch is acknowledged — or wherever the batch is dropped instead, so a socket
  that closes leaves the host waiting on nothing. The RDP client keeps the host
  a few frames ahead of the page rather than as far as the host would go
  ([The pipeline, passed on](rdp-client.md#the-pipeline-passed-on)). Measured
  with a stand-in page composing at 35 ms a frame against a host drawing at
  30 frames/s: acknowledged as the gateway read them, the page was sent frames
  four to a batch and showed 8 a second of the 28 it composed, a quarter of a
  second and then two thirds behind; paced this way it shows 28 a second, each
  35 ms after the last, a tenth of a second behind.
- **A pipeline is announced where it starts.** `graphicsStart` goes out when the
  host confirms the pipeline, ahead of its first command, and again if the host
  closes the channel and opens another. It says the pipeline starts from nothing,
  and the page makes a compositor with nothing in it. A `resize` still announces
  the desktop's size and density, ahead of the run whose ResetGraphics the page's
  compositor resizes itself by.
- **The page composes with the gateway's compositor.** The RDP client's graphics
  are a crate, `crates/remotex-rdp-graphics`, that the gateway is built with and
  that `frontend/wasm/egfx` binds for the page, built for
  `wasm32-unknown-unknown` by the frontend's build (`bun run build:wasm`), so
  there is one reading of the protocol and its codecs. It runs in the paint
  worker (`frontend/src/egfxCompositor.ts`): each record is composed in its turn,
  the output changes at each EndFrame as it does on the host's own clients, and
  the rectangles a frame painted are uploaded out of the module's memory into the
  pipeline's picture. A command that does not decode ends the pipeline there — the
  compositor no longer holds what the host believes its client does — and the
  page says so and asks for nothing: the way back is a session that starts.
- **The tiles are decoded on threads.** A desktop in motion is Progressive
  tiles, each of which holds its own coefficients and is nothing to the tile
  beside it, so a region's tile blocks are read in their order and then decoded
  side by side, on rayon's pool (`proto/progressive.rs` in the crate), in the
  gateway as in the page. A page's threads are workers: the paint worker starts
  up to four (`frontend/src/egfxPool.worker.ts`), each an instance of the module
  on the one memory, and makes the pool of them once every one has answered.
  Everything else a pipeline carries — ClearCodec, the caches, the copies between
  surfaces — is order itself and stays on the paint worker. A shared memory is
  given only to a page that is cross-origin isolated, so the gateway sends
  `Cross-Origin-Opener-Policy: same-origin` and `Cross-Origin-Embedder-Policy:
  require-corp` with every file of the page (`src/assets.rs`); behind a proxy
  that drops them the threads do not start, and the page says the compositor
  could not be loaded. A canvas takes no image data out of a shared memory, and
  a WebGL texture takes an upload from one, so the painted rectangles are
  uploaded out of the framebuffer where it is, into a texture on a WebGL canvas
  (`frontend/src/egfxPicture.ts`). That canvas is the pipeline's own and the one
  the page shows: a second canvas laid over the desktop's in the same box, which
  the paint worker draws on and the page shows from a pipeline's first drawn run
  until its picture is given back. Drawing the rectangles from it onto the
  desktop's canvas instead made the GPU copy the whole picture for every run, at
  twice the GPU's time on an integrated one. A browser without WebGL 2 is told
  the compositor could not be loaded, and sees such a host through the gateway's
  encoding. The module's
  standard library has to be built for threads, which takes a nightly Cargo:
  `frontend/wasm/egfx/rust-toolchain.toml` pins one by its date, for that
  directory alone.
- **Nothing is resumed.** The host draws against what its client already holds:
  surfaces, cache slots, ClearCodec's glyph and bar caches, Progressive's tiles.
  It answers even a repaint out of them — measured against Windows 11, a
  compositor that joined with nothing and asked for a repaint was left with most
  of the desktop wrong, after a graphics reset as after a Suppress Output — so a
  page that comes back cannot be repaired by the engine that is running. A
  reattach to a passed pipeline therefore ends the engine and starts another
  ([Session lifecycle](#session-lifecycle)), which is a logon to the session the
  host kept. A `refresh` from a page that still holds the pipeline's state is
  passed to the host as a repaint.
- **A host without the pipeline is encoded here.** The key asks for the pipeline
  to be passed, and a server that answers the offer with bitmap updates has none:
  its picture is decoded and encoded as VP9 at the target's dial, as without the
  key, and a `videoFormat` rather than a `graphicsStart` opens it.

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
needed: a reattach decides at attach
([session lifecycle](#session-lifecycle)), before the browser has sent
anything, whether the running engine is still one that browser can be given. It
is held on the attachment (`ClientSlot`) and read by both engine starts —
including the one reattachment that would otherwise resume a running engine,
which compares the plan the returning browser resolves to against the plan that
is running and rebuilds when they differ.

This is **selection, never refusal**. Only a definite `supported === false` gives up the colour; a "yes", an
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

**`"420"` — profile 0 for every browser.** A selection rather than a default: a
decoder that would have taken profile 1 is sent the subsampled stream anyway. Right for a fleet that must stay on
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
The walk is shared with wlshare — `QualityWalk` in the `screen-vp9` crate, the one walk both
run, driven from `src/encode.rs`. It watches one local signal — how long queueing an
access unit blocked — and walks the 1–100 dial down to its floor of 20 when the link is
behind, then the frame rate, and back up towards the configured quality when it is
not; never past it. It moves the
dial rather than a quantizer because a quantizer is the codec module's own scale
and never leaves it. What TCP hides is
*headroom*, and this never needs headroom, because exceeding the operator's setting
was never a goal. "Am I behind?" is the whole question, and the outbound queue
answers it. Quality moves through `Stream::set_quality`, which re-tunes the running
encoder rather than rebuilding it: a rebuild would force a keyframe per adjustment,
spending a few hundred KB exactly when bytes are scarce.

The walk has two knobs in a fixed order. Quality goes first, to the floor, and
by more the further behind the link is: ten points for a frame 60 ms behind, twenty
at 150 ms, thirty at 400 ms, where the frame rate is halved as well. On the floor
the frame interval doubles instead, 33 ms up to 267 — bytes on the wire are bytes
per frame times frames per second, and the floor bounds only the first: at 2 Mbit/s
the picture ran 0.7 s behind on the floor with nothing left to give up, and with the
frames going as well it ran 0.2 s behind at a fifth of the frames, every one of them
fresh. A verdict is two behind frames among the last four, not a run — the queueing
a link that is barely too small shows is intermittent, and a run that one clear
frame reset took eleven seconds per step — and a keyframe is no verdict, being the
whole picture and slow by its nature; the frames behind it queue behind its
crossing, which says how big it was and not what the link bears, so the verdicts
wait two seconds after one. A step is taken at most once a second, and
while the lag is still falling a fifth per second from the step before, the queue
that step left is draining and no further one is taken: judging the drain as a
link still too small was measured to take 69 to the floor in three seconds on a
link that carried 55. A second of clear frames, four at the least, takes the frames
back first, then the quality, in steps that double from three to twenty-four while
the link keeps taking them — a span rather than thirty frames, because a link
slowed to four frames a second never saw thirty inside a burst of motion and stayed
on the floor; a step the link refuses within four seconds is walked back after
300 ms rather than a full cooldown, to the quality it came from, and for fifteen
seconds the walk climbs no further than halfway back towards the one refused —
TCP's slow-start threshold, on the dial, found by bisection. Without it a walk
whose steps double would spend a session bouncing off the same quality; with plain
steps of three, 5 Mbit/s already cycled 47 → 59 → 49 every ten seconds, and a walk
that stepped ten down from a refusal and climbed to one under it cycled 43 → 49 →
39 → 48 → 38 every few seconds, the ceiling dropping a point a cycle.

`render_adaptive` gives the same walk a second signal on every VP9 picture
encoded here, unless its target turned the walk off. On a
decoded High Performance session that is the whole picture; on a passed one it
is only the VP9 picture between HEVC stretches. The signal is the client's own lag: the paint window already tracks how
long the oldest unacknowledged batch has been owed, and `LinkFeedback`
(`src/feedback.rs`) publishes that age minus a baseline — the link's distance,
so distance never reads as queueing; RustDesk and Guacamole both make the same
subtraction. The distance is the smallest ping round trip of the last minute
rather than the smallest batch's end-to-end time, because a batch's time carries
its own transmission, and a stream whose frames are all one size spends the
same time sending every one: taken from them, the baseline read that time as
distance and the walk stood still. A ping is a few bytes; every ping carries its
own number, every one sent is kept until its pong times it, and a parked wait's
ping does not displace the heartbeat's — on the slow link where the distance is
wanted, a pong takes the queue ahead of it to return, and one that answered a
displaced ping measured nothing. Sixty milliseconds of queueing lag
counts as a behind frame even when nothing local blocked, which is exactly the
case the paint window measured a VP9 attachment falling 222 ms behind at 7
batches in flight while every queue stayed shallow. Under `render_adaptive =
false` the walk is pressure-only.

The walk only runs when a round is taken, and a round is only taken when something
changed, so a desktop that stops moving right after the link coarsened it would keep
that picture until something changed again. The order task's settle tick is what
comes back for it. Once the stream has been idle `SETTLE_IDLE` since a round that
went out below the dial, and on a `render_adaptive` target the lag has cleared, it
puts the encoder at the dial and marks the unchanged mirror dirty. The engine
encodes that as one inter frame. libvpx codes the residual of unchanged blocks at
the finer quantizer, so the frame sharpens the whole desktop without a keyframe; the
vp9 test `a_finer_quantizer_sharpens_an_unchanged_picture_without_a_keyframe` guards
that. The walk keeps its place through it: the round after the settle's goes back
to the quality the link bears, and the settle's frame is no verdict. The screen
stopping says nothing about the link, and a walk that started every burst of motion
from the dial was measured, on a desktop moving four seconds in eight over 5 Mbit/s,
to put the picture 0.45 s behind on average and the walk back at 50 or 60 every
burst; keeping its place, the same run held 42 to 59 and 0.12 s. The clear frames
before the quiet do not span it either: the walk's run of clear frames starts over
at the settle, so a burst earns its step back up from its own frames rather than
taking one on its first. A stream that went
out at the dial owes nothing and sends nothing when it goes quiet.

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
configuration states no `hardwareAcceleration`. A `prefer-software` hint would
buy one platform's decoder at most — WebKit honours it
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

The gateway **encodes VP9 only** (`src/vp9.rs`), and there is no codec key: one
encoder is one to maintain. The encoder is the `screen-vp9` crate, its own
repository pinned by release tag here and in wlshare, and the one place libvpx is
spoken to for this gateway and for wlshare's own stream: the quantizer pinned to the dial, screen-content
tuning, no lag, no dropped frames, no keyframe unasked, the colour declared in the
bitstream, the retune without a keyframe, and the RGB→YUV conversion in front of it
all live there, proved once by tests that read every frame back with the archive's
own decoder. `src/vp9.rs` is the stream over the mirror: the picture limits, the
keyframe owed until a frame carries it, and the codec string. The one stream the
gateway sends in another codec is one it does not encode: a High Performance Mac's
own HEVC, passed through for a browser that takes it
([Apple's media stream, passed through](#apples-media-stream-passed-through)). VP9 is BSD-3-Clause
with a patent grant and present in every browser build, the ones that carry no
proprietary codecs included. On synthetic screen content at 1080p and quality 60 it
encodes a frame in **4.7 ms** at **18 KB** — measure with
`cargo test --release measure_the_encoder -- --ignored --nocapture`; a debug build
reports nonsense, because the RGB→YUV conversion it also times is Rust — the `yuv`
crate's, on the AVX2 or NEON path the machine has — and runs an order of magnitude
slower unoptimised.

Nothing downstream of `TargetConfig::render_plan` names a codec: `encode.rs`,
`stream.rs` and the wire carry access units, a keyframe bit and a configuration
string, and `vp9.rs` is reachable only from `stream.rs`.

**The browser is not asked for a codec, and never asked to justify itself.**
There is no codec probe ahead of a session: it would put a round trip and a
decoder query in front of every one, `isConfigSupported` is not reliable enough
on the same browser twice to build a refusal on, and a refusal phrased as "this
browser accepted none" turns any fault near the path into an accusation against
the browser.

What the browser is asked is four questions. One selects rather than refuses: how
much colour this decoder takes, for `render_chroma = "auto"` to resolve against
([choosing a chroma](#choosing-a-chroma)). A wrong answer to it costs a picture,
not a desktop. One decides what a host is told and nothing else: whether this
browser decodes the H.264 a passed RDP pipeline may carry. The other two say
which passthrough the browser can take: a High
Performance Mac's HEVC, and an RDP host's graphics pipeline. They
decide what the picker offers before a session starts, where a "no" starts the
target encoded here, and they keep a session started with a passthrough from
being sent to a browser that cannot show it
([What a session is started with](#what-a-session-is-started-with)).

The refusal itself stays where it always was: one honest failure at the client's own
decoder. The gateway announces the configuration in `ServerMsg::VideoFormat` before
the stream's first unit, `VideoDecoder.configure` accepts it or refuses it, and a
refusal is reported by name — "this browser cannot decode the video this target
sends" — with the configuration string beside it.

## Session lifecycle

Authentication and desktop ownership are separate:

1. `POST /api/auth/login` creates the login cookie.
2. `POST /api/session` claims the single slot. A conflicting claim returns
   `409` unless the request reclaims its token or forces takeover. Its answer
   and `GET /api/targets`' state the gateway's version in `X-Remotex-Version`,
   and a page whose own differs, a tab left open across an upgrade, opens no
   session and lists no target: it says both versions and offers a reload.
3. `/ws?session=<token>&chroma=420|444&apple_media=true|false&rdp_graphics=true|false&rdp_h264=true|false`
   attaches to the slot and reports the target picker or the current connected
   target. `chroma`, `apple_media`, `rdp_graphics`
   and `rdp_h264` are required: the most colour this browser's video decoder
   takes, whether it decodes a High Performance Mac's HEVC,
   whether it composes an RDP host's graphics pipeline, and whether it decodes
   the H.264 such a pipeline may carry; see
   [Choosing a chroma](#choosing-a-chroma),
   [Apple's media stream, passed through](#apples-media-stream-passed-through) and
   [RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through).
   The media sockets carry the token alone.
   Beside it the page opens `/ws/display?display=1`, the first display's socket:
   the picture, its size and pointer, and the paint acknowledgments that pace it
   ([`ServerMsg::is_display`] routes an engine's output between the two). It
   carries no token: the claim records the login cookie it was made with, and a
   display socket is let in by that login alone. The two are opened together, and
   the gateway holds an engine's picture for up to five seconds for the display's
   socket of a page whose session socket is attached, since a passed stream's
   opening is the part no repaint replaces. A display socket that attaches after
   its picture lost anything is repainted. The page closes its session socket
   when its display socket drops, and the reattach brings both back. `/ws/display?display=2`
   is the second display shown in a tab of its own, *All Displays* over two
   virtual displays (alpha); see
   [Display geometry](#display-geometry).
4. `connect` starts the selected engine with the choices made at the picker.
   `disconnect` stops it and returns to the picker.
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
running is one that browser has just said it cannot decode. An engine passing an
RDP host's graphics pipeline is never resumed: the page that comes back holds
none of what the host draws against
([RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through)),
so it is started over with the choices it was started with. And an owner that
comes back unable to take the session's passthrough has nothing to resume or
restart: the session ends, and it lands on the picker with the reason. Opening
size, density, display selection, and connection state do not carry into any
other session.

A claim by a different browser ends the session: the previous WebSocket, its
engine and the selected target with its choices. That is a forced takeover, or a
second browser arriving during the first one's reattach grace, when nothing is
attached to refuse it and so no takeover is asked. Its attach lands on the
picker, where it starts the target with its own choices, for its own screen and
decoders: a session started on one device never carries its size, density,
colour or passthrough over to another. The remote keeps its own session, so
picking the target again logs back on to the same desktop. Only the owner
reclaiming its token resumes the running engine, with a full-repaint request
instead of a reconnect.

### What a session is started with

Three things about a session are chosen by whoever starts it, before it starts:
how the desktop is sized, whether the remote's sound is taken and as what, and
whether the remote's own stream is passed through. Picking a target at the picker opens it,
its options show under it with a Start button, and Start sends `connect` with the
choices (`Choices` in `src/config.rs`). None of them is a config key.

Which options a target shows is its type's to say, from `TargetConfig::offers`,
and `GET /api/targets` carries it:

| Target | Window drives the size | Sound | Passthrough |
|---|---|---|---|
| `rdp` | yes | shown | shown: the graphics pipeline, beta; hidden beside `virtual_displays = 2` |
| `vnc` | no | hidden | hidden |
| `vnc`, `wlshare` | yes | shown | hidden: its VP9 is the subtype's picture |
| `vnc`, `ard` | no | hidden | hidden |
| `vnc`, `ard` with `virtual_display` | yes | hidden | hidden |
| `vnc`, `ard-high-performance` | yes | hidden: always carried | shown: the Mac's media stream |

- **The size is shown before Start.** A desktop is sized one of three ways
  (`Sizing` in `src/config.rs`): kept at the target's size, which is its
  `size = "1920x1080"` or the default 1440×900 where it sets none; kept at the
  default on a target that configures another; or driven by the client's window,
  which is what *started with resize* means throughout and what `connected`
  reports as `resize`. Which of them the picker shows depends on the target and
  on the client (`sizeOptions` in `frontend/src/targetChoices.ts`):

  | Target | Desktop browser or tablet | Phone |
  |---|---|---|
  | the window cannot drive it | the target's size | the target's size |
  | the window can, and a `size` is set | the target's size, or the window | the target's size, or the default |
  | the window can, and no `size` is set | the window | the default |

  One size is stated; two are a choice, the configured one first, so a size the
  operator set is the size until somebody chooses another. A phone is offered no
  window, because a portrait screen that small is no desktop's shape. A tablet's
  window is its screen: it asks once, in landscape, and rotating does not ask
  again. A plain `vnc` target is not offered the window either: whether its
  server takes a size is known only once it is dialled, which is too late for a
  picker, so it is asked once for the size it keeps and the picker says a server
  that takes none keeps its own. A Mac sharing its physical displays is shown at
  their size and takes no `size` key. A Mac's virtual display takes one of at
  most 1920×1080: it opens at the client's density under a 3840×2160 ceiling of
  pixels, so a larger size would be shrunk for a Retina client after the picker
  had stated it, and is refused at parse instead. A `connect` always names its
  size: one without `choices.size` is refused, since the gateway picks no size
  on a browser's behalf.
- **Not offered is not shown.** An option the target type does not have has no
  row. High Performance's sound is such a one: the Mac refuses the picture
  without it, so there is nothing to choose, and the session's Mute is what a
  person has. An `rdp` target with `egfx = false` has no pipeline, so neither
  the window nor the pipeline's row; one with `virtual_displays = 2` has the window
  and not the pipeline's row, since a passed pipeline is composed whole and the
  browser shows one display of two. A `connect` that names a choice the target does
  not offer is refused with an `error`, and the slot stays as it was.
- **Offered but unavailable is greyed, with the reason.** A passthrough is
  greyed wherever the browser cannot take it: one that does not decode the Mac's
  HEVC, a gateway with no HEVC decoder archive to serve a
  browser that needs it, a page that cannot compose the pipeline. `/api/targets`
  also says, as `passthroughOnly`, where a gateway's host lacks FFmpeg
  and so cannot decode a Mac's picture at all: there the picture can only
  be passed, the row shows it chosen, and where the browser cannot take it
  either Start is greyed and says why, before the Mac is dialled.
  A `connect`
  that asks for a passthrough the browser said it cannot take is refused like an
  unoffered one.
- **Sound is off, Opus or lossless.** `choices.audio` says `off`, `opus` or
  `flac` (`Sound` in `src/config.rs`). On a target that offers it the picker
  shows a Sound tick, and under a ticked one the two formats side by side, Opus
  on the left, which ticking takes, and lossless on the right. Opus is sent at the rate the target's audio
  keys hold; lossless is FLAC, with no rate ([Lossless sound](#lossless-sound)).
  A `connect` that names none takes none.
- **The choice reaches the remote.** A session started without sound asks for
  none: RDP names no sound channel and a `wlshare` target lists no audio
  extension, so the host keeps playing where it did. The session's audio button
  opens and closes the browser's subscription and nothing else, which is why it
  reads Mute and Unmute. Start's click is the gesture a browser needs for an
  audio context, so a session started with sound comes up playing; in Safari and
  on iOS a reload has no gesture, and comes back muted. A mute is the tab's, for
  its session, and survives a reload.
- **The browser remembers.** What was chosen under a target is kept in the
  browser's local storage per target, and is how the target opens next time.
  Until then a configured size is the size and no remote's sound plays. A greyed
  row is not remembered, and a remembered size the target does not offer this
  client is not sent.
- **A session is held to its choices, and to the browser that made them.** The
  slot keeps them beside the selected target (`Selected` in `src/session.rs`).
  `connected` reports them: `resize`, `audio` and `passthrough`. A reattach
  resumes the session, or starts it over, with them. No other browser is given
  them: a takeover ends the session and lands on the picker, where that browser
  is offered what it can take and remembers what it chose. The owner coming back
  unable to take the session's passthrough lands there too, behind an `error`
  that says which stream: no engine is rebuilt with other choices.

Login tokens are held in memory with sliding expiry and delivered through an
`HttpOnly`, `SameSite=Strict` cookie. The cookie is marked `Secure` when
`x-forwarded-proto` reports HTTPS. Restarting the gateway invalidates all
logins.

## Client protocol

`src/protocol.rs` and `frontend/src/protocol.ts` define the client contract.
`GET /api/config` publishes the deployment branding before authentication —
the display name, and whether `GET /api/logo` (equally public) serves an icon
the page then sets as its favicon. There
is no client/server version negotiation: the gateway serves the matching SPA from
the same build, and no second client is supported.

Control and input messages are tagged JSON. Server messages cover picker and
connected state, desktop size, display selection, cursor shape, clipboard,
audio format, and errors. The `connected` message says what the session was
started with — `resize`, `audio` and `passthrough` — and includes the
`camera` and `microphone` capability flags, so clients
expose only supported controls.

It also carries two things a client cannot work out and nothing else reveals:
`render`, the resolved render dial, and `subtype`, the target's `wlshare`, `ard`
or `ard-high-performance` where it has one. The
last is there because `protocol` is not an answer on VNC — a plain server, a
wlshare one and a Mac on either subtype all say `vnc`, and they differ in whether
resize is offered, where the picture and sound come from, whether there is a
display list or a density, and whether the path beneath is the reverse-engineered
one. Both appear on the client's session card, which
`frontend/src/connectionLabel.ts` words, beside the video decoder's configuration
(`mediaLabel.ts`).

`GET /api/targets` carries `subtype` too, beside the options each target offers
(`resize`, `audio`, `passthrough` and `passthroughOnly`) and the sizes it keeps
(`size`, the configured one, and `defaultSize`), so the picker names it one step
earlier — the difference between two Macs in that list is a choice being made,
not something to discover after connecting. The row uses the config spelling
alone (`VNC · ard · 192.0.2.10:5900`); the card, which describes one target and
has the room, spells it out.

### Image batches

Screen updates use little-endian binary frames:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | u32 sequence | records

VIDEO    op 0x03: u8 flags | u16 w | u16 h | u32 len | payload[len]
GRAPHICS op 0x04: u32 len | commands[len]
```

One frame carries every record ready at once, so a backlog does not cost one
WebSocket event per record — save that a `GRAPHICS` record ending a frame ends
its batch, since a batch is shown once. Receivers reject unknown operations and truncated
records, and reject a nonzero frame flags byte. A `VIDEO` record's own flags byte
is `0x01` for a keyframe and nothing else — any other bit is rejected the same
way. A session's records are `VIDEO` unless its target passes an RDP host's
[graphics pipeline](#rdps-graphics-pipeline-passed-through): a `GRAPHICS` record
is a run of that pipeline's commands, whole, which means something only after
every run before it from the `graphicsStart` that began the pipeline. One of no
length is rejected.

`sequence` starts at one and increases for the lifetime of one session-socket
attachment. After the paint worker has finished the batch's ordered
parse/decode/draw pass, the client sends `paintAck` with that sequence plus its
worker queue and draw times. `ws.rs` consumes this transport feedback rather than
forwarding it to the remote engine — save a passed graphics pipeline, where a
batch's acknowledgment is its frame's to the host
([RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through)) —
and logs those measurements with the attachment totals. A socket generation travels through the worker so a late
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
trip, less the link's ping round trip (or the fastest acknowledgment the socket has
shown, until a pong has measured one), within 100 ms — gets a
batch's share back at the write, and its flight is the window's to bound: holding
it to receipt instead capped an unthrottled attachment 100 ms away at 22 Mbit/s
that otherwise carried 65. A client that is behind keeps the share with the batch
until its acknowledgment or a pong says it arrived, because there a written batch
has only moved from a queue into the send buffer. Measured with an incompressible
12 Mbit/s of damage, that holds the picture 0.6 s behind at 4 Mbit/s and 4 s at
1 Mbit/s (23 s without), and the link's return to full speed is immediate.

`VIDEO` carries one access unit of the desktop: VP9, or a passed High Performance
Mac's HEVC as Annex B. Its keyframe bit comes from the encoder, or the stream's own
NAL unit types, rather than from the client parsing the payload — VP9 carries no
parameter sets to read one out of. `(w, h)` is the desktop's true size, and the decoded picture may exceed
it by a pixel on either axis (see [the video stream](#the-video-stream)); a size that
differs from the last unit's is a stream that started over, preceded by a fresh
`videoFormat`.

### Audio frames

Remote audio is a session's choice — sound, chosen at the picker on an `rdp` or
a `wlshare` target as Opus or lossless — and always on for `ard-high-performance`, whose sound comes
with its picture; `ard` and a plain `vnc` target carry none. It has a socket of
its own. **Opening `/ws/audio?session=<token>` is the subscription** — there is no
message that turns sound on, and closing the socket is the only way to stop. The
page opens it when a session that carries sound starts, and its Mute and Unmute
close and open it.

The separation is the point. The display socket's bounded queue is four frames
deep; an audio pump waiting behind a video backlog on it would stop draining the
bridge, and what the bridge then drops is wave buffers. A lost wave buffer is a
hole. The dedicated socket has no picture-induced loss path.

The socket is bound to the *claim*, not to an attachment, so it survives a session
socket reconnecting and a target switch: the gateway re-announces the format when it
arms the next engine. It ends when the claim does — a takeover, or a log out — and is
superseded by a newer audio socket on the same claim. Its refusals mirror the session
socket's: 401 before the upgrade without a login, close code 4000 for a token that is
not the current claim, 4001 on eviction.

The gateway answers with `audioFormat` — the codec string, the decoder
configuration, the samples in one packet, and as `passthrough` whether the
packets are the remote's own or coded here, which the session card's Audio row
states — followed by binary frames:

```text
u8 kind = 0x03 | u8 flags | u16 packet count
repeated: u16 packet length | packet bytes
```

The one flag is bit 0, a gap, on a frame of no packets. The gateway sends it
when a listener fell behind a passed stream and units the remote had coded were
dropped: the next packet does not follow the last one sent, so the player resets
its decoder, an Opus or AAC-ELD one carrying state from packet to packet. Sound
coded here needs none, since what a slow listener loses there is PCM, before the
encoder.

There is no codec byte in the binary frame; the codec is named once, out of
band, in `audioFormat`. It is Opus encoded here: `codec` is `opus`, `sampleRate`
48 000, `packetFrames` 960 (20 ms), and `head` the `OpusHead`. The rate is
`audio_bitrate`, default 96 kbit/s, walking down to the floor sound-opus fixes,
32 kbit/s. The one exception is a High Performance Mac's AAC-ELD, passed in
every session on such a target: `codec` `mp4a.40.39`, `packetFrames` 480 (10 ms), `head`
the Mac's AudioSpecificConfig, and each packet one of the Mac's units, with no
encoder and none of the keys below reaching it
([Apple's media stream, passed through](#apples-media-stream-passed-through)).

Opus is encoded in `Application::Audio` mode under constrained VBR, set
explicitly in `src/opus_stream.rs`: `audio_bitrate` is the average the encoder
holds to, silence costs a few bytes a packet and a loud passage a little more
than the number, and the running rate stays close enough to it that the
configured kbit/s is what the link sees. There is no FEC and no DTX — the socket
is TCP, nothing is lost, and the adaptive walk already sheds silence.

The rate is a per-target key and the audio dial's `video_quality`: a ceiling the
link may fall below, on by default like `render_adaptive`, with
`audio_adaptive = false` holding the rate whatever the link does. The walk is
sound-opus's (`sound_opus::walk::BitrateWalk`), the one crate that codes a
desktop's sound as Opus for this gateway and for wlshare, so nothing adaptive
about sound is this gateway's own: `AudioWalk` (`src/audio.rs`) owns one beside
the pump's send and publishes its word through `AudioSignals`. The audio
socket's queue is deliberately two deep, and how long a send waited is the
walk's whole signal; two slow sends among four are a behind verdict. The walk
gives a third up a step, more the longer the send blocked, down to the floor the
crate fixes at 32 kbit/s — where Opus stereo still codes the whole band, so
there is no floor key — and takes rate back after three seconds of clear sends
in steps that double from a sixteenth of the ceiling, held under a rate the link
refused, never past the ceiling. The change reaches the live encoder through
`OPUS_SET_BITRATE`; packets stay 20 ms and independently decodable, so nothing
is re-announced. While the link is *behind*, wave buffers that are pure silence
are shed before the encoder instead of queued — silence is the one content whose
loss cannot be heard, the client just receives no packets for a while (what a
quiet remote already produces), and the backlog drains by exactly that much.
Both keys are refused on a target none of whose sessions can carry sound, which
is `ard` and a plain `vnc` target; a ceiling at or under the floor keeps a walk
with nothing to give up rather than being refused.

The RDP engine carries sound over MS-RDPEA (`rdp_client/proto/rdpsnd.rs`).
A session started with sound names the `rdpsnd` and `rdpdr` static channels and
leaves `INFO_NOAUDIOPLAYBACK` out of the Client Info PDU; the host opens
`AUDIO_PLAYBACK_DVC`, negotiates 44.1 kHz 16-bit stereo PCM the moment something
plays, and every Wave2 buffer reaches `AudioBridge` from the client's own thread,
never through the event queue. One started without it sets the flag and names
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
of an incoming buffer instead of turning temporary jitter into lasting latency.
A `wlshare` target's Opus has no encoder here to walk: the pump's walk is the
same, fed by the same sends, and each rate it arrives at goes back through the
bridge (`AudioBridge::ask_rate`) to the VNC engine, which names it to wlshare
as a set-bitrate; wlshare's encoder moves at its next packet. The walk starts
from the ceiling with every listener, and so is wlshare told. Silence is not
shed from a passed stream — its packets are a few bytes each already.

The client decodes Opus and the Mac's AAC-ELD with WebCodecs and nothing of its
own, so a codec a browser will not take surfaces as a decoder error naming it
rather than as silence. FLAC is the one stream it decodes itself
([Lossless sound](#lossless-sound)).

#### Lossless sound

A session started with its sound lossless is sent it as FLAC,
on the same socket and in the same frames: `audioFormat` says `codec` `flac`, an
empty `head`, the source's own `sampleRate` and a `packetFrames` of twenty
milliseconds of it, and each packet is one FLAC frame. It is a choice at the picker, on an
`rdp` or a `wlshare` target, and not a config key; the target's Opus keys do
nothing in such a session, since there is no rate to set or walk.

| Target | What the gateway does | `audioFormat` |
|---|---|---|
| `wlshare` | passes wlshare's frames as they came | 48 kHz, 960 frames |
| `rdp` | codes the host's PCM as FLAC | 44.1 kHz, 882 frames |
| `ard-high-performance` | not supported: the Mac's AAC-ELD is passed | |
| any other | not supported: no sound | |

- **wlshare's frames are passed**, as its Opus packets are in a session started
  with Opus: the engine lists the audio encoding without the Opus one beside it,
  which is how wlshare is asked for FLAC. It reads each frame message and
  queues the frame undecoded (`AudioBridge::unit`), between a begin and an end as
  ever, and the audio socket hands the units on (`vnc_audio::PASSED_FLAC`,
  `AudioListener::into_passed`). No decoder is made. Nothing
  here checks a frame but its length, which must fit the socket's 16-bit packet
  length: the page's decoder is what refuses a bad one.
- **An RDP host's PCM is coded here.** `AudioListener::into_flac` takes the wave
  buffers as they come, at the 44.1 kHz the host sends with no resampler in the
  way, and makes a frame of every 882, each a FLAC stream of its own as
  wlshare's are (`sound-flac`'s encoder). What does not fill a block waits for
  the next buffer, and is dropped with a buffer the queue dropped, so no frame
  joins samples that were never neighbours. libFLAC is linked into the gateway
  statically, so the host needs none installed.
- **The page decodes in a module of its own.** `frontend/wasm/flac` is a Rust
  FLAC frame decoder built to WebAssembly, loaded by the first FLAC stream
  (`frontend/src/flacDecoder.ts`). WebCodecs is not asked: what travels is bare
  frames with no stream header, each numbered zero. The decoder holds every
  frame to what `audioFormat` announced — rate, channels, block, 16 bits — and
  checks its CRCs; a frame it refuses is dropped with a console warning and
  costs its twenty milliseconds. The samples become an `AudioBuffer` at the
  stream's rate and go through the schedule every other stream uses, so the
  lead clamp and the splice fades are unchanged. A page whose module does not
  load plays the session without sound and says so under Audio.
- **There is no walk.** Music is roughly a megabit a second and silence a few
  bytes a frame. A link that cannot carry it loses whole buffers at the bridge,
  oldest first, as an Opus stream at its floor would; use Opus there.

An **`ard-high-performance`** engine always carries sound, from the Mac's media
stream: AAC-ELD at 48 kHz stereo over SRTP, authenticated and decrypted per
packet, and handed to the bridge unit by unit as it came (`src/aac_eld.rs` says
what it is), for the browser to decode. The picker offers
no choice of it: the Mac refuses the picture without the sound, and mutes its own
output while it streams. See
[The media stream](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).

An **`ard`** engine carries no sound: Standard has no measured audio path, so
the target has no audio bridge and offers no sound. Standard mode never
touches the Mac's sound output either, which keeps playing where the Mac sends it
— its speakers, or an AirPlay receiver outside remotex.

A **`wlshare`** session started with sound has no channel to negotiate either. It
lists wlshare's audio pseudo-encoding, and wlshare announces that it speaks it
with an empty rectangle, at which point the gateway names the format it wants —
48 kHz, 16-bit stereo, little-endian, which is Opus's own rate — and the rate
Opus is to be coded at, and turns the stream on; the sound then
arrives on the RFB connection itself as wlshare coded it, Opus packets or the
FLAC frames of a lossless session, and each goes to the queue and on to the
browser as it came, with no decoder or encoder here. A server
that announces nothing, because it is not wlshare or has its own switch off,
gives a desktop and no sound, which is the whole of the failure mode. The extension is wlshare's
own, its control messages borrowed from `rfbproto`'s QEMU Audio extension — see
[`wlshare-audio.md`](wlshare-audio.md).

A quiet remote and one that never negotiates audio are indistinguishable to the
client, so detailed negotiation status remains in the gateway log.

### Camera frames

**Experimental.** The socket rules and message encodings are unit tested like
everything else here — the claim and engine binding, the eviction, the
byte-for-byte control frames — and so is the MS-RDPECAM wire the RDP client
speaks, against the specification's own examples. The wlshare path is exercised
by its container test; RDP has no container host.
`tests/rdp_client_probe.rs` checks against a real host that the camera is
negotiated and its device opened, and its `a_real_host_streams_the_camera` opens
the host's Camera app and carries H.264 frames from a file to it. Like every real-host
probe it is ignored by default, and it checks that the host started the stream
and took samples, not the pixels the host displays. The displayed picture is
verified by hand against a Windows host, and a change here needs a hand check.

The browser's camera goes the other way, on a third socket, to an RDP target or a
`wlshare` target that opted in with `camera = true` (refused on a plain `vnc`
target and on both Apple subtypes at parse time: neither has anywhere to put a
camera). **Opening
`/ws/camera?session=<token>` is the enable** — explicit, per session, and never a
remembered preference or something a session is started with, unlike sound. Its refusals add one
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

The gateway never transcodes. The browser encodes Annex B Constrained Baseline H.264
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

On a `wlshare` target the camera goes to wlshare instead, over its private
camera extension, and wlshare makes it a PipeWire camera on the wlroots desktop.
`src/vnc_camera.rs` asks for the extension beside the density and outputs
requests, holds the browser's plug until wlshare says it takes a camera, and
relays wlshare's start, stop and keyframe as the same bridge signals, so the
socket and the browser behave as they do against RDP. A server that never answers
is sent nothing and the enabled camera is never started, as on a Windows Server
without the RDSH role. See
[The browser's camera over VNC with wlshare](wlshare-camera.md).

#### Microphone frames

**Experimental**, under the same testing boundary as the camera. The browser's
microphone goes the other way on the fourth socket, to an RDP target or a
`wlshare` target that opted in with `microphone = true`; a plain `vnc` target and
both Apple subtypes refuse the key. Opening `/ws/mic?session=<token>` is the enable. It has the camera socket's
authentication, 4000/4001/4002 close codes, claim-and-engine binding, and
per-session lifetime, so an engine end, takeover, or ordinary socket close stops
the feed and the next session starts with the microphone off.

The remote controls when samples are useful. It sends `micOpen` when an
application starts recording and `micClose` when it stops; the browser captures
and encodes only between them. Inbound binary frames contain one packet:

```text
u8 kind = 0x05 | one Opus packet
```

The browser sends mono Opus in voice mode at 16 kbit/s in 60 ms packets. The
gateway decodes it at 48 kHz, groups it in 20 ms blocks, and resamples it to the
16-bit mono or stereo PCM format the remote requested. Packets arriving while
the remote is not recording are dropped, and a close discards queued and partial
audio so a later recording starts cleanly.

On RDP, `microphone = true` lets the host open the MS-RDPEAI `AUDIO_INPUT`
dynamic channel. The host speaks first: version, recording formats, then an Open
when an application records. The client offers exactly one of the host's 16-bit
PCM formats, preferring mono and 16 kHz, and cuts the decoded PCM into the
`FramesPerPacket` groups the host named. A full sixteen-buffer queue drops its
oldest audio. `tests/rdp_client_probe.rs` drives this negotiation against a real
host under `REMOTEX_UAT_MICROPHONE=1` and feeds a tone while the host's Recording
panel holds the device open. See
[The RDP client](rdp-client.md#microphone-ms-rdpeai).

On a `wlshare` target, `src/vnc_mic.rs` asks for wlshare's microphone
extension, plugs the microphone when the socket attaches and unplugs it when the
socket closes, relays wlshare's start and stop as the bridge's open and close,
and sends the bridge's decoded PCM between them. See
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

A session started with resize has the window drive the remote's size,
continuously and on every engine alike: an engine that has it applies every
`viewport` it is sent — in points, the window's CSS pixels, rendered at the
engine's own density — an engine without it drops them all, and the client sends
them exactly when `connected` said `resize` — on every window change, with no
toggle and no manual button in the session. Standard `ard` does not offer
resize, because it shares physical displays, and a plain `vnc` target does not,
because its server's answer is not known before it is dialled.

The opening size is one rule for every engine that can ask for one
(`TargetConfig::opening_size`). A session that keeps its size opens at it: the
target's `size`, else `DEFAULT_SIZE`, 1440×900 points, or that default on a
target whose configured size a phone declined. A session started with resize
opens at the full resolution of the client's own screen — carried in the
`connect` message so it exists before the engine's handshake — and the configured
size is not used. The default is sized for what an unasked-for session costs at
2x: a HiDPI client renders it at twice the points, so 1920×1080 would mean
capturing, scaling and encoding 4K every frame, where 1440×900 comes to
2880×1800. An operator who wants the larger desk configures a `size`. A mobile
`HostDisplay::fit` client has no screen suitable for laying out a desktop: a
tablet that starts a session with resize opens it at the default and then asks
once for its screen in landscape, and a phone is never offered resize.

A kept size is stated the way each engine states one. RDP connects at it, a Mac's
virtual display is created at it, and a plain or `wlshare` target asks for it
with a single `SetDesktopSize` as soon as the server declares support — seeded
into the same held-request slot a viewport report uses, so it goes out on the
first `ExtendedDesktopSize` rect and no earlier. A server that never declares
support, or refuses the request, keeps its own size. Standard `ard` is the only
engine with no size to state, and `size` is refused on it at config load.

What a kept size says about density follows each vendor's own client on a Mac.
An RDP session at a kept size states no scale factor to a pointer client's host,
so the host keeps its own scaling, which is how Microsoft's client behaves with
"Optimize for Retina displays" unchecked; a session started with resize renders
at the client's density.
A High Performance Mac opens its virtual display at the client screen's density
whatever names the points, which is how Apple's Screen Sharing opens one.
A `HostDisplay::fit` client gets that on RDP too: it fits the desktop to its
width, usually on a phone's or tablet's 2x or 3x screen, so an RDP session at a
kept size states the client's density at connect, as a High Performance Mac opens
at it. Without resize there is no Display Control channel to restate it later.

What is engine-specific is the mechanism:

| Engine | Started with resize |
|---|---|
| Plain VNC | not offered: it is asked once for the size it keeps |
| wlshare | applies a requested size, and the client's reported display density |
| Apple Standard VNC | not offered: it shares physical displays |
| Apple High Performance VNC | applies dynamic-resolution sizes within its fixed 3840×2160 backing ceiling |
| RDP | applies a requested size, and the client's reported display density |

Every desktop is also held under the gateway's 3840-pixel long
side by 2400-pixel short side ceiling at the negotiated density. RDP opening and
layout sizes and the sizes a plain or `wlshare` target asks for all pass through
`video::fit_ceiling`; High Performance separately keeps its native 3840×2160
backing ceiling. This changes what the remote is asked to render, not how the
browser scales it: a 5K window receives at most a 3840×2400 desktop at 100%, with
the remainder bare. A configured size already
over the ceiling at 1x is rejected during config parsing; a physical or
non-resizable remote may still answer past it because the gateway cannot ask it
for a smaller desktop: a VNC one [holds the session](#past-the-ceiling) without a
picture, and any other reaches the encoder's refusal.

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
and its density. Mid-session only the density is acted on, and only in a
session started with resize: RDP quantizes it to 1x or 2x at a midpoint (and opens at it, from the
screen `connect` names), a High Performance virtual
display re-renders the same points at it, and a `wlshare` target declares it to
the server over wlshare's density extension, which sets its output's scale; the
resulting density travels back as the `scale` on `resize`, and clients present
the framebuffer at `pixels / scale`. On `wlshare` the label follows the server's
reports and is never applied by the gateway itself; see
[Pixel density over VNC with wlshare](wlshare-density.md). Apple Standard on
the Mac's physical displays reconfigures none of them and asks the Mac to scale
what it sends instead. A plain
`vnc` target lists no extension to RFB, so its wire carries no density whatever
server it reaches: it is presented at 1x and takes no density from the client;
see [HiDPI over standard RFB](standard-rfb-hidpi.md).

A client shows the display picker exactly when the target sends it a
`ServerMsg::Displays`, and hides it otherwise. The VNC engine sends one on both
Apple subtypes and on a `wlshare` target: it parses an `AppleDisplayLayout`, or
wlshare's `OutputList`, into a `displays` message and acts on a `selectDisplay`
by asking that remote for that screen. RDP exposes a single framebuffer spanning
every remote screen. On an `rdp` target left at one display it has nothing to
enumerate, and a plain `vnc` target reads its server the same way, so neither
sends the message and the picker stays hidden there. An `rdp` target with
`virtual_displays = 2` (alpha) asks the host for two monitors of the session's
size in a row, in the connect-time monitor data and in every monitor layout, and
the host spans one framebuffer over both. The engine shows one column of it: it
announces the column's size as the desktop, cuts damage to it, offsets pointer
positions into it, and lists the columns as `Display 1` and `Display 2`. A
`selectDisplay` is answered in the gateway, out of the framebuffer it already
holds, with the list, the size and a repaint of the chosen column, and the host
is asked for nothing. The list follows what the host laid out, read off the
desktop it opened and off each graphics reset's monitor count, so a host that
opens one desktop lists nothing. The pipeline's passthrough is not offered beside
it, since the browser composes a passed pipeline whole.

With two columns the list ends with *All Displays* (alpha), under the id Apple's
own entry uses: the first column on the canvas and the second in a browser tab of
its own, which the list names as `tab: 2` and the display panel links to as
`/display/2`. That page is the same SPA with no menu and no picker; it claims
nothing, since a claim would take the session from the tab holding it, and opens
`/ws/display?display=2` by the login cookie alone — opened with `noopener`, so it
shares none of the first tab's storage, the token included. The session lets that
socket in only while the engine's last list names the tab, and hands the engine a
feed for it (`ClientMsg::DisplayShown`): a sink, an encoder and a shadow of its
own over the second column, the same pointer shape, and a repaint when it
attaches or asks. Its input arrives wrapped as `ClientMsg::OnDisplay` and is
offset into its column. On a session that follows the window the tab's window is
the second monitor's size: its viewport, sent on its own socket, makes the next
layout a row of two sizes, top-aligned, and the second keeps that size for as long
as *All Displays* is chosen — a tab reloading does not reset it — and is the
first's again once it is not. Choosing a display, a host that lays out one
monitor, or the engine ending takes the tab away, and the session closes its
socket; a closed tab only ends the feed. The root page is always the first
display, so there is no `/display/1`.

The display is one tab's. The first socket for it after the engine lists its tab is
given a token, sent first on it as `displayToken`; the tab keeps it in its
`sessionStorage` and presents it on every display socket it opens after
(`/ws/display?display=2&token=…`), so a reload gets back in. Any other socket for
the display — a second tab, or one holding a token from before — is closed with
4003. The token lasts while the tab is listed: choosing another display and then
*All Displays* again frees the display for whichever tab opens it next, as does a
host laying out one monitor, the engine ending, or the claim or login changing. It
is not the claim's token, and it opens nothing but that display. A page at
`/display/2` whose display is not shown — *All Displays* not chosen, a target
without it, no session in this browser, or another tab holding it — says the
display is not available, why, and offers Retry; it does not keep reconnecting.

An `ard-high-performance` target with `virtual_displays = 2` (alpha) is listed and
shown the same way, from another source. Its `SetDisplayConfiguration` names two
virtual displays, Apple's viewer's "2 Virtual Displays", and the Mac creates the
second to the right of the first and sends each as a video leg of its own in the
one media stream. So the engine cuts no column out of anything: the display on
the canvas is one leg's pictures and the display in the tab the other's, decoded
here or passed, each through a sink of its own, and the leg of a display nobody
is shown is authenticated and dropped at the receiver, neither decoded nor
passed. A `selectDisplay` is answered in the gateway and asks the Mac for nothing
but the keyframe a display coming into view starts at; the canvas stays behind
its resize notice until that picture, as it does across any display change. The
framebuffer the Mac spans over both displays is only the space its rectangles,
stepped over, and pointer positions are addressed in: a position made on the
second display is offset by where the layout places it. On a session that follows
the window the tab's window sizes the second display through the same
configuration, which always names both. The sound is the session's, on the first
tab. See [Two virtual displays](apple-vnc-889.md#two-virtual-displays).

Where the list is sent, the checkmark moves only when the remote comes back naming
the screen it is now sending — never on the click. On a Mac the engine prepends an
*All Displays* entry of its own so a client that picks a screen can get back; see
[`apple-vnc-889.md`](apple-vnc-889.md). wlshare captures one output at a time and
has no combined view to offer, so its list is the compositor's outputs and nothing
else; see [Switching outputs over VNC with wlshare](wlshare-outputs.md).

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine.

### Clipboard

Every session bridges the clipboard, on all engines; no target key turns it on
or off. The backend
holds the latest remote value and its observed change time:

- plain and wlshare VNC forward and buffer `ServerCutText` or Extended Clipboard
  data;
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

Base RFB acknowledges nothing and announces no clipboard, so a plain VNC server
without one looks like one where nothing has been copied. The reply to a fetch
carries `unconfirmed` while such a server has announced no Extended Clipboard
and sent no cut text, and the Clipboard panel says so. Every other engine's
clipboard is negotiated and never reports it.

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
On a display socket a ping's payload is the sequence of the last screen batch
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
coalesces overlapping damage, compares it with a shadow of pixels already sent,
trims it to the changed bounding rectangle, and encodes off the event loop. Input
is mapped from DOM codes to scancodes and queued to the client's thread as
fast-path events.

The client carries the desktop, the pointer, keyboard, mouse, resize, the
clipboard, sound, and the browser's camera and microphone, and no touch: touch is
announced only by a host that opens MS-RDPEI, which this client never asks for. What it would take is in
[`roadmap.md`](roadmap.md).

Static virtual channels are asked for by what the session needs: `drdynvc` for a
session started with resize, the default `egfx = true`, `camera = true`, or
`microphone = true`; `cliprdr` always; and `rdpsnd` with `rdpdr`
for a session started with sound.

`virtual_displays = 2` (alpha) asks the host for two monitors in a row, each the
session's size, in the connect-time monitor data and in every layout a resizing
session sends; the host spans one framebuffer over both and the engine shows one
column of it, switched from the display picker without asking the host, or —
on *All Displays* — the second column in a browser tab of its own
([Display geometry](#display-geometry)). The key is shared by every target type
that can create virtual displays: `rdp`, and `ard-high-performance`, whose two are
a media stream each rather than columns of one framebuffer. It is refused
elsewhere, and held to two ([Roadmap](roadmap.md#more-than-two-virtual-displays-on-a-target)).
Under the Graphics Pipeline (MS-RDPEGFX) the server draws through surfaces on a
dynamic channel, marks every frame's end — which is the engine's flush signal, with
the 16 ms coalescer demoted to a 100 ms safety net — and answers a monitor layout
with a graphics reset. Its decoders cover what a current Windows host draws with —
ClearCodec and the NSCodec inside it, RemoteFX Progressive, planar, uncompressed —
and its compositor carries the copies and caches between them, so the desktop is
lit and sharp; a rectangle that will not decode is left for the host to draw again,
not made the end of the session. H.264 is refused in the capability advertise: a
host would hand the parts of the desktop that move like video to it, and a lossy
video codec would lose detail before the gateway ever encodes the picture. In a
session started with the passthrough the same channel is answered and acknowledged here and
composed in the browser, which also decodes the H.264 a target with the
experimental `egfx_h264` key lets the host draw with
([RDP's graphics pipeline, passed through](#rdps-graphics-pipeline-passed-through)).
`egfx = false` is the bitmap path: the
server draws with bitmap updates, damage is flushed on the 16 ms guess because those
carry no frame boundary, and the desktop keeps its opening size — resize is
not offered beside it, because an RDP resize is the pipeline's graphics reset.
On either path the pointer travels as its own shape rather than in the framebuffer.

Read [The RDP client, written here](rdp-client.md) for the whole of it: the
connection sequence, the channels and the chunk flags a Windows host silently
requires, the codec and damage path, resize and density, the clipboard, and sound.
It also covers camera and microphone redirection.

[MS-RDPBCGR]: https://learn.microsoft.com/en-us/openspecs/windows_protocols/ms-rdpbcgr/5073f4ed-1e93-45e1-b039-6e30c385867c

### VNC

The built-in client speaks two dialects, chosen by the target's `subtype`, that
share everything below the handshake — one read loop, one input path, one video
path. Both force the same 32-bit true-color BGRX pixel format rather than
negotiating one, and use the same shadow and encoder path as RDP. `src/vnc_encodings.rs`
decodes whichever encoding a server picks into the packed RGB888 the shadow and the
mirror take, so nothing above it knows which was chosen.

**RFB 3.8** is the dialect of a plain target and of a `wlshare` one. The dialect
and the baseline below are what the two share. A `wlshare` target is a subtype
the way `ard-high-performance` is: its picture is the server's own stream passed
through, and its density, display list, sound, camera and microphone come from
extensions only that server speaks, where a plain target has the baseline alone.
The dialect supports None, classic VNC
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

A plain target and a `wlshare` one advertise the standard lossless encodings in
preference order —
CopyRect, ZRLE, zlib, Hextile, RRE, Raw — and a server encodes with the first it
supports, so a modern one settles on ZRLE and uses CopyRect for scrolls and window
moves. Tight, TightPNG, JPEG and H.264 are deliberately absent: vendor or lossy,
and this gateway re-encodes every frame for the browser anyway. CopyRect names a
source region rather than carrying pixels, so its source is read back out of the
shadow — the VNC link still carries no pixels for a scroll — and a source the
shadow does not know costs one non-incremental repaint rather than an invented
picture.

A `wlshare` target also lists wlshare's own encodings after those: its VP9
stream at their head for a desktop within the ceiling, the density and
output-list requests at their tail, and between them the **audio**, camera and
microphone pseudo-encodings where the target asked for them. A plain target lists
none of these. `src/vnc_audio.rs` is the audio wire (`WLSF`): the server announces
support with an empty rectangle of the encoding, the client answers with the
QEMU Audio extension's set-format, wlshare's own set-bitrate and QEMU's enable,
and the server sends QEMU's begin, a run of frames in message `0xE4` — Opus
packets where the client listed `WLOP` beside the encoding, FLAC frames where
it did not — and QEMU's end. A server that never
announces leaves the session silent rather than failing it. See
[`wlshare-audio.md`](wlshare-audio.md).

Plain and `wlshare` targets also advertise **ContinuousUpdates** and **Fence**,
which go
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

The client advertises DesktopSize and ExtendedDesktopSize on every RFB 3.8
target, so a server can always say its size changed; the session's sizing decides
what asks it to change with `SetDesktopSize`: the size it keeps, once, or the
window on a `wlshare` target started with resize. Their clipboard uses Extended Clipboard when the server
advertises it and falls back to Latin-1 `ServerCutText` otherwise. Both Apple
subtypes negotiate Apple's display metadata and native pasteboard instead, and ask
for ZRLE in their first `SetEncodings`.

**Apple Standard mode remains fixed-size.** It offers no resize, shares the
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
Taken at factor 1.0, that combined framebuffer is often past the video ceiling —
a 2x screen beside a 1x one measured 5376×2287 — and then has no picture: the page
offers the Mac's screens instead, since one screen is a smaller desktop. All
Displays over more than two screens has none either, whatever its size or
densities ([past the ceiling](#past-the-ceiling)).

**RFB 003.889** is Apple's own protocol revision, and both Apple subtypes speak
it, as Apple's viewer answers every Mac before choosing a mode after ServerInit.
None of it is documented by Apple, so every claim in this section is measurement
or a reading of Apple's binaries rather than specification, holding for the Macs
in [apple-vnc-889.md](apple-vnc-889.md) rather than for the protocol. The
dynamic-resolution path behind a resizing session remains reverse engineered. It
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

**High Performance mode is a virtual-display mode.** (So, unofficially, is `ard`
with `virtual_display = true`: everything in this paragraph about the display and
its resizing, with Standard's ZRLE picture, no media stream offered and no sound.
Apple's viewer never offers that combination; it was tested on macOS 26 only.)
The gateway sends
`SetDisplayConfiguration` (`0x1d`) during setup, with one mode built from the
size the session keeps, or from the connecting client's screen resolution in a
session started with resize, at that screen's density either way. The mode sits under the
native descriptor's fixed 3840×2160 backing ceiling. Once connected, the remote
Mac's physical displays are disabled and all of its windows are placed on that
virtual display. Apple's
official macOS Screen Sharing client can choose up to two virtual displays, and so
can a target: Remotex requests one, or two with `virtual_displays = 2` (alpha). The full descriptor enables dynamic resolution on
every fresh session. In a session started with resize, the window continuously drives the
virtual display through Apple's dynamic-resolution feature: later viewport reports
resend the same full descriptor with the requested mode, and the Mac's answering
display layout sets the actual framebuffer geometry. There is no resize mode or
one-shot button in the session. The Mac supplies that virtual display the way
it does to Apple's viewer: as HEVC over its media stream, offered once the display has
settled and decoded in the gateway by the host's FFmpeg libavcodec (`src/vnc_apple_media.rs`),
or passed to the browser in a session started with the passthrough. The gateway's
decoder runs four slice threads because one is too slow for 60 pictures a second:
on one core of an i5-8500T a 1600×1000 picture took 14–23 ms, on four 7–14 ms.
ZRLE rectangles never carry the
picture: the browser stays behind its resize notice until the stream delivers, at
connect and across every display change, as Apple's viewer keeps its curtain up
until its stream is hooked up, and a session that passes the stream builds no video
encoder at all. A stream the
Mac refuses, that brings no picture or no sound, or that stops ends the session,
as it ends Apple's viewer's. While it runs, polling holds to one pixel, which still brings
cursor shapes and layouts. Apple's resolution-preset control remains
unimplemented.

The wire constraints remain load-bearing: `SetEncodings` must list both
`DisplayInfo` (`0x44d`) and the layout (`0x451`), in any order, or the Mac reports
no layout; and a layout's `u16` length counts the bytes after itself, with a `u16`
display count ahead of the records. The byte layouts and protocol corrections are
in [`apple-vnc-889.md`](apple-vnc-889.md) — read that before touching this path.

Deliberately absent: Apple's own still-image codecs. The media stream's sound
leg comes with its picture on `ard-high-performance`; `ard` carries no sound.
The transport's measurements are in
[Apple RFB 003.889](apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).
The native Apple pasteboard works on every
subtype; 003.889 enables monitoring before the rekey and carries the fetch and
data messages inside its encrypted record layer. See [`roadmap.md`](roadmap.md).

## Clients

### Browser SPA

The React SPA has login, target picker, and remote desktop states. It decodes
the desktop's video stream onto a canvas — or composes an RDP host's passed
graphics pipeline onto it, with the gateway's compositor built to WebAssembly —
applies incoming frames serially, and overlays mouse,
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
alone, because audio is a target's choice and every target may fall back to video: a
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
included, without the bare tap a seen release would send. The soft keyboard's
modifiers are outside it: the page holds those, and no event's flags know them.

The canvas is presented at the remote's point size, derived from framebuffer
pixels and remote scale. Desktop clients scroll when necessary. Touch clients
use fit-to-width presentation, pinch zoom, pan, a virtual cursor, and
multi-finger gestures without changing framebuffer coordinates.

A `wheel` message's pixels are points of the remote desktop: a pointer client
sends the browser's deltas, which are that at 100%, and the touch layer sends
two-finger travel through the scale the desktop is shown at, so content follows
the fingers. Each engine spends the distance in what its wire has: RDP as
proportional wheel rotation, an Apple target as as many wheel pulses as it is
worth there, a `wlshare` target as the distance itself, in wlshare's scroll
message (`0xE5`), which the compositor hands its applications as a touchpad's
continuous axis. Plain VNC has only the wheel buttons, a notch an event.

That is a distance, and a mouse wheel has none: it has notches, which an
application spends as a step of its own. A browser reports them in lines or
pages, or — Chromium and WebKit — in pixels, as the ~100 a notch is worth
locally, which a `wlshare` target sent as a distance scrolled several steps too
far. So the pointer client tells the two apart: a pixel delta whose legacy
`wheelDelta` is whole multiples of 120, and not the three times its pixels a
macOS trackpad reports, is sent in the unit `notch`. A `wlshare` target is sent
`notch`, `line` (three to a notch) and `page` deltas as wheel-button notches,
which wlshare injects as a wheel's discrete axis, and only `pixel` deltas as the
distance. RDP counts a `notch` as one notch of rotation and an Apple target as
the 100 pixels it stood for.

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

While the floating menu has something over the desktop — its drawer, the one
modal card that opens from it and leaves the drawer standing, or the clipboard
panel — the desktop is
**view-only**. No input listener is attached at all (`useRemoteDesktop.ts`), which
is what gives the page back the chords the surface would otherwise take: ⌘C and
Ctrl+C among them, so the text on a card can be copied. The automatic clipboard
sync stands down with them, in both directions — a remote copy arriving behind the
card is not mirrored onto the browser's clipboard, and the browser's is not pushed
to the remote — because for as long as the menu is up that clipboard holds what was
copied off this page rather than anything the remote sent. The surface keeps
painting, under a dimmed layer that says which of the two it is doing. Every way
back out hands the keyboard to the surface as it goes, because the key listeners
live there: the ✕, a click on the dimmed layer, the chord that hides the menu, a
drawer button that closed the drawer behind it. The soft keyboard is the one control in this menu that is itself
keyboard input, so a key pressed there takes the drawer down as it sends — the
label never stands over a remote being typed on.

The soft keyboard (`SoftKeyboardPanel.tsx`) is pages of keys (`softKeyboard.ts`),
each a DOM code the backend already maps, and one press engine
(`softKeyPress.ts`) that turns fingers into keys for all of them. The engine
listens on the key area, not on the keys: a key is a cell that meets its
neighbours edge to edge, so there is no gap a finger can land in, and where a
finger is comes from a measured table of the cells (`softKeyGeometry.ts`) rather
than from the DOM under it. A letter commits when the finger lifts, after any
slide to a neighbour, and the key under the finger is shown over it as it goes;
Backspace, Delete, the arrows, Space, Tab and the page keys commit on touch and
repeat while held; the scrollable shortcut row commits on a tap that stayed put
and lets a slide scroll. A modifier tapped once wraps the next key and is spent,
tapped again it is off, and under a resting finger it chords the other fingers'
keys; nothing locks. Every such modifier goes down ahead of the key in the order
it was taken and up after it, a repeat tick included, and never reaches the wire
on its own. The shortcut rows' Shift, Ctrl, Alt and Super, drawn dashed, are
not that kind: each is a key of the row like the Esc beside it, so a tap that
stayed put sends it alone, down then up — Super alone is the Start key — a
slide scrolls the row and sends nothing, and it arms nothing. The Sym page's
row has the same four ahead of F1 to F12. The PC grid has no such row, so its
Sticky key, in the Caps Lock slot, switches its modifiers between the two: on,
as it starts, they stick; off, each is sent alone and is drawn dashed like the
row's. It is drawn as the switch it is, a green pill with a lamp lit while they
stick, so it is not taken for a key that types. The key area
refuses every browser gesture (`touch-action: none`), so a cancelled touch means
the system took the finger and commits nothing; the shortcut row alone allows
the horizontal pan it scrolls by. A phone — a touch screen whose short side is a
phone's (`tabletGuestSize.ts`) — gets the keyboard docked along the bottom edge in
either orientation, padded above the home indicator, and the canvas insets
above it; every other client, tablets and narrow windows included, gets the
floating PC grid. The keyboard is dismissed whenever the desktop is not what
is on screen — reconnecting, an error, a claim conflict, the picker, the login —
and stays closed when the desktop returns.

Each tab stores its claim token in `sessionStorage`, allowing reconnects to
reclaim the same slot. Busy and evicted states require explicit takeover or
reclaim actions.

### Local multi-instance control plane

`remotex tui --port <port>` is the native local control plane. It discovers one
instance per immediate subdirectory, creates and edits its serverless
`remotex.toml`, and starts, stops or
restarts each gateway from its own list. `remotex.localhost:<port>` is a landing
page; `<instance>.remotex.localhost:<port>` is that instance's browser origin.

```text
browser: <instance>.remotex.localhost:<port>
                    │ Host-routed HTTP and WebSockets
                    ▼
             TUI master process
                    │ <instance>/gateway.sock, or \\.\pipe\remotex-<random> on Windows
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

The loopback boundary is the machine, not the OS account. The master gives any
local caller the selected worker's session cookie before proxying it, so any
local user can list and drive every running instance. The owner-only instance
directories and worker endpoints protect the configs and the private transport;
they do not authenticate the public loopback surface. Do not run the TUI on a
machine shared with users who must not reach its desktops.

Each hidden worker binds its private endpoint, prints one JSON readiness line —
`{"endpoint","token"}` — after binding, reads only that instance's
`remotex.toml`, and stops when its parent's stdin closes (`src/embedded.rs`,
`Audience::Embedded`). Before any of that it claims the instance: an exclusive
lock on `<instance>/gateway.lock` through std's `File::try_lock`, the same on
every platform and released by the operating system however the worker ends. A
second worker for the same instance — another TUI on the same directory, or one
started by hand — is refused before it binds, and the TUI asks the same lock
before it spawns, so it can say why. A killed worker leaves no lock to clear,
and on Unix its leftover socket is simply replaced. On Unix the endpoint is `<instance>/gateway.sock` at mode
`0600` in a `0700` directory. On Windows it is a named pipe
(`src/embedded/transport.rs`): a random `\\.\pipe\remotex-<random>` per
launch, because pipe names are one machine-wide namespace any user may create
in, created as its first instance so the name printed is one the worker holds,
with a DACL naming only the user and remote clients refused. Several instances
of it wait at once, a listen backlog for the connections a page load opens
together. The instance directories get the equivalent of `0700`: a protected
DACL naming the user and `SYSTEM`, inherited by the configs in them. A worker
runs with a hidden console of its own, so the TUI's Ctrl+C and window close
never reach it directly; closing the window still stops every worker, by ending
the TUI and so their stdin. The master seeds the token as a host-only HttpOnly
`remotex_session` cookie before proxying the browser to the child. Raw connection
proxying preserves both ordinary HTTP and WebSocket upgrades without another
gateway protocol implementation.

The entire substrate is behind the default `embedded-gateway` Cargo feature:
the module, token authentication, config audience, CLI commands, and their
`check-config --embedded` validation mode compile out together. Native packages
retain it. Container artifacts are built separately with
`--no-default-features`; the build script and Dockerfile reject a
binary that exposes any embedded CLI surface.

There is no separate native client: every instance is the same SPA loaded
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
gateway over one HTTP origin and up to four WebSockets, all of which need a host
and a port, so whatever terminates the proxy is what a browser talks to. An embedded
gateway is that arrangement in one process tree: the worker listens on its
private endpoint — `<instance>/gateway.sock`, or a named pipe on Windows — and
never on TCP, and the thing terminating the proxy is the TUI master, which the
browser reaches over loopback TCP and which forwards each connection to that
endpoint.

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
flow and protocol engines. Containerized dummy servers cover plain VNC and
wlshare; RDP end-to-end probes borrow a real Windows host.
[Development](development.md) says how each is run.

Stable headless browser tests under
[`tests/playwright`](../tests/playwright/README.md) cover deterministic DOM,
control-plane, HTTP, and WebSocket behavior. Rendering races and timing
measurements remain in raw-protocol and container tests.
