# remotex

Remote desktop in a browser. remotex is a small gateway you run next to your
machines: it speaks RDP and VNC to them and serves one web page that shows the
desktop. Anything with a browser, on Windows, macOS, Linux, iOS or Android,
reaches every one of them, with no client to install.

```text
browser ── HTTPS or localhost ── remotex ── RDP / VNC ── your desktops
```

`remotex serve` is single-user: one login, a list of your targets, and one
active session. Native installs also include [`remotex tui`](docs/guide.md#local-instances),
a local control plane that runs several independent gateway instances side by
side.

> **No backward compatibility.** A release may change or remove a configuration
> key, the WebSocket protocol or a feature, with nothing that reads the old
> form. Check [`remotex.example.toml`](remotex.example.toml) against your config
> when you upgrade. The page and the gateway are one release too: a tab left
> open across an upgrade names both versions and asks to be reloaded.

## Features

- **One browser client.** It needs WebCodecs and nothing else: Chrome, Edge or
  Safari on a desktop, Chrome on Android, and Safari on an iPhone or iPad.
  Firefox runs it too, but its native decoders take neither a High Performance
  Mac's HEVC nor its AAC-ELD. For desktop use, install the page as a Chrome or
  Edge app.
- **Remote-sized, density-aware display.** RDP with its default graphics
  pipeline, wlshare and a Mac's virtual display can follow the browser window at
  its screen density. A Mac's physical displays are shown at the density they
  report. Standard VNC cannot report density and is shown at 1x.
- **Adaptive VP9, or the remote's own picture path.** The gateway's VP9 follows
  the browser link; wlshare's passed VP9 does too. For a LAN, a High Performance
  Mac's HEVC can instead pass unchanged, or a Windows host's graphics pipeline
  can pass for the browser to compose; those two do not adapt to the browser
  link.
- **Remote sound where the server provides it.** RDP and wlshare sessions choose
  off, Opus or FLAC, which is lossless. A High Performance Mac always sends
  its AAC-ELD sound. Standard Screen Sharing and plain VNC carry none.
- **Clipboard text in both directions.** RDP and Apple Screen Sharing carry
  Unicode. VNC carries Unicode with Extended Clipboard and otherwise falls back
  to Latin-1. Automatic browser clipboard sync is used where the browser permits
  it; the Clipboard panel remains available when it does not.
- **Experimental camera and microphone redirection.** An RDP or wlshare target
  can receive the browser's devices for a call or recording.
- **Reattachment without losing the remote desktop.** The owning browser has a
  60-second grace period to reattach after a reload or dropped socket. A takeover
  or later connection starts a fresh gateway session, but the prioritized
  servers keep their desktop running, so reconnecting to the target returns to
  the windows left there.
- **Mouse and keyboard input, with a touch-screen interface.** An installed
  Chrome or Edge app receives the key chords a tab reserves. On a phone or
  tablet, fingers act as a trackpad with pinch zoom and a soft keyboard; they
  are not sent to the remote as touchscreen contacts.

## Supported remotes

remotex is built around the native remote desktop server on Windows and macOS,
and around [wlshare](https://github.com/andrewtheguy/wlshare), this project's
wlroots VNC server on Linux. They are ranked by how the picture reaches the
browser:

| Tier | Remote | Target | Picture |
|---|---|---|---|
| **1** | wlshare on Linux | `vnc`, `subtype = "wlshare"` | wlshare's adaptive VP9, passed through the gateway |
| **2** | Windows 10 and 11 Remote Desktop | `rdp` | adaptive VP9 from the gateway, or the host's graphics pipeline passed for browser composition |
| **2** | macOS Screen Sharing, High Performance | `vnc`, `subtype = "ard-high-performance"` | adaptive VP9 from the gateway, or the Mac's HEVC passed through |
| **3** | macOS Screen Sharing, Standard | `vnc`, `subtype = "ard"` | adaptive VP9 from the gateway |
| baseline | any standard VNC server | `vnc` | adaptive VP9 from the gateway, presented at 1x |

Tier 1 is the preferred path: wlshare encodes the desktop itself and adjusts it
from feedback that reaches the browser, so the gateway does no video encoding
and a slow browser link still lowers the stream. Tier 2 can pass the remote's
own picture path to remove most gateway video work on a LAN. Neither passed path
has the gateway's browser-link quality walk: the Mac's rate feedback stops at
the gateway, and the Windows pipeline is sent as the host draws it. Starting the
same target without passthrough uses the gateway's adaptive VP9. Tier 3 always
decodes the remote picture and encodes it again as VP9.

Design, testing and optimization follow that order when work competes. Standard
VNC remains supported through the RFB baseline but has no remotex extensions for
density, sound, camera or microphone. Other RDP servers may work if they speak
what the built-in client implements, but they are not tested. See
[server tiers](docs/architecture.md#server-tiers) for the design rules behind
the ranking.

## Quick start

Install the package for your platform from the
[latest release](https://github.com/andrewtheguy/remotex/releases/latest). On
Debian 13 or Ubuntu 24.04 and later:

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
| [Development](docs/development.md) | running from a checkout, checks, end-to-end tests, builds |
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
[Development](docs/development.md) has the details, and
[Packaging](packaging/README.md) covers release builds.

## Licence

remotex is under the MIT licence in [`LICENSE`](LICENSE).
