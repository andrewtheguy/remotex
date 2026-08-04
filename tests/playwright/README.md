# Stable headless browser tests

One rule decides what belongs here: **assert on what the system decides, never on
what a machine's timing decides.**

In scope, because their values are deterministic — DOM state and accessible roles,
control-plane JSON, HTTP responses, and WebSocket frame bytes (header fields,
record counts, payload lengths, ordering). `framereceived` qualifies because it is
a transport event carrying a fixed payload.

Out of scope, because their values are a race — canvas pixels, whether or how many
paints happened, frame rate or latency, cursor rendering, synthetic pointer input
and gestures, and screenshot comparison. The Rust protocol and container E2E tests
cover those; they control their own clock.

The quieter flaky shapes are worth naming too, because they pass locally and fail
in a year: fixed sleeps, CSS or nth-child selectors, assertions on transient states
a fast machine skips through, and counts taken over a wall-clock window. Where a
count is the point, assert a relationship that holds for any sample — `records >
frames` — not a number that depends on how long the run happened to watch.

`batch-envelope.spec.ts` is the v3 binary envelope, read off the SPA's own socket.
It exists because it is the only test that watches the browser link as the browser
actually uses it — the Rust E2E tests drive a raw WebSocket client, and the
TypeScript unit tests parse frames they built themselves, so both ends can
agree with their own fixtures and disagree with each other. Its frame parser is
deliberately a second implementation rather than an import of the SPA's, because a
wrong parser would otherwise agree with itself.

`video-stream.spec.ts` is the video dial read from the same socket, and it exists
because the codec is now a **config key** rather than a negotiation: what a target
streams is decided in the operator's file, so both ends of that decision are
assertable without asking the browser anything. It parses VIDEO records itself —
op, keyframe flags byte, the coded rectangle's even sides — checks that no stream's
first access unit outran the `videoFormat` that says how to decode it, and reads the
codec back off the session card's Video row. The one browser-dependent claim, whether
this runtime decodes the H.264 a target may be configured for, is put to the runtime
with `VideoDecoder.isConfigSupported` and then asserted both ways: a decoder that
refuses must produce the banner naming the codec, and one that accepts must produce no
banner at all.

`audio-socket.spec.ts` keeps sound on its dedicated `/ws/audio` connection. It
asserts which socket receives the format and packets, and that opening and closing
that socket is the whole subscription. The deterministic tone harness in
`src/server.rs` supplies audio without a remote.

`clipboard.spec.ts` is the live-Mac regression for the web clipboard panel. It
proves that unsolicited remote copies still auto-sync, while opening and
revealing the panel leave the local clipboard untouched until explicit Copy.

`oversized-clipboard.spec.ts` covers the refusal path: a Mac pasteboard larger
than `MAX_CLIPBOARD_BYTES` reaches the panel as its size, not as the first 64 KiB
of itself. It is here rather than only in the Rust unit tests because
the claim spans macOS Screen Sharing, the gateway, the browser link and the panel,
and the failure it guards against — a truncated value arriving *successfully* —
is invisible to any one of them.

`support.ts` holds what the specs share: the login/target flow and the SSH hooks
that read and write the Mac's pasteboard. Two conventions live there. Every spec
ends by handing the session back to the picker, because the server keeps a target
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
REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING='<mac-host>:5900' \
npx playwright test
```

`REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING` opts into the specs that need a live Mac
target; without it they skip, so a plain `npx playwright test` never assumes a VM
is up. The helper checks that the Mac's Screen Sharing service is listening at
that address before starting, rather than failing later inside the browser and
making an unavailable target look like a product bug. This is the same bargain
the Rust e2e tests make with `#[ignore]`.

The video spec needs a local gateway config with video targets. Put that
gitignored config under `tmp/` (for example, `tmp/qa_video.toml`) and name the
targets through one variable per codec, so a run can cover the VP9 default alone
or both codecs:

```sh
cargo run -- serve --config tmp/qa_video.toml
```

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_VIDEO_TARGET='video' \
REMOTEX_PLAYWRIGHT_VIDEO_H264_TARGET='video-h264' \
npm run test:video
```

That gateway serves the built SPA from `frontend/dist`, so run `bun run build` in
`frontend/` first; a stale bundle is exactly what these specs cannot see.

The audio spec uses the test-tone gateway instead of a live target:

```sh
cargo test --lib serve_a_test_tone -- --ignored --nocapture
```

Against the URL it prints, run:

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:<port>/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='hunter2' \
REMOTEX_PLAYWRIGHT_AUDIO_TARGET='test-tone' \
npx playwright test '/audio-socket\.spec\.ts$'
```

A bare `npx playwright test` runs all specs. `npm run test:clipboard`,
`npm run test:oversized` and `npm run test:video` run one each; their filters are
anchored (`'/clipboard\.spec\.ts$'`) because a positional argument is a regex matched
against the whole path, and the bare name `clipboard.spec.ts` also matches
`oversized-clipboard.spec.ts`.

The specs are TypeScript, which Playwright transpiles itself — and transpiling is
all it does, so a type error would otherwise never surface. `npm run typecheck`
is what actually checks them:

```sh
cd tests/playwright
npm run typecheck
```

The defaults are `http://127.0.0.1:5173/` and target `mac`. Override the URL
with `REMOTEX_PLAYWRIGHT_BASE_URL` when the dev server uses another address.
Each test is skipped with a list of missing variables when its live-Mac
configuration is absent. They always run headless with one worker, and share the
single session slot, which is why they are sequential by configuration rather
than by luck.
