# remotex

A single-user, browser-based remote desktop gateway for RDP, VNC, and macOS.
The Rust backend owns each protocol session and streams image tiles to a
React/TypeScript frontend over a common WebSocket protocol.

- RDP uses [IronRDP](https://crates.io/crates/ironrdp).
- VNC uses a built-in RFB 3.8 client.
- macOS uses the companion `remotex-agent` and the encrypted `rxa` protocol.

See [`docs/architecture.md`](docs/architecture.md) for the system design and
[`docs/mac-agent-architecture.md`](docs/mac-agent-architecture.md) for the
macOS agent.

## Install

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
$EDITOR /opt/remotex/etc/remotex.toml
remotex serve
```

The installer verifies the release digest, installs versioned files under
`/opt/remotex`, and links `remotex` into `/usr/local/bin`. See
[`docs/install.md`](docs/install.md) for custom locations, upgrades, rollback,
and uninstall.

To connect to a Mac with `protocol = "rxa"`, install the agent DMG from the
same release. See [`packaging/macos/README.md`](packaging/macos/README.md).

## Container

```sh
docker run -d --name remotex -p 52380:52380 \
  -v ./remotex.toml:/opt/remotex/etc/remotex.toml:ro \
  ghcr.io/andrewtheguy/remotex:latest
```

Set `[server].host = "0.0.0.0"` in the mounted config. Images are published for
Linux amd64 and arm64 with `latest` and `v<version>` tags.

Generate the required web-login credential with:

```sh
docker run --rm -it ghcr.io/andrewtheguy/remotex:latest gen-passwd admin
```

## Development

Run the backend and frontend separately; Vite proxies `/api` and `/ws` to the
backend.

```sh
# terminal 1
cargo run -- serve -c remotex.toml

# terminal 2
cd frontend
bun install
bun run dev
```

Open <http://localhost:5173>. Use `RUST_LOG=info` or `RUST_LOG=debug` for
backend logs.

The main directories are:

| Path | Contents |
|---|---|
| `src/` | gateway, session management, and RDP/VNC/`rxa` engines |
| `frontend/` | React SPA |
| `crates/rxa-proto/` | protocol shared by gateway and macOS agent |
| `crates/rxa-agent/` | macOS agent |
| `tests/` | protocol and engine end-to-end tests |
| `packaging/` | release, install, container, and macOS bundle scripts |

## Configuration

remotex reads one TOML file. Installed deployments default to
`<prefix>/etc/remotex.toml`; a checkout should pass `--config`.

```toml
[server]
site_passwd = "admin:$2b$..."

[[targets]]
name = "workstation"
protocol = "rdp" # rdp, vnc, or rxa
host = "192.0.2.10"
username = "Administrator"
password = "change-me"
```

Generate `site_passwd` with `remotex gen-passwd <username>`. Generate an `rxa`
key with `remotex gen-psk`. Keep the config mode `0600`; target credentials
remain server-side but are stored in this file.

All fields and per-protocol examples are in
[`packaging/etc/remotex.toml.example`](packaging/etc/remotex.toml.example).

## Checks

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo test

cd frontend
bun run check
```

The container-backed RDP and VNC tests use Docker or Podman and do not start a
browser. `tests/rxa_e2e.rs` uses an in-process fake agent.

For a remote Podman connection:

```sh
CONTAINER_CONNECTION=linuxbox \
REMOTEX_TEST_CONTAINER_HOST=host \
cargo test
```

## Build

```sh
cd frontend && bun install && bun run build && cd ..
cargo build --release
bash packaging/build-tarball.sh
```

The tarball contains the gateway binary and built frontend. The macOS agent is
a separate DMG built by `packaging/macos/build-agent-app.sh`.
