# Using remotex

What an operator and a user need to know once the gateway is
[installed](install.md): how the page is reached, what a session is started
with, how each kind of target is set up, and the local multi-instance control
plane. Every configuration key is in the annotated
[`remotex.example.toml`](../remotex.example.toml), and the design behind all of
it is in [Architecture](architecture.md).

## The client

The client is the page the gateway serves, compiled into the gateway binary.
It needs:

- **A secure context.** The gateway speaks plain HTTP and has no TLS listener,
  so reach it on loopback (`localhost`, `127.0.0.1`, `[::1]`, any `.localhost`
  name) or through a TLS-terminating reverse proxy. A LAN address over plain
  `http://` is refused, by name.
- **WebCodecs**: `VideoDecoder` and `AudioDecoder`. There are no fallback paths
  for a browser without them.

For desktop use, install the page as an app in Chrome or Edge. An app window
hands the page the key chords a normal tab keeps for itself (⌘W, ⌘T, ⌘N and the
like). In a tab, **Menu → Immersive full screen** does the same and adds the
keys no window is otherwise given, such as the Super key and Alt+Tab.

The desktop is shown at 100%, never scaled to fit: a larger desktop scrolls, and
in a session started with resize the window drives the remote's size instead. A
phone or tablet fits to width and pinch-zooms, with fingers as a trackpad and a
soft keyboard.

