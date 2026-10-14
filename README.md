# remotex

Remote desktop in a browser, for the remote desktop server each platform
already has. remotex is a small gateway you run next to your machines. It
connects to:

- **Windows 10 and 11's own Remote Desktop**, over RDP with its modern graphics
  pipeline.
- **macOS's built-in Screen Sharing**, in both of its modes: Standard and High
  Performance.
- **Linux**, through [wlshare](https://github.com/andrewtheguy/wlshare), our own
  VNC server for wlroots compositors.
- Other RDP and VNC servers, through each protocol's baseline.

Windows and macOS need nothing installed on the host, only their own sharing
switched on. The gateway serves one web page that shows the desktop, so anything
with a browser, on Windows, macOS, Linux, iOS or Android, reaches every one of
them with no client to install either.

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

## Demo with Two Remote Virtual Displays

https://github.com/user-attachments/assets/596640b5-9b35-45c3-a875-87934d1c0c0f




## Features

- **One browser client.** It needs WebCodecs and nothing else: Chrome, Edge or
  Safari on a desktop, Chrome on Android, and Safari on an iPhone or iPad.
  Firefox runs it too, but its native decoders take neither a High Performance
  Mac's HEVC nor its AAC-ELD, so there the Mac's picture is its H.264, or its
  HEVC in the page's software decoder. For desktop use, install the page as a Chrome or
  Edge app.
- **Remote-sized, density-aware display.** RDP with its default graphics
  pipeline, wlshare and a Mac's virtual display can follow the browser window at
  its screen density. A Mac's physical displays are shown at the density they
  report. Standard VNC cannot report density and is shown at 1x.
- **Two resizable virtual displays on a High Performance Mac (alpha).** Each
  display has a browser tab of its own, and each tab's window sizes its
  display. Apple's own viewer on macOS 26 does not offer this: with two virtual
  displays it reports that dynamic resolution is not available.
- **Adaptive VP9, or the remote's own picture path.** The gateway's VP9 follows
  the browser link; wlshare's passed VP9 does too. A Windows host's graphics
  pipeline is passed for the browser to compose, drawn in high quality for a LAN or, in
  alpha, as H.264 the host fits to the browser link. A High Performance Mac's
  HEVC or its H.264 is always passed unchanged, and neither adapts to the
  browser link: a still screen sends little, motion much. The H.264 needs no
  HEVC decoder in the page, so every build serves a High Performance Mac to any
  browser.
- **Remote sound in the browser, as adaptive Opus or lossless FLAC.** An RDP
  or wlshare session plays the remote's sound here, as Opus at a bitrate that
  follows the browser link or as FLAC, lossless for a LAN. A High Performance
  Mac always sends its AAC-ELD sound. Standard Screen Sharing and plain VNC
  carry none.
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

## Supported servers

remotex is built around the remote desktop servers native to each platform:
Windows' own Remote Desktop, macOS's built-in Screen Sharing, and on Linux
[wlshare](https://github.com/andrewtheguy/wlshare), our own. Windows and macOS
come first: they are what most people connect to, so features are tested and
optimized on them first. wlshare follows them, though it integrates the most
seamlessly of the three, since what RFB lacks is added to it.

- **Windows 10 and 11's Remote Desktop** (`rdp`). Follows the window at its
  density and carries Opus or FLAC sound, with the camera and microphone beside
  it. Its picture is its graphics pipeline, passed through, high quality or, in
  alpha, adaptive H.264.
- **A Mac's High Performance Screen Sharing** (`vnc`,
  `subtype = "ard-high-performance"`). Follows the window at its density, on
  one virtual display or two, and always carries AAC-ELD sound. Its picture is
  its HEVC or its H.264 passed through, which the Mac sends at its own rate
  rather than the browser link's: little on a still screen, much with motion.
  The H.264 needs nothing added, and the HEVC a browser that decodes it or the
  page's software decoder.
- **A Mac's Standard Screen Sharing** (`vnc`, `subtype = "ard"`). The Mac's
  physical displays, one or all, at their own size and density, as VP9 from the
  gateway, with no sound, camera or microphone.
- **wlshare on Linux** (`vnc`, `subtype = "wlshare"`). Because the server is
  ours, what RFB lacks is added to it: it switches outputs, follows the window
  at its density, carries Opus or FLAC sound and the browser's camera and
  microphone, and codes its own VP9 for the gateway to pass through, adapting
  to the browser's link. Two outputs can be shown at once, the second in a
  browser tab of its own (alpha).

Every other VNC server is a plain `vnc` target reached through the RFB
baseline: one framebuffer at 1x, not resized, no sound, VP9 from the gateway.
Another RDP server may happen to work but is not tested against. See
[Prioritized servers](docs/architecture.md#prioritized-servers) for the
comparison table, what they share, and what each passes.

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

- [Using remotex](docs/guide.md): the browser client, starting a session, setting up Windows, Mac, wlshare and VNC targets, `remotex tui`
- [Installing remotex](docs/install.md): packages, first configuration, upgrade, removal
- [`remotex.example.toml`](remotex.example.toml): every configuration key, annotated
- [Known issues](docs/known-issues.md): faults worth recognising rather than re-investigating
- [Roadmap](docs/roadmap.md): what is planned, and what is not
- [Architecture](docs/architecture.md): the design and its constraints, the client protocol, the engines
- [The RDP client](docs/rdp-client.md), [Apple RFB 003.889](docs/apple-vnc-889.md): each protocol as implemented and measured
- [Development](docs/development.md): running from a checkout, checks, end-to-end tests, builds
- [Packaging](packaging/README.md): package layouts, build scripts, releases

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
