## General

- strict no backward compatibility since it is a personal project
- no cargo fmt
- run cargo clippy with `-- -D warnings` to treat warnings as errors and cargo test after rust code changes
- run biome checks on frontend/ after JS/TS code changes
- use tmp/ for temporary files and test config
- for local (not github actions) one-off scripts that are more efficient with python, always run with `uv`.
- error handling: `anyhow` for application errors, `thiserror` for typed API errors
- keep e2e tests under tests/*; dummy RDP or VNC servers may run with docker or podman when needed
- headless Playwright is allowed only for DOM/control-plane flows that have proved stable; the current whitelist is `tests/playwright/clipboard.spec.ts` (login, target selection, menu/panel state, clipboard metadata/reveal/copy/send, and responsive panel docking) and `tests/playwright/oversized-clipboard.spec.ts` (a remote clipboard over `MAX_CLIPBOARD_BYTES` reported as its size rather than truncated). Shared login/target and SSH pasteboard helpers live in `tests/playwright/support.ts`. Preserve approved tests in `tests/playwright/` instead of leaving one-off copies in `tmp/`; add another flow to this whitelist only after repeated local passes
- every Playwright spec must hand the session back to the picker before it ends (`returnToPicker`): the server keeps a target session alive when its browser goes away, so a spec that stops on the desktop leaves the next run reattached to it. `logInAndConnect` tolerates either landing, so one abandoned run cannot break every run after it
- the Playwright setup is TypeScript, and Playwright only transpiles it — type errors never surface at runtime, so run `npm run typecheck` in `tests/playwright/` after changing anything there
- run Playwright tests headless and single-worker, use accessible locators plus web-first assertions/polling, and do not add fixed sleeps; framebuffer/canvas pixels, paint timing, cursor rendering, pointer input, and gesture behaviour remain out of scope for browser automation because those are the flaky paths the original restriction covered — test them through the existing raw WebSocket, protocol, and container e2e tests
- multi session is always out of scope (never planned, not merely deferred): this is a single-user program with one active session only, with session takeover logic (a new browser force-claims the single session slot and evicts the previous holder) — no concurrent sessions, session sharing, or session broker

## macOS viewer

- after Swift changes under `apps/remotex-viewer/`, run `swift test --package-path apps/remotex-viewer`, then run `packaging/macos-viewer/build-viewer-app.sh --no-dmg` and launch the packaged app
- launch a QA run with its own settings, never bare: `open -n dist/remotex-viewer.app --args --settings qa --gateway http://127.0.0.1:<test port>`. `--settings qa` puts the gateway address in its own `UserDefaults` suite and gives the run an ephemeral cookie jar, so a test run cannot overwrite the address a real one saved or log the real session out — `HTTPCookieStorage` matches by host and ignores the port, so without it a test gateway on `127.0.0.1` shares the real one's login cookie. The trade is that a QA launch always starts at the login screen instead of resuming. Wipe the slate with `defaults delete remotex-viewer.qa`
- never validate the viewer with `swift run`, a standalone `swift build`, or the executable under `.build`; those bypass the `.app` bundle and can behave differently, including missing menus and `Info.plist` metadata
- for routine viewer development, the disk-image layer is out of scope: use `packaging/macos-viewer/build-viewer-app.sh --no-dmg` and work exclusively with `dist/remotex-viewer.app`
- use the viewer script's default DMG build only for production/release validation, changes to the viewer's DMG packaging, or when the user explicitly asks for it

## macOS remotex agent

- after changes affecting the macOS `remotex-agent` or its packaging, build and validate it with `packaging/macos/build-agent-app.sh`; a standalone Cargo build is not agent packaging validation
- follow machine-local signing instructions in `CLAUDE.local.md` when present
- use `--no-dmg` only when the disk-image layer is explicitly out of scope; otherwise build the DMG, mount it, copy the app out as a user would, verify it with `codesign --verify --deep --strict`, and run the bundled executable with `--version`
