# Stable headless browser tests

These Playwright checks cover DOM/control-plane behaviour only. They do not
assert canvas pixels, framebuffer timing, cursor rendering, pointer input, or
remote-desktop gestures; the Rust protocol and container E2E tests cover those
paths.

`clipboard.spec.js` is the live-Mac regression for the web clipboard panel. It
proves that unsolicited remote copies still auto-sync, while opening and
revealing the panel leave the local clipboard untouched until explicit Copy.

`oversized-clipboard.spec.js` covers the refusal path: a Mac pasteboard larger
than `MAX_CLIPBOARD_BYTES` reaches the panel as its size, not as the first 64 KiB
of itself. It is here rather than only in the Rust and Swift unit tests because
the claim spans the agent, the gateway, the browser link and the panel, and the
failure it guards against — a truncated value arriving *successfully* — is
invisible to any one of them.

`support.js` holds what both share: the login/target flow and the SSH hooks that
read and write the Mac's pasteboard. Two conventions live there. Every spec ends
by handing the session back to the picker, because the server keeps a target
session running when its browser goes away; and `logInAndConnect` accepts either
landing, so a run abandoned on the desktop does not break the next one.

## Run

Install the pinned runner and Chromium once:

```sh
cd tests/playwright
npm ci
npx playwright install chromium
```

Start the gateway and Vite proxy from the repository root in separate terminals:

```sh
cargo run -- serve --config tmp/test_config.toml
```

```sh
cd frontend
REMOTEX_DEV_BACKEND=52675 bun run dev -- --host 127.0.0.1
```

Then provide the local test login and SSH destination for the Mac target:

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_USERNAME='<username>' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_TARGET='mac' \
REMOTEX_PLAYWRIGHT_MAC_SSH='<ssh-user>@<mac-host>' \
npx playwright test
```

That runs both specs; name one (`npx playwright test clipboard.spec.js`) to run
it alone.

The defaults are `http://127.0.0.1:5173/` and target `mac`. Override the URL
with `REMOTEX_PLAYWRIGHT_BASE_URL` when the dev server uses another address.
Each test is skipped with a list of missing variables when its live-Mac
configuration is absent. They always run headless with one worker, and share the
single session slot, which is why they are sequential by configuration rather
than by luck.
