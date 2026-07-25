# Plan — `rxa`: a purpose-built macOS agent for remotex

Status: **implemented.** Kept as the design record — the reasoning behind the
decisions, and the risks that were live at the time. For the system as built,
see [`architecture.md`](architecture.md#rxa-srcrxars) and
[`packaging/macos/README.md`](../packaging/macos/README.md).

## What changed on contact with the machine

The plan survived largely intact. Five things went differently, and they are
worth recording because each was a real discovery rather than a preference:

1. **Packaging (§3.7).** The plan called for an install script laying a plist
   into `~/Library/LaunchAgents` and running `launchctl`. `SMAppService` does
   it better: the bundle carries its own LaunchAgent plist, registers itself on
   first launch, and appears in System Settings → General → Login Items.
   Installing is dragging the app in and opening it once; there is no install
   script. This raised the floor to macOS 14.
2. **The Swift bridge bites (§3.1).** `screencapturekit` was the right pick —
   it exposes `dirtyRects`, which the plan named as the single biggest risk —
   but its bridge is built with `swift build --triple arm64-apple-macosx`, no
   OS version. Swift then links the *back-deployment* concurrency runtime as
   `@rpath/libswift_Concurrency.dylib` with no matching runpath, so the agent
   builds clean and dies at startup. Fixed at the root by setting
   `MACOSX_DEPLOYMENT_TARGET` (`.cargo/config.toml`), not by adding a runpath.
3. **`SCFrameStatus::Started` carries pixels.** Accepting only `Complete`
   meant a static screen produced *no tiles at all* — `Started` is the stream's
   first frame and, on a screen that then sits still, the only content-bearing
   one that ever arrives.
4. **Cursor representations lie about scale (§3.4).** Taking the largest
   `NSBitmapImageRep` is wrong: a system cursor can carry a vector-backed rep
   at an arbitrary resolution. The I-beam reports a 14×20 point size with a
   280×400 rep available, so "largest" produced a cursor 20× oversized with its
   hotspot scaled to match. The rep closest to *point size × backing scale* is
   the right one.
5. **`clamp_u16` moved too (§4.5).** The plan said to extract only
   `host_port`. The rxa engine needs coordinate clamping as well, so both live
   in `src/engine.rs` — still not a `trait Engine`, and the duplication did not
   grow.

One thing the plan **omitted entirely**: a menu bar item. The plan specified a
headless background agent, and that is exactly what shipped — with the result
that nothing on the Mac indicated the agent was running, nothing indicated when
somebody was watching the screen, and stopping it meant `--unregister` plus
`pkill` from a terminal. For software whose whole job is to let a remote machine
see and drive this one, an invisible process with no off switch is the wrong
default. `menubar.rs` adds the status item; it also forced the LaunchAgent's
`KeepAlive` from `true` to `SuccessfulExit: false`, since under a plain `true`
launchd resurrects the agent seconds after Quit and the menu item is a lie.

Two items from §3.6 were **not** implemented, deliberately: the post-disconnect
stream linger (it only pays for outages shorter than the gateway's own 1 s
minimum backoff, and restarting the stream costs about as much — noted in
`session.rs`), and a multi-threaded encoder pool (a pool lets two frames' tiles
finish out of order, and the same region is commonly dirty in consecutive
frames, so an older tile could land on top of a newer one).

## Context

remotex reaches a Mac today over **macOS Screen Sharing** (Apple's built-in VNC
server), via the built-in RFB client in `src/vnc.rs`. That has been an ongoing
pain point:

- It is flaky — the session drops and does not come back cleanly.
- A disconnect forces you to **log in again**. The credential prompt is a
  property of Apple's server, not of remotex, so there is nothing to fix on our
  side of the RFB connection.
- macOS Screen Sharing ignores RFB `SetDesktopSize`, which is why
  `tmp/remotex-old/tools/displaymode.swift` had to exist.
- It never composites a cursor into the framebuffer, which is the whole reason
  the Cursor pseudo-encoding path (`-239`) exists in `src/vnc.rs`.

RealVNC is stable and a disconnect does *not* force a re-login, but its free
tier has no LAN direct-connect, so it is not an option.

**The fix:** stop speaking VNC to the Mac. Write our own macOS agent with a
protocol designed for this one client, and have remotex dial it with a
pre-shared key. Because the PSK lives in remotex's config file, a reconnect is
a sub-millisecond cryptographic handshake with **no interactive login, ever** —
which is precisely the RealVNC property that is missing today.

**Intended outcome:** picking the Mac target in the picker gives a session that
survives Wi-Fi blips silently, never prompts for a password, paints faster than
raw-encoding VNC, and shows a pointer.

## Decisions

These were settled before planning and are not open questions:

| Decision | Choice |
|---|---|
| Where the agent lives | A cargo **workspace** inside this repo (not a sibling repo), so the wire protocol cannot drift between the two sides |
| Pixel path | **Pass-through tiles**, adaptive PNG/JPEG per tile; the gateway relays encoded bytes with zero decode/re-encode |
| Transport | TCP + `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` via `snow` |
| v1 scope | Screen + keyboard/mouse + **cursor shapes**. Out: dynamic/display-mode resize, clipboard, audio |

Naming used throughout: the protocol is **`rxa`** (remote**X** **a**gent);
`protocol = "rxa"` in a `[[targets]]` profile.

---

## 1. Workspace conversion

Today the repo is a single crate. It becomes:

```
remotex/
  Cargo.toml            [package] remotex  +  [workspace]
  Cargo.lock            single lock at the root
  src/                  the gateway (unchanged, plus src/rxa.rs)
  crates/
    rxa-proto/          cross-platform: wire types, framing, Noise, PSK, keymap
    rxa-agent/          macOS-only binary
  frontend/
```

Root `Cargo.toml` keeps its existing `[package]` and gains:

```toml
[workspace]
members      = [".", "crates/rxa-proto", "crates/rxa-agent"]
default-members = [".", "crates/rxa-proto"]
```

`default-members` is load-bearing: it keeps bare `cargo build`,
`cargo clippy -- -D warnings` and `cargo test` on Linux behaving **exactly as
today**, never attempting to compile the macOS crate. On the Mac you build the
agent explicitly with `-p rxa-agent`.

Things that read `Cargo.toml` — all verified safe, with one caveat:

| Site | Reads | Safe? |
|---|---|---|
| `.github/workflows/release.yml:31` | `tomllib.load(f)["package"]["version"]` | Yes — keyed on `[package]` |
| `packaging/build-tarball.sh` | same tomllib expression | Yes |
| `frontend/vite.config.ts` | regex `/^version\s*=\s*"([^"]+)"/m` | **Caveat** — it takes the *first* line starting with `version =`. Do **not** add a `[workspace.package] version = …` inheritance block, or add it strictly below `[package]`. Simplest: give each member crate its own explicit `version` and add no second top-level `version` key. |

`[profile.release]` (`strip`, `lto = "thin"`, `codegen-units = 1`,
`panic = "abort"`) already sits in the root manifest, which is also the
workspace root, so it keeps applying to every member.

`packaging/build-tarball.sh` needs a Darwin branch: after `cargo build
--release`, also `cargo build --release -p rxa-agent`, and stage
`bin/remotex-agent` plus the LaunchAgent plist and config example. The linux
tarball and `packaging/Dockerfile` are untouched.

---

## 2. `crates/rxa-proto` — the shared crate

Pure Rust, no platform dependencies, builds and unit-tests on Linux. This is
where everything both sides must agree on lives, so it cannot drift.

```
crates/rxa-proto/src/
  lib.rs        version constant, prologue
  psk.rs        PSK parse / generate
  noise.rs      handshake helpers (initiator + responder)
  frame.rs      length-prefixed framing over the Noise transport
  msg.rs        AgentMsg / GatewayMsg
  keymap.rs     DOM KeyboardEvent.code -> macOS CGKeyCode
```

### 2.1 PSK (`psk.rs`)

Reuses the house format found in `flextunnel/crates/flextunnel-core/src/auth.rs`
and `ezvpn/src/auth.rs`:

```
rxa<base64url-no-pad( 32 random bytes ‖ CRC16-CCITT-FALSE(those 32 bytes), BE )>
```

49 characters: a 3-char `rxa` prefix + 46 base64url chars. The CRC catches
transcription typos before they become an opaque handshake failure. `psk.rs`
exposes `generate() -> String` and `parse(&str) -> Result<[u8; 32]>`.

### 2.2 Handshake (`noise.rs`)

`Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s`, two messages:

```
gateway ──▶ e, psk          (ephemeral pubkey, PSK mixed into the chaining key)
agent   ──▶ e, ee           (ephemeral, DH)
── both sides now hold a forward-secret AEAD session ──
```

The PSK alone provides mutual authentication: neither side can complete the
handshake without it. There are no certificates, no CA, no pinning, nothing
that expires. The Noise **prologue is `b"rxa/1"`**, which binds the protocol
version into the handshake transcript so a version mismatch fails cleanly at
the handshake rather than producing confused garbage later.

Each Noise message is written to the socket as `u16 BE length + bytes`.

### 2.3 Framing (`frame.rs`)

A Noise transport message caps at 65535 bytes on the wire (65519 plaintext), and
a full-screen JPEG keyframe is much larger than that. Rather than inventing a
chunking flag, treat the Noise transport as a **reliable byte stream**: a
`NoiseStream` adapter that encrypts on write and decrypts on read, splitting
into ≤65519-byte Noise messages transparently. Application framing then sits on
top as plain `u32 LE length + u8 msg_type + body`, exactly as if it were a TCP
stream.

This keeps chunking out of the message definitions entirely and makes `frame.rs`
unit-testable on Linux against an in-memory duplex pipe.

### 2.4 Messages (`msg.rs`)

**Agent → gateway**

| Message | Payload | Notes |
|---|---|---|
| `Hello` | `version: u16`, `agent_version: String`, `w: u16`, `h: u16` | First message after the handshake |
| `Tile` | `format: u8`, `x, y, w, h: u16`, `data: Vec<u8>` | Payload is **already** a PNG or JPEG stream — the exact bytes the browser will decode |
| `Cursor` | `Option<{ w, h, hx, hy, png }>` | `None` = pointer hidden |
| `DisplaySize` | `w, h: u16` | Display reconfigured on the Mac |
| `Pong` | `nonce: u64` | |
| `Error` | `message: String` | e.g. Screen Recording permission not granted |

**Gateway → agent**

| Message | Payload | Notes |
|---|---|---|
| `Attach` | — | Start the capture stream; agent replies with a full keyframe |
| `Refresh` | — | Full repaint (mapped from `ClientMsg::Refresh`) |
| `PointerMove` | `x, y: u16` | Framebuffer pixel coordinates |
| `PointerButton` | `button: u8`, `pressed: bool` | |
| `Wheel` | `dx, dy: f32` | Raw DOM deltas, as remotex already carries them |
| `Key` | `code: String`, `pressed: bool`, `caps: bool` | DOM `KeyboardEvent.code`, unchanged from `ClientMsg::Key` |
| `Ping` | `nonce: u64` | |

Serialization: `serde` + a compact binary codec, **or** hand-rolled
little-endian encode/decode in the style of `src/vnc.rs`. Hand-rolling avoids a
new dependency and matches the house style; the message set is small enough
that it is maybe 200 lines with tests. Decide at implementation time; either
way it is confined to `msg.rs` and covered by roundtrip tests.

### 2.5 Keymap (`keymap.rs`)

`mac_keycode(code: &str) -> Option<u16>` — DOM `KeyboardEvent.code` → macOS
virtual keycode (`kVK_*`), a US-layout `match` table in exactly the shape of the
existing `keymap::scancode()` and `keymap::keysym()` in `src/keymap.rs`.

It lives in `rxa-proto`, not in the agent crate, **specifically so it is unit
tested on Linux** — the agent crate never compiles in local dev or on a Linux
CI runner, so anything testable must live outside it.

---

## 3. `crates/rxa-agent` — the macOS agent

macOS-only binary, `remotex-agent`. Excluded from `default-members`.

### 3.1 Capture

ScreenCaptureKit (`SCStream`), macOS 12.3+. Two viable binding crates:

- **`objc2-screen-capture-kit`** (0.3.2, objc2 family) — pure Rust bindings, no
  extra toolchain. Requires implementing the `SCStreamOutput` delegate via
  objc2's `define_class!`, and reaching into the `CMSampleBuffer` attachments
  dictionary by hand.
- **`screencapturekit`** (8.0.1, doom-fish) — higher level, exposes
  `frame_info()` for `SCStreamFrameInfo` directly, but reportedly builds through
  a **custom Swift FFI bridge** rather than objc2. Verify this on the Mac before
  committing: a Swift bridge means the build needs a Swift toolchain, which is
  fine on `macos-latest` CI but is a real constraint for anyone building from a
  checkout.

**Recommendation:** try `screencapturekit` 8.x first for the shorter path to a
working capture loop; fall back to `objc2-screen-capture-kit` if the Swift
bridge causes build friction. The seam between "get dirty rects + BGRA pixels"
and everything downstream should be a single trait/function so swapping
bindings is a contained change.

`SCStreamConfiguration`: BGRA pixel format, `showsCursor = false` (we send
shapes separately), `minimumFrameInterval` capped around 30 fps, small
`queueDepth`, `capturesAudio = false`.

**Dirty rects are load-bearing.** ScreenCaptureKit hands them over in the
`CMSampleBuffer` attachments (`SCStreamFrameInfo` → `dirtyRects`, plus
`contentRect`, `scaleFactor`, and a frame `status` where *idle* means no new
IOSurface). Only dirty regions get encoded and sent. Verify the exact accessor
on the Mac — this is the single most important API detail in the whole plan.

Two bugs to pre-empt:

- **Stride.** `CVPixelBuffer` `bytesPerRow` is **not** `width * 4`. Read row by
  row using the reported stride. This is the classic ScreenCaptureKit bug.
- **Backing scale.** A Retina display captures at pixel dimensions that differ
  from the point dimensions `CGEventPost` wants. Keep both and convert at the
  input boundary (§3.3).

The delegate callback arrives on an arbitrary dispatch queue. It should do the
minimum — copy/reference the surface, hand it to the encoder side over a
channel — and never block.

### 3.2 Encoding

Each dirty rect becomes one or more tiles in remotex's existing binary layout.
Keep the existing `STRIP_ROWS = 64` split so a full-screen repaint doesn't
produce one enormous message and the browser can start painting early;
revisit only if measurement says otherwise.

Per tile, choose a codec:

- **PNG** (`png` 0.18, `Compression::Fast` — same settings the gateway uses
  today) for flat/text content, where lossless is both smaller and sharper.
- **JPEG** (`jpeg-encoder` 0.7 with its `simd` feature — pure Rust, no C
  dependency, actively maintained) for photographic content.

The classifier must be cheap enough to run on every tile. A sampled unique-colour
count over a strided subset of pixels is the usual approach: few distinct
colours ⇒ UI/text ⇒ PNG; many ⇒ photo/video ⇒ JPEG. Tune the threshold and the
JPEG quality against a real desktop; start around quality 80.

Set `format = 1` (PNG) or `format = 2` (JPEG) in the tile header. The gateway
never looks inside the payload.

Encoding a Retina desktop is the CPU hot spot. Two mitigations: run encoding on
a small worker pool off the capture callback, and — when the link is slow —
**coalesce dirty rects rather than queueing them**. Falling behind should
degrade into a coarser, later repaint, never an unbounded backlog.

### 3.3 Input injection

`CGEventCreateKeyboardEvent` / `CGEventCreateMouseEvent` /
`CGEventCreateScrollWheelEvent` + `CGEventPost`, via `objc2-core-graphics`.

- **Keys** — `rxa_proto::keymap::mac_keycode(code)`, plus `CGEventFlags` tracked
  from the modifier keys the browser reports. remotex sends `caps` as an
  authoritative flag on every `Key` message (see `ClientMsg::Key` in
  `src/protocol.rs:49`), so the agent never has to infer lock state.
- **Pointer** — remotex sends **framebuffer pixel** coordinates; `CGEventPost`
  wants **global display points**. Divide by the capture's backing scale factor
  and offset by the display's origin. Getting this wrong is the most likely
  "clicks land in the wrong place" bug.
- **Wheel** — remotex forwards raw DOM `deltaX`/`deltaY`. Convert to
  `CGEventCreateScrollWheelEvent` line/pixel units; note the RDP engine already
  found DOM's sign convention to be inverted relative to the native one
  (`src/rdp.rs:382`), so verify the direction empirically.

### 3.4 Cursor shapes

Capture with `showsCursor = false` so the pointer is not composited, read the
current cursor image (`NSCursor.currentSystem` / `CGSCurrentCursor`) and its
hotspot, and send it as a `Cursor` message when it changes. The gateway forwards
this straight into the existing `ServerMsg::Cursor` path, and the frontend's
`paintCursor` — already built for VNC against macOS Screen Sharing — draws it
with no changes at all.

Cache the latest shape and resend it on `Attach`/`Refresh`; a client attaching
later would otherwise have no pointer until the shape happened to change.

### 3.5 Permissions (TCC) — read this before writing any code

Two separate grants are required, and both are user-visible one-time prompts in
System Settings → Privacy & Security:

- **Screen Recording** — for `SCStream`.
- **Accessibility** — for `CGEventPost`.

Three consequences that shape the design:

1. **The agent must run in the user's GUI (Aqua) session** — a **LaunchAgent**,
   not a LaunchDaemon. A LaunchDaemon has no window server connection and both
   capture and event injection fail.
2. Therefore the agent is **not running at the login window**, and cannot be. If
   nobody is logged in on the Mac, there is nothing to connect to. This is an
   honest limitation of the design and should be documented, not papered over.
3. **TCC grants are keyed to the binary's identity.** An unsigned binary's grant
   can be invalidated when the binary is rebuilt or replaced, forcing a
   re-approval. Ad-hoc code signing (`codesign -s -`) with a stable bundle
   identifier, ideally shipping the agent as a small `.app` bundle, avoids
   re-approving on every upgrade. Budget time for this — it is the sort of thing
   that silently eats an afternoon.

Detect a missing grant at startup and on stream failure, and report it as an
`Error` message so the gateway surfaces it in the browser rather than failing
silently.

### 3.6 Stability and reconnect — the actual point of the project

- The agent is a long-lived LaunchAgent. Gateway disconnects do not end it.
- A reconnect is: TCP connect → two-message Noise handshake → `Attach` → full
  keyframe. On a LAN that is a couple of milliseconds and **no user
  interaction**. This is the whole reason for the project, and the reason there
  is deliberately **no session-resume state** to maintain — resume is
  unnecessary when the handshake is this cheap, and the framebuffer is just
  whatever the Mac's screen shows right now.
- The `SCStream` starts on `Attach` and stops on disconnect (battery/CPU), with
  a short linger — around 5 seconds — so a network blip doesn't cause a
  teardown/restart cycle.
- Application-level `Ping`/`Pong` on an idle timer detects a half-open TCP
  connection that `SO_KEEPALIVE` would take far too long to notice.
- If `SCStream` fails mid-session (`didStopWithError`, display reconfiguration),
  restart the stream and send a fresh keyframe; report an `Error` only if
  restart fails repeatedly.

### 3.7 Agent config and packaging

TOML, matching the house style of `packaging/etc/remotex.toml.example`:

```toml
# ~/Library/Application Support/remotex-agent/config.toml
listen = "0.0.0.0:52381"
psk    = "rxa..."          # must match the gateway target's psk
# display = 0              # main display by default
```

Ship: the `remotex-agent` binary, a LaunchAgent plist
(`~/Library/LaunchAgents/dev.remotex.agent.plist`, `RunAtLoad` + `KeepAlive`),
a config example, and an install script that lays them down, loads the agent,
and tells the user which two permissions to grant.

CI already builds a `macos-arm64` tarball on `macos-latest`; extend
`packaging/build-tarball.sh`'s Darwin path to build and stage the agent
alongside `remotex`.

---

## 4. Gateway changes

Small and contained — this is the payoff of the pass-through design.

### 4.1 `src/protocol.rs` — pass-through tiles

`Tile` currently owns PNG encoding via `from_rgb` and hardcodes the format byte
in `to_frame` (`src/protocol.rs:100-129`). Add a `format: u8` field:

- `Tile::FORMAT_JPEG: u8 = 2` alongside the existing `FORMAT_PNG = 1`.
- `Tile::from_rgb(...)` keeps its signature and sets `format: Self::FORMAT_PNG`.
- New `Tile::encoded(format, x, y, w, h, data: Vec<u8>)` for the pass-through
  path — no encode, no copy of the pixel data.
- `to_frame()` writes `self.format` instead of the constant at line 121.

RDP and VNC are unaffected. Per CLAUDE.md there is no backward-compatibility
constraint, so this is a clean edit rather than an additive one.

### 4.2 `src/rxa.rs` — the new engine

Same seam as every other engine:

```rust
pub async fn run(
    config: TargetConfig,
    input_rx: mpsc::UnboundedReceiver<ClientMsg>,
    frame_tx: mpsc::Sender<ServerMsg>,
)
```

It dials the agent, runs the handshake, sends `Attach`, then pumps in both
directions: `AgentMsg::Tile` → `ServerMsg::Tile(Tile::encoded(..))`,
`AgentMsg::Cursor` → `ServerMsg::Cursor`, `AgentMsg::Hello`/`DisplaySize` →
`ServerMsg::Resize`; and `ClientMsg` input → the corresponding `GatewayMsg`.
`ClientMsg::Viewport` is dropped (no resize in v1). Follow the connect-error
preamble shape of `vnc::run` (`src/vnc.rs:114-162`).

**Reconnect behaviour — the one place this engine deliberately differs from
RDP/VNC:**

- **Initial connect fails** → `ServerMsg::Error` and return, exactly like the
  other engines. Wrong host or wrong PSK should be visible immediately in the
  picker.
- **An established session drops** → retry silently with capped backoff
  (1 s → 15 s), forever. On each successful reconnect, re-send `Resize` and
  request a full repaint. The browser sees frames pause and resume; it never
  bounces back to the picker, and there is never a credential prompt.

That second rule is the concrete expression of "a disconnect doesn't force you
to log in again."

### 4.3 `src/config.rs`

- `Protocol::Rxa` variant (`src/config.rs:52`), `name() -> "rxa"`,
  `default_port() -> 52381` — adjacent to the web server's 52380, in the same
  private range, not colliding with 3389/5900.
- A `psk: String` field on `TargetConfig`. Note `TargetConfig` is
  `#[serde(deny_unknown_fields)]` (`src/config.rs:80`), so the field must be
  declared with `#[serde(default)]` even though only `rxa` targets use it —
  same as the existing RDP-only `security`/`width`/`height`.
- Validate in `ConfigFile::parse`: an `rxa` target with a missing or malformed
  PSK is a config error, not a runtime surprise.
- Update `packaging/etc/remotex.toml.example` with a commented `mac` target.

### 4.4 `src/session.rs`, `src/lib.rs`, `src/cli.rs`

- One `match` arm in `spawn_engine` (`src/session.rs:432`).
- `pub mod rxa;` in `src/lib.rs`.
- `Commands::GenPsk` in `src/cli.rs` — prints a fresh PSK. Simpler than
  `gen-passwd`: no prompt, no argument.

### 4.5 Shared helpers

`host_port()` is currently duplicated between `src/rdp.rs:500` and
`src/vnc.rs:935`. A third engine would triplicate it. Extract that one function
(and nothing else) to a small shared home. Explicitly **not** doing a broader
`trait Engine` refactor: the rxa engine needs neither the strip loop nor
`clamp_u16`, so the remaining duplication doesn't grow.

### 4.6 Frontend

Two small edits:

- `frontend/src/protocol.ts:74 decodeTileFrame` — accept format `1` *or* `2`,
  and return a `mime` field on `TileMsg` (`"image/png"` / `"image/jpeg"`).
- `frontend/src/useRemoteDesktop.ts:576 drawTile` — pass `tile.mime` to the
  `Blob` instead of the hardcoded `"image/png"`.

Nothing else changes. `createImageBitmap` decodes JPEG natively, the cursor path
already exists, and the `msg.protocol === "rdp"` check at
`useRemoteDesktop.ts:655` correctly leaves `rxa` on automatic-viewport
behaviour (which the engine ignores in v1).

Run `biome check` in `frontend/` afterwards.

---

## 5. Testing

The agent only runs on macOS, so containers are impossible for it. Push as much
as possible into cross-platform unit tests:

**Unit (run on Linux, in `rxa-proto`):**
- PSK generate → parse roundtrip; rejects bad prefix, bad length, bad CRC.
- Noise handshake roundtrip over an in-memory duplex, including that a
  **mismatched PSK fails** and a mismatched prologue/version fails.
- Framing: messages larger than one Noise message chunk survive a roundtrip;
  partial reads reassemble.
- `AgentMsg`/`GatewayMsg` encode/decode roundtrip for every variant.
- `mac_keycode()` table spot-checks, in the style of the existing
  `src/keymap.rs` tests.

**Unit (in the gateway):**
- `Tile::encoded()` produces the documented header with `format = 2`
  (mirror the existing byte-for-byte assertions at `src/protocol.rs:284+`).
- `ClientMsg` → `GatewayMsg` input translation.

**E2E (`tests/rxa_e2e.rs`, container-free):** an in-process fake agent in the
style of `spawn_fake_vnc()` in `tests/protocol_e2e.rs` — completes the real
Noise handshake with a known PSK, sends `Hello` + a small pre-encoded JPEG tile
+ a cursor shape, and asserts the browser-facing WebSocket receives a
`format = 2` tile frame and a `cursor` control message. Then drop the fake
agent's connection and assert the engine **reconnects and repaints instead of
erroring** — that is the behaviour this whole project exists for, so it deserves
a test.

**Manual, on the Mac** (nothing else can cover these):
1. Both TCC prompts appear on first run; both grants persist across a rebuild.
2. Full-screen paint on connect; typing in a text field; clicking accurately at
   the screen corners (the backing-scale check); scrolling in both directions.
3. Pointer visible and correctly positioned, including after reattach.
4. Pull the network for 30 s → session resumes with no prompt.
5. Kill the gateway → agent stays alive; restart → reconnect with no prompt.
6. Sleep/wake and a display resolution change mid-session.

Per CLAUDE.md: `cargo clippy -- -D warnings` and `cargo test` after Rust
changes, `biome check` in `frontend/` after TS changes, no `cargo fmt`, and
never a headless browser.

---

## 6. Work order

1. **Workspace conversion** — `[workspace]` + `default-members`, empty
   `crates/rxa-proto`. Verify `cargo build`/`clippy`/`test` and the frontend
   version injection are unchanged. Commit on its own.
2. **`rxa-proto`** — PSK, Noise, framing, messages, keymap, with the full unit
   test suite. Entirely Linux-side work; nothing macOS yet.
3. **Gateway** — `Tile::encoded` + format byte, `src/rxa.rs`, config/CLI wiring,
   the two frontend edits, and `tests/rxa_e2e.rs` against the fake agent. At
   this point the whole gateway half is testable and done without a Mac.
4. **`rxa-agent` capture** — on the Mac: SCStream, dirty rects, stride-correct
   BGRA reads, tile encode + classifier. Paint-only, no input.
5. **`rxa-agent` input** — CGEvent injection, coordinate/scale conversion.
6. **Cursor shapes.**
7. **Reconnect hardening** — ping/pong, stream linger, restart-on-error.
8. **Packaging** — code signing, LaunchAgent plist, install script,
   `build-tarball.sh` Darwin path, release workflow.
9. **Docs** — fold this into `docs/architecture.md`, update `README.md`'s config
   reference, and note the macOS agent in `docs/roadmap.md`.

Steps 1–3 are all doable on Linux; step 4 is where the Mac becomes necessary.

## 7. Risks

- **ScreenCaptureKit binding maturity** is the biggest unknown. Confirm dirty
  rects are actually reachable through the chosen crate *before* building on it;
  without them the design falls back to full-frame diffing, which is much more
  CPU.
- **TCC grant persistence across rebuilds** will be annoying during development.
  Sort out ad-hoc signing early rather than fighting it every iteration.
- **Retina encode cost.** A 3456×2234 display generates a lot of pixels. If the
  PNG/JPEG classifier plus encoding can't keep up, the fallback ladder is:
  downscale in `SCStreamConfiguration` → coarser tiles → hardware H.264 via
  VideoToolbox with WebCodecs in the browser (deliberately deferred; it would
  add an in-browser decoder path, which `docs/architecture.md` lists as a design
  tenet to avoid).
- **No login-window support.** A LaunchAgent cannot run there. If reaching a
  logged-out Mac ever matters, that is a separate design problem.
- **`panic = "abort"`** applies to the agent too. An unwrap in a dispatch-queue
  callback kills the agent; `KeepAlive` restarts it, but be disciplined about
  error handling in the capture path.
