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

`batch-envelope.spec.ts` is the v4 binary envelope, read off the SPA's own socket.
It exists because it is the only test that watches the browser link as the browser
actually uses it — the Rust E2E tests drive a raw WebSocket client, and the
TypeScript unit tests parse frames they built themselves, so both ends can
agree with their own fixtures and disagree with each other. Its frame parser is
deliberately a second implementation rather than an import of the SPA's, because a
wrong parser would otherwise agree with itself.

`video-stream.spec.ts` is the desktop's stream read from the same socket. Video is
VP9 only, and the harness desktop is well within the video ceiling, so every record
is VIDEO and everything it asserts is
decidable without asking the browser anything. It parses VIDEO records itself — op, keyframe flags byte, the desktop size the last
`resize` announced — and checks that no access unit outran the `videoFormat` that
says how to decode it.

`egfx-passthrough.spec.ts` is an RDP host's graphics pipeline passed for the page
to compose, read from the same socket: that `graphicsStart` comes ahead of the
first `GRAPHICS` record, that every record is whole commands by their own
headers' lengths, that the page acknowledges each batch once its paint worker has
composed it, and that a page that reloads is given a pipeline from its first
command rather than the one that was running. A compositor that refused a command
says so in the DOM, which is what stands in for the picture here. Against a target
with the EXPERIMENTAL `egfx_h264` key, and a host playing a video, it also asserts
that the page said it decodes H.264, that the session says it carries it, and that
a batch whose commands draw with H.264 is acknowledged: the acknowledgment follows
the decode of every access unit in the batch, and a decoder that gave no picture
says so in the DOM instead.

`egfx-two-displays.spec.ts` is the same pipeline passed over two virtual
displays, read from both displays' sockets of one browser: that each socket
names the column of the composed picture its display is (`graphicsView`) beside
its size, that the second display's tab is sent no picture of its own — no
`videoFormat`, no binary frame, no `graphicsStart` — and shows the canvas it is
painted on from the session page's picture all the same, and that the picker
moving the session's page to the other display moves the view without starting
the pipeline over.

`display-drag.spec.ts` reads the input each display socket sends while a held
drag crosses the edge between two virtual displays. It checks that the edge
towards the display beside it is open and every outer edge still clamps the
pointer, without judging whether a remote window visibly followed the drag.

`software-hevc.spec.ts` is the BETA software HEVC decoder, in a High
Performance session started with the Mac's stream passed and *Decode in this
page* chosen at the picker. Against a gateway that has the decoder's archive it
asserts that the gateway lists the target as offering the decoder, that Start
sends both choices, that the gateway passes the HEVC and says with its format
that this page decodes it, that the decode worker loads the decoder and nothing
asks for it beforehand, and that the first passed keyframe's batch is
acknowledged with no video error or repaint request before it — an ordering, not
a timing, because a failed decoder reports before the paint worker acknowledges.
Against a gateway without the archive it asserts the other decision, in a
browser whose own decoder refuses the Mac's HEVC, which Playwright's Chromium
is: the target offers no such decoder, the session starts without the
passthrough and is sent VP9, and nothing under `/hevc/` is asked for.

`software-vp9.spec.ts` is the BETA software VP9 decoder, the vp9-wasm module in
the bundle, on a gateway whose config sets `[vp9_wasm]`, in a session started
with *Decode VP9 in this page*. It asserts that the gateway lists the target as
offering the decoder, that Start sends the choice, that every format announced
is profile 1 and says this page decodes it, that the decode worker loads the
module's file, once, that the first keyframe's batch is acknowledged with no
video error or repaint request before it, and that the page shows the canvas the
module's planes are drawn on. The page is then reloaded, which sends no
`connect`: it must be told the same of the stream it is repainted with, since
the choice is the session's. A second case makes the browser answer no to
profile 1, as iOS Safari does, by replacing `isConfigSupported` for that one
question: the row is then found ticked, the socket says `chroma=420`, and the
session is 4:4:4 decoded in the module all the same. With the row unticked it
asserts the other decision: the browser's own decoder, the module never fetched,
and that canvas hidden. Against a gateway without the table it asserts that the
picker has no such row.

