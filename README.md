# zDesk

English | [简体中文](README.zh-CN.md)

Remote desktop in a browser, for the remote desktop server each platform
already has. zDesk is a small gateway you run next to your machines. It
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
browser ── HTTPS or localhost ── zDesk ── RDP / VNC ── your desktops
```

`zdesk serve` is single-user: one login, a list of your targets, and one
active session. Native installs also include [`zdesk tui`](docs/guide.md#local-instances),
a local control plane that runs several independent gateway instances side by
side.

> [!WARNING]
> **No backward compatibility.** zDesk is still settling its
> [direction](docs/architecture.md#direction), and a release may change or
> remove a configuration key, the WebSocket protocol or a feature, with nothing
> that reads the old form. Check [`zdesk.example.toml`](zdesk.example.toml) against your config
> when you upgrade. The page and the gateway are one release too: a tab left
> open across an upgrade names both versions and asks to be reloaded.

## Demo

### Windows -> Mac with One Remote Virtual Display

https://github.com/user-attachments/assets/982bf2d0-3fac-4a3c-8213-27412d10977d



### Windows -> Mac with Two Remote Virtual Displays

- Windows -> Mac with Two Remote Virtual Displays shown in separate browser tabs with a window being able to be dragged across displays right between the two tabs.

https://github.com/user-attachments/assets/a31030c8-dab9-467c-9231-20329e138e7d




### iOS -> Windows
https://github.com/user-attachments/assets/21b869ad-d353-4c64-aac2-9b980893dd2f



### iOS -> Mac


https://github.com/user-attachments/assets/cde4e192-340f-47dd-86db-3d2d89049007



### Mac -> Windows at Retina/HiDPI Density with Audio

- Windows' own Remote Desktop shown at the Mac's Retina/HiDPI density, with the remote's sound playing in the browser.

https://github.com/user-attachments/assets/3dfd0dc2-dc7d-4d41-b99e-1340f685390c





### Mac -> Linux (wlshare on Sway) at Retina/HiDPI Density with Audio

- wlshare on Sway shown at the Mac's Retina/HiDPI density, with the remote's sound, and the remote desktop resizing to follow the browser into and out of immersive full screen.

https://github.com/user-attachments/assets/68b73718-9406-4003-a374-2829f2536395





## Features

- **One browser client, nothing to install.** Chrome, Edge or Safari on a
  desktop, Chrome on Android, and Safari on an iPhone or iPad. For desktop use,
  install the page as an app from Chrome's or Edge's menu, so it opens in a
  window of its own.
- **Remote-sized, density-aware display.** The remote desktop follows the
  browser window at its screen density, so it is sharp on a Retina or HiDPI
  display and shown at its true size.
- **Two displays at once (alpha).** On Windows, a High Performance Mac and
  wlshare, the second display opens in a browser tab of its own. In a session
  that follows the window, each tab's window sizes its display if it is virtual
  or headless; a physical monitor keeps its own mode. Apple's own viewer on macOS 26 does not offer this:
  with two virtual displays it reports that dynamic resolution is not available.
- **The remote's own picture, decoded in the browser, or the gateway's tiles.**
  A Windows host's graphics pipeline is passed for the browser to compose, drawn
  in high quality for a LAN or, in alpha, as H.264 the host fits to the browser
  link. A High Performance Mac's HEVC or its H.264 is passed unchanged. A
  Standard Mac's own Adaptive picture is passed (beta) and paced by the browser
  link: a slower link gets fewer updates and later refinement at the same
  precision. wlshare's passed VP9 follows the browser link too. Where the
  gateway codes the picture, it sends 64×64 tiles, exact as PNG where they hold
  few colours and as JPEG where they move, sent again exactly once still, as
  fast as the browser paints them, which settle to lossless.
- **Remote sound in the browser**, as Opus at a bitrate that follows the browser
  link, as lossless FLAC for a LAN, or as a High Performance Mac's own AAC-ELD.
- **Clipboard text in both directions**, synced automatically where the browser
  permits it and through the Clipboard panel where it does not.
- **Camera and microphone redirection (experimental).** An RDP or wlshare
  session started with them at the picker receives the browser's devices for a
  call or recording.
- **Reattachment without losing the remote desktop.** The owning browser has a
  60-second grace period to reattach after a reload or dropped socket. A takeover
  or later connection starts a fresh gateway session, but the prioritized
  servers keep their desktop running, so reconnecting to the target returns to
  the windows left there.
- **Mouse, keyboard and a touch-screen interface.** The page installed as a
  Chrome or Edge app receives the key chords a tab keeps for the browser. On a phone or tablet, fingers act
  as a trackpad with pinch zoom and a soft keyboard.

## Supported servers

zDesk is built around the remote desktop servers native to each platform:
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
  physical displays, one or all, at their own size and density, as the
  gateway's tiles of the Mac's lossless Full quality or as the Mac's own Adaptive
  picture passed through (beta), with no sound, camera or microphone.
- **wlshare on Linux** (`vnc`, `subtype = "wlshare"`). Because the server is
  ours, what RFB lacks is added to it: it switches outputs, follows the window
  at its density, carries Opus or FLAC sound and the browser's camera and
  microphone, and codes its own VP9 for the gateway to pass through, adapting
  to the browser's link — or, for a link with room to spare, its own exact
  tiles with a video stream for each region that moves, H.264 from hardware or
  libx264, with VP9 as the fallback. Two outputs can be shown at once, the second
  in a browser tab of its own (alpha).

Every other VNC server is a plain `vnc` target reached through the RFB
baseline: one framebuffer at 1x, not resized, no sound, tiles from the gateway.
Another RDP server may happen to work but is not tested against. See
[Prioritized servers](docs/architecture.md#prioritized-servers) for the
comparison table, what they share, and what each passes.

## Limitations

What each server and browser does not do, in one place.

| | Windows Remote Desktop | Mac, High Performance | Mac, Standard | wlshare | Other VNC |
|---|---|---|---|---|---|
| Resizes to the browser window | ✅ with the graphics pipeline (default) | ✅ | ❌ physical displays at their own size | ✅ headless outputs only | ❌ the server's size |
| Sharp at HiDPI/Retina density | ✅ | ✅ | ✅ at the density the Mac reports | ✅ | ❌ 1x only |
| Second display in its own tab | ⚠️ alpha, with the graphics pipeline | ⚠️ alpha | ❌ all displays combined in one tab | ⚠️ alpha, with two outputs | ❌ |
| Sound | ✅ Opus or FLAC | ✅ AAC-ELD, always | ❌ | ✅ Opus or FLAC | ❌ |
| Camera and microphone | ⚠️ experimental | ❌ | ❌ | ⚠️ experimental | ❌ |
| Unicode clipboard | ✅ | ✅ | ✅ | ✅ | ⚠️ with Extended Clipboard, else Latin-1 |
| Picture paced to the browser link | ✅ | ❌ the Mac's own rate | ✅ | ✅ | ✅ |
| Picture settles to lossless | ⚠️ only when the host's policy asks | ❌ | ✅ with Full quality | ⚠️ with *Tiles with motion* | ✅ |

| | Chrome, Edge | Safari | Firefox |
|---|---|---|---|
| A High Performance Mac's HEVC | ✅ native; on Windows only where the GPU decodes HEVC 4:4:4, else hevc-wasm | ✅ native | ⚠️ hevc-wasm only |
| A High Performance Mac's sound (AAC-ELD) | ✅ | ✅ except 16.4 to 18, which play no sound at all | ❌ |
| Lossless FLAC sound | ✅ | ❌ Opus only | ✅ |
| Key chords the browser keeps for itself (⌘W, ⌘T, ⌘N) | ✅ in an app window or Immersive full screen | ❌ | ❌ |

hevc-wasm is the page's software HEVC decoder, served by a gateway with
`[hevc_wasm]` set. Where neither it nor the browser decodes the HEVC, the
picker offers the Mac's H.264.

An app window is the page installed from the browser's menu: in Chrome, ⋮ →
*Cast, save and share* → *Install page as app*; in Edge, ⋯ → *Apps* → *Install
this site as an app*. It opens in a window of its own, without tabs or an address
bar, and receives ⌘W, ⌘T, ⌘N and the like. *Menu → Immersive full screen* in a
tab does the same and adds Alt+Tab and the Super key.

- **A secure context is required:** HTTPS, or `localhost` directly or through an
  SSH tunnel. A LAN address over plain HTTP is refused.
- **WebCodecs carries video streams and sound.** Without it, only the gateway's
  tiles, a Standard Mac's Adaptive picture and an RDP pipeline without H.264
  work, with no sound.
- **A High Performance Mac's picture does not adapt to the browser link:** a
  still screen sends little, motion much.
- **One active session per gateway.** A takeover ends it and starts at the
  picker.
- **Touch is a trackpad:** fingers are not sent to the remote as touchscreen
  contacts.
- **Other RDP servers** may work but are not tested against.

## Quick start

Install the package for your platform from the
[latest release](https://github.com/andrewtheguy/zdesk/releases/latest). On
Debian 13 or Ubuntu 24.04 and later:

```sh
curl -fsSLO https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-linux-amd64.deb
sudo apt install ./zdesk-linux-amd64.deb
```

Create the config from the shipped example and generate a login:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/zdesk /var/lib/zdesk
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/zdesk/zdesk.example.toml /etc/zdesk/zdesk.toml
zdesk gen-passwd admin
```

