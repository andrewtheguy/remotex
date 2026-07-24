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
   │  the session slot (src/session.rs): claim/attach/detach/takeover; the
   │  engine is spawned per *session*, not per WebSocket, and survives
   │  detach. One spawn path, dispatch on the target's protocol — each
   │  engine implements the same run(config, input_rx, frame_tx) contract
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
  resume/takeover, and makes "add a protocol" mean "write another
  engine", not "ship another in-browser decoder".
- **Single session, permanently.** This is a single-user program with one
  active session slot. Session takeover (a new browser force-claims the slot
  and evicts the previous holder) and detach/reattach exist;
  concurrent sessions, session sharing, or a session broker are permanently
  out of scope.
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
  cli.rs             clap CLI (serve --config/--target, gen-passwd)
  config.rs          TOML config ([server] + [[targets]] profiles)
  auth.rs            web login (site_passwd credential + auth sessions)
  server.rs          axum router (/api/*, /ws, disk-served SPA + fallback)
  ws.rs              WebSocket <-> session bridge
  session.rs         the session slot (claim/attach/detach/takeover) and the
                     engine seam: spawns rdp::run or vnc::run per session
  rdp.rs             RDP engine (IronRDP): connect + active loop
  vnc.rs             VNC engine (built-in RFB client, raw-only + resize)
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode / X11 keysym
  protocol.rs        wire messages (ClientMsg / ServerMsg / Tile)
  error.rs           AppError
```

Each engine runs on a dedicated thread with a current-thread tokio runtime
(IronRDP's futures are not `Send`; one shared spawn path keeps the seam
uniform). The engine lives as long as the remote session: it is spawned when
the first browser attaches and ends when the remote host disconnects — not
when the browser does.

## The session slot

`SessionManager` (src/session.rs) decouples the engine session (backend ↔
remote host) from the browser attachment (backend ↔ WebSocket), remotex's
claim rules on top of a persistent engine:

- **Claim** — `POST /api/session` (`{force?, sessionId?}`) mints the slot
  token. While another browser's WebSocket is attached the claim answers
  `409` unless `force` (takeover) or `sessionId` is the current token (the
  same browser reclaiming after a network drop). Claiming evicts the
  previously attached WebSocket — its socket closes with code **4001** —
  but never the engine.
- **Attach** — `/ws?session=<token>` joins the slot (a stale token closes
  with code **4000**; the browser claims again). The first attach spawns the
  engine; a reattach injects `ClientMsg::Refresh`, making the engine
  re-announce the desktop size and repaint fully — RDP repacks its
  server-owned `DecodedImage`, VNC issues a non-incremental update request
  (the VNC server is one LAN hop away; duplicating the framebuffer
  server-side would buy nothing).
- **Detach** — the WebSocket went away; the engine keeps running and its
  frames are dropped until the next attach. Closing the browser therefore
  *detaches* from the desktop rather than ending it; the remote session ends
  only when the remote host ends it.

One slot, permanently: takeover replaces the attached browser, never adds
one — concurrent sessions, sharing, and brokers stay out of scope.

## Web login

Everything session-related refuses unauthenticated requests, remotex-style
(src/auth.rs):

- **Credential** — `[server].site_passwd` holds `username:bcrypt_hash`
  verbatim (TOML needs no escaping for bcrypt's alphabet; no base64 wrapping
  like remotex). Required; generated with `rdpweb gen-passwd <username>`
  (hidden prompt on a TTY, reads a line when piped).
- **Login** — `POST /api/auth/login` (`{username, password}`) verifies via
  bcrypt (off the async workers) and sets the `rdpweb_session` cookie:
  `HttpOnly; SameSite=Strict; Path=/`, plus `Secure` only when
  `x-forwarded-proto: https` says a TLS proxy is in front (Safari drops
  Secure cookies set over plain HTTP). Tokens live in an in-memory map with
  a sliding 6-hour TTL — a restart logs every browser out, harmless for a
  single-user program. `POST /api/auth/logout` invalidates the caller's
  token; `GET /api/auth/status` answers `{authenticated}` for the SPA's
  mount-time check.
- **Guards** — middleware refuses `/api/config`, `/api/session`, and the
  `/ws` upgrade (the handshake itself 401s) without a live token. Public:
  `/api/health`, `/api/auth/*`, and the SPA shell — it renders the login
  screen and holds no secrets.

The auth session ("may this browser talk to the server?") is independent of
the session slot ("which browser owns the desktop?"): takeover evicts the
other browser's WebSocket but never logs it out. In the frontend, `App.tsx`
mounts the desktop only once authenticated (mounting claims the slot), shows
the login screen otherwise — with the app version at the bottom, injected
from Cargo.toml via a Vite define — and returns to it when a claim answers
401. Until the floating chrome exists, logout is the reserved
**Ctrl+Alt+Shift+L** chord (swallowed before key pass-through, held input
released first) or, on touch devices only, the minimal **Disconnect** button
in the dead space below the fixed-size canvas; both end the browser's login,
not the engine.

## The wire protocol (browser ↔ backend)

Defined in `src/protocol.rs`, mirrored in `frontend/src/protocol.ts`.

**Server → browser.** Split by weight: screen tiles are **binary
WebSocket frames** — a 10-byte little-endian header (kind, format, x, y, w, h)
followed by a PNG-compressed RGB payload; dirty rectangles taller than
`STRIP_ROWS` (64) are split into strips. Control messages stay JSON text with
a `type` tag: `resize` (the remote desktop size changed) and `error` (fatal
session error). Measured ~10x smaller than the old base64-in-JSON baseline on
a full-screen paint; per-session byte totals are logged on disconnect.

**Browser → server.** JSON text frames: `mouseMove`, `mouseButton`, `wheel`,
`key` (DOM `KeyboardEvent.code`), `viewport` — the browser's viewport in
device pixels, i.e. the size it *wants* the remote desktop to be (engines
that can drive the remote size act on viewport reports; the rest ignore
them) — and `refresh`, a full-repaint request. `refresh` is normally
injected server-side by the session layer on reattach, but a
browser may also send it to recover a corrupted canvas.

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

**Dynamic resize (opt-in via `resize = true` on the target).** The
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
- `App.tsx` — the auth gate: login screen vs the desktop.
- `Login.tsx` — the login form, with the app version pinned at the bottom.
- `useRemoteDesktop.ts` — the one hook: session claim + WebSocket lifecycle,
  tile rendering, input capture, viewport reporting, the logout chord, the
  touch view transform (fit-to-width × pinch zoom + pan).
- `touchGestures.ts` — remotex's touch gesture engine, ported.
- `RemoteDesktop.tsx` — the full-screen canvas + input overlay + the
  connection-status overlay + the disconnect CTA below the canvas.

**Connection flow.** The hook claims the session slot, opens the
WebSocket with the token, and reconnects automatically with capped backoff
after any drop (network, server restart, session ended) — no page reload.
The per-tab token (sessionStorage) makes a reconnect a *reclaim*, so it
never trips the takeover prompt. Three states wait for the user instead of
retrying: **busy** (another browser holds the slot; "Take over"
force-claims), **taken over** (this tab was evicted with close code 4001;
"Take it back" force-claims), and **error** (the server reported a fatal
session error; the message is shown with "Retry"). The reconnect backoff
resets only once a desktop actually arrives, so a session that dies right
after connecting can't hot-loop.

**Full-screen canvas.** The canvas fills the browser viewport and
renders at **1:1 device pixels**: the backing store stays at the remote pixel
size, the CSS size is remote ÷ `devicePixelRatio` — no scaling, no
letterboxing. A remote desktop larger than the viewport overflows into native
scrollbars. A re-armed `matchMedia` listener re-derives the CSS size when
`devicePixelRatio` changes (monitor moves, browser zoom), and the CSS size
snaps to the viewport when the remote matched it so fractional-dpr rounding
can't spawn phantom scrollbars.

**Viewport reporting.** On connect and on window-resize/dpr changes
(debounced 250ms, deduped) the browser sends `viewport` = viewport size ×
`devicePixelRatio`. Where the engine can act on it (VNC with `resize = true`
against a TigerVNC-family server) the desktop follows the window and the
scrollbars disappear.

**Mobile.** Pinch-zoom-capable touch devices
(`navigator.maxTouchPoints >= 2`) diverge from the desktop model in two ways,
both with remotex's battle-tested bounds. *Sizing:* the viewport report uses
CSS pixels (no dpr — a phone's 3× dpr would mint an enormous desktop),
floored per axis at a constant 1024×768; the constant floor (not remotex's
geometry-found-on-connect) means a phone connecting to a desktop a previous
session left too tall repairs it on connect, since the engine outlives the
browser here. *Display and input:* native scrolling is off; `applyCanvasCss`
positions the canvas by fit-to-width scale × pinch zoom (1–4×) plus a clamped
pan (`translate3d`), and `touchGestures.ts` drives it — a trackpad model
where the cursor is a persistent position (the server composites it into the
framebuffer): one-finger tap clicks at the cursor, one-finger drag moves it
(edge-panning the view), double-tap-and-hold holds the left button with a
second finger assisting, two-finger tap right-clicks, pinch zooms, two-finger
drag pans, three-finger swipe scrolls axis-locked. Gesture wheel ticks are
sign-only `wheel` messages; the input overlay covers the whole viewport (the
disconnect bar is z-lifted above it), and hybrid mouse input maps through
the canvas rect so it tracks the zoom/pan.

PNG tiles decode asynchronously (`createImageBitmap`), so all incoming
messages run through one promise queue: draws land in arrival order and a
resize can't jump past queued tiles. Input is captured on a transparent
overlay exactly covering the canvas; held keys/buttons are released on blur
so nothing sticks on the remote.

## Configuration

One global TOML file (`--config <path>`, or `<prefix>/etc/rdpweb.toml` in the
installed layout — see [`install.md`](install.md) and `packaging/`). A
`[server]` block (bind host/port, static dir, the required `site_passwd`
web-login credential) plus `[[targets]]` profiles:
protocol, host/port, credentials, RDP-only `width`/`height`/`security`, and
the VNC `resize` opt-in. The serve subcommand picks a target with `--target`
(default: the first). See `packaging/etc/rdpweb.toml.example`.

## Testing

- **Unit tests** live with the code (protocol encoding, RFB handshake pieces,
  VncAuth vectors, input translation, keymaps, config parsing).
- **E2E tests** (`tests/`): protocol-level tests against the real axum server
  (`protocol_e2e.rs` — claim/attach flows, takeover eviction, and
  detach/reattach run against a scripted in-process RFB server, so the
  session-slot semantics are covered deterministically without containers),
  and container-backed happy paths — `rdp_tiles_e2e.rs` against a dummy xrdp,
  `vnc_tiles_e2e.rs` against a dummy TigerVNC (full-desktop paint, dynamic
  resize, and detach/reattach repaint through a real server). Containers run
  under podman or docker. **Never a headless browser** — browser automation
  is flaky by policy.

## Status

Everything described above is built: the tile transport, both engines, TOML
config, the full-screen canvas, VNC dynamic resize, the connection-flow UX,
session management, web login, and mobile gestures. What remains — the
floating UI, the soft keyboard, the remotex-v2 rename, and the multi-target
picker — lives in [`roadmap.md`](roadmap.md).
