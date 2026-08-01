# remotex

A single-user, browser-based remote desktop gateway for RDP and VNC targets,
including Macs using the built-in Screen Sharing service. The Rust backend owns
each protocol session and streams image tiles to a React/TypeScript frontend
over a common WebSocket protocol.

One reason the RDP path exists: Microsoft Remote Desktop on macOS handles
resize-to-window badly, and fonts can come out blurry after a resize. Here a
resize renegotiates the desktop with the server, so the framebuffer is the size
that was asked for rather than a resampling of the size it used to be.

- RDP uses [IronRDP](https://crates.io/crates/ironrdp).
- VNC uses a built-in RFB 3.8 client and can connect directly to macOS Screen
  Sharing. A Mac is a `vnc` target with `subtype = "ard"` for Apple Remote
  Desktop authentication.
- The optional macOS 26 `remotex.app` is a self-contained native client: it
  carries its own gateway, starts it on a loopback port at launch, and needs no
  server, address, or login. Metal rendering, AppKit input, menus, clipboard
  access, and audio playback.

See [`docs/architecture.md`](docs/architecture.md) for the system design and
[`docs/macos-viewer.md`](docs/macos-viewer.md) for the app.

## Install

Install the latest release:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
```

Generate a web-login credential, then edit the installed config:

```sh
remotex gen-passwd admin
${EDITOR:-vi} /opt/remotex/etc/remotex.toml
```

Paste the generated `admin:$2b$...` value into `[server].site_passwd` and
replace the example `[[targets]]` entry with the remote desktop you want to
reach. Then start the server in the foreground:

```sh
remotex serve
```

The installer verifies the release digest, installs versioned files under
`/opt/remotex`, and links `remotex` into `/usr/local/bin`. See
[`docs/install.md`](docs/install.md) for custom locations, upgrades, rollback,
and uninstall.

Macs can be configured as ordinary VNC targets using macOS Screen Sharing, with
no companion software. Use `protocol = "vnc"` with `subtype = "ard"` and the Mac
account's username and password; that selects Apple Remote Desktop authentication
so the connection lands at the user's own screen rather than a login-window
session.

Both Apple subtypes list the Mac's screens, can show one screen or all of them, and
report each screen's pixel density. A selected screen is drawn at its 100% point
size from its full backing-pixel framebuffer; the host resamples it when the two
displays have different densities. For *All Displays*, the gateway composes every
screen's backing rectangle into the combined logical coordinate space, so mixed 1×
and 2× displays keep their correct relative sizes. Plain
`ard` keeps pixels raw and supports the native Apple pasteboard;
`ard-high-performance` takes the same credentials and adds zlib compression over
Apple's record-layer revision (around fifty times fewer bytes on a static desktop),
but does not yet support `clipboard`. Neither supports `resize`. See
[`docs/apple-vnc-889.md`](docs/apple-vnc-889.md).

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
| `src/` | gateway, session management, and RDP/VNC engines |
| `frontend/` | React SPA |
| `apps/remotex-viewer/` | `remotex.app`, the native macOS 26 SwiftUI/Metal client |
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
protocol = "rdp" # rdp or vnc
host = "192.0.2.10"
username = "Administrator"
password = "change-me"
```

Generate `site_passwd` with `remotex gen-passwd <username>`. A Mac is a `vnc`
target with `subtype = "ard"` (or `"ard-high-performance"`, to compress the picture)
and the Mac account's username and password. Keep the config mode `0600`; target
credentials remain server-side but are stored in this file.

All fields and per-protocol examples are in
[`packaging/etc/remotex.toml.example`](packaging/etc/remotex.toml.example).

## Checks

```sh
cargo clippy --all-targets --all-features -- -D warnings
cargo test

cd frontend
bun run check
cd ..

swift test --package-path apps/remotex-viewer
```

The container-backed RDP and VNC tests use Docker or Podman and do not start a
browser. They are ignored by default; run them explicitly with:

```sh
cargo test --test rdp_tiles_e2e --test vnc_tiles_e2e -- --ignored
```

Stable headless browser checks for DOM/control-plane flows live under
[`tests/playwright`](tests/playwright/README.md). They intentionally do not
assert framebuffer/canvas output, cursor rendering, or gesture timing.

For a remote Podman connection:

```sh
CONTAINER_CONNECTION=workstation-wsl \
REMOTEX_TEST_CONTAINER_HOST=10.22.34.32 \
cargo test --test rdp_tiles_e2e --test vnc_tiles_e2e -- --ignored
```

`CONTAINER_CONNECTION` is the Podman system connection name.
`REMOTEX_TEST_CONTAINER_HOST` is the engine host's IP address or DNS name as
reachable from the machine running the tests; an SSH config alias is not
resolved for the tests' direct RDP and VNC connections.

## Build

```sh
cd frontend && bun install && bun run build && cd ..
cargo build --release
bash packaging/build-tarball.sh
```

The tarball contains the gateway binary and built frontend. `remotex.app` is
built by `packaging/macos-viewer/build-viewer-app.sh`, which bundles the gateway
binary into the app rather than shipping the frontend.
