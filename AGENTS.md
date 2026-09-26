# Repository instructions

Keep this file to rules that change how work is performed. Design explanations,
protocol details, measurements, and operational guides belong in the linked
documentation.

## Workflow

- Strict no backward-compatibility or legacy paths.
- Do not run `cargo fmt`.
- No squash merges
- After Rust changes, run `cargo clippy --all-targets -- -D warnings` and
  `cargo test`, and both again with `--features apple-hp-media` when the change
  reaches the Apple engine or the media stream.
- After frontend JS/TS changes, run the Biome checks in `frontend/`.
- Before browser QA of a frontend change, rebuild the gateway and say so: the
  frontend bundle is compiled into the binary, so a running `remotex serve`
  never sees a newer bundle. For source-based iteration, use
  `REMOTEX_DEV_BACKEND=<port> bun run dev`.
- After Playwright changes, run `bun run typecheck` in `tests/playwright/`.
- Put temporary files and test configuration under `tmp/`. Always run local
  Python through `uv` (GitHub Actions excluded).
- Errors are `anyhow` by default, and `thiserror` wherever a caller branches on
  the kind instead of reporting it. Carry the cause: add `.context()` on the way
  up, keep an error typed rather than flattening it to a `String` and rebuilding
  one, and drop a source only when it says nothing the message does not.
- Keep end-to-end tests under `tests/`; dummy RDP/VNC servers may use Docker or
  Podman.

## Product boundaries

- There is one client: the browser SPA, including when installed as a Chrome or
  Edge app. Do not add a native wrapper or a second implementation of a page
  feature.
- There is one frontend build, compiled from Cargo's `OUT_DIR` into the gateway
  binary (`src/assets.rs`) and served from its origin root. A standalone frontend
  build and the platform-independent release artifact use `frontend/dist`; a
  Cargo build either produces the same bundle in its private output or stages
  that artifact there. Do not add a web root, a `static_dir`, or any run-time path
  the SPA is read from. Every URL the page uses goes through
  `frontend/src/gateway.ts`.
- The page requires a secure context plus `VideoDecoder` and `AudioDecoder` and
  refuses startup in `frontend/src/preflight.ts` when they are absent. Do not add
  fallback browser paths.
