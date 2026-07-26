# Stable headless browser tests

These Playwright checks cover DOM/control-plane behaviour only. They do not
assert canvas pixels, framebuffer timing, cursor rendering, pointer input, or
remote-desktop gestures; the Rust protocol and container E2E tests cover those
paths.

`clipboard.spec.js` is the live-Mac regression for the web clipboard panel. It
proves that unsolicited remote copies still auto-sync, while opening and
revealing the panel leave the local clipboard untouched until explicit Copy.

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
npx playwright test clipboard.spec.js
```

The defaults are `http://127.0.0.1:5173/` and target `mac`. Override the URL
with `REMOTEX_PLAYWRIGHT_BASE_URL` when the dev server uses another address.
The test is skipped with a list of missing variables when its live-Mac
configuration is absent. It always runs headless with one worker.
