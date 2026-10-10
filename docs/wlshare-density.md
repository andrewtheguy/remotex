# Pixel density over VNC with wlshare

How a wlroots-based Wayland desktop behind wlshare tells the gateway what scale
its framebuffer is drawn at, so a `scale 2` output is shown as a sharp 2x desktop
rather than half a desktop stretched up — and how the browser's own density
becomes that scale, so a 2x browser gets a 2x desktop with nothing to configure,
as it does over RDP.
Standard RFB carries pixels and nothing else
([`standard-rfb-hidpi.md`](standard-rfb-hidpi.md)); this is one private extension
on top of it, in the shape Apple's display layout already gives the gateway: the
*server* reports the density, and the label the browser sees is the wire's word.
The browser's density is only ever a request to the server, never a label.
Measured 2026-09-07 on `workstation-ct`, a headless sway with one `HEADLESS-1`
output, through `tests/ws_probe.py`.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare), a VNC
server written for this: RFB 3.8 with ZRLE as its standard lossless pixel encoding
and a VP9 encoding of its own, 4:4:4, that the gateway passes through
([wlshare's stream, passed through](architecture.md#wlshares-stream-passed-through)),
wlr-screencopy capture, and this extension built in. It tracks each output's
exact scale from wlr-output-management, with `wl_output.scale` as the fallback,
and sets the output's scale and mode through the same protocol. It replaced a
patch series on neatvnc and wayvnc that carried the same wire.

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "wlshare"
host = "127.0.0.1"
port = 5900
username = "me"              # the account wlshare runs as; its [pam] table checks the login
password = "…"
```

The session is started with resize: its size is the window's, chosen under the
target at the picker.
`subtype = "wlshare"` names the server, and is what makes the gateway list the
pseudo-encoding in its `SetEncodings`, after everything that decides pixels and
immediately before the output-list request, so it never weighs on encoding
preference. wlshare answers it before its first framebuffer update. A target
that says wlshare and reaches some other server gets pixels with no report
before them, which settle the request as unanswered: the desktop is plain RFB
at 1x from there, and a report that arrives after all is still taken, since the
label is the wire's word. A plain `vnc` target is not asked, whatever server it
reaches, and is 1x; nor are the Apple subtypes, since a Mac reports its
densities in its display layout.

The credentials are the account wlshare runs as, carried by RSA-AES and checked
through PAM on the server, the way an `ard` target is a Mac account; wlshare
offers RSA-AES or None and nothing else, so a target with `username` and
`password` takes the encrypted login by the plain target's rule.

## The wire

The extension is wlshare's, and its messages and their layout are in wlshare's
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-density-extension):
the pseudo-encoding `WLSH` and message type `0xE0` in both directions,
`OutputScale` from the server and `ClientDensity` from the client, each the
framebuffer's size in pixels and a scale. What the gateway makes of them:

- **`OutputScale`** is the answer to the `SetEncodings` that lists the
  pseudo-encoding, which is the only way support is announced, and comes again
  whenever the shared output's scale or size changes or the capture moves to
  another output. On a resize it precedes the `ExtendedDesktopSize` rectangle
  that carries the new framebuffer.
- **`ClientDensity`** is sent once the first `OutputScale` has arrived, whenever
  the client's screen changes density (`hostDisplay`), and on a switch of shared
  output — in a session started with resize, and in any session of a pinch-zoom
  client (`HostDisplay::fit`: a phone or tablet, which fits the desktop to its
  width on a 2x or 3x screen). A pointer client's session at a kept size declares
  nothing and keeps the output's own scale. It carries the density the browser
  would like the desktop rendered at,
  quantized to 1x or 2x like every other engine's request
  ([`protocol::render_density`]), *and* the size in pixels at that density — the
  window, or the points the session keeps —
  every time, so a density change is one output configuration. A resize at an
  unchanged density is still `SetDesktopSize`.
- **Every declaration is answered** with an `OutputScale`, after the change or at
  once with the output as it is when wlshare refuses or has nothing to change.
  The gateway relies on that answer arriving. A new size then arrives as an
  `ExtendedDesktopSize` rectangle whose reason is this client.

## What the gateway does with it

`src/vnc.rs` keeps the extension's state per connection as `Density`: `Off`
on a plain target and on the Apple dialects, `Asked` from a `wlshare` target's
handshake, `Reported` after the first
message, and `Unanswered` once pixels have arrived while still `Asked`, since
wlshare answers `SetEncodings` before its first update and any other server
never will. `Unanswered` releases a held resize at 1x and still takes a late
report.

- **Label.** Every `DesktopSize` and `ExtendedDesktopSize` rectangle of a
  `wlshare` target is applied with the last reported scale; before the first
  report, or on a plain target, that is `UNSCALED`. A report whose size is the
  current framebuffer's
  relabels it at once — the same pixels shown at a new density are a new canvas
  — and a report naming a size the framebuffer does not have yet is the label
  for the rectangle about to arrive.
- **Resize.** In a session started with resize the window's points are asked for as
  `points × scale` pixels, so the logical desktop is the window. The first
  request waits for the report, or for the first update that shows none is
  coming, because a request in the wrong pixels is a desktop redrawn twice; on
  a server without the extension nothing is lost by the wait, since the
  request also needs the `ExtendedDesktopSize` rect that update carries. Every
  report that changes the scale, or answers a
  declaration, re-asks for the window in the new pixels, unless the report
  already names them. A relabel empties the browser's canvas, and the resize
  request's rect is what repaints it; when no request goes out, or the server
  refuses it, the gateway asks for the whole framebuffer instead, since the
  report arrives outside any `FramebufferUpdate` and an idle desktop would
  otherwise stay blank.
- **Declare and follow.** The browser's density goes to the server on the first
  report, on every change and on a switch of output, with the window in points
  × that density — the held window on the first report, or the desktop's own
  points before the browser has sized one. Every declaration is a request: the
  server answers it with an `OutputScale`, after setting the output's mode and
  scale or at once with the output unchanged when it refuses, and every resize
  request is held (`DesktopState::following`) until that report, including a
  window that changes size meanwhile, which replaces the held one. The
  answering report re-asks the window only when the server settled on other
  pixels: nothing when it followed, points × the old scale when it refused.
  One declaration is out at a time: a density that changes while one is
  unanswered is recorded, and the answering report declares it then, so the
  server is walked through one transition at a time and ends at the browser's
  newest density rather than the one it passed through. Declarations are
  decided and written under the same lock as resize requests, so the wire
  carries them in the order they were decided. The gateway never applies a
  density the server has not reported, and never re-declares on a report that
  disagrees with it, so a `swaymsg output … scale` from inside the session is
  an override the gateway follows rather than fights.

Measured sequence, a 1728×883-point window on a 2x screen while the output is
toggled from `scale 1` to `scale 2` and back on the host:

```
resize  1728x883   scale=1.0  -> 1728x883 CSS px      connect, output at scale 1
resize  1728x883   scale=2.0  -> 864x441.5 CSS px     OutputScale 1728x883 @ 2: relabel
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      re-asked at points × 2; the rect follows
resize  3456x1766  scale=1.0  -> 3456x1766 CSS px     OutputScale 3456x1766 @ 1: relabel
resize  1728x883   scale=1.0  -> 1728x883 CSS px      re-asked at points × 1
```

The two intermediate lines are the moment between the compositor's scale change
and the desktop's resize. Any other server against the same target stays at
`scale=1.0` throughout, never having answered the pseudo-encoding.

Reproduce it:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 24
# meanwhile, on the host
swaymsg output HEADLESS-1 scale 2 ; sleep 7 ; swaymsg output HEADLESS-1 scale 1
```

## Following the client

A 2x browser connecting to an output at scale 1, in a session started with resize:

```
resize  3456x1802  scale=1.0  -> 3456x1802 CSS px     connect: the framebuffer, unlabelled
                                                      ClientDensity 3456x1766 @ 2 declared
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      OutputScale 3456x1766 @ 2 answering it; the rect follows
```

The first report says 1x; the gateway declares 2x with the window at points ×
2 and holds any resize. wlshare sets the output's mode and scale in one
configuration, the compositor's head change produces the report at 2x for the
declared pixels, and the rect that follows carries them: one output
configuration on the host, one desktop drawn, and the logical size never
passes through half or double the window. A 1x browser against the same
output at scale 2 runs the mirror sequence. The Toggle Display Scale launcher
entry on the host still works and is followed like any other scale change;
the browser's density is re-declared only when it changes.

Reproduce it with the host at `scale 1`:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 10
```
