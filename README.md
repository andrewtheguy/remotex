# remotex

A single-user remote desktop gateway for RDP and VNC targets, including Macs
using the built-in Screen Sharing service. The Rust backend owns each protocol
session and streams image tiles over one WebSocket protocol to the browser SPA
or native macOS client.

One reason the RDP path exists: Microsoft Remote Desktop on macOS handles
resize-to-window badly, and fonts can come out blurry after a resize. Here a
resize renegotiates the desktop with the server, so the framebuffer is the size
that was asked for rather than a resampling of the size it used to be.

- RDP uses [IronRDP](https://crates.io/crates/ironrdp).
- VNC uses a built-in RFB client and connects directly to macOS Screen Sharing.
  `subtype = "ard"` selects Apple Screen Sharing's Standard mode over RFB 3.8
  with Apple Remote Desktop authentication.
- The optional macOS 26 `remotex.app` is a self-contained native client with
  Metal rendering, AppKit input, menus, clipboard access, and audio playback.
  Choose its bundled loopback gateway with no login, or enter the address and
  login for a remote gateway.

See [`docs/architecture.md`](docs/architecture.md) for the system design,
[`docs/macos-viewer.md`](docs/macos-viewer.md) for the app, and
[`docs/known-issues.md`](docs/known-issues.md) for faults worth recognising rather
than re-investigating.

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

Apple Screen Sharing Standard mode (`ard`) lists the Mac's physical screens, can
show one screen or all of them, reports each screen's pixel density, keeps pixels
raw, and supports the native Apple pasteboard. Apple Screen Sharing High
Performance mode (`ard-high-performance`, **experimental**) takes the same
credentials, requests
one virtual display at the target's configured `width` and `height`, disables the
remote Mac's physical displays once connected, and puts all of the remote Mac's
windows on that virtual display. It also adds zlib compression over Apple's
record-layer revision (around fifty times fewer bytes on a static desktop).
Apple's official macOS Screen Sharing client can instead choose up to two virtual
displays. Both Apple subtypes
support the native Apple pasteboard when `clipboard = true`. With `resize = true`,
High Performance supports **Resize to Window** like RDP, using Apple's dynamic
resolution feature to replace the virtual display's mode from client viewport
reports. Every fresh connection turns the Mac's Dynamic resolution setting back
on. Standard `ard` still refuses resize, and the one/two-virtual-display control
is not implemented. See
[`docs/apple-vnc-889.md`](docs/apple-vnc-889.md).

High Performance mode is **experimental**, and is the one part of remotex built
entirely without a specification: Apple documents none of the protocol revision,
its record layer, its control messages or its virtual display handling, so all of
it is reverse engineered and only as correct as the Macs it has been measured
against. A macOS update is free to change any of it. The dynamic-resolution path
behind `resize = true` is the least settled part, and what a resize can leave
behind is in [`docs/known-issues.md`](docs/known-issues.md). Prefer `ard` unless
you need a virtual display.

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

Install the frontend dependencies once, then use Cargo for local development.
`cargo run` rebuilds the frontend when its sources change and serves the generated
bundle with the gateway.

```sh
bun install --cwd frontend
cargo run -- serve -c remotex.toml
```

Open <http://localhost:52380>. Use `RUST_LOG=info` or `RUST_LOG=debug` for backend
logs. Use `cargo build` when you only need to compile without starting the
gateway.

The gateway serves `frontend/dist` from disk, and `build.rs` declares only the
frontend *sources* as Cargo inputs, so Cargo cannot notice that the bundle itself
is missing. Build it explicitly whenever `frontend/dist` may be absent or stale
without a source change — after deleting it, after `CI=true` builds, which skip
the frontend step entirely, or when a prebuilt `target/` came from elsewhere:

```sh
bun run --cwd frontend build && cargo run -- serve -c remotex.toml
```

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
target with `subtype = "ard"` for Apple Screen Sharing Standard mode and its
physical displays, or
`"ard-high-performance"` (experimental) for one configured virtual display
containing all of its windows, with its physical displays disabled for the
connection, and the Mac account's username and password. Keep the config mode `0600`; target
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
bun install --cwd frontend
cargo build --release
bash packaging/build-tarball.sh
```

Local Cargo builds automatically rebuild the frontend when its sources change.
The tarball contains the gateway binary and built frontend. `remotex.app` is built
by `packaging/macos-viewer/build-viewer-app.sh`, which bundles the gateway binary
into the app rather than shipping the frontend.