An installed app has **Window → Size to _width_×_height_**, which resizes the
local window to the remote desktop's size through
[`window.resizeTo()`](https://developer.mozilla.org/en-US/docs/Web/API/Window/resizeTo).
It resizes the local window and never scales or resizes the remote desktop. It
works in installed Chrome and Edge app windows and is best-effort in other
browsers; ordinary tabs cannot resize their window, and mobile browsers ignore
the request.

On macOS the window's rounded corners are masked over the page, so a window
sized to the desktop loses the framebuffer's corner pixels. The radius is a
system-wide setting; one point is square enough to see every pixel:

```sh
defaults write -g NSConvolutionOverride1 -float 1
```

Quit and reopen the browser app to pick it up.
`defaults delete -g NSConvolutionOverride1` restores Apple's radius: 26 points
on macOS 26, 10 before it.

## Starting a session

After login the page shows the target picker. Picking a target opens it, its
options show under it, and Start connects with them. They are chosen per
session, held for its life, and remembered by the browser per target; none is a
config key.

| Target | Window drives the size | Sound | Passthrough |
|---|---|---|---|
| `rdp` | yes | off, Opus or lossless | the graphics pipeline (experimental) |
| `vnc` | no | none | — |
| `vnc`, `wlshare` | yes | off, Opus or lossless | — (its VP9 is always passed) |
| `vnc`, `ard` | no | none | — |
| `vnc`, `ard` with `virtual_display` | yes | none | — |
| `vnc`, `ard-high-performance` | yes | always carried | the Mac's HEVC |

- **Size.** A desktop either keeps one size, the target's `size = "1920x1080"`
  or 1440×900 where it sets none, or follows the window: the remote is asked to
  render at the window's size and the browser's density, rather than being
  scaled on the client. A phone is offered the kept sizes only.
- **Sound.** Opus, or lossless FLAC (experimental). A session started without
  sound asks the remote for none, so the host keeps playing where it did. In the
  session, Mute and Unmute change only whether this browser listens.
- **Passthrough.** Sends the remote's own stream to the browser as it came, for
  a LAN, instead of VP9 encoded by the gateway. It is greyed where the browser
  cannot take the stream. A passed stream does not follow the browser's link:
  on a slow one, start the session without it.

One gateway holds one session. A reload or a dropped connection normally
resumes it; a passed RDP graphics pipeline instead reconnects the engine,
because the browser's compositor state is gone. A different browser that finds
the owner attached is asked whether to take over; one arriving during the
owner's reattach grace may claim the slot directly. Either starts at the picker
with its own choices. The remote keeps its own desktop, so starting the target
again returns to the same windows.

See [What a session is started with](architecture.md#what-a-session-is-started-with).

## Windows (`rdp`)

```toml
[[targets]]
name = "workstation"
protocol = "rdp"
host = "192.0.2.10"
username = "Administrator"
password = "change-me"
```

The RDP client is the gateway's own, tested against Windows 10 and 11's Remote
Desktop over NLA. It carries the desktop, pointer, keyboard, mouse, resize, the
clipboard, sound, and the browser's camera and microphone. It does not carry
touch.

- `egfx = false` makes the host draw with bitmap updates instead of the graphics
  pipeline (MS-RDPEGFX). The desktop then keeps its size, since an RDP resize is
  the pipeline's graphics reset.
- **Experimental:** the passthrough sends the pipeline to the browser, which
  composes it. That takes nearly all of the picture's work off the gateway. It
  needs WebGL 2 and a cross-origin isolated page, which a proxy that drops the
  gateway's two isolation headers undoes.
- **Experimental:** `egfx_h264 = true` lets the host draw video with H.264 on a
  passed pipeline, which the browser decodes. Without it a passed pipeline is
  lossless.

Another RDP server, an older Windows or xrdp say, may happen to work but is not
tested against. See [The RDP client](rdp-client.md).

## Macs (`vnc` with an Apple subtype)

```toml
[[targets]]
name = "mac"
protocol = "vnc"
subtype = "ard"   # or "ard-high-performance"
host = "192.0.2.11"
username = "andrew"
password = "the-account-password"
```

A Mac needs no additional software. Give the target the Mac *account's* username
and password, not a Screen Sharing password: that selects Apple Remote Desktop
authentication, so the connection lands at the user's own screen rather than a
login window. The Mac must grant that account Observe and Control in Remote
Management's per-user access list; the default "All users" setting rejects the
connection with the same error as wrong credentials. See
[Remote Management access](apple-vnc-889.md#remote-management-access).

- **`subtype = "ard"`** is Screen Sharing's Standard mode: the Mac's physical
  displays, one or all of them, at their own density. Its lossless ZRLE source
  is decoded by the gateway and encoded as VP9, adaptive by default. It has no
  resize and no sound; the Mac keeps playing on its own output.
- Every Apple subtype carries the Mac's native pasteboard.
- **`subtype = "ard-high-performance"`** is High Performance mode as Apple's
  viewer has it, and is **experimental**. The Mac disables its physical displays
  and puts every window on one virtual display, sized by the session, up to
  3840×2160 backing pixels. Picture and sound come over the Mac's media stream,
  HEVC and AAC-ELD over SRTP:
  - The Mac sends to the gateway's UDP ports 5900 and 5901, so a firewall or
    NAT between them must let that through.
  - A Linux gateway needs `net.core.rmem_max` of at least 4194304 for the
    picture's socket, and the log warns when it is lower: the stock 212992
    loses keyframes at Retina sizes.
  - The gateway decodes the picture with the host's FFmpeg (libavcodec), which
    no release artifact contains: install it as
    [High Performance decoder](high-performance-decoder.md) says. Without it the
    picture can only be passed through.
  - The passthrough sends the HEVC only when the page's startup probe finds a
    decoder path. Support depends on the browser, platform and GPU, so the
    picker is authoritative. The gateway can also serve the optional software
    picture decoder described in [`remotex.example.toml`](../remotex.example.toml).
  - The Mac mutes its own speakers while it streams, so a session always
    carries AAC-ELD sound. The page probes that separately; if it cannot decode
    the sound, the session continues without audio and the menu says why.
- **Unofficial:** `virtual_display = true` on an `ard` target opens Standard
  mode on such a virtual display, with resize and without the media stream or
  sound. Apple's viewer never offers this combination; it was tested on macOS 26
  only.

Apple documents none of this protocol. Both modes are reverse engineered and
only as correct as the Macs they were measured against, and a macOS update is
free to change any of it. See
[Apple RFB 003.889, as measured](apple-vnc-889.md).

## wlshare (`vnc`, `subtype = "wlshare"`)

```toml
[[targets]]
name = "wayland"
protocol = "vnc"
subtype = "wlshare"
host = "192.0.2.12"
username = "me"
password = "that-account's-password"
```

[wlshare](https://github.com/andrewtheguy/wlshare) is this project's own VNC
server for wlroots-based Wayland desktops. The subtype makes the gateway list
wlshare's private RFB extensions: its VP9 stream, which is passed through
untouched and adapts to the browser's link, pixel density, the output list,
sound, and with `camera = true` or `microphone = true` the browser's camera and
microphone as PipeWire devices on the desktop. Without the subtype the same
server is read as any VNC server is.

The example uses the account wlshare runs as, checked through PAM over
RSA-AES. If wlshare instead has a `password_file`, set that server password as
`vnc_password` and omit `username` and `password`.

See [density](wlshare-density.md), [outputs](wlshare-outputs.md),
[audio](wlshare-audio.md), [camera](wlshare-camera.md) and
[microphone](wlshare-microphone.md).

## Other VNC servers (`vnc`)

```toml
[[targets]]
name = "workstation-vnc"
protocol = "vnc"
host = "192.0.2.13"
vnc_password = "change-me"
```

A plain target takes `vnc_password` for classic VNC authentication, or
`username` and `password` for RSA-AES. The gateway reads the standard lossless
encodings (ZRLE, zlib, Hextile, RRE, Raw and CopyRect); Tight and the other
vendor or lossy encodings are not listed. The target is asked for its size once,
shown at 1x, and carries no sound, camera or microphone. Standard RFB cannot say
that its pixels are HiDPI; see [HiDPI over standard RFB](standard-rfb-hidpi.md).

## Camera and microphone

`camera = true` and `microphone = true` offer an `rdp` or `wlshare` target this
browser's camera and microphone. Both are **experimental**, off by default,
turned on per session from the floating menu and never remembered. They are for
someone who needs one for a while, a call or a recording, and are sent as
cheaply as that allows: H.264 from the browser passed untranscoded, and mono
speech Opus at a low fixed bitrate. An Apple target and a plain `vnc` target
refuse the keys.

On RDP the camera needs a Windows host that redirects cameras: a workstation, or
a Windows Server carrying the Remote Desktop Session Host role. A Windows host
starts the microphone only once something on it records.

## Local instances

Native installs include a terminal control plane that runs several gateways on
one machine, each with its own config and browser origin:

```sh
remotex tui
```

It listens on loopback only, on port 52380 unless `--port` or
`REMOTEX_TUI_PORT` says otherwise, and refuses to start on a port something else
is serving. Open <http://remotex.localhost:52380>; each running instance is at
`http://<instance>.remotex.localhost:52380`.

| Key | Action |
|---|---|
| `↑`/`↓`, `k`/`j` | select an instance |
| `n` | create an instance |
| `e` | edit its `remotex.toml` in `$VISUAL` or `$EDITOR` (`vi`, or Notepad on Windows); the edit is checked, and applies when the instance is restarted |
| `s`, `x`, `r` | start, stop, restart it |
| `a` | start every stopped one |
| `o` | open a running one in the browser |
| Enter | show its settings |
| `R` | rescan the instances directory |
| `q`, Esc | stop every instance and quit |

Each immediate subdirectory of the instances root is one instance:
`~/.local/share/remotex/instances` on Linux (or
`$XDG_DATA_HOME/remotex/instances`),
`~/Library/Application Support/remotex/instances` on macOS, and
`%LOCALAPPDATA%\remotex\instances` on Windows; `--instances-dir` chooses
another. The root is private to your account, because the configs hold
credentials. An instance's config has the same `[branding]` and `[[targets]]`
as a gateway's and no `[server]` block: the TUI owns the port and proxies each
subdomain to that instance's private Unix socket or named pipe.

The private files and worker endpoints protect credentials from other local
users, but the shared loopback listener does not authenticate the OS user: any
account on the machine can open the landing page and drive a running instance.
Do not run `remotex tui` on a machine shared with users who must not reach those
desktops.

See [Local multi-instance control plane](architecture.md#local-multi-instance-control-plane).
