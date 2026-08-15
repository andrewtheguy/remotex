## General

- Strict no backward-compatibility or legacy paths since it is a personal project.
- Require secure contexts and WebCodecs (`VideoDecoder` and `AudioDecoder`) for all web features. The frontend denies startup when either is missing (`frontend/src/preflight.ts`); do not add a second path for a browser without them.
- Do not run `cargo fmt`.
- After Rust changes, run `cargo clippy --all-targets -- -D warnings` and
  `cargo test`.
- After frontend JS/TS changes, run Biome checks in `frontend/`.
- Before browser QA of a frontend change, run `bun run build` in `frontend/`
  and say so. `remotex serve` serves the gitignored `frontend/dist` from disk
  (`ServeDir` in `src/server.rs`); Biome, `tsc -b`, `bun test`, and backend tests
  do not detect a stale bundle. For source-based iteration, use
  `REMOTEX_DEV_BACKEND=<port> bun run dev`.
- **There is one client, and it is the page a browser loads.** Desktop app use is
  that page installed as a Chrome or Edge app, not a native wrapper. Do not add a
  second implementation of anything the page already does.
- One `bun run build`, one `frontend/dist`, served over HTTP from the gateway's
  origin root. Every URL the page uses goes through `frontend/src/gateway.ts`.
- Put temporary files and test config under `tmp/`. Run efficient local Python
  one-offs with `uv` (GitHub Actions excluded).
- Use `anyhow` for application errors and `thiserror` for typed API errors.
- Keep e2e tests under `tests/`. Dummy RDP/VNC servers may use Docker or Podman.
- The native `embedded-gateway` feature is the `remotex tui` multi-instance
  control plane plus its hidden `serve-embedded` workers. The master alone owns
  `remotex.localhost:<port>` and routes instance subdomains to private
  `<instance>/gateway.sock` sockets; workers remain tied to its stdin liveness
  pipes. Container binaries must be built through
  `packaging/build-container-binary.sh`, which disables default features; never
  put `tui`, `serve-embedded`, or `check-config --embedded` in an image.
- Multi-session support is permanently out of scope. Each gateway has one active
  session. A force-claim evicts the current holder (`src/session.rs`) through the
  clients' **Take over** flow. Reconnects, target switches, and browser takeovers
  resume that same session without another prompt.
- **No AppleScript, no synthetic clicks, no screenshot loops.** Do not drive a
  browser's UI to find out what the client is doing; ask it, and ask the user for
  the one thing only eyes can answer. `remotex serve` puts its reasons on stderr,
  and the deterministic half of the client's behaviour is
  [`tests/playwright`](tests/playwright/README.md).

## SSH and tmux

Do not infer what this shell can do from how a client attached to it. An
existing tmux server keeps the environment and access of the user that started
it, so arriving over SSH does not mean the session is headless or restricted.

In particular this Mac has a **logged-in GUI session**, and the shell can use it.
If a capability looks unavailable, test it with one command and read the error; do
not assume it from the shape of the session.

Being *able* to drive the UI is not permission to. Leave clicking and looking to
the user.

## Display geometry

Pointer clients always show desktops at 100%. Oversized desktops scroll; never
add fit-to-window, zoom-to-fit, or viewport-derived scaling. Mobile is the sole
exception: `CAN_PINCH_ZOOM` in `frontend/src/useRemoteDesktop.ts` gates its
fit-to-width base scale and pinch zoom.

`ServerMsg::Resize { w, h, scale }` uses remote pixel density, not a fit factor.
`applyCanvasCss` presents the framebuffer at `w / scale` by `h / scale` CSS
pixels. Thus 3840×2160 at `scale: 2.0` is a 1920×1080 desktop at full pixel
fidelity. Producers are RDP `Density` (`src/rdp.rs`) and Apple layout's
backing/logical ratio (`src/vnc_apple.rs`); neither depends on the viewport.

To fit the window, ask the remote to render at that size via `resize = true`,
`ClientMsg::Viewport`, and the engine's resize mechanism. Lack of resize support
never permits client scaling.

`resize = true` means the window drives the remote's size, continuously, on every
engine alike. There is **no client-side resize control**: no auto-resize toggle,
no "Resize to window" button, no remembered preference — the gateway states the
one policy on `connected` and the client obeys. Standard `ard` refuses `resize`
outright (config parse rejects it). High Performance's descriptor must keep the
native fixed 3840×2160 backing ceiling: using its current mode as the maximum
makes the Mac decline any later request beyond the initial size.

**Opening size is one rule for every engine** (`TargetConfig::opening_size`):
the pinned `width`/`height` when the operator set both, else the full resolution
of the client's own screen — carried in `ClientMsg::Connect` so it exists before
the engine's handshake — else `DEFAULT_SIZE`. `width`/`height` are `Option`s;
*specified* is meaningful. Mid-session, `ClientMsg::HostDisplay` is a density
report and only `resize = true` targets act on it.

