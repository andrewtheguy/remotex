# rdpweb

A browser-based RDP client: connect to a Remote Desktop host and drive it with
mouse and keyboard from a web browser.

- **Backend** — Rust, [axum](https://github.com/tokio-rs/axum) + tokio. The RDP
  protocol engine runs server-side ([IronRDP](https://crates.io/crates/ironrdp));
  the browser talks to it over a WebSocket.
- **Frontend** — [Vite](https://vite.dev/) + React 19 + TypeScript, managed with
  [Bun](https://bun.sh/). The built assets ship alongside the binary and are
  served from disk (`share/rdpweb/web`), resolved relative to the executable.

> **Status: Phase 1 MVP.** Connects to one RDP host, renders its screen in the
> browser (dirty-rectangle RGBA tiles over the WebSocket), and forwards mouse and
> keyboard input. Credentials live server-side and are never sent to the browser.
> See [`docs/phase1-mvp.md`](docs/phase1-mvp.md) for scope and design.

## Install (Linux & macOS)

```bash
curl -fsSL https://andrewtheguy.github.io/rdpweb/install.sh | bash
```

Downloads the release tarball for your platform, verifies its SHA-256 against
the GitHub-published digest, and installs under `/usr/local/opt/rdpweb` with a
`rdpweb` launcher on your `PATH` (may prompt for `sudo`). Then:

```bash
$EDITOR /usr/local/opt/rdpweb/etc/rdpweb.toml   # set RDP target + creds
rdpweb serve
```

See [`docs/install.md`](docs/install.md) for options, custom locations, and the
upgrade/rollback model, and [`packaging/`](packaging/) for the on-disk layout and
building tarballs.

## Layout

```
src/                 Rust backend (flat module layout)
  main.rs            entry: CLI dispatch + serve
  lib.rs             library surface (shared with the integration tests)
  cli.rs             clap CLI (serve --config/--target)
  config.rs          TOML config ([server] + [[targets]] profiles)
  server.rs          axum router (/api/*, /ws, disk-served SPA + fallback)
  ws.rs              WebSocket <-> RDP session bridge
  rdp.rs             server-side RDP session (IronRDP): connect + active loop
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode
  protocol.rs        wire messages (ClientMsg / ServerMsg)
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
# Terminal 1 — backend. Put the RDP target + credentials in a rdpweb.toml
# file (gitignored) and pass it explicitly — only an installed deployment has
# a default config location.
cargo run -- serve -c rdpweb.toml  # http://localhost:52380

# Terminal 2 — frontend (with hot reload)
cd frontend
bun install
bun run dev                        # http://localhost:5173
```

Open http://localhost:5173. The remote desktop renders on the canvas; mouse and
keyboard over it drive the session. Use `RUST_LOG=info` (or `debug`) for logs.

## Configuration

All configuration lives in one TOML file — there are no environment variables
and no `.env` loading (env files silently shadowing the real environment caused
subtle bugs). The `serve` subcommand takes only two selectors:

- `--config <path>` — the config file. Defaults to the installed
  `<prefix>/etc/rdpweb.toml`; config is global-only (no per-user or
  working-directory files), so in a dev checkout this flag is required.
- `--target <name>` — which `[[targets]]` profile to serve (default: the first).

```toml
[server]
#host = "127.0.0.1"        # web UI bind address
#port = 52380              # web UI port
#static_dir = ""           # built frontend; defaults to the installed
                           # share/rdpweb/web, else frontend/dist

[[targets]]
name = "example"           # unique profile name (picked with --target)
#protocol = "rdp"          # only "rdp" today; "vnc" arrives in phase 2
host = "192.0.2.10"
#port = 3389
username = "Administrator"
password = "change-me"
#domain = ""               # optional; unset = local account
#width = 1280              # initial desktop size to request
#height = 800
#security = "auto"         # auto (TLS+NLA), nla (NLA only), tls (no NLA)
```

`security` is `auto` (advertise TLS + NLA/CredSSP, server picks), `nla`
(require NLA), or `tls` (plain TLS, no NLA — the remote shows a graphical login).
Self-signed server certificates are accepted.

Credentials are used only server-side for the RDP handshake; `GET /api/config`
returns only the non-secret target name/host/port.

> **Password handling.** The config file holds credentials — keep it out of
> version control (`rdpweb.toml` is gitignored here) and `chmod 600` it on real
> hosts.

## Tests

```bash
cargo test        # unit tests + protocol-level end-to-end tests
```

The end-to-end tests in `tests/protocol_e2e.rs` drive the real HTTP + WebSocket
server without a browser or a real RDP server: the RDP target points at a socket
that hangs up, so the session-failure path is reported back over `/ws` as a
`ServerMsg::Error`.

## Production build

The frontend is served from disk (not embedded), so build it and point the
server at `frontend/dist`:

```bash
cd frontend && bun install && bun run build   # -> frontend/dist/
cd ..
cargo build --release
./target/release/rdpweb serve --static-dir frontend/dist
```

To produce a distributable, relocatable tarball (`bin` + `share`) that
installs under `/usr/local/opt/rdpweb`, use the packaging scripts:

```bash
bash packaging/build-tarball.sh               # -> dist/rdpweb-<version>-<os>-<arch>.tar.gz
```

See [`packaging/README.md`](packaging/README.md) for the full layout, the
atomic-swap upgrade model, and rollback.
