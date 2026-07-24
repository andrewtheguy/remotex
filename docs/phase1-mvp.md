# Phase 1 — MVP

> **Status: implemented.** The server connects to one RDP host over TLS/CredSSP
> (IronRDP, server-side), streams the framebuffer to the browser as
> dirty-rectangle RGBA tiles over `/ws`, and injects mouse + keyboard as RDP
> fast-path PDUs. Credentials come from the TOML config and stay server-side.
> Covered by unit tests plus protocol-level end-to-end tests
> (`tests/protocol_e2e.rs`). Notes on what was deferred are inline below.
>
> Implementation notes:
> - Tiles are sent as JSON `ServerMsg::Tile` with base64 RGBA (milestone-2
>   "decide during milestone 2" resolved in favor of the simple JSON path;
>   binary framing / PNG remain a later optimization).
> - The RDP session runs on a dedicated thread with a current-thread runtime:
>   IronRDP's `read_pdu` future is not `Send`-general, so it can't live on the
>   shared multi-threaded runtime via `tokio::spawn`.
> - Server-pointer software rendering is enabled so the cursor is composited into
>   the framebuffer (and therefore visible in the browser).
> - Deactivation-Reactivation (resolution renegotiation) is logged but not
>   handled — consistent with "no dynamic resize" being out of scope.

## Goal

Connect to a single RDP host, render its screen in the browser, and drive it
with mouse and keyboard. Nothing more.

Success = open the web UI, see the remote Windows desktop, move the mouse, click,
and type — with acceptable latency on a LAN.

### In scope

- One RDP connection at a time to one configured host.
- TLS-secured RDP (the standard modern path).
- Framebuffer → browser rendering via image tiles.
- Mouse: move, left/middle/right button, wheel.
- Keyboard: key down/up.
- Server-side credentials (from the TOML config), never sent to the browser.

### Explicitly NOT in scope (later phases)

- Clipboard, audio, file/drive/printer redirection, USB.
- Multi-monitor, dynamic resize renegotiation.
- H.264 / WebCodecs video streaming (start with tiles; revisit for bandwidth).
- Touch gestures / mobile input.
- Multiple concurrent sessions, session sharing, or a session broker —
  **permanently**, not a later phase: single user, one active session only.
  A later phase adds remotex-style session takeover (a new browser claims the
  single session slot, evicting the previous holder) — takeover, not concurrency.
- Web login / auth UI, RD Gateway, NLA-as-a-service.
- Reconnect, clipboard sync, latency adaptation.

## Architecture