RDP's graphics pipeline is the `egfx` config key (default true), decoupled from
`resize`. With both on, a resize is a Display Control layout under the pipeline —
a graphics reset, no reactivation, no reconnect — which is what makes auto-resize
affordable there; the trade is a Windows host's text staying soft after it. With
`egfx = false` the legacy path re-renders sharp at the price of a reactivation
per resize, and a session whose sound negotiated on the dynamic `rdpsnd`
transport (Windows, and only Windows) then resizes by *reconnecting*, because
that host's audio redirector does not survive its own reactivation. The wrapper
decides by reading its live settings and debounces reconnect-resizes at 300 ms;
the gateway just asks. xrdp keeps the plain layout resize with audio intact.

Apple display modes:

- `ard` is Apple Screen Sharing's **Standard mode** over RFB 3.8. It uses Apple
  DH authentication, shares the Mac's physical displays, and refuses `resize`.
  Like High Performance it asks for zlib in the second `SetEncodings`, the one a
  display layout triggers; the first list must stay zlib-free or the layout is lost.
- `ard-high-performance` is **experimental** and the one path built with no
  specification at all — the revision, record layer, control messages and virtual
  display handling are reverse engineered, so treat `docs/apple-vnc-889.md` as
  measurement, not contract. Its dynamic-resolution path remains reverse
  engineered.
  Prefer widening `ard` over deepening this. It is Apple Screen Sharing's
  **High Performance mode** over
  RFB 003.889. It requests one virtual display, disables physical displays, and
  moves all remote windows onto it. It opens at `opening_size` at the client
  screen's density — the client's own screen resolution unless a size is pinned,
  which is how Apple's client opens, and it matters more than any later size:
  windows squeezed onto a small opening display never spread back out. With
  `resize = true`, viewport reports then replace the virtual display
  configuration.
  Its setup descriptor always enables dynamic resolution. Apple's client can
  choose up to two virtual displays and fixed resolution presets; remotex
  implements neither control.

## Browser tests

Headless Playwright tests live in `tests/playwright/`. Assert system decisions,
never machine timing. A valid assertion should not change if the machine is twice
as slow.

Deterministic and in scope: DOM/accessibility state, control-plane JSON, HTTP
responses, WebSocket bytes and ordering, and `framereceived` transport events.
Out of scope: canvas pixels or `toDataURL`, paint occurrence/counts, frame rate,
latency or deadlines, cursor rendering, screenshots, and synthetic pointer input
or gestures whose coordinates depend on settled layout. Cover those through raw
WebSocket, protocol, or container e2e tests with controlled clocks.

Avoid fixed sleeps, CSS/nth-child selectors, transient states, and wall-clock
event counts. Prefer accessible locators, web-first assertions, and `expect.poll`.
For counts, assert invariant relationships such as `records > frames`.

- Run headless with one worker. Shared login/target and SSH pasteboard helpers
  are in `tests/playwright/support.ts`.
- Every spec must call `returnToPicker`; otherwise the live target session can
  leak into the next spec. `logInAndConnect` tolerates either initial landing.
- After Playwright changes, run `bun run typecheck` in `tests/playwright/`.
- Keep accepted specs there, not in `tmp/`, and run new specs repeatedly.
- A wire-format spec must use its own parser, not the SPA's. Rust e2e drives a raw
  WebSocket, while TS unit tests parse self-built frames; this is the
  independent check that both wire ends agree.
- `audio-socket.spec.ts` needs a gateway serving audio rather than a live Mac, so it
  opts in separately with `REMOTEX_PLAYWRIGHT_AUDIO_TARGET=<target>`. The tone harness
  supplies one with no remote at all: `cargo test --lib serve_a_test_tone -- --ignored
  --nocapture`, then point `REMOTEX_PLAYWRIGHT_BASE_URL` at the address it prints.
- use `tests/ws_probe.py` to drive a local gateway WebSocket and print the control messages a browser sees.

## Remote audio

Remote audio uses `opus-prebuilt`, an `opus` 0.3.1 fork whose sys crate downloads
a prebuilt static libopus archive. Its library name remains `opus`, so
`use opus::…` does not change. Do not restore a CMake libopus build, `LIBOPUS_STATIC`,
`LIBOPUS_NO_PKG`, or `CMAKE_POLICY_VERSION_MINIMUM` in
`packaging/build-tarball.sh`.

VP9 strikes the same bargain through `libvpx-prebuilt` (`vpx-sys` in `Cargo.toml`,
pinned by tag): a static libvpx built once per target, and a sys crate whose build
script only downloads and links. No `configure`, no assembler, no `pkg-config` and no
libclang on this side — the bindings are committed in that repository. Do not restore a
source build, a vcpkg dependency, or bindgen at build time. Its archives are VP9 only
and `--enable-realtime-only`, which is all `src/vp9.rs` asks for; anything else needs
its own build behind `LIBVPX_PREBUILT_DIR`.

The only other option is `audio_codec = "pcm"` (`src/pcm_stream.rs`), and it adds
no dependency at all: the remote's wave buffer goes to the socket as it arrived,
one packet per buffer, unresampled and unencoded, at 1.41 Mbit/s. Do not add a
second *encoder* here. HE-AAC was built and reverted on measurement — Guacamole
carries this sound as raw `audio/L16` at the same 1.41 Mbit/s and does not
stutter, so compressing harder was never the axis the problem was on, and an
encoder whose licence is not OSI-approved was a real cost for it.