`soft-keyboard.spec.ts` is the soft keyboard, read from the same socket: that a
key tapped on it is the `key` frames the page sends, down then up; that a tapped
modifier wraps the next key and is spent, and a twice-tapped one is off again;
that with the Sticky key off a modifier is sent alone; and
that a phone — a touch screen of a phone's size, which the test declares — gets the
docked keyboard with its strip, its shortcut row, led on both pages by the
Sticky key, and its Sym page, whose row is the F-keys
and where a shifted symbol is Shift and its key. It asserts frames and accessible state, never repeat or how long a
key is held, which is timing, or where a key is drawn.

`input-held.spec.ts` is a High Performance notice holding input back: that while
the gateway's `resizing` or `screenUnavailable` is true the page sends no `key`
frame, and that a key held when the notice went up is released. The session
socket is passed through the spec, which keeps the gateway's word on both from
the page and says them itself, so when the notice is up is the spec's decision
rather than a Mac's display settling, and it runs against whatever target the
run is configured for. It asserts the order of the frames the page sent and
whether the notice is in the DOM.

`audio-socket.spec.ts` keeps sound on its dedicated `/ws/audio` connection. It
asserts which socket receives the format and packets, that a session started with
sound opens that socket and one started without it does not, and that opening and
closing it is the whole subscription, a mute surviving a reload. The deterministic
tone harness in `src/server.rs` supplies audio without a remote.

`picker-options.spec.ts` is what a session is started with: that Start sends
the choices made under the target and `connected` reports them back, that a
target opens to the size it will have and the options its type offers and the
browser remembers what was chosen, and that a second browser taking the session
over is given none of it: it lands on a picker where a passthrough it cannot take
is greyed. It needs an `rdp` target that configures a `size`, the one type
that offers all three choices, and the tone harness is one.

`clipboard.spec.ts` is the live-Mac regression for the web clipboard panel. It
proves that unsolicited remote copies still auto-sync, while opening and
revealing the panel leave the local clipboard untouched until explicit Copy.

`oversized-clipboard.spec.ts` covers the refusal path: a Mac pasteboard larger
than `MAX_CLIPBOARD_BYTES` reaches the panel as its size, not as the first 512 KiB
of itself. It is here rather than only in the Rust unit tests because
the claim spans macOS Screen Sharing, the gateway, the browser link and the panel,
and the failure it guards against — a truncated value arriving *successfully* —
is invisible to any one of them.

`support.ts` holds what the specs share: the login/target flow and the SSH hooks
that read and write the Mac's pasteboard. A spec names what its session is started
with — resize, sound, the target's passthrough — and the flow sets every option the
target shows at the picker before pressing Start, so a run does not depend on what
an earlier one left remembered in the browser. Two conventions live there. Every spec
hands the session back to the picker in an `afterEach` through `leaveSession`,
because the server keeps a target session running when its browser goes away and a
spec that failed halfway would otherwise leave it there; and `logInAndConnect`
accepts either landing, so a run abandoned on the desktop does not break the next
one.

## Run

Install Chromium once:

```sh
cd tests/playwright
bunx playwright install chromium
```

The runner itself needs no separate step: every `bun run` script here installs the
pinned dependencies first if `node_modules` is missing, which it is in a fresh
clone.

Start the gateway from the repository root. It serves the page the specs open,
the bundle compiled into it, so a frontend change is in a run only after the
gateway is rebuilt:

```sh
cargo run -- serve --config tmp/test_config.toml
```

Then provide the gateway's address, the local test login and the SSH destination
for the Mac target:

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:<port>/' \
REMOTEX_PLAYWRIGHT_USERNAME='<username>' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_TARGET='mac' \
REMOTEX_PLAYWRIGHT_MAC_SSH='<ssh-user>@<mac-host>' \
REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING='<mac-host>:5900' \
bun run test
```

`REMOTEX_PLAYWRIGHT_MAC_SCREEN_SHARING` opts into the specs that need a live Mac
target; without it they skip, so a plain `bun run test` never assumes a VM
is up. The helper checks that the Mac's Screen Sharing service is listening at
that address before starting, rather than failing later inside the browser and
making an unavailable target look like a product bug. This is the same bargain
the Rust e2e tests make with `#[ignore]`.

The video spec needs a local gateway config with a live target. Put that
gitignored config under `tmp/` (for example, `tmp/qa_video.toml`) and name the
target with `REMOTEX_PLAYWRIGHT_VIDEO_TARGET`:

