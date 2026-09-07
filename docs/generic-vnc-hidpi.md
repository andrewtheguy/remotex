# HiDPI over generic VNC

Why a plain VNC server — wayvnc on sway is the measured case — is shown at 1x
whatever scale its compositor renders at, and the one wayvnc rule that makes a
resize come back *prohibited*. Measured 2026-09-05 against wayvnc (server name
`WayVNC`) on a sway `HEADLESS-1` output through the gateway, with
`tests/ws_probe.py` reading the control messages a browser sees.

## Why a generic server is 1x

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
  ask about. So the generic VNC engine reports `UNSCALED` (`src/vnc.rs`), the
  browser shows every pixel one-to-one, and the Help card reads
  `1728×883 at 1x` under a browser at 2x.

The density a framebuffer is *labelled* with — `Resize.scale` — is the wire's
word alone. The browser does report its own screen's density (`hostDisplay`),
and on RDP and Apple High Performance that report is what the remote is *asked*
to render at; the label still waits on the wire's answer — Apple's display
layout reports the scale it granted, and RDP, which reports none back, is
labelled with the declared density only once the resize that carried it comes
back from the server. Generic VNC has neither answer, so it accepts no
client-declared density: a label nothing on the wire can confirm is one the
server may not have honoured, and a desktop presented at it is shown at the
wrong size. So a generic server is 1x, and stays 1x, until its protocol can say
otherwise — which is what the swayvnc density extension does for a sway desktop
behind a patched wayvnc, under `subtype = "swayvnc"`
([`swayvnc-density.md`](swayvnc-density.md)).

The consequence on a sway output at `scale 2` is that it is *worse* than at
`scale 1`: with `resize = true` the window asks for its 1728×883 points as
1728×883 pixels, sway makes that an 864×441 logical desktop, and the browser
stretches it back up. Half the workspace, still soft. Run the output wayvnc
captures at `scale 1` for a generic target. Applying that is live and keeps every
window — `swaymsg output HEADLESS-1 scale 1` over the IPC socket, which a headless
sway started by a user service leaves at `/run/user/<uid>/sway-ipc.*.sock` — so
there is no reason to re-render the sway config and restart the service for it.

Without a browser, the probe drives the resize path and prints the size and scale
each `resize` announces:

```sh
uv run --with websockets --with requests tests/ws_probe.py \
  --port <gateway port> --target sway --user <user> \
  --viewport 1728x883 --viewport-after-resize
```

`--viewport-after-resize` is needed on a generic server, which never sends the
display list the probe's viewport requests otherwise wait for.

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
