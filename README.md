# remotex

A single-user remote desktop gateway for RDP and VNC targets, including Macs
using the built-in Screen Sharing service. The Rust backend owns each protocol
session and streams desktop updates over a WebSocket protocol to the browser
SPA. Remote audio uses a dedicated WebSocket so sound never queues behind the
picture; redirected camera and microphone media each use their own socket too.

The main reason this exists is the client: it is a browser, so anything with one
reaches every target — RDP, VNC and Macs alike — with nothing to install per
platform and nothing that has to exist for your OS. Picking a target at the
picker opens it: the size the desktop will have, whether the remote's sound is
taken and whether the remote's own stream is passed through are chosen under it,
and Start connects with them. The size is shown before Start. It is either one
the desktop keeps — the target's `size = "1920x1080"`, or 1440×900 where it sets
none — or the window's: in a session started with resize the window drives the
remote's size, so the desktop is renegotiated at the size asked for rather than
scaled on the client. A wlshare server, a Mac's virtual display and `rdp` can be
handed the window, by a desktop browser or a tablet; a phone is offered the kept
sizes instead. On RDP a resize is a graphics reset of the default graphics
pipeline, so a target with `egfx = false` keeps its size, and so does a plain
`vnc` target, which is asked for its size once.
Not every server is served alike: see [Supported servers](#supported-servers)
for the tiers they are ranked in.

- RDP uses a built-in client, protocol and all: the desktop over the graphics
  pipeline (MS-RDPEGFX) or plain bitmap updates, pointer, keyboard, mouse and
  resize, spoken over NLA to modern Windows' own Remote Desktop server, tested
  on Windows 10 and 11. It carries the clipboard, sound (MS-RDPEA), and the
  browser's camera and microphone, and does not carry touch. See
  [`docs/rdp-client.md`](docs/rdp-client.md).
- VNC uses a built-in RFB client and connects directly to macOS Screen Sharing,
  over Apple's own RFB 003.889 with Apple Remote Desktop authentication, as
  Apple's viewer does. `subtype = "ard"` selects Screen Sharing's Standard mode,
  on the Mac's own displays.
  `subtype = "ard-high-performance"` is its High Performance mode as Apple's
  viewer has it: one virtual display holding every remote window,
  with the picture as HEVC and the sound as AAC-ELD over the Mac's SRTP media
  stream: the picture decoded by the host's FFmpeg, the sound by the browser. Resize
  needs a virtual display: High Performance's, or the unofficial
  `virtual_display = true` under `ard`, which puts Standard mode's picture on one
  and was tested on macOS 26 only. Both modes are reverse engineered, having no
  specification.
  A wlroots-based Wayland desktop behind
  [wlshare](https://github.com/andrewtheguy/wlshare) is a `vnc` target with
  `subtype = "wlshare"`: the gateway then lists wlshare's private RFB
  extensions, so its own VP9 stream is passed through, the output's scale is
  reported and shown as such, and in a session started with resize the output
  follows the browser's density. Without the subtype the same server is read as any VNC
  server is.

There is one client: the page a browser loads. It works on Windows, macOS,
Linux, iOS and Android. For desktop use, install that page
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

There is no backward compatibility between releases, and no legacy path is kept
for one. A release may change or remove a configuration key, a browser API, the
WebSocket protocol or a feature, with nothing that reads the old form: check
[`remotex.example.toml`](remotex.example.toml) against your config when you
upgrade. The page and the gateway are one release too, so a tab left open across
an upgrade says both versions and asks to be reloaded.

See [`docs/architecture.md`](docs/architecture.md) for the system design and
[`docs/known-issues.md`](docs/known-issues.md) for faults worth recognising rather
than re-investigating.

## Supported servers

What sets servers apart here is how the picture reaches the browser: passed
through as the server made it, or decoded and encoded again as VP9 in the
gateway, which adapts to the link.

### Native servers, prioritized

remotex is built around the remote desktop servers native to each platform:
Windows' own Remote Desktop, macOS's built-in Screen Sharing, and on Linux
wlshare, our own. They are ranked in tiers. Design, testing and optimization
start from the first tier, and a higher tier comes first when work for two
competes.

All three have these in common:

- **The desktop outlives the viewer.** A Windows host holds a disconnected
  session for the next logon, and a Mac's or a wlshare desktop keeps running
  with nobody watching. A browser coming back from a reload or a dropped
  connection resumes where it was. A different browser, a phone picking up what
  a desktop started, say, starts at the picker and chooses its own size, sound
  and passthrough; starting the target returns it to the same desktop, with its
  windows as they were left.
- **HiDPI and Retina.** Each renders at the pixel density of the browser's
  screen, or says which density its pixels are, so the desktop is sharp on a
  Retina display and is shown at its true size.
- **The pointer travels apart from the picture.** It arrives as its own shape
  and the browser wears it on its own pointer, so it moves with the hand rather
  than a network round trip behind it.

#### Tier 1: wlshare on Linux

[wlshare](https://github.com/andrewtheguy/wlshare), this project's own VNC
server for wlroots-based Wayland desktops, is the ideal. It codes the desktop as
VP9 itself, at the quality and chroma the target asks for, and walks that quality
by the browser's link, so the gateway passes its stream through untouched and
the session still adapts to a slow link. Because wlshare is ours, what RFB
lacks is added to it as an extension: pixel density, switching outputs,
sound, and the browser's camera and microphone. It is a `vnc` target with
`subtype = "wlshare"`, which is what makes the gateway list those extensions and
ask for the stream.

#### Tier 2: modern Windows' Remote Desktop, and a Mac's High Performance Screen Sharing

The host's own stream, passed through to the browser for a LAN:

- **Modern Windows' own Remote Desktop server**, in a session started with the
  passthrough: the host's graphics pipeline (MS-RDPEGFX), composed in the browser
  by the gateway's own compositor built to WebAssembly, which takes nearly all of
  the picture's work off the gateway. It is **experimental**: run against one
  Windows 11 host, with sound and the clipboard beside it, and not yet with the
  camera or the microphone. A target with `egfx_h264 = true`, more experimental
  still, lets the host draw video with H.264 on that pipeline, which the browser
  decodes; without the key what is passed is lossless.
- **macOS Screen Sharing's High Performance mode** (`ard-high-performance`), in a
  session started with the passthrough: the Mac's HEVC picture, to a browser
  that decodes it (Chrome and Safari; not Firefox). Its AAC-ELD sound is passed
  in every session.

The passthrough is a choice made at the picker, greyed for a browser that cannot
take the stream. A passed stream does not adapt to a slow link: the Mac's own
rate control keeps it between 20 and 60 Mbit/s, and a Windows host's pipeline is
sent as drawn. A session started without it is the gateway's VP9, which does
adapt and is the answer for a slow link. VP9 also serves a browser that cannot
decode the Mac's stream, and a Windows host that draws with plain bitmap updates
rather than the pipeline. On RDP the passthrough is the only way past VP9:
without it the gateway composes the host's pipeline, or takes its bitmap updates,
and encodes the picture as VP9.

#### Tier 3: a Mac's Standard Screen Sharing

Screen Sharing's Standard mode (`ard`, including the unofficial
`virtual_display = true`) is decoded in the gateway and encoded as VP9, adapting
to the link. Nothing of it is passed through.

### Other servers, not prioritized

Every other VNC server is a plain `vnc` target, reached through the RFB baseline
and always encoded as VP9 in the gateway, at 1x and without sound. The gateway
reads the standard lossless encodings, ZRLE first, then zlib, Hextile, RRE and
Raw, with CopyRect beside them; Tight and the other vendor or lossy ones are not
listed. A
wlshare server behind a plain target is read the same way: its fallback for
ordinary VNC clients. Plain VNC stays supported, and is worked on as needed
rather than ahead of the tiers.

Another RDP server, an older Windows or xrdp say, may happen to work if it
speaks what the client implements ([The RDP client](docs/rdp-client.md)), but
it is not a target: it is not tested against. Its picture follows
Windows' rule, encoded as VP9 in the gateway unless the session was started with
the pipeline passed.

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

The MSI is unsigned, so SmartScreen asks first. By default it installs the gateway
under `%ProgramFiles%\remotex`, puts `bin` on the machine `PATH`, and reads
`%ProgramData%\remotex\remotex.toml`.

Packages do not own the live config because it contains credentials. On Linux,
create it for the account that will run the gateway:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/remotex /var/lib/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/remotex/remotex.example.toml /etc/remotex/remotex.toml
remotex gen-passwd admin
${EDITOR:-vi} /etc/remotex/remotex.toml
```

On macOS, use `/usr/local/etc/remotex/remotex.toml` and the example under
`/usr/local/share/doc/remotex/` instead; on Windows,
`%ProgramData%\remotex\remotex.toml` and the example under the MSI's selected
install directory (by default
`%ProgramFiles%\remotex\share\doc\remotex\`). Paste the generated `admin:$2b$...`
value into `[server].site_passwd`, replace the example `[[targets]]` entry, then
start the server in the foreground:

```sh
remotex serve
```

See [`docs/install.md`](docs/install.md) for package upgrades, removal, and macOS
config setup, and [`docs/high-performance-decoder.md`](docs/high-performance-decoder.md)
for the library an `ard-high-performance` target needs beside the package.

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
`e` to edit its `remotex.toml`, `s` to start it, `x` to stop it, `r` to restart
it, `a` to start every stopped one, `o` to open a running one in your browser,
Enter to see its settings, and `q` to stop every child and quit.

Each immediate subdirectory is one instance. The default root is
`~/.local/share/remotex/instances` on Linux (or
`$XDG_DATA_HOME/remotex/instances`),
`~/Library/Application Support/remotex/instances` on macOS, and
`%LOCALAPPDATA%\remotex\instances` on Windows; pass `--instances-dir` to choose
another. The root is made private to your account — mode `0700`, or on Windows
an ACL naming only you and `SYSTEM` — because the configs hold credentials. Its
config uses the same `[branding]` and `[[targets]]` format as a gateway's and
deliberately has no `[server]` block. `e` opens it in `$VISUAL` or
`$EDITOR` — `vi` when neither is set, and Notepad on Windows. The supervisor owns
the shared TCP port and proxies each subdomain to that child's private endpoint:
`<instance>/gateway.sock`, or on Windows a named pipe only your account can open.

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
asks the Mac for ZRLE rectangles from the start, although High Performance steps
over them undecoded and never displays them.

High Performance (`ard-high-performance`) takes the same credentials and the same
encrypted protocol revision. It requests one virtual display at the size the
session keeps, or at the full resolution of the client's screen where the window
drives it, and at the density of the client's screen either way. Once connected, it disables the remote
Mac's physical displays and puts all of the remote Mac's windows on that virtual
display. Apple's official macOS Screen Sharing client can instead choose up to
two virtual displays. It takes the picture and sound Apple's own viewer takes:
after the first layout the gateway offers the Mac's media stream, and the Mac
then sends the screen as HEVC 4:4:4 and its sound as AAC-ELD, over UDP with
SRTP, to the gateway's ports 5900 and 5901, at 20 to 60 Mbit/s as the delay the
gateway reports to it every 50 ms allows, as Apple's viewer reports. A Linux
gateway needs `net.core.rmem_max` of at least 4194304 for the screen's socket,
which the log warns about when it is lower: the stock 212992 loses keyframes at
Retina sizes. The gateway authenticates and decrypts every packet. The sound it sends on
as the Mac sent it, AAC-ELD the browser decodes, and to no browser that has muted it. The picture it
decodes and sends on as the VP9 every target uses — or, in a session started with the passthrough, which the picker
offers a browser that decodes it (Chrome and Safari; not Firefox), sends the HEVC
on as the Mac sent it, for a LAN;
the browser stays behind its resize notice until the stream sends its first picture,
at connect and across display changes, and a stream that fails ends the session, as
it does in Apple's viewer. A playing
video does not delay the Mac's reading of the input, as RFB pixels' deflate does. The Mac refuses the picture without the sound, and
mutes its own speakers while it streams, so a session always carries sound, with
nothing to choose at the picker, and nothing reaches an AirPlay speaker the Mac
plays to. It is **experimental**.

Decoding the picture needs a library on the gateway's host that no release
artifact contains: FFmpeg's libavcodec. Install it as
[High Performance decoder](docs/high-performance-decoder.md) says for Linux,
macOS and Windows. A gateway without it can only pass the picture: the picker
shows the passthrough as already chosen, and a browser that cannot decode the
HEVC cannot start the target. See
[The media stream](docs/apple-vnc-889.md#the-media-stream-high-performances-picture-and-sound).

Every Apple subtype carries the native Apple pasteboard. In a session started with resize, the window continuously drives High
Performance's virtual display, using Apple's
dynamic-resolution feature to replace its mode from client viewport reports.
The size is chosen before the session starts and holds for it: there is no
auto-resize toggle or one-shot remote-resize button in the session. The local
app-window sizing control described above does not change that. The descriptor's
fixed 3840×2160 backing ceiling permits successive arbitrary sizes within that
bound, and every fresh connection turns the Mac's Dynamic resolution setting back
on. Standard `ard` on the Mac's physical displays offers no resize, and the
one/two-virtual-display control is not implemented.

**Unofficial:** `virtual_display = true` on an `ard` target opens Standard mode on
one virtual display instead of the Mac's physical ones — the display and the
resizing above are High Performance's, and the picture stays ZRLE, with no media
stream offered, no decoders needed and no sound. Apple's viewer never offers this
combination, so nothing but remotex exercises the Mac's side of it; it was tested
against macOS 26 only, and a macOS update is free to break it while leaving the
two official modes alone.
See [`docs/apple-vnc-889.md`](docs/apple-vnc-889.md).

A plain `vnc` target has no way to learn that its pixels are HiDPI — standard RFB
carries sizes in pixels and nothing else, and a plain target lists no extension
to it — so it is shown at 1x, one CSS pixel per framebuffer pixel, and the
size it is asked for goes to the server as pixels, whatever server it reaches.
See [`docs/standard-rfb-hidpi.md`](docs/standard-rfb-hidpi.md) for what that means
on a sway output at scale 2 and why a second client's size request can come back
prohibited. Density over VNC is a wlshare extension, listed for a target with
`subtype = "wlshare"` and no other: wlshare reports its output's scale, the
gateway labels the framebuffer with it, and the browser's density is declared
back to the server together with the window in points × that density, so the
output changes mode and scale at once. See
[`docs/wlshare-density.md`](docs/wlshare-density.md).

Sound is chosen at the picker on an `rdp` target and on a `wlshare` one. On
wlshare it comes through wlshare's audio extension: the gateway lists its
pseudo-encoding, wlshare announces so and then streams the desktop's sound on the
RFB connection itself, coded there as Opus at the target's rate and passed to
the browser as it came. While a client listens the host is
silent: the desktop plays into a PipeWire sink of wlshare's own, whose monitor is
what is captured. A session started without sound asks for none, and the host
keeps playing where it did. In the session the menu's Mute and Unmute change only
whether this browser listens. A plain `vnc` target carries no sound and offers
none. See [`docs/wlshare-audio.md`](docs/wlshare-audio.md).

On a Mac, audio is **experimental**. An `ard-high-performance` target receives
it from Screen Sharing itself, beside the picture (above). No such path has been
measured in Standard mode, so an `ard` target carries no sound; no Mac target
offers sound as a choice. Standard mode never touches the Mac's sound output, so the
Mac keeps playing where it did — its own speakers, or an AirPlay receiver that
runs outside remotex, on Linux or Windows.

Two redirections send this browser's own media the other way and are
**experimental**: `camera = true` offers the remote a virtual
webcam over MS-RDPECAM — or, on a `wlshare` target, over wlshare's camera
extension, which makes it a PipeWire camera on the wlroots desktop (see
[`docs/wlshare-camera.md`](docs/wlshare-camera.md)) — and `microphone = true`
offers an RDP host a microphone over MS-RDPEAI, or a `wlshare` target one over
wlshare's microphone extension, which makes it a PipeWire audio source on the
wlroots desktop (see
[`docs/wlshare-microphone.md`](docs/wlshare-microphone.md)). They serve a
different purpose from the rest of the session. The
screen and the remote's sound aim to match sitting at the desktop and spend the
bandwidth that takes on a fast link; the camera and the microphone are for
someone who needs one for a while — a call, a recording — and are sent as
cheaply as that allows on any link. The microphone goes as mono speech Opus at
16 kbit/s, which the gateway decodes to the PCM the host records in. Both are off
by default and enabled per session from the floating menu, never remembered, and
both are refused on Apple's Screen Sharing and on a plain `vnc` target. A Windows host starts the microphone only once something on it
records. Their socket rules, control messages and channel wire formats are tested
like everything else, and the wlshare paths have container coverage. On RDP,
`a_real_host_records_the_microphone` feeds a host's recording device.
`a_real_host_streams_the_camera` in `tests/rdp_client_probe.rs` carries H.264
frames to a host's Camera app, but it is ignored by default and does not check
the pixels the host displays. The camera channel is created only by a Windows
host that redirects cameras — a workstation, or a Windows Server carrying the
Remote Desktop Session Host role. The picture is verified by hand there, where
remote audio and the rest of the RDP feature set are exercised on
every test run. Expect to re-check it by hand after a change.

Apple's protocol revision is the one part of remotex built entirely without a specification: Apple
documents none of it — the revision, its record layer, its control messages, High
Performance's virtual display handling or its media stream — so all of it is reverse engineered and only as correct as the Macs it has been measured
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
`/usr/local/etc/remotex/remotex.toml` on macOS, and
`%ProgramData%\remotex\remotex.toml` on Windows; the container defaults to
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
connection and its picture and sound over the Mac's media stream — each with the Mac account's username and password. The unofficial `virtual_display = true` under `subtype = "ard"` opens Standard mode on such a virtual display, tested on macOS 26 only. Keep the config mode `0600`; target
credentials remain server-side but are stored in this file.

All fields and per-protocol examples are in
[`remotex.example.toml`](remotex.example.toml).

## Checks

```sh
cargo clippy --all-targets -- -D warnings
cargo test --lib

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

A gateway for QA is built with `cargo build --profile qa` into `target/qa`:
optimised as a release build is, without its link-time optimisation, so a
change rebuilds in seconds rather than minutes. Artifacts are always built
`--release`.

## Licence

remotex is under the MIT licence in [`LICENSE`](LICENSE).
