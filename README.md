# rdpweb

A browser-based RDP client: connect to a Remote Desktop host and drive it with
mouse and keyboard from a web browser.

- **Backend** — Rust, [axum](https://github.com/tokio-rs/axum) + tokio. The RDP
  protocol engine runs server-side ([IronRDP](https://crates.io/crates/ironrdp));
  the browser talks to it over a WebSocket.
- **Frontend** — [Vite](https://vite.dev/) + React 19 + TypeScript, managed with
  [Bun](https://bun.sh/). The built assets are embedded into the Rust binary
  (`rust-embed`) so the release is a single self-contained executable.

> **Status: Phase 1 MVP.** Connects to one RDP host, renders its screen in the
> browser (dirty-rectangle RGBA tiles over the WebSocket), and forwards mouse and
> keyboard input. Credentials live server-side and are never sent to the browser.
> See [`docs/phase1-mvp.md`](docs/phase1-mvp.md) for scope and design.

## Layout

```
src/                 Rust backend (flat module layout)
  main.rs            entry: CLI dispatch + serve
  lib.rs             library surface (shared with the integration tests)
  cli.rs             clap CLI (serve)
  config.rs          AppConfig (bind + RDP target + credentials)
  server.rs          axum router (/api/*, /ws, SPA fallback)
  ws.rs              WebSocket <-> RDP session bridge
  rdp.rs             server-side RDP session (IronRDP): connect + active loop
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode
  protocol.rs        wire messages (ClientMsg / ServerMsg)
  assets.rs          rust-embed static file handler
  error.rs           AppError
frontend/            Vite + React + TS SPA
  src/
    protocol.ts      TS mirror of the wire protocol
    useRemoteDesktop.ts  WebSocket + tile rendering + input capture hook
    RemoteDesktop.tsx    canvas + input overlay
tests/protocol_e2e.rs  protocol-level end-to-end tests (no browser / no real RDP)
docs/phase1-mvp.md   Phase 1 MVP plan
```

## Development

Run the backend and frontend in two terminals. In dev, Vite (`:5173`) proxies
`/api` and `/ws` to the Rust server (`:52380`).

```bash
# Terminal 1 — backend (RDP target + credentials via env or flags)
RDPWEB_RDP_HOST=192.0.2.10 \
RDPWEB_RDP_USERNAME=alice \
RDPWEB_RDP_PASSWORD=secret \
cargo run -- serve                 # http://localhost:52380

# Terminal 2 — frontend (with hot reload)
cd frontend
bun install
bun run dev                        # http://localhost:5173
```

Open http://localhost:5173. The remote desktop renders on the canvas; mouse and
keyboard over it drive the session. Use `RUST_LOG=info` (or `debug`) for logs.

`serve` flags (each with a matching `RDPWEB_*` env var):

| flag | env | default |
| --- | --- | --- |
| `--host` | `RDPWEB_HOST` | `127.0.0.1` |
| `--port` | `RDPWEB_PORT` | `52380` |
| `--rdp-host` | `RDPWEB_RDP_HOST` | `127.0.0.1` |
| `--rdp-port` | `RDPWEB_RDP_PORT` | `3389` |
| `--rdp-username` | `RDPWEB_RDP_USERNAME` | — |
| `--rdp-password` | `RDPWEB_RDP_PASSWORD` | — |
| `--rdp-domain` | `RDPWEB_RDP_DOMAIN` | — |
| `--rdp-width` | `RDPWEB_RDP_WIDTH` | `1280` |
| `--rdp-height` | `RDPWEB_RDP_HEIGHT` | `800` |
| `--rdp-security` | `RDPWEB_RDP_SECURITY` | `auto` |

`--rdp-security` is `auto` (advertise TLS + NLA/CredSSP, server picks), `nla`
(require NLA), or `tls` (plain TLS, no NLA — the remote shows a graphical login).
Self-signed server certificates are accepted.

Credentials are used only server-side for the RDP handshake; `GET /api/config`
returns only the non-secret target host/port.

## Tests

```bash
cargo test        # unit tests + protocol-level end-to-end tests
```

The end-to-end tests in `tests/protocol_e2e.rs` drive the real HTTP + WebSocket
server without a browser or a real RDP server: the RDP target points at a socket
that hangs up, so the session-failure path is reported back over `/ws` as a
`ServerMsg::Error`.

## Production build

The Rust binary embeds `frontend/dist/`, so **build the frontend first**:

```bash
cd frontend && bun install && bun run build   # -> frontend/dist/
cd ..
cargo build --release                          # embeds dist/ into the binary
./target/release/rdpweb serve
```

> A plain `cargo build` requires `frontend/dist/` to exist (it is `.gitignore`d),
> so run the frontend build once before building the backend.