The choice is per target, not per client, and the gateway names it on the wire —
`ServerMsg::AudioFormat` carries the codec string, the decoder configuration and
`packetFrames`. The client does not decode anything itself: it hands encoded
packets to WebCodecs, so a codec a browser refuses surfaces as a named decoder
error rather than as silence. Passthrough reaches no decoder at all — Web Audio
alone plays it — but that is a property of the path and not a compatibility escape
hatch: WebCodecs is the client's entry condition (`frontend/src/preflight.ts`), so a
browser without it never reaches a target of either kind.

`src/pcm48.rs` is the *encoded* front half — deinterleave and resample in exact
882-to-960 groups — and a codec draws its own packet size out of it. Passthrough
does not go through it and must not start: resampling is the thing it exists not
to do. Do not tie the group size to a packet size either; the ratio is what makes
882-to-960 exact.

Audio has **its own WebSocket and its own queue**, `/ws/audio`. Opening it is the
subscription — there is no `ClientMsg` for audio, and closing it is the only way to
stop. Do not put sound back on the session socket: that queue is four frames deep on
`render_type = "video"`, and a pump waiting behind a video backlog stops draining the
bridge, which then drops wave buffers. A lost tile is repainted; a lost wave buffer is
a hole.

The audio socket is bound to the **claim**, not to an attachment, so it survives a
reattach and a target switch — `SessionManager::arm_audio` re-arms it from `connect`.
It is closed only where the claim changes (`evict_audio`), and that eviction is
load-bearing rather than tidy: without it the next `connect` re-arms an evicted
browser onto the new holder's desktop.

## Camera

The browser's camera goes the other way, over MS-RDPECAM, and it is RDP-only like
audio: `camera = true` per target (`src/config.rs`), refused on VNC at parse time.
The channel itself is implemented in Rust in the wrapper crate
(`libfreerdp-prebuilt/crates/freerdp/src/camera.rs`) as a generic DVC plugin over
`drdynvc` — the archives compile FreeRDP's own `rdpecam` **out**, deliberately: that
implementation is a V4L capture stack and an H.264 encoder, and this camera's source
is a browser. Do not turn `CHANNEL_RDPECAM` on.

The gateway carries **H.264 only and never transcodes** — the same bargain as PCM
passthrough, in the other direction. The browser's `VideoEncoder` produces Annex B
Constrained Baseline (`frontend/src/cameraSender.ts`), the Windows host decodes it,
and one media type is advertised: the geometry the socket announced. There is no
codec key beside `camera`, no gateway encoder, and no fallback; a browser that
cannot encode says so by name.

The camera has **its own WebSocket**, `/ws/camera`, and opening it is the enable.
Unlike audio the enable is **explicit and per session — never persisted**: no
localStorage key, no seed on connect, and the socket is bound to the claim *and the
engine* (`CameraSlot`), so every engine end and every claim change closes it and the
next session starts with the camera off. Closing the socket (either side) unplugs
the virtual device from the remote — turning the camera off in the browser turns it
off on the host. A camera socket against a target without `camera = true` is closed
with `4002`, not silently tolerated.

The remote drives the traffic: `cameraStart`/`cameraStop`/`cameraKeyframe` relay the
host's MS-RDPECAM decisions to the browser, which encodes only between start and
stop and restarts at a keyframe. Samples are credit-metered in the wrapper; on
overflow it drops the queue whole and asks for a keyframe, because H.264 cannot
resume mid-GOP. Streaming needs an application on the host to open the camera —
enumeration, announcement and device installation are testable without one
(`freerdp-e2e`'s camera leg, `tmp/camera_probe.py`), the picture itself is not.

Whether a camera can exist at all is the host's decision, made before any client
message: the server side creates the MS-RDPECAM enumeration channel, and **Windows
Server never does** over plain RDP — measured on Server 2025 Datacenter, where
Microsoft's own client fails the same way, and where installing Media Foundation
and clearing `fDisableCameraRedir` changed nothing. Against such a host
`camera = true` is an enable nothing ever answers: the socket opens, the device
plugs, no channel arrives, no device appears. Do not debug the gateway for it;
`FREERDP_ECAM_TRACE=1` (wrapper) shows the difference as an enumeration channel
that never opens. Windows 11 offers the channel and installs the device
("Remotex Camera (redirected)"), where the one remaining default to know about is
the Camera app's: it opens on the host's own camera when one exists, and the
redirected one is behind its change-camera button.

## Video codec

Video — `render_type = "video"` and `render_motion_subtype = "stream"` alike — is
**VP9 only** (`src/vp9.rs`): BSD-licensed with a patent grant, and in every browser
build. There is no codec key, no probe, no gate, and no fallback path.
`ServerMsg::VideoFormat` announces the exact WebCodecs configuration string before a
stream's first unit, and a client that cannot decode what arrives says so from its
own `VideoDecoder`, naming the configuration. See
[`docs/architecture.md`](docs/architecture.md).
