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

`tiles.spec.ts` is the picture the gateway codes, read from the same socket, on a
plain `vnc` target or an `rdp` one with `egfx = false`: the harness desktop is
well within the ceiling, so every record is TILE and everything it asserts is
decidable without asking the browser anything. It parses TILE records itself — op,
format byte, the image's own signature, a rectangle on the 64-pixel grid inside the
desktop the last `resize` announced — and checks that no tile outran the
`tilesStart` that begins the picture, that the whole desktop is covered, that no
`videoFormat` is sent, and that the page acknowledges the batches.

`egfx-passthrough.spec.ts` is an RDP host's graphics pipeline passed for the page
to compose, read from the same socket: that `graphicsStart` comes ahead of the
first `GRAPHICS` record, that every record is whole commands by their own
headers' lengths, that the page acknowledges each batch once its paint worker has
composed it, that a page that reloads is given a pipeline from its first
command rather than the one that was running, and that a session's end ends the
paint worker that loaded the compositor, the next session composing in a paint
worker of its own, which Playwright reports starting and closing. A compositor
whose memory the paint worker could not make says the page ran out of memory and
offers a reload, which comes back to the session composing, and a page whose
session another browser takes over ends its paint worker. A compositor that refused a command
says so in the DOM, which is what stands in for the picture here. Started with
the picker's Adaptive (ALPHA), H.264 over the full screen, it also asserts
that the page said it decodes H.264, that the session says it carries it, and that
a batch whose commands draw with H.264 is acknowledged: the acknowledgment follows
the decode of every access unit in the batch, and a decoder that gave no picture
says so in the DOM instead.

`standard-passthrough.spec.ts` is a Standard Mac's Adaptive picture passed for
the page to decode, read from the same socket: that `standardStart` follows the
`resize` and comes ahead of the first `STANDARD` record, that the Mac's
quantization tables come first, that every record is framed by its own header
and lies on the framebuffer the last `resize` announced, that the page
acknowledges each batch once its paint worker has decoded it, that a page
that comes back is given the picture from its start, and that a session's end
ends the paint worker that loaded the decoder, the next session decoding in one
of its own. A rectangle that did not
decode says so in the DOM. A session started with the Mac's Full-quality
picture instead is read the same way, with its `standardStart` naming `full`,
and every record's body inflated in the spec on one zlib stream for the session,
which must come to each rectangle's pixels. Against a Mac on a virtual display
it also resizes the window, in each quality, and checks that the picture goes on
at the new size, decoded, without starting again. Against a Mac of a 1x and a 2x
screen it checks that a session of one screen at a time lists the screens alone,
starts on one, never composes, and goes on decoding across a switch to the other
and back.

`standard-combined.spec.ts` is a Standard Mac's Combined Display, the gateway's
tiles of the Mac's Full quality: that the session renders as Combined Display
tiles, passes nothing, sends `tilesStart` and no display list, is never held, and
that every TILE record lies on the grid of the last `resize` and is painted. At
a 1x browser the desktop is one plain composition with no `mosaic`, every tile
of it sent, gaps included; at a 2x one it is the Mac's own pixels behind a
two-region `mosaic`, at 2x, held only past the ceiling's area in the screens'
pixels.

`target-switch.spec.ts` switches one page, without a reload, between an RDP
host's pipeline and bitmap targets and a Mac's Adaptive and High Performance
pictures, with the browser's own decoder made to refuse the HEVC: that each
session's picture starts and is acknowledged, the bitmap target's as tiles,
that a session that loaded the compositor or the decoder of a Mac's own picture
ends its paint worker and any other keeps it, and, by how often the HEVC
module's file is fetched, that a kept worker ends its HEVC decode worker.

`egfx-two-displays.spec.ts` is the same pipeline passed over two virtual
displays, read from both displays' sockets of one browser: that each socket
names the column of the composed picture its display is (`graphicsView`) beside
its size, that the second display's tab is sent no picture of its own — no
`videoFormat`, no binary frame, no `graphicsStart` — and shows the canvas it is
painted on from the session page's picture all the same, and that the picker
moving the session's page to the other display moves the view without starting
the pipeline over.

`display-drag.spec.ts` reads the input each display socket sends while a held
drag crosses the edge between two virtual displays. It checks that a page
showing one display holds the pointer at every edge, and that while a display
is shown beside, the page sends where the pointer is, past any edge, for the
gateway's engine to hold on a display (unit tested in `src/engine.rs`), without
judging whether a remote window visibly followed the drag.

`software-hevc-chrome.spec.ts`, `software-hevc-firefox.spec.ts` and
`software-hevc-faked.spec.ts` are the software HEVC decoder, which a page
falls back to by itself where the browser's own decoder refuses a High
Performance Mac's HEVC. Whether it refuses is the machine's to say: Chrome takes
the HEVC on a Mac and refuses it on a PC whose GPU does not decode HEVC Range
Extensions 4:4:4. So all three run one test (`softwareHevc.ts`), which asks the
browser itself, apart from the page, and asserts the decision that answer calls
for:

- The browser's own decoder takes it: the session socket says so, the passed
  HEVC is decoded there, and nothing under `/hevc/` is asked for.
