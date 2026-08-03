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
- There is one client. `remotex.app` shows this same SPA in a `WKWebView`, served
  by the gateway in its bundle, so a frontend change is a change to both — and
  `NATIVE_HOST` in `frontend/src/nativeHost.ts` is the only thing that may differ
  between them. Do not add a second implementation of anything the page already
  does.
- Put temporary files and test config under `tmp/`. Run efficient local Python
  one-offs with `uv` (GitHub Actions excluded).
- Use `anyhow` for application errors and `thiserror` for typed API errors.
- Keep e2e tests under `tests/`. Dummy RDP/VNC servers may use Docker or Podman.
- Multi-session support is permanently out of scope. Each gateway has one active
  session. A force-claim evicts the current holder (`src/session.rs`) through the
  clients' **Take over** flow. Reconnects, target switches, and browser takeovers
  resume that same session without another prompt.

## SSH and tmux

Do not infer what this shell can do from how a client attached to it. An
existing tmux server keeps the environment and access of the user that started
it, so arriving over SSH does not mean the session is headless or restricted.

In particular this Mac has a **logged-in GUI session**, and the shell can use
it: launching `.app` bundles, `screencapture`, `osascript`, `codesign`, and
driving the UI all work. macOS GUI QA is therefore something to *do* here, not
something to hand back — see the QA steps in
[`docs/macos-viewer.md`](docs/macos-viewer.md). If a capability looks
unavailable, test it with one command and read the error; do not assume it from
the shape of the session.

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

`resize` is permission to resize **when the user asks**. Letting the window drive
the size unasked is a second permission — `TargetConfig::auto_resize`, carried as
`autoResize` on `connected` — and plain `vnc` alone has it. RDP and both Apple
subtypes offer the manual control only, because of the faults in
[`docs/known-issues.md`](docs/known-issues.md); it is not a config key, since the
operator cannot know which engines survive a stream of resizes.

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
- `audio-socket.spec.ts` needs a gateway serving audio rather than a live Mac, so it
  opts in separately with `REMOTEX_PLAYWRIGHT_AUDIO_TARGET=<target>`. The tone harness
  supplies one with no remote at all: `cargo test --lib serve_a_test_tone -- --ignored
  --nocapture`, then point `REMOTEX_PLAYWRIGHT_BASE_URL` at the address it prints.

## remotex.app

It is a **shell**, not a second client. The bundle starts `remotex-gateway
serve-embedded --instance-dir <dir> --web-root <dir>` on an ephemeral `127.0.0.1`
port, and shows the SPA that gateway serves in a `WKWebView`. There is one
gateway, in this bundle; a gateway elsewhere is reached with a browser.

`dist/remotex.app` contains `Contents/MacOS/remotex-viewer`,
`Contents/MacOS/remotex-gateway`, and `Contents/Resources/web` — the built SPA,
named by `--web-root` because nothing about a bundle's layout is the gateway
binary's to guess.

Each launch mints a random token, sends it only to the app in one stdout JSON
line, and keeps it in memory. The app puts it in the web view's cookie store as
`remotex_session` before the first load. It must be a **cookie**: the page issues
its own `fetch` calls and opens its own `ws://` sockets, and neither can be given
a header from outside the document. `require_auth` reads that cookie on both
kinds of gateway and differs only in what makes the value valid.

The web view's data store must be **persistent and per instance**
(`WKWebsiteDataStore(forIdentifier:)`, keyed off `InstanceDirectory.dataStoreIdentifier`).
The client's three remembered preferences live in its `localStorage`, so a
non-persistent store drops them at every launch without saying so, and a shared
one would leak a QA instance's into the real one.

The app holds **no session**: no claim, no socket, no wire format, no protocol
version. Do not put any of it back. Everything about the session is the client's,
and it is the same client a browser runs.

The seam is one `WKScriptMessageHandler` and one `evaluateJavaScript` call, mirrored
in `frontend/src/nativeHost.ts`. The page posts one `state` object; every menu
title, tick and enabled state derives from it. **Nothing in the app is ever set
optimistically** — a tick moves when the client says the thing changed, not when
the item was pressed. Commands go out as encoded JSON, never interpolated: remote
clipboard text reaches this app and then goes into a JavaScript call.

What stays native is what a page cannot do: the `NSEvent` local monitor (which is
why ⌘Q and ⌘W reach the guest), `NSPasteboard` in both directions, **Resize to
Display**, the menu bar, and the gateway process. What a chord *means* is the
client's — `KeyboardCodes` sends DOM codes and `frontend/src/macKeys.ts` translates,
for both clients, with one bigger chord table under `NATIVE_HOST`.

The client's own panels stay the client's. **Remote › Clipboard…** and the Display
menu drive the page's panels rather than reimplementing them; a native
`ClipboardPanel`/`DisplayPanel` is on the roadmap and not worth the regression risk
now.

`--instance-dir` is the only GUI-launch argument; `--version` is the only other
CLI path. There is no preferences file in the instance directory any more — the
three remembered defaults belong to the client. Do not add `UserDefaults`: a
defaults suite lives in the user's Preferences directory regardless of
`--instance-dir`.

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
error rather than as silence. Passthrough reaches no
decoder at all, which is why it is the one option that plays on a plain `http://`
origin — WebCodecs needs a secure context and Web Audio does not.

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

After Swift changes:

1. Run `swift test --package-path apps/remotex-viewer`.
2. Run `packaging/macos-viewer/build-viewer-app.sh`; this also rebuilds the
   bundled gateway **and the SPA it shows**, so `bun` is required. Run
   `bun run check` and `bun test src` in `frontend/` too — the build only proves
   the client compiles.
3. Manually launch `open -n dist/remotex.app --args --instance-dir
   "$PWD/tmp/app-instance"`. All QA state remains under that directory; delete it
   for a clean run. Never launch QA bare: the real instance is
   `~/Library/Application Support/remotex`.

Never validate with `swift run`, standalone `swift build`, or `.build` binaries;
they lack the bundle menus, `Info.plist` metadata, and gateway. Run the build script
bare: with no arguments it is exactly what `release.yml` runs, so what you validate
and what ships are not two commands apart. `--no-dmg` remains as a shortcut for a
fast edit-build-launch loop and produces the same bundle — the image is made from
it and no longer consumes it — but the bare command is the one to trust a result
from. The image's filename remains `remotex-viewer-<version>` while the contained
app is `remotex.app`. macOS GUI QA is manual only.
