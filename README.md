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
for rather than scaled on the client; plain `vnc`, a High Performance Mac and
`rdp` can all be handed the window. On RDP a resize is a graphics reset of the
default graphics pipeline, so `resize = true` is refused beside `egfx = false`.

- RDP uses a built-in client, protocol and all: the desktop over the graphics
  pipeline (MS-RDPEGFX) or plain bitmap updates, pointer, keyboard, mouse and
  resize, spoken to a current Windows host over NLA — tested on Windows 10 and 11,
  not on older Windows or xrdp. It carries the clipboard and
  sound (MS-RDPEA), and does not carry touch. See
  [`docs/rdp-client.md`](docs/rdp-client.md).
- VNC uses a built-in RFB client and connects directly to macOS Screen Sharing.
  `subtype = "ard"` selects Apple Screen Sharing's Standard mode over RFB 3.8
  with Apple Remote Desktop authentication.
  `subtype = "ard-high-performance"` is its High Performance mode as Apple's
  viewer has it: RFB 003.889 on one virtual display holding every remote window,
  with the picture as HEVC and the sound as AAC-ELD over the Mac's SRTP media
  stream, in a gateway built with the `apple-hp-media` feature. Only it accepts
  `resize = true`. It is reverse engineered, having no specification.
  A wlroots-based Wayland desktop behind
  [wlshare](https://github.com/andrewtheguy/wlshare) is a plain `vnc` target:
  that server carries pixel density over one private RFB extension the gateway
  asks every generic server for, so the output's scale is reported and shown as
  such, and with `resize = true` the output follows the browser's density.

There is one client: the page a browser loads. For desktop use, install that page
as an app in Chrome or Edge. The app window gives the client the browser-reserved
key chords that a normal windowed tab keeps for itself, without a separate native
wrapper or a second client lifecycle.

An installed desktop browser app has **Window → Size to _width_×_height_**. It
uses [`window.resizeTo()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/resizeTo)
to make the app's content area exactly the remote desktop's logical size; it
resizes the local window and never scales or resizes the remote desktop. This is
supported in installed Chrome and Edge desktop app windows and is best-effort in
other browsers. Ordinary tabs cannot resize their containing browser window, and
mobile browsers ignore the request.

On macOS, the window's own rounded corners are masked over the page, so a window
sized to the desktop loses the framebuffer's corner pixels — a desktop shown at
100% has nothing to spare there. The corner radius is a system-wide setting
rather than a per-app one; one point is square enough to see every pixel:

```sh
defaults write -g NSConvolutionOverride1 -float 1
```

Windows pick the value up when their app next launches, so quit and reopen the
browser app. `defaults delete -g NSConvolutionOverride1` restores Apple's radius
— 26 points on macOS 26, 10 before it.

See [`docs/architecture.md`](docs/architecture.md) for the system design and
[`docs/known-issues.md`](docs/known-issues.md) for faults worth recognising rather
than re-investigating.

## Install

The complete annotated configuration is
[`remotex.example.toml`](remotex.example.toml).

Native packages are the supported install method. Download the matching asset
from the [latest release](https://github.com/andrewtheguy/remotex/releases/latest).

Debian or Ubuntu, on x86-64 (use the `arm64` asset on arm64):

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-linux-amd64.deb
sudo apt install ./remotex-linux-amd64.deb
```

Fedora, RHEL, or another RPM-based distribution, on x86-64 (use the `arm64`
asset on arm64, and your distribution's own RPM frontend in place of `dnf`):

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-linux-amd64.rpm
sudo dnf install ./remotex-linux-amd64.rpm
```

macOS arm64:

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-macos-arm64.pkg
sudo installer -pkg remotex-macos-arm64.pkg -target /
```

The macOS package is unsigned and not notarized, so fetch it with `curl` as
shown rather than through a browser. It installs the gateway CLI, with the web
client compiled into it.

Windows x86-64, from PowerShell 7 (`pwsh`) run as administrator:

```powershell
Invoke-WebRequest https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-windows-x86_64.msi -OutFile remotex-windows-x86_64.msi
msiexec /i remotex-windows-x86_64.msi
```

The MSI is unsigned, so SmartScreen asks first. It installs the gateway under
`%ProgramFiles%\remotex`, puts `bin` on the machine `PATH`, and reads
`%ProgramData%\remotex\remotex.toml`. `remotex tui` is not available on Windows.

Packages do not own the live config because it contains credentials. On Linux,
create it for the account that will run the gateway:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/remotex/remotex.example.toml /etc/remotex/remotex.toml
remotex gen-passwd admin
${EDITOR:-vi} /etc/remotex/remotex.toml
```

On macOS, use `/usr/local/etc/remotex/remotex.toml` and the example under
`/usr/local/share/doc/remotex/` instead; on Windows,
`%ProgramData%\remotex\remotex.toml` and the example under
`%ProgramFiles%\remotex\share\doc\remotex\`. Paste the generated `admin:$2b$...`
value into `[server].site_passwd`, replace the example `[[targets]]` entry, then
start the server in the foreground:

```sh
remotex serve
```

See [`docs/install.md`](docs/install.md) for package upgrades, removal, and macOS
config setup.

## Local instances

Native installs also include a multi-instance terminal control plane:

```sh
remotex tui
```

It listens only on both loopbacks, at `serve`'s port: 52380 unless `--port` or
`REMOTEX_TUI_PORT` says otherwise, and never a port the kernel picked, because
this is a number you type into a browser. A port something else is already
serving refuses the start rather than answering on half of it.
Open <http://remotex.localhost:52380>; each
running instance has its own origin at
`http://<instance>.remotex.localhost:52380`. Press `n` to create an instance,
`e` to edit its `remotex.toml`, Enter to start or stop it, `r` to restart it,
and `q` to stop every child and quit.

Each immediate subdirectory is one instance. The default root is
`~/.local/share/remotex/instances` on Linux (or
`$XDG_DATA_HOME/remotex/instances`) and
`~/Library/Application Support/remotex/instances` on macOS; pass
`--instances-dir` to choose another. Its config uses the same `[branding]` and
`[[targets]]` format as the former native viewer and deliberately has no
`[server]` block. The supervisor owns the shared TCP port and proxies each
subdomain to that child's private `<instance>/gateway.sock`.

Macs can be configured as ordinary VNC targets using macOS Screen Sharing, with
no additional software. Use `protocol = "vnc"` with `subtype = "ard"` and the Mac
account's username and password; that selects Apple Remote Desktop authentication
so the connection lands at the user's own screen rather than a login-window
session.

The Mac must grant that account Observe and Control in Remote Management's
per-user access list. The default "All users" setting rejects the connection
with the same authentication error as incorrect credentials. See
[Remote Management access](docs/apple-vnc-889.md#remote-management-access).

Apple Screen Sharing Standard mode (`ard`) lists the Mac's physical screens, can
show one screen or all of them, reports each screen's pixel density, keeps pixels
at full fidelity, and supports the native Apple pasteboard. Every Apple subtype
asks the Mac for zlib rectangles from the start (around fifty times fewer bytes
than raw on a static desktop).

High Performance (`ard-high-performance`) takes the same credentials and speaks
Apple's encrypted record-layer revision. It requests one virtual display at the
pinned `width` and `height` when both are set, or at the full resolution and
density of the client's screen otherwise. Once connected, it disables the remote
Mac's physical displays and puts all of the remote Mac's windows on that virtual
display. Apple's official macOS Screen Sharing client can instead choose up to
two virtual displays. It takes the picture and sound Apple's own viewer takes:
after the first layout the gateway offers the Mac's media stream, and the Mac
then sends the screen as HEVC 4:4:4 and its sound as AAC-ELD, over UDP with
SRTP, to the gateway's ports 5900 and 5901. The gateway authenticates and
decrypts every packet, decodes both, and sends them on as the VP9 and Opus every
target uses; zlib carries the picture only until the stream does. A playing
video does not delay the Mac's reading of the input, as zlib's deflate does. The Mac refuses the picture without the sound, and
mutes its own speakers while it streams, so the target always carries sound and
never uses AirPlay. It is **experimental** and needs a gateway built with
`--features apple-hp-media`, which links FFmpeg's HEVC decoder (LGPL) and Fraunhofer's
AAC-ELD decoder (licence not OSI-approved); no release artifact carries it. See
[The media stream](docs/apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).

Every Apple subtype supports the native Apple pasteboard when `clipboard =
true`. With `resize = true`, the window continuously drives High Performance's
virtual display, using Apple's
dynamic-resolution feature to replace its mode from client viewport reports.
There is no client-side control for resizing the remote: no auto-resize toggle or
one-shot remote-resize button. The local app-window sizing control described above
does not change that policy. The descriptor's fixed 3840×2160 backing ceiling
permits successive arbitrary sizes within that bound, and every fresh connection
turns the Mac's Dynamic resolution setting back on. Standard `ard` still refuses
resize, and the one/two-virtual-display control is not implemented.
See [`docs/apple-vnc-889.md`](docs/apple-vnc-889.md).

A plain VNC server has no way to say its pixels are HiDPI — standard RFB carries
sizes in pixels and nothing else — so those targets are shown at 1x, one CSS
pixel per framebuffer pixel, and the window's points go to the server as pixels.
See [`docs/generic-vnc-hidpi.md`](docs/generic-vnc-hidpi.md) for what that means
on a sway output at scale 2 and why a second client's resize can come back
prohibited. The one exception is a server that answers the density request the
gateway puts in every generic `SetEncodings`, which today is wlshare: it reports
its output's scale, the gateway labels the framebuffer with it, and the
browser's density is declared back to the server together with the window in
points × that density, so the output changes mode and scale at once. See [`docs/wlshare-density.md`](docs/wlshare-density.md).

`audio = true` works on a plain VNC target too, through wlshare's audio
extension: the gateway lists its pseudo-encoding, wlshare announces so and then
streams the desktop's sound on the RFB connection itself as lossless FLAC. While
a client listens the host is silent: the desktop plays into a PipeWire sink of
wlshare's own, whose monitor is what is captured, and a server that does not
speak it — wayvnc, TigerVNC, x11vnc, QEMU — gives the desktop and no sound. See [`docs/wlshare-audio.md`](docs/wlshare-audio.md).

On a Mac, audio is **experimental**. An `ard-high-performance` target receives
it from Screen Sharing itself, beside the picture (above). No such path has been
measured in Standard mode, so for `ard` the gateway is
instead an AirPlay 1 speaker on the LAN, named after its branding with ` - remotex` after it, turned on for every Mac
by the gateway-wide `[airplay]` table and protected by its password; no Mac target
takes an `audio` key. The Mac picks the speaker once from its Sound menu, and what
it plays reaches whichever Mac session is running. The Mac must share the
gateway's link, since it finds the speaker by mDNS, and a video playing on the Mac
is heard about two seconds before it is seen. See
[`docs/airplay-audio.md`](docs/airplay-audio.md).

Two redirections send this browser's own media the other way and are
**experimental**, for lack of tests: `camera = true` offers the remote a virtual
webcam over MS-RDPECAM — or, on a generic `vnc` target, over wlshare's camera
extension, which makes it a PipeWire camera on the wlroots desktop (see
[`docs/wlshare-camera.md`](docs/wlshare-camera.md)) — and `microphone = true`
offers an RDP host a microphone over MS-RDPEAI, or a generic `vnc` target one over
wlshare's microphone extension, which makes it a PipeWire audio source on the
wlroots desktop (see [`docs/wlshare-microphone.md`](docs/wlshare-microphone.md)). They serve a different purpose from the rest of the session. The
screen and the remote's sound aim to match sitting at the desktop and spend the
bandwidth that takes on a fast link; the camera and the microphone are for
someone who needs one for a while — a call, a recording — and are sent as
cheaply as that allows on any link. The microphone goes as mono speech Opus at
16 kbit/s, which the gateway decodes to the PCM the host records in. Both are off
by default and enabled per session from the floating menu, never remembered, and
both are refused on Apple's Screen Sharing. A Windows host starts the microphone only once something on it
records. Their socket rules,
control messages and channel wire formats are tested like everything else, and
`a_real_host_records_the_microphone` feeds a host's recording device.
`a_real_host_streams_the_camera` in `tests/rdp_client_probe.rs` carries H.264
frames to a host's Camera app, but it is ignored by default and does not check
the pixels the host displays. The camera channel is created only by a Windows
host that redirects cameras — a workstation, or a Windows Server carrying the
Remote Desktop Session Host role. The picture is verified by hand there, where
remote audio (`audio = true`) and the rest of the RDP feature set are exercised on
every test run. Expect to re-check it by hand after a change.

High Performance's protocol revision is the one part of remotex built entirely without a specification: Apple
documents none of it — the revision, its record layer, its control messages, its
virtual display handling or its media stream — so all of it is reverse engineered and only as correct as the Macs it has been measured
against. A macOS update is free to change any of it. The dynamic-resolution
descriptor has been measured across its arbitrary-size boundary and a burst of
viewport reports, but remains reverse engineered.

## Container

```sh
docker run -d --name remotex -p 52380:52380 \
  -v ./remotex.toml:/opt/remotex/etc/remotex.toml:ro \
  ghcr.io/andrewtheguy/remotex:latest
```

Set `[server].listen = "0.0.0.0:52380"` in the mounted config, or pass the same
address as `-e REMOTEX_LISTEN=0.0.0.0:52380`. With `[meter].enabled` set, mount a volume
at `/opt/remotex/var` too, or the records go with the container. Images are
published for Linux amd64 and arm64 with `latest` and `v<version>` tags.

Generate the required web-login credential with:

```sh
docker run --rm -it ghcr.io/andrewtheguy/remotex:latest gen-passwd admin
```

## Development

Install the frontend dependencies once, then use Cargo for local development.
`cargo run` rebuilds the frontend when its sources change and compiles the
generated bundle into the gateway binary.

```sh
bun install --cwd frontend
cp remotex.example.toml remotex.toml
cargo run -- gen-passwd admin
# Paste the generated credential into remotex.toml, then:
cargo run -- serve -c remotex.toml
```

Open <http://localhost:52380>. Use `RUST_LOG=info` or `RUST_LOG=debug` for backend
logs. Use `cargo build` when you only need to compile without starting the
gateway.

The built frontend is embedded in the binary at compile time (`src/assets.rs`),
so `target/release/remotex` runs on its own with no `frontend/dist` beside it.
`build.rs` runs `bun run build` into Cargo's private output directory before the
crate compiles and fails the build if `index.html` is missing afterwards. A
release builder can name a platform-independent bundle that it built earlier:

```sh
bun run --cwd frontend build
REMOTEX_PREBUILT_FRONTEND=frontend/dist cargo build --release
```

The main directories are:

| Path | Contents |
|---|---|
| `src/` | gateway, session management, and RDP/VNC engines |
| `frontend/` | React SPA |
| `tests/` | protocol and engine end-to-end tests |
| `packaging/` | release, install, and container scripts |

## Configuration

remotex reads one TOML file. Native packages default to
`/etc/remotex/remotex.toml` on Linux and
`/usr/local/etc/remotex/remotex.toml` on macOS, and the container to
`/opt/remotex/etc/remotex.toml`; a checkout should pass `--config`.

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
physical displays, or `"ard-high-performance"` for one virtual display
containing all of its windows, with its physical displays disabled for the
connection and its picture and sound over the Mac's media stream — each with the Mac account's username and password. Keep the config mode `0600`; target
credentials remain server-side but are stored in this file.

All fields and per-protocol examples are in
[`remotex.example.toml`](remotex.example.toml).

## Checks

```sh
cargo clippy --all-targets -- -D warnings
cargo test

cd frontend
bun run check
cd ..
```

The container-backed VNC test uses Docker or Podman and does not start a
browser. It is ignored by default; run it explicitly with:

```sh
cargo test --test vnc_e2e -- --ignored
```

RDP has no container to test against: the gateway's RDP client speaks NLA to a
current Windows host and nothing else, so its end-to-end tests borrow a real
machine — see [`tests/rdp_proto_probe.rs`](tests/rdp_proto_probe.rs) and
[`tests/rdp_client_probe.rs`](tests/rdp_client_probe.rs).

Stable headless browser checks for DOM/control-plane flows live under
[`tests/playwright`](tests/playwright/README.md). They intentionally do not
assert framebuffer/canvas output, cursor rendering, or gesture timing.

For a remote Podman connection:

```sh
CONTAINER_CONNECTION=workstation-wsl \
REMOTEX_TEST_CONTAINER_HOST=<engine-host> \
cargo test --test vnc_e2e -- --ignored
```

`CONTAINER_CONNECTION` is the Podman system connection name.
`REMOTEX_TEST_CONTAINER_HOST` is the engine host's IP address or DNS name as
reachable from the machine running the tests; an SSH config alias is not
resolved for the tests' direct VNC connections.

## Build

```sh
bun install --cwd frontend
cargo build --release
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

Local Cargo builds automatically rebuild the frontend when its sources change,
and the binary carries it. The native package builder consumes the tarball so
every artifact contains the same gateway binary.
