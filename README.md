# remotex

A single-user remote desktop gateway for RDP and VNC targets, including Macs
using the built-in Screen Sharing service. The Rust backend owns each protocol
session and streams desktop updates over a WebSocket protocol to the browser
SPA. Remote audio uses a dedicated second WebSocket so sound never queues behind
the picture.

The main reason this exists is the client: it is a browser, so anything with one
reaches every target — RDP, VNC and Macs alike — with nothing to install per
platform and nothing that has to exist for your OS. With `resize = true`, the
window drives the remote's size, so the desktop is renegotiated at the size asked
for rather than scaled on the client; plain `vnc`, Apple High Performance and
`rdp` can all be handed the window. On RDP the default `egfx` pipeline makes that
cheap — a display layout, no reactivation — at the cost of a Windows host's text
staying soft afterwards; `egfx = false` re-renders sharp and pays a reactivation
per resize.

- RDP uses **FreeRDP 3**, linked from static archives that
  [libfreerdp-prebuilt](https://github.com/andrewtheguy/libfreerdp-prebuilt) builds
  once per target — so this project still builds with `cargo build` alone: no cmake,
  no pkg-config, no OpenSSL to install and no libclang.
- VNC uses a built-in RFB client and connects directly to macOS Screen Sharing.
  `subtype = "ard"` selects Apple Screen Sharing's Standard mode over RFB 3.8
  with Apple Remote Desktop authentication.
  `subtype = "ard-high-performance"` selects its High Performance mode over RFB
  003.889 — one virtual display holding every remote window, and the only Apple
  path that accepts `resize = true`. It is **experimental** because it has not
  been widely tested — it is also reverse engineered, having no specification.
  Prefer `ard` unless you need a virtual display.

There is one client, and it is the page a browser loads. **remotex.app** shows
that same page in a macOS window with a gateway of its own, and adds the two
things a browser cannot do: ⌘Q and ⌘W reaching the guest, and a clipboard that
keeps syncing while the window is unfocused. It is a released disk image, and
[`docs/macos-viewer.md`](docs/macos-viewer.md) describes it. Giving a *browser*
those same two is a companion Chrome extension's to do, measured and written up
in [`docs/roadmap.md`](docs/roadmap.md).

See [`docs/architecture.md`](docs/architecture.md) for the system design and
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
at full fidelity, and supports the native Apple pasteboard. After the initial
display layout it asks the Mac to switch from raw rectangles to zlib; that second
encoding request is required in both Apple modes. Apple Screen Sharing High
Performance mode (`ard-high-performance`, **experimental**) takes the same
credentials, requests one virtual display at the pinned `width` and `height` when
both are set, or at the full resolution and density of the client's screen
otherwise. Once connected, it disables the remote Mac's physical displays and
puts all of the remote Mac's windows on that virtual display. It carries the same
zlib rectangles over Apple's encrypted record-layer revision (around fifty times
fewer bytes than raw on a static desktop).
Apple's official macOS Screen Sharing client can instead choose up to two virtual
displays. Both Apple subtypes
support the native Apple pasteboard when `clipboard = true`. With `resize = true`,
the window continuously drives High Performance's virtual display, using Apple's
dynamic-resolution feature to replace its mode from client viewport reports.
There is no client-side resize toggle or one-shot resize button. The descriptor's
fixed 3840×2160 backing ceiling permits successive arbitrary sizes within that
bound, and every fresh connection turns the Mac's Dynamic resolution setting back
on. Standard `ard` still refuses resize, and the one/two-virtual-display control is
not implemented.
See [`docs/apple-vnc-889.md`](docs/apple-vnc-889.md).

High Performance mode is **experimental** because it has not been widely tested,
and it is the one part of remotex built
entirely without a specification: Apple documents none of the protocol revision,
its record layer, its control messages or its virtual display handling, so all of
it is reverse engineered and only as correct as the Macs it has been measured
against. A macOS update is free to change any of it. The dynamic-resolution
descriptor has been measured across its arbitrary-size boundary and a burst of
viewport reports, but remains reverse engineered. Prefer `ard` unless you need a
virtual display.

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
| `tests/` | protocol and engine end-to-end tests |
| `packaging/` | release, install, and container scripts |

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
`"ard-high-performance"` (experimental) for one virtual display
containing all of its windows, with its physical displays disabled for the
connection, and the Mac account's username and password. Keep the config mode `0600`; target
credentials remain server-side but are stored in this file.

All fields and per-protocol examples are in
[`packaging/etc/remotex.toml.example`](packaging/etc/remotex.toml.example).

## Checks

```sh
cargo clippy --all-targets -- -D warnings
cargo test

cd frontend
bun run check
cd ..
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
The tarball contains the gateway binary and built frontend.
