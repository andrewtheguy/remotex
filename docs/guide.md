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
- **WebCodecs**: `VideoDecoder`, with no fallback path for a browser without
  it. `AudioDecoder` plays Opus and a Mac's AAC-ELD; a browser without it
  (Safari 16.4 to 18) plays FLAC alone, which the page decodes itself.

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

| Target | Window drives the size | Sound | Passthrough | Graphics | VP9 encoder |
|---|---|---|---|---|---|
| `rdp` | yes | off, Opus or FLAC | the graphics pipeline, always passed | high quality or adaptive H.264 | — |
| `rdp` with `egfx = false` | no | off, Opus or FLAC | — | — | — |
| `vnc` | no | none | — | — | — |
| `vnc`, `wlshare` | yes | off, Opus or FLAC | — (its VP9 is always passed) | — | libvpx or native |
| `vnc`, `ard` | no | none | — | — | — |
| `vnc`, `ard` with `virtual_display` | yes | none | — | — | — |
| `vnc`, `ard-high-performance` | yes | always carried | the Mac's HEVC, or its H.264, always passed | — | — |

- **Size.** A desktop either keeps one size, the target's `size = "1920x1080"`
  or 1440×900 where it sets none, or follows the window: the remote is asked to
  render at the window's size and the browser's density, rather than being
  scaled on the client. A kept size follows the browser's density too. A phone
  is offered the kept sizes only. A plain `vnc` target and `ard` on the Mac's
  physical displays are shown at the remote's own size and take no `size`.
- **Sound.** Opus, or FLAC, which is lossless. A session started without
  sound asks the remote for none, so the host keeps playing where it did. In the
  session, Mute and Unmute change only whether this browser listens. A phone,
  a tablet and Safari start muted. A browser without `AudioDecoder` greys Opus,
  so ticking Sound there takes FLAC. On a High Performance Mac the menu's Audio
  button reads *Audio not supported* in such a browser, and *AAC-ELD not
  supported* in one that cannot decode the Mac's AAC-ELD.
