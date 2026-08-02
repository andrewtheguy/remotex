## General

- Strict no backward-compatibility or legacy paths since it is a personal project.
- Do not run `cargo fmt`.
- After Rust changes, run `cargo clippy -- -D warnings` and `cargo test`.
- After frontend JS/TS changes, run Biome checks in `frontend/`.
- Before browser QA of a frontend change, run `bun run build` in `frontend/`
  and say so. `remotex serve` serves the gitignored `frontend/dist` from disk
  (`ServeDir` in `src/server.rs`); Biome, `tsc -b`, `bun test`, and backend tests
  do not detect a stale bundle. For source-based iteration, use
  `REMOTEX_DEV_BACKEND=<port> bun run dev`.
- `frontend/src/viewer` is `remotex.app`'s remote surface, not part of the SPA.
  It builds separately (`bun run build:viewer` → `dist-viewer/`) and is copied
  into the bundle; `REMOTEX_VIEWER_DEV_URL` points the app's web view at
  `bun run dev` instead. Shared modules (`protocol.ts`, `tilePainter.ts`,
  `cursorCss.ts`, `audioPlayer.ts`) are used by both, so a change to one is a
  change to both clients.
- Put temporary files and test config under `tmp/`. Run efficient local Python
  one-offs with `uv` (GitHub Actions excluded).
- Use `anyhow` for application errors and `thiserror` for typed API errors.
- Keep e2e tests under `tests/`. Dummy RDP/VNC servers may use Docker or Podman.
- Multi-session support is permanently out of scope. Each gateway has one active
  session. A force-claim evicts the current holder (`src/session.rs`) through the
  clients' **Take over** flow. Reconnects, target switches, and browser takeovers
  resume that same session without another prompt.

## SSH and tmux

Do not infer shell permissions from how a client attached to tmux. An existing
tmux server retains the environment and access of the user that started it.

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

Apple display modes:

- `ard` is Apple Screen Sharing's **Standard mode** over RFB 3.8. It uses Apple
  DH authentication, shares the Mac's physical displays, and refuses `resize`.
  Like High Performance it asks for zlib in the second `SetEncodings`, the one a
  display layout triggers; the first list must stay zlib-free or the layout is lost.
- `ard-high-performance` is **experimental** and the one path built with no
  specification at all — the revision, record layer, control messages and virtual
  display handling are reverse engineered, so treat `docs/apple-vnc-889.md` as
  measurement, not contract. Its dynamic-resolution path is the least settled part.
  Prefer widening `ard` over deepening this. It is Apple Screen Sharing's
  **High Performance mode** over
  RFB 003.889. It requests one virtual display at configured `width` and `height`,
  disables physical displays, and moves all remote windows onto it.
  Its setup descriptor always enables dynamic resolution. With `resize = true`,
  viewport reports replace the virtual display configuration. Apple's client can
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
- After Playwright changes, run `npm run typecheck` in `tests/playwright/`.
- Keep accepted specs there, not in `tmp/`, and run new specs repeatedly.
- A wire-format spec must use its own parser, not the SPA's. Rust e2e drives a raw
  WebSocket, while Swift and TS unit tests parse self-built frames; this is the
  independent check that both wire ends agree.

## remotex.app

The first screen always asks which gateway to use, with the last choice
preselected:

- **On This Mac:** the bundle starts `remotex-gateway serve-embedded
  --instance-dir <dir>` on an ephemeral `127.0.0.1` port with no web UI. Each
  launch mints a random bearer token, sends it only to the app in one stdout JSON
  line, and keeps it in memory. The app is the only client that knows it.
- **Somewhere Else:** the user enters a gateway address and login. This is the
  correct placement across a slow link, where the gateway should be near targets.

The home screen appears on every launch; **Change Gateway…** returns to it.

