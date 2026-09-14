# HiDPI over generic VNC

Why a plain VNC server — wayvnc on sway is the measured case — is labelled 1x
whatever scale its compositor renders at, what its desktop then looks like on a
Retina screen, and the one wayvnc rule that makes a resize come back
*prohibited*. The wire was measured 2026-09-05 against wayvnc (server name
`WayVNC`) on a sway `HEADLESS-1` output through the gateway, with
`tests/ws_probe.py` reading the control messages a browser sees; the pixel
counts below follow the rule the browser has drawn by since 2026-09-13, one
framebuffer pixel per device pixel.

## Why a generic server is labelled 1x

Two of the three protocols carry pixel density on the wire, and remotex reads it
from them:

- **RDP** — the client *declares* it. remotex puts the browser screen's density,
  quantized to 100% or 200%, in the monitor layout's `DesktopScaleFactor`
  (`src/rdp.rs`), asks for the desktop in pixels at that density, and the host is
  bound to honour it. This is why xrdp shows 2x with nothing to configure.
- **Apple Screen Sharing** — the server *reports* it. Apple's display layout
  message carries each screen's logical size, backing size and scale factor as a
  double (`src/vnc_apple.rs`), and remotex reads it off every layout.
- **Standard RFB** — nothing. `SetDesktopSize` and `ExtendedDesktopSize` are
  pixel width and height and nothing else. A sway output at `scale 2` renders a
  framebuffer wayvnc cannot describe as anything but pixels, and remotex cannot
  ask about. So the generic VNC engine reports `UNSCALED` (`src/vnc.rs`) and the
  Help card reads `3456×1766 at 1x` under a browser at 2x.

The label is all the density on `Resize.scale` is. The browser draws every
framebuffer pixel on one device pixel of its own screen whatever the label says
(`frontend/src/desktopCanvas.ts`), so a 3456×1766 framebuffer is 1728×883 CSS
pixels on a 2x screen and 3456×1766 on a 1x one, sharp on both; the scale is
read off the Help card beside the browser's own, and the tile lattice is cut at
it. The label is the wire's word alone. The browser does report its own screen's
density (`hostDisplay`), and on RDP and Apple High Performance that report is
what the remote is *asked* to render at; the label still waits on the wire's
answer — Apple's display layout reports the scale it granted, and RDP, which
reports none back, is labelled with the declared density only once the resize
that carried it comes back from the server. Generic VNC has neither answer, so
it takes no client-declared density as a label: a label nothing on the wire can
confirm is one the server may not have honoured. So a generic server is labelled
1x, and stays 1x, until its protocol can say otherwise — which is what the
wlshare density extension does for a wlroots-based Wayland desktop behind the
wlshare server, asked for on every generic target and answered by that server
alone ([`wlshare-density.md`](wlshare-density.md)).

What the browser's density does decide is how many pixels the window asks for.
With `resize = true` a 1728×883-point window on a 2x screen asks a generic server
for 3456×1766 pixels — its device pixels — and shows them one per device pixel
(`DesktopState::generic_pixels`). On a sway output at `scale 1` that is a
3456×1766-point desktop: sharp, filling the window, with every menu and font
half the size it has on a 1x screen, because that is what a 1x desktop drawn at
2x is. At `scale 2` sway makes the same pixels a 1728×883-point desktop, which is
the window's own size at the browser's density: right-sized and sharp, with
nothing on the wire having said so, and the Help card still reading `at 1x`. So
run the output wayvnc captures at the scale of the browsers that will look at
it; the gateway has nothing to configure. Applying that is live and keeps every
window — `swaymsg output HEADLESS-1 scale 2` over the IPC socket, which a headless
sway started by a user service leaves at `/run/user/<uid>/sway-ipc.*.sock` — so
there is no reason to re-render the sway config and restart the service for it.
A server whose desktop has no scale of its own — QEMU's VNC on a Proxmox guest,
where the framebuffer is the guest's display — gets the 3456×1766 pixels and
draws its 1x desktop into them; making that desktop right-sized on a Retina
screen is the guest's own display scaling.

Without a browser, the probe drives the resize path and prints the size and scale
each `resize` announces:

```sh
uv run --with websockets --with requests tests/ws_probe.py \
  --port <gateway port> --target sway --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize
```

`--viewport-after-resize` is needed on a generic server, which never sends the
display list the probe's viewport requests otherwise wait for. `--display` is the
screen the probe claims; at `@200` the viewport goes to the server as 3456×1766
pixels, at `@100` as 1728×883.

## wayvnc grants the layout to one client

The finding that looked like a regression and was not. wayvnc's resize callback
(`on_client_resize` in its `main.c`) remembers the **first client that resized**
as its *master layout client* and returns false for every other client until
that one disconnects. neatvnc turns the false into `ExtendedDesktopSize`
status 1, *prohibited*. So a browser on one gateway that has resized — the
installed release beside a development build, say — makes every resize from a
second gateway against the same wayvnc come back prohibited, and the second
desktop simply stays its size. The bytes on the wire are identical; only the
order of arrival differs. The fix is to disconnect the other client, not to debug
the gateway.

neatvnc's status codes, as `remotex serve` names them on stderr:

| Status | Meaning | On stderr |
|---|---|---|
| 0 | success | applied, `desktop resized from … to …` |
| 1 | prohibited — another client holds the layout, or the output is not headless | `server prohibited the SetDesktopSize; wayvnc grants the layout to the first client that resized…` |
| 2 | out of resources | `rejected … out of resources` |
| 3 | invalid layout | `rejected … invalid layout` |
| 4 | **request forwarded** — neatvnc's own code: the size went to the compositor and the real resize arrives as a server-initiated rect a moment later | `server forwarded the SetDesktopSize; the new size arrives as its own rect` (debug) |

A granted resize therefore looks like a status 4 reply immediately followed by a
`reason=0` rect at the new size; that is success, not a refusal followed by a
coincidence. Run with `RUST_LOG=remotex=debug` to see the `ExtendedDesktopSize`
rects and the *holding … until the server declares SetDesktopSize support* line
that precedes the first request on every connection — wayvnc declares support with
its first framebuffer update, about a second after connect, and a viewport report
sent before that is replayed on it.
