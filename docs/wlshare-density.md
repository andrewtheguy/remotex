# Pixel density over VNC with wlshare

How a wlroots-based Wayland desktop behind wlshare tells the gateway what scale
its framebuffer is drawn at, so the Help card can say `at 2x` for a `scale 2`
output — and how the browser's own density becomes that scale, so a 2x browser
gets a desktop drawn at 2x with nothing to configure, as it does over RDP.
Standard RFB carries pixels and nothing else
([`generic-vnc-hidpi.md`](generic-vnc-hidpi.md)); this is one private extension
on top of it, in the shape Apple's display layout already gives the gateway: the
*server* reports the density, and the label the browser sees is the wire's word.
The browser's density is only ever a request to the server, never a label — and
never a layout either: the browser draws every framebuffer pixel on one device
pixel whatever the label, and asks for the window in those pixels whatever the
server answers. What the extension changes is how much desktop the server draws
into them. The wire was measured 2026-09-07 on `workstation-ct`, a headless sway
with one `HEADLESS-1` output, through `tests/ws_probe.py`; the sequences below
follow the device-pixel rule the browser has drawn by since 2026-09-13.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), a VNC
server written for this: RFB 3.8 with ZRLE as its one pixel encoding,
wlr-screencopy capture, and this extension built in. It tracks each output's
exact scale from wlr-output-management, with `wl_output.scale` as the fallback,
and sets the output's scale and mode through the same protocol. It replaced a
patch series on neatvnc and wayvnc that carried the same wire.

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
host = "127.0.0.1"
port = 5900
username = "me"              # the account wlshare runs as; its [pam] table checks the login
password = "…"
resize = true
```

Nothing names the server: wlshare is a plain `vnc` target, and the extension is
discovered the way ContinuousUpdates and Fence are. The gateway lists the
pseudo-encoding in every generic `SetEncodings`, last so it never weighs on
encoding preference; wlshare answers it before its first framebuffer update, and
any other server ignores it, as RFB requires of an encoding it does not know,
and sends pixels. Pixels before any report settle the request as unanswered:
the desktop is generic RFB labelled 1x from there, and a report that arrives
after all is still taken, since the label is the wire's word. The Apple subtypes are not
asked; a Mac reports its densities in its display layout.

The credentials are the account wlshare runs as, carried by RSA-AES and checked
through PAM on the server, the way an `ard` target is a Mac account; wlshare
offers RSA-AES or None and nothing else, so a plain target with `username` and
`password` takes the encrypted login by the ordinary rule.

## The wire

One pseudo-encoding and one message type, private and unregistered.

- **Pseudo-encoding** `0x574c5348`, the ASCII bytes `WLSH`, listed in the
  client's `SetEncodings` beside the standard ones. A server that does not know
  it ignores it, as RFB requires.
- **Message type** `0xE0` in both directions, outside every registered client
  and server message type.
- **Scale** is 16.16 unsigned fixed point: `0x0002_0000` is 2.0, `0x0001_8000`
  is 1.5. The server sends the compositor's exact value, fractional included.

### Server → client: OutputScale

Sent as the answer to a `SetEncodings` carrying the pseudo-encoding — the only
way support is announced, the same pattern as ContinuousUpdates — and again
whenever the captured output's scale or size changes, or the capture moves to
another output. The size is the framebuffer's, in pixels.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE0` |
| 1 | U8 | padding |
| 2 | U16 | width, pixels |
| 4 | U16 | height, pixels |
| 6 | U32 | scale, 16.16 fixed |

Ten bytes. wlshare sends it as soon as the compositor reports the output's new
scale or mode, before the frame at the new size has been captured, so on a resize
the report precedes the `ExtendedDesktopSize` rectangle that carries the new
framebuffer.

### Client → server: ClientDensity

Sent once the first `OutputScale` has arrived, and again whenever the client's
screen changes density (`hostDisplay`) — only on a target with `resize = true`,
because the answer changes how much desktop the output's pixels hold, and only
a window that drives the size gets to ask for the pixels that give the desktop
back. It carries the density the browser would like the desktop rendered at:
the screen's exact ratio ([`protocol::scale_ratio`]), fractional included,
since a wlroots compositor renders at 1.5 where a Mac or an RDP host cannot.