Put the generated value and a target in `/etc/zdesk/zdesk.toml`, with the
file the gateway reads its master password from:

```toml
[server]
site_passwd = "admin:$2b$..."
master_password_file = "master-password"

[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password_ref = "workstation"
```

Then write the master password to that file and paste in the target's
password, encrypted under it (see
[Target passwords](docs/guide.md#target-passwords)): `password master` prints
a `[master]` block for the config, and `password encrypt` prints the value to
add as `workstation = "…"` under `[passwords]`.

```sh
(umask 077; trap 'stty echo' EXIT; trap 'exit 1' INT HUP TERM; stty -echo; printf 'Master password: '; IFS= read -r pw; echo; printf '%s' "$pw" > /etc/zdesk/master-password)
zdesk password master
zdesk password encrypt
```

Start it and open <http://localhost:52380>:

```sh
zdesk serve
```

The page needs a secure context, so from another machine put the gateway behind
a TLS-terminating reverse proxy; `localhost` works as it is.

There are also `.rpm`, macOS `.pkg` and Windows `.msi` packages, and a container
image. Its config sets
`[server].master_password_file = "/run/secrets/zdesk_master"` and the master
password is mounted there; [Container](docs/install.md#container) shows how to
create that file and print the config's `[master]` and `[passwords]` with the
image:

```sh
docker run -d --name zdesk -p 52380:52380 \
  -e ZDESK_LISTEN=0.0.0.0:52380 \
  -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro \
  -v ./zdesk_master:/run/secrets/zdesk_master:ro \
  ghcr.io/andrewtheguy/zdesk:latest
```

[Installing zDesk](docs/install.md) covers every platform, upgrades and
removal.

## Documentation

- [Using zDesk](docs/guide.md): the browser client, starting a session, setting up Windows, Mac, wlshare and VNC targets, `zdesk tui`
- [Installing zDesk](docs/install.md): packages, first configuration, upgrade, removal
- [`zdesk.example.toml`](zdesk.example.toml): every configuration key, annotated
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
cp zdesk.example.toml zdesk.toml
cargo run -- gen-passwd admin
# Set [server].site_passwd, configure a target and its passwords (see below), then:
cargo run -- serve -c zdesk.toml
```

The copied example names `[passwords].example` but leaves it for you to add.
Set up the target's encrypted password and master-password file as described in
[Target passwords](docs/guide.md#target-passwords). For a target with an empty
password, add `example = ""` under `[passwords]` instead; it needs no master
password.

Cargo builds the frontend and compiles it into the binary, so the gateway is
the only thing that serves the page. The checks are:

```sh
cargo clippy --all-targets -- -D warnings
cargo test --lib
bun run --cwd frontend check
```

The self-contained integration tests under `tests/` run with `cargo test --tests`.
The container-backed VNC and wlshare tests are ignored by default because they
need Docker or Podman:

```sh
cargo test --test vnc_e2e --test wlshare_e2e -- --ignored
```

The RDP probes borrow a real Windows host, so they are examples run
on demand rather than tests (`cargo run --example rdp_client_probe -- <case>`),
and the headless browser tests are described in
[`tests/playwright`](tests/playwright/README.md).
[Development](docs/development.md) has the details, and
[Packaging](packaging/README.md) covers release builds.

## Licence

zDesk is under the MIT licence in [`LICENSE`](LICENSE).