- One gateway has one active session. Multi-session support is out of scope.
  Every `connect` starts from scratch after the previous engine exits; only the
  owning browser's reattach to the same target resumes an engine. Preserve the
  takeover and fresh-session behavior documented in
  [Session lifecycle](docs/architecture.md#session-lifecycle).

## Input and display

- Touch has two mutually exclusive layers. `touchGestures.ts` treats fingers as
  a trackpad and interprets gestures in the page. `touchPassthrough.ts` forwards
  uninterpreted MS-RDPEI contacts when an RDP host reports `touchReady`. Never add
  gesture recognition to touch passthrough. See
  [Browser SPA](docs/architecture.md#browser-spa).
- The display picker is the remote's list and the remote's checkmark. Engines fill
  it — Apple's display layout, wlshare's output list — and the browser holds no
  display state of its own, so never move the checkmark on the click or add a
  client-side selection. An engine with nothing to choose between sends no list and
  the panel stays hidden. See
  [Switching outputs over VNC with wlshare](docs/wlshare-outputs.md).
- Pointer clients present the remote desktop at 100%; oversized desktops scroll.
  Do not add fit-to-window, zoom-to-fit, or viewport-derived scaling. Mobile,
  gated by `CAN_PINCH_ZOOM`, is the sole fit-to-width/pinch-zoom exception.
- Neither the gateway nor the browser rescales what a remote sends. Frames pass
  through at the remote's pixels and are presented at `w / scale` by the density
  the remote confirmed. When the size or density is wrong for the browser, ask the
  remote to render the right one — RDP's negotiated density, a High Performance
  virtual-display mode, Apple Standard's `SetServerScaling`, wlshare's density —
  and output its answer as is. The sole exception is Apple Standard's All
  Displays over screens of different densities, which no one factor can render:
  the gateway sends a `mosaic` and the browser composes each screen at its points,
  as Apple's viewer does (`frontend/src/mosaic.ts`). Do not extend it to another
  engine, view or density.
- `ClientMsg::Viewport` is in CSS points. `ServerMsg::Resize.scale` is remote
  pixel density, not a fit factor. `resize = true` means the window continuously
  drives the remote size; do not add a client resize toggle or remembered resize
  preference. Density is the wire's word alone: RDP negotiates it, Apple
  reports it, wlshare reports it over a private extension every generic VNC
  session asks for and only it answers, and generic VNC that does not answer is
  presented at 1x. Do not add a
  client-side density control, and never label a framebuffer with a density the
  server has not confirmed. Read
  [Display geometry](docs/architecture.md#display-geometry),
  [HiDPI over generic VNC](docs/generic-vnc-hidpi.md) and
  [Pixel density over VNC with wlshare](docs/wlshare-density.md) before
  changing geometry.
- Read [Apple RFB 003.889, as measured](docs/apple-vnc-889.md) before changing
  either Apple Screen Sharing subtype. Treat High Performance behavior as
  reverse-engineered measurements, not a specification.

## Media paths

- The gateway encodes video as VP9 only: one encoder to maintain. Do not add a
  second encoder (H.264, AV1 or any other), a codec probe, or a codec key that
  selects one. A stream a remote codes itself may be passed to the browser
  untouched where a rule below allows it; transcoded, it is only ever to VP9. The
  browser is asked two questions, each once at page load and stated on the session
  socket: which VP9 profile its decoder takes, for `render_chroma = "auto"`, and
  whether it takes a High Performance Mac's HEVC, for `hevc_passthrough`. The
  gateway *selects* on the answers and never refuses a client for them. Do not grow them into a
  capability negotiation or a reason to turn a session away. Preserve the announced
  configuration and color-space behavior described in
  [The codec](docs/architecture.md#the-codec) and
  [Choosing a chroma](docs/architecture.md#choosing-a-chroma).
- The one picture that is not video is PNG tiles: for a desktop past the video
  ceiling on a source that hands over its own rectangles, VNC without `resize`.
  Each is a rectangle exactly as the server sent it. Do not add a key that selects tiles, use them
  within the ceiling on any other session, cut, merge or cache rectangles in the
  gateway, or give them to any other source with `resize` or without rectangles,
  which still ends on the ceiling's refusal. See
  [Tiles past the ceiling](docs/architecture.md#tiles-past-the-ceiling).
- A generic VNC target whose plan resolves to 4:4:4 lists wlshare's VP9
  encoding and passes its frames to the browser untouched; 4:2:0 is always
  encoded here from ZRLE. Do not transcode a passed frame, pass one to a 4:2:0
  browser or from another server, or add a key that selects it. See
  [wlshare's stream, passed through](docs/architecture.md#wlshares-stream-passed-through).
- Remote audio uses its own `/ws/audio` socket and queue; opening the socket is
  the subscription. Do not put audio on the session socket. Remote audio is Opus
  only; there is no codec key, and do not add another encoder or a passthrough.
  Preserve claim-bound eviction and the source-format/resampling boundaries in
  [Audio frames](docs/architecture.md#audio-frames).
- Generic VNC audio is wlshare's audio extension — FLAC frames, with the QEMU
  Audio extension's control messages — discovered on the connection the way the
  density extension is: `audio = true` makes the gateway ask, and a server that
  never announces it leaves the session silent rather than failing it. Do not
  take raw PCM from the RFB connection, add a second codec to it, or add a
  configuration key naming the server. See
  [Desktop audio over VNC with wlshare](docs/wlshare-audio.md).
- `ard` carries no sound and takes no `audio` key. Standard mode never touches
  the Mac's sound output, which keeps playing where the Mac sends it — its own
  speakers, or an AirPlay receiver that runs outside remotex. Do not add an
  AirPlay receiver or any other sound path for it to the gateway.
- `ard-high-performance` takes the Mac's picture and sound together from High
  Performance's media stream (`src/vnc_apple_media.rs`): HEVC and AAC-ELD over
  SRTP, every packet authenticated before it is decrypted and every report sent
  as SRTCP. The Mac refuses one leg without the other, so the target always
  carries sound and takes no `audio` key. While the sound leg runs the Mac mutes
  its own output, so it plays nothing to an AirPlay speaker. Its two decoders
  are the non-default `apple-hp-media` feature, which no release artifact
  enables; a build without it refuses the subtype. Zlib carries its picture only
  until the stream is up and across display changes, and a stream that fails
  ends the session, as in Apple's viewer. Do not add a High Performance subtype
  without the stream or a fallback to zlib from a failed one, combinations
  Apple's viewer never offers. See
  [The media stream](docs/apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).
- `hevc_passthrough` on `ard-high-performance` passes the Mac's HEVC access units,
  as the Mac sent them, to a browser that said its decoder takes them, for a LAN;
  every other browser is sent VP9 as without the key. The Mac's ZRLE rectangles
  fill the stream's gaps as VP9 encoded here, each switch between the two starting
  at a keyframe, and a PLI is a passed stream's repaint. Keep it
  to that stream: no other remote's HEVC, and a passed unit is never altered. See
  [Apple's HEVC, passed through](docs/architecture.md#apples-hevc-passed-through).
- Browser camera redirection is MS-RDPECAM on RDP and wlshare's camera extension
  on generic VNC, H.264-only, and never transcoded by the gateway. It uses its own
  `/ws/camera` socket, is explicit per session, and is bound to both claim and
  engine. See [Camera frames](docs/architecture.md#camera-frames) and
  [The browser's camera over VNC with wlshare](docs/wlshare-camera.md).
- Browser microphone redirection is MS-RDPEAI on RDP and wlshare's microphone
  extension on generic VNC. The browser sends low-bitrate mono Opus, which the
  gateway decodes to the 16-bit PCM the host records in. It uses its own `/ws/mic`
  socket, follows the camera socket's rules — explicit per session, refused with
  `4002` when the target carries no microphone, closed with the engine — and is
  never put on the session socket. See
  [Camera frames](docs/architecture.md#camera-frames) and
  [The browser's microphone over VNC with wlshare](docs/wlshare-microphone.md).
- Do not use Windows Server for ordinary camera QA; without the Remote Desktop
  Session Host role it does not offer the enumeration channel. Camera redirection
  is not required in the normal QA flow.

## Packaging and platforms

- Linux x86-64 artifacts target the baseline x86-64 ISA and dispatch SIMD at
  runtime. Never set a global `target-cpu`; use runtime detection and
  per-function `#[target_feature]` when needed. Prebuilt native archives must
  keep the same floor. See
  [x86-64 CPU compatibility](packaging/README.md#x86-64-cpu-compatibility).
- The native `embedded-gateway` feature is the Unix-only `remotex tui` control
  plane and its hidden `serve-embedded` workers. Containers must be built through
  `packaging/build-container-binary.sh`, with default features disabled, and must
  never expose `tui`, `serve-embedded`, or `check-config --embedded`.
- Windows ships only `serve`, `check-config`, and `gen-passwd` in the MSI. Build
  it with `packaging/build-windows-msi.ps1` on `windows-ci-build` through
  `ci/windows/remote.ps1`. Do not add a service or package-owned live config.
- Follow [Packaging](packaging/README.md) for native layouts, prebuilt dependency
  rules, and release workflow.

## Testing and interactive QA

- Follow [Stable headless browser tests](tests/playwright/README.md). Assert
  deterministic system decisions, not pixels, paint timing, frame rate, latency,
  cursor rendering, or layout-dependent synthetic input. Run headless with one
  worker, use accessible locators and web-first assertions, give wire-format
  specs an independent parser, and call `returnToPicker` from every spec.
- Use `tests/ws_probe.py` to inspect the control messages a browser sees.
- Do not use AppleScript, synthetic clicks, or screenshot loops to inspect a
  browser. Ask the client through deterministic interfaces, and ask the user for
  observations only eyes can provide.
- A virtual Mac (an Apple Virtualization guest) is for smoke tests only: that
  a session connects, shows its picture, starts its sound and survives a
  resize. It lags whenever it plays sound, and its High Performance sound goes
  distorted and then silent after a minute or so, under Apple's own viewer too.
  Judge performance, sound quality and long sessions on a physical Mac, or
  against Apple's viewer on the same machine first. See
  [The sound](docs/apple-vnc-889.md#the-sound).
- Do not infer GUI or network capabilities from an SSH attachment. A tmux server
  retains the environment and access of the user that started it. Test a
  capability once and read its error; being able to drive the GUI is still not
  permission to do so.