wlshare sets the captured output's scale to it under the rules its
`SetDesktopSize` handling already has — a headless output, resizing enabled,
and the client owns the layout or nobody does yet — and **answers every
declaration with an `OutputScale`**: after the compositor has applied the
change, through the same head-scale path as a `swaymsg output … scale` in the
measured Sway session, or at
once with the scale as it is when nothing is to be changed or nothing can be
(resizing disabled, another client owning the layout, a density out of the
0.5–8 range, a configuration the compositor rejects). A configuration the
compositor accepts without changing the head's scale is answered too: wlshare
follows the `succeeded` with one round trip and reports the scale as it is
when no head change arrived by then. The gateway relies on that answer
arriving.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE0` |
| 1 | U8[3] | padding |
| 4 | U32 | scale, 16.16 fixed |

Eight bytes.

## What the gateway does with it

`src/vnc.rs` keeps the extension's state per connection as `Density`: `Off`
on the Apple dialects, `Asked` from the handshake, `Reported` after the first
message, and `Unanswered` once pixels have arrived while still `Asked`, since
wlshare answers `SetEncodings` before its first update and any other server
never will. `Unanswered` releases a held resize and still takes a late report.

- **Label.** Every generic `DesktopSize` and `ExtendedDesktopSize` rectangle is
  applied with the last reported scale; before the first report, or on any other
  target, that is `UNSCALED`. A report whose size is the current framebuffer's
  relabels it at once — the same pixels shown at a new density are a new canvas
  — and a report naming a size the framebuffer does not have yet is the label
  for the rectangle about to arrive.
- **Resize.** With `resize = true` the window's points are asked for as
  `points × the browser's density` pixels — the window's device pixels, which
  the browser shows one per framebuffer pixel — and the reported scale plays no
  part in the count (`DesktopState::generic_pixels`). The first request waits
  for the report, or for the first update that shows none is coming, because a
  reporting server is about to set its output's scale to the browser's and a
  desktop asked for before then is drawn twice; on a server without the
  extension nothing is lost by the wait, since the request also needs the
  `ExtendedDesktopSize` rect that update carries. Every report that changes
  the scale, or answers a declaration, re-asks for the window's pixels unless
  the report already names them, so an output whose mode moved under the window
  comes back to it. A relabel empties the browser's canvas, and the resize
  request's rect is what repaints it; when no request goes out, or the server
  refuses it, the gateway asks for the whole framebuffer instead, since the
  report arrives outside any `FramebufferUpdate` and an idle desktop would
  otherwise stay blank. A browser that moves to a screen of another density
  asks for the window again in its new pixels, held for the report if a
  declaration went out with it.
- **Declare and follow.** The browser's density goes to the server on the first
  report and on every change. When it differs from the reported scale, the
  declaration is a request: the server answers it with an `OutputScale`, after
  setting the output to it or at once with the scale unchanged when it refuses,
  and every resize request is held (`DesktopState::following`) until that
  report, including a window that changes size meanwhile, which replaces the
  held one. The answering report then releases the held resize — for the
  window's own pixels either way; what the server settled on decides how much
  desktop it draws into them, the browser's density when it followed and the
  old scale when it refused. One declaration is out at a time: a density that changes while one is
  unanswered is recorded, and the answering report declares it then, so the
  server is walked through one transition at a time and ends at the browser's
  newest density rather than the one it passed through. Declarations are
  decided and written under the same lock as resize requests, so the wire
  carries them in the order they were decided. The gateway never labels a
  framebuffer with a density the server has not reported, and never re-declares
  on a report that disagrees with it, so a `swaymsg output … scale` from inside
  the session is an override the gateway follows rather than fights.

The sequence for a 1728×883-point window on a 2x screen, connecting to an output
at `scale 2` and then toggling it to `scale 1` and back on the host:

```
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      connect: the window's pixels, reported at 2
resize  3456x1766  scale=1.0  -> 1728x883 CSS px      OutputScale 3456x1766 @ 1: relabel, same canvas
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      OutputScale 3456x1766 @ 2: relabel again
```

The pixels never change, because the window did not: a headless output keeps its
mode when its scale changes, and the window's pixels are what the gateway asks
for whatever the scale. What the person sees change is the desktop drawn into
them — a 1728×883-point desktop at `scale 2`, a 3456×1766-point one with
half-size UI at `scale 1` — and the Help card's label. Any other server against
the same target stays at `scale=1.0` throughout, never having answered the
pseudo-encoding.

Reproduce it:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 24
# meanwhile, on the host
swaymsg output HEADLESS-1 scale 2 ; sleep 7 ; swaymsg output HEADLESS-1 scale 1
```

## Following the client

A 2x browser connecting to an output at scale 1, with `resize = true`:

```
resize  3456x1802  scale=1.0  -> 1728x901 CSS px      connect: the framebuffer as it was, labelled 1
resize  3456x1802  scale=2.0  -> 1728x901 CSS px      OutputScale @ 2 answering the declaration: relabel
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      asked once, for the window's pixels; the rect follows
```

The first report says 1x; the gateway declares 2x and holds its resize. wlshare
sets the output's scale, the compositor's head change produces the second
report at 2x for the same pixels, and only then is the window asked for: one
mode change on the host, one desktop drawn. A 1x browser against the same output
at scale 2 runs the mirror sequence — `3456x1766 @ 1` (connect), `@ 2` (first
report), `@ 1` (the answer), then `1728x883 @ 1` asked once — and shows the
3456×1766 framebuffer at 3456×1766 CSS pixels, scrolling, until the smaller one
arrives. The Toggle Display Scale launcher entry on the host still works and is
followed like any other scale change; the browser's density is re-declared only
when it changes.

Reproduce it with the host at `scale 1`:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 10
```
