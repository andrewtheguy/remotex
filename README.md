# rdpweb

A browser-based RDP client: connect to a Remote Desktop host and drive it with
mouse and keyboard from a web browser.

- **Backend** — Rust, [axum](https://github.com/tokio-rs/axum) + tokio. The RDP
  protocol engine runs server-side ([IronRDP](https://crates.io/crates/ironrdp),
  wired up in Phase 1); the browser talks to it over a WebSocket.
- **Frontend** — [Vite](https://vite.dev/) + React 19 + TypeScript, managed with
  [Bun](https://bun.sh/). The built assets are embedded into the Rust binary
  (`rust-embed`) so the release is a single self-contained executable.

> **Status: skeleton.** No RDP logic is implemented yet — every RDP touchpoint is
> a `TODO(phase1)` placeholder. The end-to-end wiring (SPA served by the binary,
> `/ws` WebSocket carrying input events) is in place and runnable. See
> [`docs/phase1-mvp.md`](docs/phase1-mvp.md) for the plan.

## Layout

```
src/                 Rust backend (flat module layout)
  main.rs            entry: CLI dispatch + serve
  cli.rs             clap CLI (serve)
  config.rs          AppConfig
  server.rs          axum router (/api/*, /ws, SPA fallback)
  ws.rs              WebSocket session (input logging stub)
  rdp.rs             server-side RDP session — PLACEHOLDER
  protocol.rs        wire messages (ClientMsg / ServerMsg)
  assets.rs          rust-embed static file handler
  error.rs           AppError
frontend/            Vite + React + TS SPA
  src/
    protocol.ts      TS mirror of the wire protocol
    useRemoteDesktop.ts  WebSocket + input capture hook
    RemoteDesktop.tsx    canvas + input overlay
docs/phase1-mvp.md   Phase 1 MVP plan
```

## Development

Run the backend and frontend in two terminals. In dev, Vite (`:5173`) proxies
`/api` and `/ws` to the Rust server (`:52380`).

```bash
# Terminal 1 — backend
cargo run -- serve                 # http://localhost:52380

# Terminal 2 — frontend (with hot reload)
cd frontend
bun install
bun run dev                        # http://localhost:5173
```

Open http://localhost:5173. Moving the mouse or pressing keys over the canvas
sends `ClientMsg` events; the backend logs them (`RUST_LOG=info`). No remote
screen renders yet — that arrives in Phase 1.

`serve` flags: `--host`, `--port` (default `52380`), `--rdp-host`,
`--rdp-port` (default `3389`), or the matching `RDPWEB_*` env vars.

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
