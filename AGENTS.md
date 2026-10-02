# Repository instructions

Keep this file to rules that change how work is performed. Design rules,
design explanations, protocol details, measurements, and operational guides
belong in the linked documentation.

## Priorities

- The project prioritizes integration with the native remote desktop servers
  of Windows and macOS: Windows' own Remote Desktop over RDP, and macOS's
  built-in Screen Sharing in both of its modes (`ard`, `ard-high-performance`).
  On Linux it prioritizes [wlshare](https://github.com/andrewtheguy/wlshare), our
  own wlroots VNC server (`../wlshare`), reached as `subtype = "wlshare"`.
- Design, QA and optimization start from these three. Behavior specific to one
  of them is welcome, and when work for them competes with work for other
  servers, they come first. Other RDP and VNC servers remain supported through
  each protocol's baseline.
- Because wlshare is ours, what RFB lacks can be added to it as an
  extension a `wlshare` target lists, as density, outputs, audio, camera,
  microphone and its VP9 stream were, or as a message it sends, as the scroll
  distance was. A plain `vnc` target lists none of them and
  reads any server, wlshare included, through the RFB baseline.

## Workflow

- Strict no backward-compatibility or legacy paths.
- Do not run `cargo fmt`.
- No squash merges.
- After Rust changes, run `cargo clippy --all-targets -- -D warnings` and
  `cargo test --lib`, once each. Run `cargo test --tests` only when the change
  reaches what those tests drive, and a test marked `#[ignore = "slow: …"]` only
  when it reaches what that test checks. Mark a test that waits seconds that way.
- After frontend JS/TS changes, run the Biome checks in `frontend/`.
- Before browser QA of a frontend change, rebuild the gateway and say so: the
  bundle is compiled into the binary, and the gateway is the only thing that
  serves it. Do not put a dev server in front of it.
- After Playwright changes, run `bun run typecheck` in `tests/playwright/`.
- Put temporary files and test configuration under `tmp/`. Always run local
  Python through `uv` (GitHub Actions excluded).
- Errors are `anyhow` by default, and `thiserror` wherever a caller branches on
  the kind. Carry the cause: add `.context()` on the way up, keep an error typed
  rather than flattening it to a `String`, and drop a source only when it says
  nothing the message does not.
- Keep end-to-end tests under `tests/`; dummy RDP/VNC servers may use Docker or
  Podman.
- For an RDP protocol detail, read the Microsoft specification in
  [ms-rdp-specs](https://github.com/andrewtheguy/ms-rdp-specs) (`../ms-rdp-specs`)
  rather than recalling it: search the `MS-XXX.md` copy and cite the PDF. When
  the work needs a spec that is not kept there, add it as that repository's
  README describes, PDF, Markdown copy and table row together, before relying
  on it.

## Design rules

The rules live in [Constraints](docs/architecture.md#constraints). Read an
area's section before changing what it covers:

- [The client and its bundle](docs/architecture.md#the-client-and-its-bundle):
  one client, the browser SPA; one frontend build compiled into the gateway; one
  WebAssembly module; the pinned HEVC decoder archive as the one file read at run
  time; no fallback browser paths.
- [Sessions](docs/architecture.md#sessions): one active session per gateway,
  with takeover, and a fresh engine for every `connect`; size, sound and
  passthrough chosen at the picker, not in the config, and held for the
  session's life.
- [Input and display](docs/architecture.md#input-and-display): the two touch
  layers, the display picker as the remote's, presentation at 100%, density as
  the wire's word, and the Apple mosaic as the one rescale.
- [Media paths](docs/architecture.md#media-paths): one VP9 encoder or passed
  untouched, with a rule for each passthrough, a desktop past the ceiling, audio,
  camera and microphone.

## Packaging and platforms

- Linux x86-64 artifacts target the baseline x86-64 ISA and dispatch SIMD at
  runtime. Never set a global `target-cpu`; use runtime detection and
  per-function `#[target_feature]` when needed. Prebuilt native archives must
  keep the same floor. See
  [x86-64 CPU compatibility](packaging/README.md#x86-64-cpu-compatibility).
- The native `embedded-gateway` feature is the `remotex tui` control plane and
  its hidden `serve-embedded` workers, on Unix and Windows alike, each worker's
  private endpoint a Unix socket or an owner-only named pipe behind
  `src/embedded/transport.rs`. Containers must be built through
  `packaging/build-container-binary.sh`, with default features disabled, and must
  never expose `tui`, `serve-embedded`, or `check-config --embedded`.
- The Windows MSI ships the native binary, `tui` included. Build it with
  `packaging/build-windows-msi.ps1` on `windows-ci-build` through
  `ci/windows/remote.ps1 ci -Package` only when packaging changes: release CI
  builds and install-tests the MSI itself. Do not add a service or
  package-owned live config to it.
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
- A virtual Mac (an Apple Virtualization guest) is for smoke tests only: that a
  session connects, shows its picture, starts its sound and survives a resize.
  It lags with sound, and its High Performance sound fails after a minute under
  Apple's own viewer too. Judge performance, sound quality and long sessions on
  a physical Mac, or against Apple's viewer on the same machine first. See
  [The sound](docs/apple-vnc-889.md#the-sound).
- Do not infer GUI or network capabilities from an SSH attachment. A tmux server
  retains the environment and access of the user that started it. Test a
  capability once and read its error; being able to drive the GUI is still not
  permission to do so.
- Do not use Windows Server for ordinary camera QA; without the Remote Desktop
  Session Host role it does not offer the enumeration channel. Camera redirection
  is not required in the normal QA flow.