The RDP protocol engine runs **server-side** in Rust
([IronRDP](https://crates.io/crates/ironrdp) via `ironrdp-async`). The browser is
a thin renderer + input source. One binary WebSocket carries both directions.

```
┌─────────────────────────────┐            ┌──────────────────────────────┐
│ Browser (React SPA)          │            │ rdpweb server (axum + tokio)  │
│                              │            │                               │
│  <canvas>  ◀── draw tiles ── │            │  ws.rs  ── input ──▶ rdp.rs   │
│  input overlay ── events ──▶ │  /ws (WS)  │  ◀── frame tiles ── (IronRDP) │
│                              │ ◀════════▶ │                    │          │
│  useRemoteDesktop.ts         │  binary    │  Session ──── RDP/TLS ───────▶│──▶ RDP host
└─────────────────────────────┘            └──────────────────────────────┘        (:3389)
```

Message shapes are already defined and shared in shape between
`src/protocol.rs` (Rust) and `frontend/src/protocol.ts` (TS):

- `ClientMsg` — browser → server: `MouseMove`, `MouseButton`, `Wheel`, `Key`.
- `ServerMsg` — server → browser: `Tile`, `Resize`, `Error`.

## Frame transport (chosen: image tiles)

The server decodes the RDP framebuffer and forwards **dirty rectangles** as tiles
(`ServerMsg::Tile { x, y, w, h, format, data }`), where `data` is base64 and
`format` is `Rgba` (raw RGBA8888) or `Png`. The browser decodes each tile and
blits it to the canvas at `(x, y)` (`ctx.putImageData` for raw RGBA, or
`drawImage` of a decoded `Image`/`ImageBitmap` for PNG).

Rationale: tiles are the simplest correct path to a working picture and map
directly onto RDP's surface/bitmap updates. Start with raw RGBA for correctness,
switch tiles to PNG if bandwidth matters. H.264 + WebCodecs is a **later phase**:
much lower bandwidth but far more complex (encoder, keyframe/damage tracking,
decoder plumbing) — not worth it for the MVP.

> Transport note: the current contract is **JSON text with base64-encoded
> tiles**, matching the `ServerMsg` shape and the integration test. A compact
> binary framing over the same socket is deferred as a throughput optimization
> (the socket is already `arraybuffer`).

## Input

The browser input overlay (`useRemoteDesktop.ts`) already captures
`mousemove/down/up`, `wheel`, `contextmenu`, and `keydown/keyup`, maps them to
`ClientMsg`, and sends them. Phase 1 completes the server half:

- **Coordinates** — map client-rect pixels to framebuffer coordinates. Once the
  remote resolution is known (from the RDP connection), scale by
  `remoteW / canvasClientW`. Today the hook sends raw canvas-relative pixels.
- **Buttons** — DOM button → RDP pointer flags (down/up per button, plus wheel).
- **Keyboard** — DOM `KeyboardEvent.code` → RDP scancode (set 1). Needs a
  `code → scancode` table; handle modifiers and extended keys.
- **Injection** — `rdp::Session::send_input` translates `ClientMsg` into IronRDP
  input PDUs and writes them to the connection.

## Credentials

RDP credentials live **only** on the server (the TOML config file)
and are used to authenticate to the host during the handshake. They are never
sent to the browser (mirrors the remotex model). `GET /api/config` returns only
non-secret target info (host/port).

## Milestones

> These are the **original plan** milestones, all delivered in the current MVP
> (see the status note at the top); kept for historical context — "config/env"
> below predates the TOML-only config. The one item flagged as open below —
> NLA/CredSSP — is implemented: the `security` key of a TOML target profile
> selects `auto` (TLS+NLA), `nla`, or `tls`.

1. **Connect** — uncomment the `ironrdp*` + TLS deps in `Cargo.toml`; implement
   `rdp::Session::connect` (TCP → TLS → RDP negotiation/activation). Log a
   successful handshake to the configured host. No rendering yet.
2. **Render** — receive the first framebuffer; emit `ServerMsg::Tile` on updates;
   browser blits tiles to the canvas. Handle `Resize` for the initial resolution.
3. **Pointer** — wire `MouseMove` / `MouseButton` / `Wheel` through
   `send_input`; verify clicking and moving in the remote session.
4. **Keyboard** — build the `code → scancode` map; wire `Key` through
   `send_input`; verify typing, modifiers, and extended keys.
5. **Credentials** — load RDP credentials server-side from config/env; complete
   authenticated login end to end.

Each milestone is independently demoable against a real RDP host (e.g. a Windows
VM or `xrdp`).

## Open questions

- **Credential source** — flags/env now, or a small config file (like
  minisearch)? *(Resolved since: a TOML config file only — flags/env were
  removed; see the README's Configuration section.)*
- **Security** — TLS only, or also support NLA/CredSSP? RDP servers increasingly
  require NLA; check what IronRDP's client supports out of the box.
- **Tile encoding** — raw RGBA vs PNG vs binary framing; measure on real traffic.
- **Scancode table** — source a maintained DOM-`code` → RDP-scancode mapping
  rather than hand-rolling; confirm layout assumptions (US vs client layout).
- **Backpressure** — bound the outbound tile queue so a slow client can't grow
  memory without limit (remotex pauses the source at a buffered-bytes threshold).