`dist/remotex.app` contains Swift client `Contents/MacOS/remotex-viewer` and
gateway `Contents/MacOS/remotex-gateway`. Both gateway choices expose identical
`/api/config`, `/api/targets`, `/api/session`, and `/ws` contracts. Only their
credential header differs: embedded uses `Authorization: Bearer`; remote uses
`Cookie: remotex_session`. `require_auth` accepts only the configured kind; any
other behavioral difference is a bug. See `GatewayCredential` in
`apps/remotex-viewer/Sources/Gateway/GatewayClient.swift`. The client manually
stores the remote cookie in `viewer.json` (mode `0600`), so login survives app
restarts, because `HTTPCookieStorage` mishandles `Secure` cookies on `wss` and
ignores ports.

Against a remote gateway, hide this bundle's **Configuration…** and **Restart
Local Gateway** (`AppModel.canEditConfiguration` and `usesEmbeddedGateway`): its
`remotex.toml` cannot configure remote targets. `--instance-dir` is the only
GUI-launch argument; gateway selection belongs to the home screen. Diagnostic
CLI paths additionally accept `--version` and the `--probe*` options.

The embedded gateway's lifetime is guaranteed by its stdin pipe. The app holds
the write end and sends nothing; clean quit, crash, Force Quit, or `kill -9`
closes it in the kernel, causing gateway EOF and exit. `SIGTERM` and explicit
termination are supplemental. Preserve
`aGatewayIgnoringSignalsStillDiesWithThePipe`, which proves the pipe alone works.

The embedded `<instance>/remotex.toml` permits top-level `branding` and
`[[targets]]`, refuses `[server]`, and permits zero targets. **Remote ›
Configuration…** validates through `remotex-gateway check-config --embedded`
and writes nothing on failure; do not add a Swift TOML parser.

Each instance is a separate app. `packaging/macos-viewer/make-instance-bundle.sh
<name> [icon.png]` copies and ad-hoc re-signs the bundle into `~/Applications`
with its own `CFBundleName`, `CFBundleIdentifier`, icon, and `~/Library/Application
Support/<CFBundleName>` directory. Re-run it after rebuilding `remotex.app`; it
never changes instance data. LaunchServices supplies no double-click arguments,
and a wrapper would put the base app rather than the instance in the Dock;
`--instance-dir` is only a QA override.

Remote audio uses `opus-prebuilt`, an `opus` 0.3.1 fork whose sys crate downloads
a prebuilt static libopus archive. Its library name remains `opus`, so
`use opus::…` does not change. Do not restore a CMake libopus build, `LIBOPUS_STATIC`,
`LIBOPUS_NO_PKG`, or `CMAKE_POLICY_VERSION_MINIMUM` in
`packaging/build-tarball.sh` or `packaging/macos-viewer/build-viewer-app.sh`.

After Swift changes:

1. Run `swift test --package-path apps/remotex-viewer`.
2. Run `packaging/macos-viewer/build-viewer-app.sh --no-dmg`; this also rebuilds
   the bundled gateway **and the canvas page**, so `bun` is required. After a
   change under `frontend/src/viewer`, run `bun run check` and `bun test src` in
   `frontend/` too — the build only proves the page compiles.
3. Manually launch `open -n dist/remotex.app --args --instance-dir
   "$PWD/tmp/app-instance"`. All QA state remains under that directory; delete it
   for a clean run. Never launch QA bare: the real instance is
   `~/Library/Application Support/remotex`.

Preferences must remain in the instance's JSON file, not `UserDefaults`: a defaults
suite lives in the user's Preferences directory regardless of `--instance-dir`.

Never validate with `swift run`, standalone `swift build`, or `.build` binaries;
they lack the bundle menus, `Info.plist` metadata, and gateway. Use `--no-dmg` for routine
development. Build a DMG only for releases, DMG changes, or an explicit request;
its filename remains `remotex-viewer-<version>` while the contained app is
`remotex.app`. macOS GUI QA is manual only.
