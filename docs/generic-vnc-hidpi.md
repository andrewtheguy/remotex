# HiDPI over generic VNC

How a plain VNC server — wayvnc on sway is the measured case — is shown at 2x,
why that takes a declaration the other protocols do not need, and the one wayvnc
rule that makes a resize come back *prohibited*. Measured 2026-09-05 against
wayvnc (server name `WayVNC`) on a sway `HEADLESS-1` output through the gateway,
with `tests/ws_probe.py` reading the control messages a browser sees.

## Why a generic server is 1x until told otherwise

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

Left there, a sway output at `scale 2` is *worse* than at `scale 1`: the window
asks for its 1728×883 points as 1728×883 pixels, sway makes that an 864×441
logical desktop, and the browser stretches it back up. Half the workspace, still
soft.

## The declaration

Since neither end can measure it, the density of a generic server is **declared**
from the client — the one place the project's no-client-side-render-controls rule
does not apply, because the protocol forces it (see `AGENTS.md`, Display
geometry). It is not scaling: the browser still shows every framebuffer pixel
one-to-one, at the CSS size `pixels / scale` like every other 2x desktop.

- The floating ☰ menu has a **Density** section on plain VNC targets only
  (`protocol = "vnc"`, no `subtype`); RDP and both Apple modes hide it, since a
  declaration there would contradict what their wire already states. Its one
  button reads `Remote is 1x` or `Remote is 2x (HiDPI)` and sends
  `ClientMsg::Density { scale: 100 | 200 }`.
- The engine keeps the declared density per session, 1x until declared, never
  remembered — a new engine starts at 1x.
- With `resize = true` and a server that has declared `SetDesktopSize` support,
  the declaration is a resize: every generic `SetDesktopSize` asks for
  `viewport points × density` pixels (held under the video stream's picture
  ceiling where the target streams), and the reply labels the framebuffer with the
  declared scale in `ServerMsg::Resize`. Toggling back asks for the points again.
- Without resize (the target has `resize = false`, or the server has not sent an
  `ExtendedDesktopSize` rect yet), the declaration re-labels the pixels the server
  is already sending and asks for a full repaint: a sharp desktop at half the CSS
  size, which is the truth about a 2x framebuffer the window cannot grow. With
  `resize = true` the density-adjusted request is stashed as well, and the rect
  that declares support replays it.
- A rejected `SetDesktopSize` keeps the size and still takes the label.
- The button's state is read off the last `resize`, not kept locally, so a
  declaration the server did not honour shows as what actually happened.

## Manually: sway + wayvnc at 2x, live

Both halves change at run time, and neither ends the session: sway applies an
output's scale over its IPC socket with every window kept, and remotex's Density
toggle is a resize on the running engine. Do **not** re-render the sway config
and restart the sway service to flip scale — that tears the desktop down and
closes every window in it, which is the one thing this walkthrough avoids.

1. Set the scale on the output wayvnc captures, over IPC, from any shell on the
   sway host. A headless sway started by a user service has no `SWAYSOCK` in an
   SSH shell, so name the socket:

   ```sh
   export SWAYSOCK=$(ls /run/user/$(id -u)/sway-ipc.*.sock | head -1)
   swaymsg output HEADLESS-1 scale 2
   swaymsg -t get_outputs | jq '.[] | {name, scale, current_mode, rect}'
   ```

   The output reports `scale: 2` at once; its pixel mode is unchanged, so the
   logical desktop halves (a 1728×883 framebuffer is now 864×441 logical) and the
   browser shows it stretched until step 3. Only the *scale* is set here; the
   pixel mode is the client's to ask for — wayvnc forwards `SetDesktopSize` to the
   compositor as a custom output mode, which a headless output accepts.

   To make it survive the next sway start, also put `output HEADLESS-1 scale 2`
   in the sway config and apply that with `swaymsg reload`, which re-reads the
   config in place. Still no restart.

2. The remotex target is an ordinary generic VNC target with resize on:

   ```toml
   [[targets]]
   name = "sway"
   protocol = "vnc"
   host = "127.0.0.1"
   port = 5900
   resize = true
   ```

   Nothing to change here between 1x and 2x; the density is declared per session
   from the client.

3. In the connected browser, open ☰ and click **Remote is 1x** under Density.
   The engine asks wayvnc for the window's points at 2x — a 1728×883 window
   becomes a 3456×1766 request — sway makes that a 1728×883 logical desktop at
   scale 2, and the browser shows it sharp at 100%. The Help card then reads
   `3456×1766 at 2x (192 dpi)` beside `This browser: 2x`. Windows on the desktop
   keep their logical size and position; only the pixels behind them double.

   The order of steps 1 and 3 does not matter — the toggle first gives a 2x
   request against a 1x output, which is a large desktop with tiny text until the
   scale follows — but doing the scale first keeps the in-between state short.

4. Back to 1x, also live:

   ```sh
   swaymsg output HEADLESS-1 scale 1
   ```

   then click **Remote is 2x (HiDPI)** to declare 1x and ask for the points back.
   remotex cannot see the output's scale change either way, so the toggle is
   always needed beside the `swaymsg`.

Without a browser, the probe drives the same path and prints the scale each
`resize` announces:

```sh
uv run --with websockets --with requests tests/ws_probe.py \
  --port <gateway port> --target sway --user <user> \
  --viewport 1728x883 --viewport-after-resize --density 200
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