```sh
cargo run -- serve --config tmp/qa_video.toml
```

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_VIDEO_TARGET='video' \
bun run test:video
```

That gateway serves the SPA compiled into its binary, so rebuild the gateway
after a frontend change and restart it; a stale bundle is exactly what these
specs cannot see.

The passthrough spec needs a live RDP host, in a target named by
`REMOTEX_PLAYWRIGHT_EGFX_TARGET`, and starts its sessions with the pipeline
passed:

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_EGFX_TARGET='win' \
bunx playwright test '/egfx-passthrough\.spec\.ts$'
```

Its H.264 case needs a target with `egfx_h264 = true`, named by
`REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET`, while the host is playing a video:

```sh
REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET='win-h264' \
bunx playwright test '/egfx-passthrough\.spec\.ts$'
```

The two-display spec needs the same kind of host, in a target with
`virtual_displays = 2`, named by `REMOTEX_PLAYWRIGHT_EGFX_DISPLAYS_TARGET`:

```sh
REMOTEX_PLAYWRIGHT_EGFX_DISPLAYS_TARGET='win2' \
bunx playwright test '/egfx-two-displays\.spec\.ts$'
```

The cross-display drag spec accepts either an RDP or High Performance target
with two virtual displays. Name it with `REMOTEX_PLAYWRIGHT_DRAG_TARGET`:

```sh
REMOTEX_PLAYWRIGHT_DRAG_TARGET='win2' \
bunx playwright test '/display-drag\.spec\.ts$'
```

The software HEVC spec needs a gateway whose config has an
`ard-high-performance` target, with the pinned
release archive beside the config (where a gateway run from a Cargo build looks
for it), and names that target with
`REMOTEX_PLAYWRIGHT_HEVC_TARGET`:

```sh
gh release download v0.0.6 --repo andrewtheguy/hevc-wasm-archives \
  --pattern hevc-wasm-v0.0.6.tar.gz --dir tmp
cargo run --profile qa -- serve --config tmp/qa_hevc.toml
```

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_HEVC_TARGET='macvmhevc' \
bun run test:hevc
```

Against a gateway without the archive, add `REMOTEX_PLAYWRIGHT_HEVC_WASM=0`,
which runs the test that the target offers no such decoder instead.

The software VP9 spec needs a gateway whose config sets `[vp9_wasm] enabled =
true` and has a live target that leaves `render_chroma` unset, named by
`REMOTEX_PLAYWRIGHT_VP9_TARGET`:

```sh
cargo run --profile qa -- serve --config tmp/qa_vp9.toml
```

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52893/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_VP9_TARGET='desktop' \
bun run test:vp9
```

Against a gateway without the table, add `REMOTEX_PLAYWRIGHT_VP9_WASM=0`, which
runs the test that the picker has no such row instead.

The audio and picker specs use the test-tone gateway instead of a live target:

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
REMOTEX_PLAYWRIGHT_PICKER_TARGET='test-tone' \
bunx playwright test '/(audio-socket|picker-options)\.spec\.ts$'
```

The notice spec runs against the same gateway, as the run's target:

```sh
REMOTEX_PLAYWRIGHT_TARGET='test-tone' \
bunx playwright test '/input-held\.spec\.ts$'
```

The picker spec also runs against a live RDP host, named the same way.

`bun run test` runs all specs. `bun run test:clipboard`,
`bun run test:oversized`, `bun run test:video` and `bun run test:hevc` run one
each; their filters are anchored (`'/clipboard\.spec\.ts$'`) because a
positional argument is a regex matched against the whole path, and the bare name `clipboard.spec.ts` also matches
`oversized-clipboard.spec.ts`.

The specs are TypeScript, which Playwright transpiles itself — and transpiling is
all it does, so a type error would otherwise never surface. `bun run typecheck`
is what actually checks them:

```sh
cd tests/playwright
bun run typecheck
```

The defaults are `http://127.0.0.1:52380/`, the gateway's built-in port, and
target `mac`. Override the URL with `REMOTEX_PLAYWRIGHT_BASE_URL` when the
gateway's config names another port.
Live-target groups are skipped with a message naming their missing opt-in when
their configuration is absent. The specs always run headless with one worker,
and share the single session slot, which is why they are sequential by
configuration rather than by luck.