- It does not, on a gateway with the decoder's archive: the target is listed as
  offering the decoder, the page states it runs it, the HEVC is sent, and
  the decode worker loads the decoder, with nothing asking for it beforehand.
- It does not, on a gateway without: the target offers no such decoder, the
  session starts with the Mac's H.264 passed instead, and nothing under
  `/hevc/` is asked for.

A passed stream's first keyframe batch must be acknowledged with no video error
or repaint request before it — an ordering, not a timing, because a failed
decoder reports before the paint worker acknowledges. Each test notes in the
report which decoder it saw (`decoder`). The Chrome spec runs the installed
Google Chrome (`channel: "chrome"`), since Playwright's bundled Chromium has no
HEVC decoder at all, and the Firefox spec Playwright's Firefox, whose own decoder
refuses the HEVC, so a Mac sees both sides of the fallback. The faked spec sees
both on any machine, in the default browser: it fakes the browser's answer to the
page's one question, once "no", after which everything is real, and once "yes",
with a stand-in for the browser's decoder put ahead of the paint worker's script,
which outputs a grey picture for each unit, so a browser with no HEVC decoder
passes it too. Both fakes are `ownDecoder.ts`'s.

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
over is given none of it: it lands on a picker where a target whose pipeline it
cannot compose has Start disabled, with the reason, and no tiles in its place. It
also checks that the pipeline is said to be always passed, with no box to tick,
and that the Graphics
group opens on High quality and, under Adaptive, on H.264 full screen. Its sound
checks are a browser with no audio decoder, which is offered FLAC alone and plays
it, and an iPhone, which loads as little WebAssembly as it can: FLAC is greyed
with the reason, Start sends Opus, and the FLAC decoder's module is never
fetched. It needs an `rdp` target that configures a `size`, the one type that
offers all three choices, and the tone harness is one.

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
with — resize, sound, the target's passthrough, how the host draws its
pipeline — and the flow sets every option the
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

The tiles spec needs a local gateway config with a live plain `vnc` target or an
`rdp` one with `egfx = false`. Put that gitignored config under `tmp/` (for
example, `tmp/qa_tiles.toml`) and name the target with
`REMOTEX_PLAYWRIGHT_TILES_TARGET`:

```sh
cargo run -- serve --config tmp/qa_tiles.toml
```

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_TILES_TARGET='bitmap' \
bun run test:tiles
```

That gateway serves the SPA compiled into its binary, so rebuild the gateway
after a frontend change and restart it; a stale bundle is exactly what these
specs cannot see.

The passthrough spec needs a live RDP host, in a target with the pipeline on
named by `REMOTEX_PLAYWRIGHT_EGFX_TARGET`, whose sessions pass it unasked:

```sh
cd tests/playwright
REMOTEX_PLAYWRIGHT_BASE_URL='http://127.0.0.1:52889/' \
REMOTEX_PLAYWRIGHT_USERNAME='admin' \
REMOTEX_PLAYWRIGHT_PASSWORD='<password>' \
REMOTEX_PLAYWRIGHT_EGFX_TARGET='win' \
bunx playwright test '/egfx-passthrough\.spec\.ts$'
```

Its H.264 case, ALPHA, starts a target with the pipeline on, named by
`REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET`, with the picker's Adaptive and H.264 over
the full screen, which the host draws everything in; by region it would need a
video playing:

```sh
REMOTEX_PLAYWRIGHT_EGFX_H264_TARGET='win-h264' \
bunx playwright test '/egfx-passthrough\.spec\.ts$'
```

The Standard passthrough spec needs a live Mac in an `ard` target, named by
`REMOTEX_PLAYWRIGHT_STANDARD_TARGET`. Its resize case needs one whose
desktop follows the window, an `ard` target with `virtual_display = true`, named
by `REMOTEX_PLAYWRIGHT_STANDARD_RESIZE_TARGET`:

```sh
REMOTEX_PLAYWRIGHT_STANDARD_TARGET='mac' \
REMOTEX_PLAYWRIGHT_STANDARD_RESIZE_TARGET='mac-virtual' \
bunx playwright test '/standard-passthrough\.spec\.ts$'
```

Its two-screen case, and the Combined Display spec, need an `ard` target on a Mac
with a 1x screen beside a 2x one, named by
`REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET`:

```sh
REMOTEX_PLAYWRIGHT_STANDARD_COMBINED_TARGET='mac' \
bunx playwright test '/standard-(passthrough|combined)\.spec\.ts$'
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

The software HEVC specs need Google Chrome installed, Playwright's Firefox
(`bunx playwright install firefox`), and a gateway whose config has an
`ard-high-performance` target, with the pinned
release archive beside the config (where a gateway run from a Cargo build looks
for it), and names that target with
`REMOTEX_PLAYWRIGHT_HEVC_TARGET`:

```sh
gh release download v0.0.17 --repo andrewtheguy/hevc-wasm-archives \
  --pattern hevc-wasm-v0.0.17.tar.gz --dir tmp
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

Against a gateway without the archive, add `REMOTEX_PLAYWRIGHT_HEVC_WASM=0`.

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