- **Passthrough.** Sends the remote's own stream to the browser as it came
  instead of VP9 encoded by the gateway. A High Performance Mac's picture is
  always passed, and which stream is the choice: its HEVC, greyed where the
  browser cannot take it, or its H.264, which is offered to every browser and
  needs no HEVC decoder in the page. On an `rdp` target with the
  pipeline on it is no choice: the picker says it is always passed, with no box
  to tick, and a browser that
  cannot compose the pipeline cannot start the target. A passed RDP pipeline
  follows the browser's link: the Windows host draws fewer and smaller frames
  for a browser that falls behind. A Mac's passed stream does not: the Mac sends
  at its own rate, little on a still screen and much with motion. For a slow
  link with much motion, an `ard` target's VP9 follows the link.
  See [A passed stream and the browser's link](architecture.md#a-passed-stream-and-the-browsers-link).
- **Graphics.** How an RDP host draws its pipeline. *High quality*, the default,
  is the pipeline's codecs without H.264, for a
  LAN: a slow link gets fewer frames, none of them coarser. *Adaptive* (alpha) is H.264
  the host fits to the browser's link: coarser on a slow link, finer on a fast
  one. Under it, *H.264 full screen*, the default, draws the whole desktop in
  H.264, and *H.264 region* only what moves like video, with the rest as High quality draws it,
  which keeps text sharp but adapts less. Adaptive is greyed in a browser whose
  `VideoDecoder` cannot decode the host's H.264. See
  [An exact picture](#an-exact-picture) for having the host draw losslessly.
- **VP9 encoder.** Which encoder wlshare codes a `wlshare` target's VP9 with:
  *libvpx*, the default, or *Native* (alpha), the encoder in Rust that
  `screen-vp9` has written. It needs a wlshare that has the encoder: an older
  one codes with libvpx. The stream is the same to the browser either way, so
  starting one target with each compares the two. Every other target's VP9 is
  coded by the gateway with libvpx, so it has no such choice.

One gateway holds one session. A reload or a dropped connection normally
resumes it; an RDP session with the graphics pipeline on instead reconnects the engine,
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

- The host draws through the graphics pipeline (MS-RDPEGFX), which every
  session passes to the browser to compose. That takes nearly all of the
  picture's work off the gateway. It needs WebGL 2 and a cross-origin isolated
  page, which a proxy that drops the gateway's two isolation headers undoes; a
  browser without them cannot start the target. How the host draws the
  pipeline, high quality or adaptive H.264 (alpha), is chosen at the picker.
- `egfx = false` makes the host draw with bitmap updates instead, which the
  gateway encodes as VP9: the target for a browser that cannot compose the
  pipeline. The desktop then keeps its size: without the pipeline Windows
  resizes by rebuilding the share, which this gateway does not take.
- **Alpha:** `virtual_displays = 2` asks the host for two displays, each the
  session's size. Where the second sits against the first is chosen under the
  target at the picker, **Second display**: right, left, top or bottom, to match
  where the client's own second display is. The session starts on *All Displays*, in the floating menu's Display
  picker, which keeps the first display here and links to the second, **Open
  Display 2 ↗** under the menu's Display button: it opens at `/display/2` in a
  new tab of the same browser, just that display, with its own pointer and
  keyboard, and clicked again it brings that tab to the front. The picker's `Display 1` and `Display 2` show one display alone on
  this page; the gateway encodes only what is shown. A window dragged over the edge between the two tabs arrives on the
  other display, as the pointer does, however their windows are arranged. With
  each tab full screen on a display of its own the edges meet, so it arrives
  where it was dragged to; with anything between the two pictures it arrives
  short by that much.
  While *All Displays* is chosen the Display picker and the menu's **Info**
  carry the same link. The page shows the display as it opens. On a session
  that follows the window, that tab's window sizes display 2. It needs the
  login of the browser holding the session and is shown in one tab at a time:
  a tab opening it while another shows it says it is in use and asks before it
  **Take**s it **over**, as a browser asks before taking a session, and the
  tab it was taken from says so and offers **Take it back**. That tab's menu
  **Disconnect**s it, and choosing one display alone closes it. Opened while it
  is not shown, it says it is not available. With the pipeline on, the browser holding the session
  composes both displays once and shows one; the second display's tab is painted
  from that same picture, so nothing is decoded twice. Checked against one
  Windows 11 host, the passthrough over two displays by a headless browser's
  sockets only, not yet by eye.

  The second display's tab has a menu of its own, behind a button showing the
  display's number where the session's page shows ☰. It holds what is that tab's
  alone, **Immersive full screen**, that display's size and density, and
  **Disconnect**; everything else stays in the menu on the session's page.

  The second display's tab is for a client with two physical displays, one for
  each. A client with one leaves the tab unopened, and has the first display
  here as it would alone, or picks a display. The tab is not supported on a
  phone or tablet, though nothing there stops you opening it.

### An exact picture

By default a Windows host draws much of its desktop lossily, as RemoteFX
Progressive and ClearCodec's NSCodec, which blur text and colours slightly. Its
own policy decides that, and no setting in remotex overrides it. To have it draw
every pixel exactly:

1. On the host, open the Local Group Policy Editor (`gpedit.msc`) and go to
   Computer Configuration > Administrative Templates > Windows Components >
   Remote Desktop Services > Remote Desktop Session Host > Remote Session
   Environment. Enable *Configure image quality for RemoteFX Adaptive
   Graphics* and set it to **Lossless**. A Windows edition without the editor
   takes the same setting in the registry, from an elevated prompt:

   ```text
   reg add "HKLM\SOFTWARE\Policies\Microsoft\Windows NT\Terminal Services" /v ImageQuality /t REG_DWORD /d 1 /f
   ```

   A domain's Group Policy, where one sets it, wins over either.
2. Run `gpupdate /force`, then start a new session: one already running keeps
   the quality it started with.
3. At the picker, keep **Graphics** on *High quality*. The host then draws only
   exact codecs, planar and uncompressed rectangles, and passes them to the
   browser as they are.
4. In the session, open the menu's **Info**: the *Host's picture* row reads
   *Lossless (Planar)* or similar. *Lossy (…)* names what the host drew with
   instead, and means the policy has not reached the session.

Lossless is costly, because the host draws what moves exactly too: the opening
desktop alone is about 7.6 MB, and a video in a window about 19 MB a second,
before the host's own compression, which reaches the browser with them and
takes about a third off planar drawing and half off scrolling. That suits a
LAN. Adaptive H.264 changes nothing on such a host: it draws no
H.264 under Lossless. See [Lossless, by the host's policy](rdp-client.md#lossless-by-the-hosts-policy)
for what was measured.

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
  - **Alpha:** `virtual_displays = 2` asks the Mac for two virtual displays,
    Apple's viewer's "2 Virtual Displays", the second to the right of the first:
    the Mac places it, so the picker offers no other side. To have it on another
    side, arrange the displays on the Mac while the session is open, in System
    Settings → Displays → Arrange: drag the second display to the left of the
    first, above it or below it. The session follows — each tab keeps its own
    display and the pointer crosses the edge they now share — and the Mac may
    keep the arrangement for the next session; when it has not, it is on the
    right again.
    The session starts on the Display picker's *All Displays*, which keeps the
    first here and opens the second at `/display/2` in a new tab of the same
    browser, as on an RDP target, and the picker shows either alone; on a session that follows the window that
    tab's window sizes the second display. The Mac sends a stream for each
    display, the second to UDP port 5902, and the gateway passes only the
    displays shown. As on RDP, a window dragged over the edge between
    the two tabs arrives on the other display, and the second display's tab is not
    recommended on a client with one physical display, and not supported on a
    phone or tablet.
  - The Mac sends to the gateway's UDP ports 5900 and 5901, and 5902 for a
    second display, so a firewall or NAT between them must let that through.
  - A Linux gateway needs `net.core.rmem_max` of at least 4194304 for the
    picture's socket, and the log warns when it is lower: the stock 212992
    loses keyframes at Retina sizes.
  - The gateway decodes neither of the Mac's streams: the picture is passed, as
    the HEVC or the H.264 chosen at the picker.
  - The H.264 passthrough needs nothing added in the page: the Mac is asked for
    H.264 in place of its HEVC, and the browser decodes it with its own decoder,
    which more browsers have for H.264 than for HEVC. So a default build serves
    the target to any browser, Firefox included, with no software decoder, and
    the picker offers the H.264 to every browser.
  - The HEVC passthrough sends the HEVC only when the page's startup probe finds a
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

- **Alpha:** a desktop with exactly two outputs, monitors or headless ones, is
  listed with *All Displays* and starts on it: the first stays here and the second opens at
  `/display/2` in a new tab of the same browser, with its own pointer and
  keyboard, as on an `rdp` target with two virtual displays. No key asks for it;
  the outputs are the compositor's, and `virtual_displays` is not a key of this
  target. It needs wlshare 0.0.54 or later.

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
`username` and `password` for RSA-AES. The gateway asks for ZRLE, which every
current VNC server has, and reads nothing else but Raw; the older standard
encodings, Tight and the other vendor or lossy ones are not listed. The target is shown at its
server's own size, at 1x, and carries no sound, camera or microphone. Standard RFB cannot say
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
