# Architecture

A single-user web gateway to remote desktops: one Rust binary (an axum web
server with **server-side protocol engines**) plus one Vite/React SPA. The
browser speaks a single uniform protocol no matter what the target speaks —
RDP and VNC sessions are indistinguishable to the frontend.

This document describes the system as built. Remaining work lives in
[`roadmap.md`](roadmap.md).

## Data path

```
Browser — full-screen canvas SPA (frontend/)
   │
   │  WebSocket /ws — the uniform protocol (src/protocol.rs, protocol.ts):
   │    server → browser: screen tiles as binary frames (PNG-compressed),
   │                      resize/error as JSON text
   │    browser → server: input events + viewport reports as JSON text
   ▼
axum server (src/server.rs) ── /ws bridge (src/ws.rs)
   │
   │  the engine seam (src/session.rs): one spawn path, dispatch on the
   │  target's protocol — each engine implements the same
   │  run(config, input_rx, frame_tx) contract
   ▼
 ┌───────────────────────┐      ┌───────────────────────────┐
 │ rdp::run (src/rdp.rs) │      │ vnc::run (src/vnc.rs)     │
 │ IronRDP client        │      │ built-in RFB 3.8 client   │
 └───────────┬───────────┘      └────────────┬──────────────┘
             │ RDP (TLS/NLA)                 │ RFB (raw encoding)
             ▼                               ▼
        RDP server                      VNC server          (LAN targets)
```

## Design tenets

- **Server-side decode for every protocol.** The backend owns the protocol
  session and the framebuffer; the browser only draws tiles. This keeps one
  transport to optimize (backend → browser is the bottleneck link — the
  targets are LAN, the browser may be on weak WAN), enables session
  resume/takeover later, and makes "add a protocol" mean "write another
  engine", not "ship another in-browser decoder".
- **Single session, permanently.** This is a single-user program with one
  active session slot. Session takeover (a new browser force-claims the slot
  and evicts the previous holder) is planned; concurrent sessions, session
  sharing, or a session broker are permanently out of scope.
- **Baseline protocol, no per-implementation workarounds.** Guacamole-style:
  speak the subset every server must support, and spend the cleverness on the
  link we control.
- **One config file.** TOML only (`[server]` + `[[targets]]`), no environment
  variables, no `.env`. Credentials stay server-side and never reach the
  browser.

## Backend modules

```
src/
  main.rs            entry: CLI dispatch + serve
  lib.rs             library surface (shared with the integration tests)
  cli.rs             clap CLI (serve --config/--target)
  config.rs          TOML config ([server] + [[targets]] profiles)
  server.rs          axum router (/api/*, /ws, disk-served SPA + fallback)
  ws.rs              WebSocket <-> protocol-engine bridge
  session.rs         the engine seam: spawns rdp::run or vnc::run per target
  rdp.rs             RDP engine (IronRDP): connect + active loop
  vnc.rs             VNC engine (built-in RFB client, raw-only + resize)
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode / X11 keysym
  protocol.rs        wire messages (ClientMsg / ServerMsg / Tile)
  error.rs           AppError
```

Each WebSocket connection spawns its engine on a dedicated thread with a
current-thread tokio runtime (IronRDP's futures are not `Send`; one shared
spawn path keeps the seam uniform). The session ends when either side goes
away: browser disconnect closes the input channel, engine death closes the
frame channel.

## The wire protocol (browser ↔ backend)

Defined in `src/protocol.rs`, mirrored in `frontend/src/protocol.ts`.

**Server → browser.** Split by weight (phase 2): screen tiles are **binary
WebSocket frames** — a 10-byte little-endian header (kind, format, x, y, w, h)
followed by a PNG-compressed RGB payload; dirty rectangles taller than
`STRIP_ROWS` (64) are split into strips. Control messages stay JSON text with
a `type` tag: `resize` (the remote desktop size changed) and `error` (fatal
session error). Measured ~10x smaller than the old base64-in-JSON baseline on
a full-screen paint; per-session byte totals are logged on disconnect.

**Browser → server.** JSON text frames: `mouseMove`, `mouseButton`, `wheel`,
`key` (DOM `KeyboardEvent.code`), and `viewport` — the browser's viewport in
device pixels, i.e. the size it *wants* the remote desktop to be. Engines
that can drive the remote size act on viewport reports; the rest ignore them.

## Engines

### RDP (src/rdp.rs)

IronRDP client: TLS/NLA per the target's `security` mode, active-stage loop
decoding into a `DecodedImage`, dirty regions repacked to RGB strips and sent
as tiles. Input is injected as fast-path PDUs (`keymap::scancode` maps DOM
codes). Deactivation-Reactivation is not implemented: the desktop size is
fixed at connect time (`width`/`height` from the target profile) and
`viewport` reports are ignored — the frontend keeps its scrollbars.

### VNC (src/vnc.rs)

