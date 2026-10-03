# remotex

Remote desktop in a browser. remotex is a small gateway you run next to your
machines: it speaks RDP and VNC to them and serves one web page that shows the
desktop. Anything with a browser, on Windows, macOS, Linux, iOS or Android,
reaches every one of them, with no client to install.

```text
browser ── HTTPS or localhost ── remotex ── RDP / VNC ── your desktops
```

It is a single-user tool: one login, a list of your targets, one session at a
time.

> **No backward compatibility.** A release may change or remove a configuration
> key or a feature, with nothing that reads the old form. Check
> [`remotex.example.toml`](remotex.example.toml) against your config when you
> upgrade.

## Features

- **A client on every OS.** The client is a web page, so it runs on Windows,
  macOS, Linux, iOS and Android, with nothing to install: Chrome or Edge on a
  desktop and on Android, and Safari on an iPhone or iPad.
- **A sharp desktop at its true size**, HiDPI and Retina included. The remote
  is asked to render at your window's size and your screen's density, rather
  than being scaled in the browser.
- **A picture that follows the link**, encoded as VP9, or on a LAN the remote's
  own stream passed through untouched.
- **Sound**, as Opus or lossless.
- **A shared clipboard.**
- **Your camera and microphone** on the remote, for a call (experimental).
- **Sessions that survive you.** Reload, drop the connection or pick up another
  device: the remote desktop is where you left it.
- **Desktop and touch.** Install the page as an app in Chrome or Edge to get the
  key chords a tab keeps for itself; on a phone or tablet, fingers are a
  trackpad with pinch zoom and a soft keyboard.

## Supported remotes

remotex is built around the remote desktop server each platform already has:

| Remote | How |
|---|---|
| **Windows 10 and 11** | Windows' own Remote Desktop, over RDP |
| **macOS** | the built-in Screen Sharing, in Standard and High Performance modes, with nothing installed on the Mac |
| **Linux (wlroots Wayland)** | [wlshare](https://github.com/andrewtheguy/wlshare), this project's own VNC server |
| **Anything else** | any standard VNC server |

Design, testing and optimization start from the first three, ranked in
[tiers](docs/architecture.md#server-tiers). Other VNC servers work through the
protocol's baseline; other RDP servers may work and are not tested against.

## Quick start

Install the package for your platform from the
[latest release](https://github.com/andrewtheguy/remotex/releases/latest). On
Debian or Ubuntu:

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-linux-amd64.deb
sudo apt install ./remotex-linux-amd64.deb
```

Create the config from the shipped example and generate a login:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/remotex /var/lib/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/remotex/remotex.example.toml /etc/remotex/remotex.toml
remotex gen-passwd admin
```

Put the generated value and a target in `/etc/remotex/remotex.toml`:

```toml
[server]
site_passwd = "admin:$2b$..."

[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password = "change-me"
```

Start it and open <http://localhost:52380>:

```sh
remotex serve
```

The page needs a secure context, so from another machine put the gateway behind
a TLS-terminating reverse proxy; `localhost` works as it is.

There are also `.rpm`, macOS `.pkg` and Windows `.msi` packages, and a container
image:

```sh
docker run -d --name remotex -p 52380:52380 \
  -e REMOTEX_LISTEN=0.0.0.0:52380 \
  -v ./remotex.toml:/opt/remotex/etc/remotex.toml:ro \
  ghcr.io/andrewtheguy/remotex:latest
```

[Installing remotex](docs/install.md) covers every platform, upgrades and
removal.

## Documentation

| | |
|---|---|
| [Using remotex](docs/guide.md) | the browser client, starting a session, setting up Windows, Mac, wlshare and VNC targets, `remotex tui` |
| [Installing remotex](docs/install.md) | packages, first configuration, upgrade, removal |
| [`remotex.example.toml`](remotex.example.toml) | every configuration key, annotated |
| [High Performance decoder](docs/high-performance-decoder.md) | the FFmpeg library a High Performance Mac target needs |
| [Known issues](docs/known-issues.md) | faults worth recognising rather than re-investigating |
| [Roadmap](docs/roadmap.md) | what is planned, and what is not |
| [Architecture](docs/architecture.md) | the design and its constraints, the client protocol, the engines |
| [The RDP client](docs/rdp-client.md), [Apple RFB 003.889](docs/apple-vnc-889.md) | each protocol as implemented and measured |
| [Packaging](packaging/README.md) | package layouts, build scripts, releases |

## Development

You need Rust with rustup and [Bun](https://bun.sh).

```sh
bun install --cwd frontend
cp remotex.example.toml remotex.toml
cargo run -- gen-passwd admin
# Paste the generated credential into remotex.toml, then:
cargo run -- serve -c remotex.toml
```

Cargo builds the frontend and compiles it into the binary, so the gateway is
the only thing that serves the page. The checks are:

```sh
cargo clippy --all-targets -- -D warnings
cargo test --lib
bun run --cwd frontend check
```

The end-to-end tests under `tests/` are ignored by default: the VNC and wlshare
ones need Docker or Podman (`cargo test --test vnc_e2e --test wlshare_e2e --
--ignored`), the RDP probes borrow a real Windows host, and the headless browser
tests are described in [`tests/playwright`](tests/playwright/README.md).
[Packaging](packaging/README.md) covers release builds.

## Licence

remotex is under the MIT licence in [`LICENSE`](LICENSE).
