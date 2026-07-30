## General

- strict no backward compatibility since it is a personal project
- no cargo fmt
- run cargo clippy with `-- -D warnings` to treat warnings as errors and cargo test after rust code changes
- run biome checks on frontend/ after JS/TS code changes
- **always `bun run build` in frontend/ before asking anyone to QA a frontend change in a browser, and say that you did.** `remotex serve` serves the SPA from `frontend/dist` on disk (`ServeDir`, `src/server.rs`) — nothing is embedded in the binary — and `dist` is gitignored, so a stale bundle is invisible and is what the browser gets. No check catches it: biome, `tsc -b` and `bun test` read `src/`, `cargo test` never loads the page, and neither does a script driving the WebSocket directly. The symptom is a feature that looks broken while the server side is provably right. To iterate without rebuilding, use `REMOTEX_DEV_BACKEND=<port> bun run dev`, which serves from source
- use tmp/ for temporary files and test config
- for local (not github actions) one-off scripts that are more efficient with python, always run with `uv`.
- error handling: `anyhow` for application errors, `thiserror` for typed API errors
- keep e2e tests under tests/*; dummy RDP or VNC servers may run with docker or podman when needed
- multi session is always out of scope (never planned, not merely deferred): no concurrent sessions, session sharing, or session broker. It means **one active session at a time**, and it is two separate slots saying so at two hops — never one client:
  - **the gateway's**: one active session per gateway instance, forever. A new browser force-claims it and evicts the previous holder (`src/session.rs`)
  - **the agent's**: one active session on a Mac running `remotex-agent`, *claimed* rather than seized. A connection asks with `GatewayMsg::Claim` and is granted, handed the slot, or refused with who holds it; a client shows the refusal with a Take over button, the same shape as the browser prompt and as Windows Remote Desktop
- the agent's slot is keyed on **the claim's session id, never on a key or an address**. Authentication (the keys, in the handshake) decides whether a peer may ask; session ownership decides whose turn it is — SSH's split. Keeping them apart is what lets several gateways be *permitted* while one is *connected* (see the authorized-key list in `docs/roadmap.md`), and what makes a reconnect, a target switch and a browser takeover reclaim the slot in silence. So "more than one client may be permitted, taking turns explicitly" is not multi session and is not out of scope; concurrency is

## Browser tests

Headless Playwright, in `tests/playwright/`.

### What may be asserted

- **assert on things the system decides, never on things a machine's timing decides.** That one rule settles what belongs here; judge a new spec by **what it observes**, not by whether it appears on a list. The existing specs are examples of the rule, not the definition of it
- in scope, because these are deterministic: DOM state and accessible roles, control-plane JSON, HTTP responses, and WebSocket frame *bytes* — header fields, record counts, payload lengths, message ordering. `framereceived` qualifies because it is a transport event carrying a fixed payload
- out of scope, because these are races: reading canvas pixels or `toDataURL`; asserting a paint happened, or how many did; timing anything (frame rate, latency, "within N ms"); cursor rendering; synthetic pointer input or gestures, whose coordinate mapping depends on layout having settled; screenshot comparison. Test those through the raw WebSocket, protocol, and container e2e tests, which control their own clock
- the test for a borderline case: would the assertion change answer if this machine were twice as slow? Then it does not belong here
- also avoid the quieter shapes, which pass locally and fail in a year: fixed sleeps, CSS/nth-child selectors, assertions on transient states a fast machine skips through, and counting events over a wall-clock window. Prefer accessible locators, web-first assertions and `expect.poll`; where a count is the point, assert a relationship that holds for any sample (`records > frames`) rather than a number that depends on how long the run happened to watch

### Writing and running them

- run headless and single-worker. Shared login/target and SSH pasteboard helpers live in `tests/playwright/support.ts`
- the specs that need the Mac agent *running* are opt-in: `REMOTEX_PLAYWRIGHT_AGENT=host:port` asks for them, and they skip when it is unset or nothing answers there. A plain run never assumes a VM is up, the same bargain `#[ignore]` makes for the Rust e2e tests
- every spec must hand the session back to the picker before it ends (`returnToPicker`): the server keeps a target session alive when its browser goes away, so a spec that stops on the desktop leaves the next run reattached to it. `logInAndConnect` tolerates either landing, so one abandoned run cannot break every run after it
- the setup is TypeScript and Playwright only transpiles it, so type errors never surface at runtime — run `npm run typecheck` in `tests/playwright/` after changing anything there
- keep approved specs in `tests/playwright/` rather than leaving one-off copies in `tmp/`, and run a new one several times before relying on it
- a spec that parses a wire format should implement its own parser rather than importing the SPA's, or a wrong parser agrees with itself. This is the only place the wire is checked as the real SPA uses it: the Rust e2e tests drive a raw WebSocket client, and the Swift and TypeScript unit tests parse frames they built themselves, so both ends can otherwise agree with their own fixtures and disagree with each other

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
- **starting at login is explicit and names an absolute path.** The agent writes `~/Library/LaunchAgents/dev.remotex.agent.plist` only when asked — the menu's Start at Login, or `--install-launchagent` — and a launch registers nothing. So `launchctl kickstart -k gui/<uid>/dev.remotex.agent` can only ever start the binary that plist names, and a copy opened from anywhere else changes nothing. Run `--install-launchagent` once per machine from the copy that should be the one launchd starts; installing from a mounted DMG is refused, since that path is gone by the next login
- **after redeploying the agent, verify what is actually running rather than what is installed.** `--version` on a bundle describes the file, not the process. `lsof -p $(pgrep -x remotex-agent) | awk '$4=="txt"'` names the executing binary, and the startup log line says it too. If the login item names some other copy, the agent says so on every launch and in its menu; re-tick Start at Login (or re-run `--install-launchagent`) from the copy you want
