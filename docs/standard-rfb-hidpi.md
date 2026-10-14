# HiDPI over standard RFB

Why a VNC server read over standard RFB is shown at 1x whatever scale its
desktop renders at, and at whatever size it has. Standard RFB is what a `vnc` target with no `subtype` speaks.
Measured against Debian 13's stock packages — wayvnc 0.9.1 on neatvnc 0.9.1
(server name `WayVNC`), serving a `HEADLESS-1` output of sway 1.10.1 on wlroots
0.18 — through the gateway, with `tests/ws_probe.py` reading the control
messages a browser sees.

## Why standard RFB is 1x

RFB carries no pixel density. `SetDesktopSize` and `ExtendedDesktopSize` are
pixel width and height and nothing else, so a sway output at `scale 2` renders a
framebuffer its VNC server cannot describe as anything but pixels, and a client
cannot ask about.

Density over VNC is therefore an extension outside RFB, and remotex lists
extensions for one server on one kind of target: wlshare, on a target with
`subtype = "wlshare"`. That subtype is closer to `ard-high-performance` than to
standard VNC: it names the server, and its picture, density, display list and
sound are that server's own. A `vnc` target with no `subtype`, called a plain
target below, has what RFB defines and nothing more, whatever server it reaches.
It lists none of wlshare's extensions, so it never asks for a density and is
never told one, and that holds when the server it reaches is wlshare itself. The
engine reports `UNSCALED` (`src/vnc.rs`), the browser shows every pixel
one-to-one, and the Info card reads `1728×883 at 1x` under a browser at 2x.

Every other kind of target has a wire that carries density, and remotex reads it
from there:

- **RDP** — the client *declares* it. remotex puts the browser screen's density,
  quantized to 100% or 200%, in the monitor layout's `DesktopScaleFactor`
  (`src/rdp.rs`), asks for the desktop in pixels at that density, and the host is
  bound to honour it. This is why xrdp shows 2x with nothing to configure.
- **Apple Screen Sharing** — the server *reports* it. Apple's display layout
  message carries each screen's logical size, backing size and scale factor as a
  double (`src/vnc_apple.rs`), and remotex reads it off every layout.
- **wlshare** — the server *reports* it and takes the client's. A `wlshare`
  target lists wlshare's density extension: the server reports its output's
  scale, and the browser's density is declared back to it with the window, or
  with the size the session keeps, so the output changes mode and scale at once
  ([`wlshare-density.md`](wlshare-density.md)).

The density a framebuffer is *labelled* with — `Resize.scale` — is the wire's
word alone. The browser does report its own screen's density (`hostDisplay`),
and on RDP, Apple High Performance and wlshare that report is what the remote is
*asked* to render at; the label still waits on the wire's answer — Apple's
display layout and wlshare's report give the scale the server granted, and RDP,
which reports none back, is labelled with the declared density only once the
resize that carried it comes back from the server. A plain target has neither
answer, so it accepts no client-declared density: a label nothing on the wire
can confirm is one the server may not have honoured, and a desktop presented at
it is shown at the wrong size. For a wlroots desktop that should be shown at its
scale, run wlshare and name it with `subtype = "wlshare"`.

A size and a density go together, so a plain target states neither: it is never
asked for a size, takes no `size` key and is offered no window to follow
(`TargetConfig::sized` in `src/config.rs`). It is shown at the size its server has,
and a server-initiated size change is still read off the `DesktopSize` and
`ExtendedDesktopSize` rects it lists, which is how a server says its framebuffer
changed. On a sway output at `scale 2` that is a framebuffer of twice the
output's logical size shown at 1x: every window at twice the size it was drawn
for. Run the output wayvnc captures at `scale 1` for a plain target. Applying that is
live and keeps every window — `swaymsg output HEADLESS-1 scale 1` over the IPC
socket, which a headless sway started by a user service leaves at
`/run/user/<uid>/sway-ipc.*.sock` — so there is no reason to re-render the sway
config and restart the service for it.

Without a browser, the probe starts the session and prints the size and scale
each `resize` announces:

```sh
uv run --with websockets --with requests tests/ws_probe.py \
  --port <gateway port> --target sway --user <user>
```
