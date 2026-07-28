## General

- strict no backward compatibility since it is a personal project
- no cargo fmt
- run cargo clippy with `-- -D warnings` to treat warnings as errors and cargo test after rust code changes
- run biome checks on frontend/ after JS/TS code changes
- use tmp/ for temporary files and test config
- for local (not github actions) one-off scripts that are more efficient with python, always run with `uv`.
- error handling: `anyhow` for application errors, `thiserror` for typed API errors
- keep e2e tests under tests/*; dummy RDP or VNC servers may run with docker or podman when needed
- headless Playwright is allowed in `tests/playwright/`. Judge a new spec by **what it observes**, not by whether it is on a list — the rule below is the whole test, and existing specs are examples of it rather than the definition. Preserve approved specs there instead of leaving one-off copies in `tmp/`, and run a new one several times before relying on it
- **assert on things the system decides, never on things a machine's timing decides.** In scope: DOM state and accessible roles, control-plane JSON, HTTP responses, and WebSocket frame *bytes* — header fields, record counts, payload lengths, message ordering. Out of scope: anything whose value depends on when a frame arrived relative to a paint, or on how fast this particular machine is. `framereceived` is fine because it is a transport event with a deterministic payload; a canvas is not, because what is on it at any instant is a race
- concretely, the flaky shapes to avoid: reading canvas pixels or `toDataURL`; asserting a paint happened, or how many did; timing anything (frame rate, latency, "within N ms"); cursor rendering; synthetic pointer input or gestures, whose coordinate mapping depends on layout that has settled; screenshot comparison; and any assertion that would change answer if the machine were twice as slow. Those belong in the raw WebSocket, protocol, and container e2e tests, which control their own clock
- also avoid the quieter kind: fixed sleeps, CSS/nth-child selectors, assertions on transient states a fast machine skips through, and counting events over a wall-clock window. Prefer accessible locators, web-first assertions and `expect.poll`; where a count is the point, assert a relationship that holds for any sample (`records > frames`) rather than a number that depends on how long the run happened to observe
- every Playwright spec must hand the session back to the picker before it ends (`returnToPicker`): the server keeps a target session alive when its browser goes away, so a spec that stops on the desktop leaves the next run reattached to it. `logInAndConnect` tolerates either landing, so one abandoned run cannot break every run after it
- the Playwright setup is TypeScript, and Playwright only transpiles it — type errors never surface at runtime, so run `npm run typecheck` in `tests/playwright/` after changing anything there
- run Playwright tests headless and single-worker. Shared login/target and SSH pasteboard helpers live in `tests/playwright/support.ts`
- a spec that parses a wire format should implement its own parser rather than importing the SPA's, or a wrong parser agrees with itself. This is the only place the wire is checked as the real SPA uses it: the Rust e2e tests drive a raw WebSocket client, and the Swift and TypeScript unit tests parse frames they built themselves, so both ends can otherwise agree with their own fixtures and disagree with each other
- multi session is always out of scope (never planned, not merely deferred): this is a single-user program with one active session only, with session takeover logic (a new browser force-claims the single session slot and evicts the previous holder) — no concurrent sessions, session sharing, or session broker

## macOS viewer

- after Swift changes under `apps/remotex-viewer/`, run `swift test --package-path apps/remotex-viewer`, then run `packaging/macos-viewer/build-viewer-app.sh --no-dmg` and launch the packaged app
- launch a QA run with its own settings, never bare: `open -n dist/remotex-viewer.app --args --settings qa --gateway http://127.0.0.1:<test port>`. `--settings qa` puts the gateway address in its own `UserDefaults` suite and gives the run an ephemeral cookie jar, so a test run cannot overwrite the address a real one saved or log the real session out — `HTTPCookieStorage` matches by host and ignores the port, so without it a test gateway on `127.0.0.1` shares the real one's login cookie. The trade is that a QA launch always starts at the login screen instead of resuming. Wipe the slate with `defaults delete remotex-viewer.qa`
- never validate the viewer with `swift run`, a standalone `swift build`, or the executable under `.build`; those bypass the `.app` bundle and can behave differently, including missing menus and `Info.plist` metadata
- for routine viewer development, the disk-image layer is out of scope: use `packaging/macos-viewer/build-viewer-app.sh --no-dmg` and work exclusively with `dist/remotex-viewer.app`
- use the viewer script's default DMG build only for production/release validation, changes to the viewer's DMG packaging, or when the user explicitly asks for it
- no intrusive QA automation for the macOS viewer GUI; manual QA only

## macOS remotex agent

- after changes affecting the macOS `remotex-agent` or its packaging, build and validate it with `packaging/macos/build-agent-app.sh`; a standalone Cargo build is not agent packaging validation
- follow machine-local signing instructions in `CLAUDE.local.md` when present
- use `--no-dmg` only when the disk-image layer is explicitly out of scope; otherwise build the DMG, mount it, copy the app out as a user would, verify it with `codesign --verify --deep --strict`, and run the bundled executable with `--version`