A minimal built-in RFB client (RFC 6143), Guacamole-style baseline:

- **Protocol 3.8.** Anything announcing at least 3.8 is answered with 3.8
  (macOS Screen Sharing greets with 3.889, RealVNC with 4.x — both accept a
  3.8 client). Older servers are rejected.
- **Security None or classic VncAuth** (DES over the 16-byte challenge with
  the RFB bit-reversed key convention). VncAuth is chosen when the target has
  a password, otherwise None. Apple/RealVNC proprietary types are not spoken.
- **Raw encoding only.** The one encoding every VNC server must support. The
  backend↔VNC hop is LAN, so VNC's clever wire encodings buy nothing there.
- **Forced pixel format:** 32bpp true-colour BGRX little-endian, repacked
  server-side to RGB and PNG-encoded in the same strips as RDP.
- **Input:** pointer events carry the tracked button mask + position (wheel =
  press/release of buttons 4–7); keys map DOM `code` → X11 keysym via
  `keymap::keysym`.

**Dynamic resize (phase 4, opt-in via `resize = true` on the target).** The
engine advertises the DesktopSize/ExtendedDesktopSize pseudo-encodings and
turns browser `viewport` reports into `SetDesktopSize` requests, so
TigerVNC-family servers (Xtigervnc, x0vncserver, …) re-render at the
browser's size and the scrollbars disappear. `SetDesktopSize` is only sent
after the server declares support with its first ExtendedDesktopSize rect; a
report arriving earlier is stashed and replayed then. Any size change —
requested or server-initiated — is forwarded to the browser as `resize` and
followed by a full framebuffer request, since a resize invalidates the
contents. Servers without the extension (and targets without the opt-in)
keep the fixed connect-time size — acceptable per the no-workarounds rule.

Deliberately out of the VNC baseline: clipboard (`ServerCutText` is drained
and dropped), Bell, and non-raw encodings.

## Frontend

Vite + React 19 + TypeScript, managed with Bun (`frontend/`). Three files
matter:

- `protocol.ts` — TS mirror of the wire protocol (binary tile parsing).
- `useRemoteDesktop.ts` — the one hook: WebSocket lifecycle, tile rendering,
  input capture, viewport reporting.
- `RemoteDesktop.tsx` — the full-screen canvas + input overlay.

**Full-screen canvas (phase 3).** The canvas fills the browser viewport and
renders at **1:1 device pixels**: the backing store stays at the remote pixel
size, the CSS size is remote ÷ `devicePixelRatio` — no scaling, no
letterboxing. A remote desktop larger than the viewport overflows into native
scrollbars. A re-armed `matchMedia` listener re-derives the CSS size when
`devicePixelRatio` changes (monitor moves, browser zoom), and the CSS size
snaps to the viewport when the remote matched it so fractional-dpr rounding
can't spawn phantom scrollbars.

**Viewport reporting (phase 4).** On connect and on window-resize/dpr changes
(debounced 250ms, deduped) the browser sends `viewport` = viewport size ×
`devicePixelRatio`. Where the engine can act on it (VNC with `resize = true`
against a TigerVNC-family server) the desktop follows the window and the
scrollbars disappear.

PNG tiles decode asynchronously (`createImageBitmap`), so all incoming
messages run through one promise queue: draws land in arrival order and a
resize can't jump past queued tiles. Input is captured on a transparent
overlay exactly covering the canvas; held keys/buttons are released on blur
so nothing sticks on the remote.

## Configuration

One global TOML file (`--config <path>`, or `<prefix>/etc/rdpweb.toml` in the
installed layout — see [`install.md`](install.md) and `packaging/`). A
`[server]` block (bind host/port, static dir) plus `[[targets]]` profiles:
protocol, host/port, credentials, RDP-only `width`/`height`/`security`, and
the VNC `resize` opt-in. The serve subcommand picks a target with `--target`
(default: the first). See `packaging/etc/rdpweb.toml.example`.

## Testing

- **Unit tests** live with the code (protocol encoding, RFB handshake pieces,
  VncAuth vectors, input translation, keymaps, config parsing).
- **E2E tests** (`tests/`): protocol-level tests against the real axum server,
  and container-backed happy paths — `rdp_tiles_e2e.rs` against a dummy xrdp,
  `vnc_tiles_e2e.rs` against a dummy TigerVNC (full-desktop paint and dynamic
  resize through a real server). Containers run under podman or docker.
  **Never a headless browser** — browser automation is flaky by policy.

## Phase status

Done: phase 1 (MVP), phase 2 (transport + VNC engine + TOML config), phase 3
(full-screen canvas), phase 4 (VNC dynamic resize). Planned: connection-flow
UX (5), session management (6), soft keyboard + floating UI (7), multi-target
picker (8), the remotex-v2 rename (9) — the list lives in
[`roadmap.md`](roadmap.md).
