# Repository instructions

Keep this file to rules that change how work is performed. Design explanations,
protocol details, measurements, and operational guides belong in the linked
documentation.

## Workflow

- Strict no backward-compatibility or legacy paths.
- Do not run `cargo fmt`.
- No squash merges
- After Rust changes, run `cargo clippy --all-targets -- -D warnings` and
  `cargo test`.
- After frontend JS/TS changes, run the Biome checks in `frontend/`.
- Before browser QA of a frontend change, run `bun run build` in `frontend/`
  and say so. `remotex serve` reads the gitignored `frontend/dist` from disk.
  For source-based iteration, use `REMOTEX_DEV_BACKEND=<port> bun run dev`.
- After Playwright changes, run `bun run typecheck` in `tests/playwright/`.
- Put temporary files and test configuration under `tmp/`. Always run local
  Python through `uv` (GitHub Actions excluded).
- Use `anyhow` for application errors and `thiserror` for typed API errors.
- Keep end-to-end tests under `tests/`; dummy RDP/VNC servers may use Docker or
  Podman.

## Product boundaries

- There is one client: the browser SPA, including when installed as a Chrome or
  Edge app. Do not add a native wrapper or a second implementation of a page
  feature.
- There is one frontend build and one `frontend/dist`, served from the gateway's
  origin root. Every URL the page uses goes through `frontend/src/gateway.ts`.
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
- Pointer clients present the remote desktop at 100%; oversized desktops scroll.
  Do not add fit-to-window, zoom-to-fit, or viewport-derived scaling. Mobile,
  gated by `CAN_PINCH_ZOOM`, is the sole fit-to-width/pinch-zoom exception.
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

- Video streams are VP9 only. There is no codec probe, codec key, or codec
  fallback. `render_chroma` is the only per-target codec choice, and its `"auto"`
  value is the only thing the browser is asked: the page states which VP9 profile
  its decoder takes on the session socket, the gateway *selects* between two
  profiles on it, and no client is ever refused for the answer. Do not grow it into
  a capability negotiation, a second codec, or a reason to turn a session away.
  Preserve the announced configuration and color-space behavior described in
  [The codec](docs/architecture.md#the-codec) and
  [Choosing a chroma](docs/architecture.md#choosing-a-chroma).
- Remote audio uses its own `/ws/audio` socket and queue; opening the socket is
  the subscription. Do not put audio on the session socket. The supported target
  choices are Opus and unresampled PCM passthrough; do not add another encoder.
  Preserve claim-bound eviction and the source-format/resampling boundaries in
  [Audio frames](docs/architecture.md#audio-frames).
- Apple High Performance system audio remains behind the non-default
  `apple-hp-audio` feature and absent from release artifacts. Do not add a second
  decoder beside the feature-gated fdk-aac path.
- Browser camera redirection is RDP-only, H.264-only, and never transcoded. It
  uses its own `/ws/camera` socket, is explicit per session, and is bound to both
  claim and engine. Do not enable FreeRDP's V4L `CHANNEL_RDPECAM` implementation.
  See [Camera frames](docs/architecture.md#camera-frames).
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
- Do not infer GUI or network capabilities from an SSH attachment. A tmux server
  retains the environment and access of the user that started it. Test a
  capability once and read its error; being able to drive the GUI is still not
  permission to do so.
