# rdpweb

A browser-based remote-desktop client: connect to an RDP or VNC host and drive
it with mouse and keyboard from a web browser.

- **Backend** — Rust, [axum](https://github.com/tokio-rs/axum) + tokio. The
  protocol engines run server-side — RDP via
  [IronRDP](https://crates.io/crates/ironrdp), VNC via a built-in minimal RFB
  client — and the browser talks to both over the same WebSocket protocol.
- **Frontend** — [Vite](https://vite.dev/) + React 19 + TypeScript, managed with
  [Bun](https://bun.sh/). The built assets ship alongside the binary and are
  served from disk (`share/rdpweb/web`), resolved relative to the executable.

> **Status: Phase 1 MVP + phase 2 (transport + VNC).** Connects to one RDP or
> VNC host, renders its screen in the browser (dirty-rectangle tiles as binary
> WebSocket frames, PNG-compressed), and forwards mouse and keyboard input.
> Credentials live server-side and are never sent to the browser. See
> [`docs/phase1-mvp.md`](docs/phase1-mvp.md),
> [`docs/phase2-consolidation.md`](docs/phase2-consolidation.md), and
> [`docs/vnc.md`](docs/vnc.md).

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
  ws.rs              WebSocket <-> protocol-engine bridge
  session.rs         the engine seam: spawns rdp::run or vnc::run per target
  rdp.rs             server-side RDP session (IronRDP): connect + active loop
  vnc.rs             server-side VNC session (built-in RFB client, raw-only)
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode / X11 keysym
  protocol.rs        wire messages (ClientMsg / ServerMsg)
  error.rs           AppError
frontend/            Vite + React + TS SPA
  src/
    protocol.ts      TS mirror of the wire protocol
    useRemoteDesktop.ts  WebSocket + tile rendering + input capture hook
    RemoteDesktop.tsx    canvas + input overlay
tests/               end-to-end tests: protocol-level (protocol_e2e.rs) and
                     container-backed happy paths (rdp_tiles_e2e.rs against a
                     dummy xrdp, vnc_tiles_e2e.rs against a dummy TigerVNC)
docs/phase1-mvp.md   Phase 1 MVP plan
docs/vnc.md          VNC engine: implemented baseline + the phase-4 resize plan
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

All configuration lives in one TOML file — no environment variables for server
or target configuration, and no `.env` loading (env files silently shadowing
the real environment caused subtle bugs). `RUST_LOG` only controls logging.
The `serve` subcommand takes only two selectors:

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
protocol = "rdp"           # required: "rdp" or "vnc"
host = "192.0.2.10"
#port = 3389               # default: the protocol's standard port (3389/5900)
username = "Administrator"
password = "change-me"
#domain = ""               # optional; unset = local account   (RDP only)
#width = 1280              # initial desktop size to request   (RDP only —
#height = 800              #  a VNC server dictates its own size)
#security = "auto"         # auto (TLS+NLA), nla (NLA only), tls (RDP only)
```

`security` is `auto` (advertise TLS + NLA/CredSSP, server picks), `nla`
(require NLA), or `tls` (plain TLS, no NLA — the remote shows a graphical login).
Self-signed server certificates are accepted. For VNC targets, `name` and `protocol = "vnc"` are still required. The
connection-specific fields are `host`, optional `port` (default 5900), and
optional `password`; `username`/`domain`/`width`/`height`/`security` are ignored.

Credentials are used only server-side for the RDP/VNC handshake;
`GET /api/config` returns only the non-secret target name/protocol/host/port.

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

`tests/rdp_tiles_e2e.rs` and `tests/vnc_tiles_e2e.rs` cover the happy paths:
each starts a dummy server in a container (plain xrdp from
`tests/xrdp-dummy/`; TigerVNC with VncAuth from `tests/vnc-dummy/`) with
podman or docker, connects through the real server, and validates the binary
tile transport on the wire — resize as JSON text first, then binary frames
with PNG payloads (the VNC test requires a full-desktop paint). They require a
container runtime; no browser is involved (automated browser tests are flaky
and deliberately avoided).

## Production build

The frontend is served from disk (not embedded), so build it first. In a
checkout the server defaults to `frontend/dist`; set `static_dir` under
`[server]` in the config to serve it from elsewhere:

```bash
cd frontend && bun install && bun run build   # -> frontend/dist/
cd ..
cargo build --release
./target/release/rdpweb serve -c rdpweb.toml  # static_dir defaults to frontend/dist
```

To produce a distributable, relocatable tarball (`bin` + `share`) that
installs under `/usr/local/opt/rdpweb`, use the packaging scripts:

```bash
bash packaging/build-tarball.sh               # -> dist/rdpweb-<version>-<os>-<arch>.tar.gz
```

See [`packaging/README.md`](packaging/README.md) for the full layout, the
atomic-swap upgrade model, and rollback.
