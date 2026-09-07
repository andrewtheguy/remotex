# Pixel density over VNC with swayvnc

How a sway desktop behind wayvnc tells the gateway what scale its framebuffer is
drawn at, so a `scale 2` output is shown as a sharp 2x desktop rather than half a
desktop stretched up — and how the browser's own density becomes that scale, so
a 2x browser gets a 2x desktop with nothing to configure, as it does over RDP.
Standard RFB carries pixels and nothing else
([`generic-vnc-hidpi.md`](generic-vnc-hidpi.md)); this is one private extension
on top of it, in the shape Apple's display layout already gives the gateway: the
*server* reports the density, and the label the browser sees is the wire's word.
The browser's density is only ever a request to the server, never a label.
Measured 2026-09-07 against the patched wayvnc 0.9.1 on `workstation-ct`, a
headless sway with one `HEADLESS-1` output, through `tests/ws_probe.py`.

The server side lives in the [swayvnc](https://github.com/andrewtheguy/swayvnc)
repository: patches on the Debian source packages of neatvnc 0.9.1 and wayvnc
0.9.1, built as `.deb` files for amd64 and arm64 and published as GitHub releases
tagged `trixie-<YYYYMMDD>-<N>`, the packages keeping Debian's versions with a
`+swayvnc<YYYYMMDD>.<N>` suffix. neatvnc gains three application hooks
(the client's `SetEncodings` list as sent, a handler for message types the
library does not dispatch, and a call to write one message to one client);
wayvnc uses them for the extension below and tracks each output's scale from
wlr-output-management, with `wl_output.scale` as the fallback.

## Configuration

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "swayvnc"
host = "127.0.0.1"
port = 5900
username = "andrew"          # wayvnc's RSA-AES login, as on a plain target
password = "…"
resize = true
```

The subtype is explicit because it changes what the gateway asks for on the
wire, and a plain VNC connection to the same server — or any other server — must
not. Authentication, clipboard, encodings and everything else are a plain `vnc`
target's. Against a stock wayvnc the request goes unanswered, and the session
ends with an error on the first framebuffer update: an explicit subtype naming a
server that is not there is a misconfiguration, not a desktop to show at a
density the server never confirmed. A plain `vnc` target is how that server is
reached.

## The wire

One pseudo-encoding and one message type, private and unregistered.

- **Pseudo-encoding** `0x53564E43`, the ASCII bytes `SVNC`, listed in the
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

Ten bytes. wayvnc sends it from `on_output_dimension_change` *before* it restarts
the capture, so on a resize the report precedes the `ExtendedDesktopSize`
rectangle that carries the new framebuffer.

### Client → server: ClientDensity

Sent once the first `OutputScale` has arrived, and again whenever the client's
screen changes density (`hostDisplay`) — only on a target with `resize = true`,
because the answer changes the output and a client that cannot then re-ask the
pixels would be left with half a desktop. It carries the density the browser
would like the desktop rendered at, quantized to 1x or 2x like every other
engine's request ([`protocol::render_density`]).

wayvnc sets the captured output's scale to it under the rules its
`SetDesktopSize` handling already has — a headless output, resizing enabled,
and the client owns the layout or nobody does yet — and **answers every
declaration with an `OutputScale`**: after the compositor has applied the
change, through the same head-scale path as a `swaymsg output … scale`, or at
once with the scale as it is when nothing is to be changed or nothing can be
(resizing disabled, another client owning the layout, a density out of the
0.5–8 range, a configuration the compositor rejects). A configuration the
compositor accepts without changing the head's scale is answered too: wayvnc
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
on every other target, `Asked` from the handshake, `Reported` after the first
message. Pixels arriving while still `Asked` end the session, since the patched
wayvnc answers `SetEncodings` before its first update.

- **Label.** Every generic `DesktopSize` and `ExtendedDesktopSize` rectangle is
  applied with the last reported scale; before the first report, or on any other
  target, that is `UNSCALED`. A report whose size is the current framebuffer's
  relabels it at once — the same pixels shown at a new density are a new canvas
  — and a report naming a size the framebuffer does not have yet is the label
  for the rectangle about to arrive.
- **Resize.** With `resize = true` the window's points are asked for as
  `points × scale` pixels, so the logical desktop is the window. The first
  request waits for the report, because a request in the wrong pixels is a
  desktop redrawn twice. Every report that changes the scale, or answers a
  declaration, re-asks for the window in the new pixels, unless the report
  already names them. A relabel empties the browser's canvas, and the resize
  request's rect is what repaints it; when no request goes out, or the server
  refuses it, the gateway asks for the whole framebuffer instead, since the
  report arrives outside any `FramebufferUpdate` and an idle desktop would
  otherwise stay blank.
- **Declare and follow.** The browser's density goes to the server on the first
  report and on every change. When it differs from the reported scale, the
  declaration is a request: the server answers it with an `OutputScale`, after
  setting the output to it or at once with the scale unchanged when it refuses,
  and every resize request is held (`DesktopState::following`) until that
  report, including a window that changes size meanwhile, which replaces the
  held one. The answering report then re-asks the window in whatever pixels
  the server settled on: 2x when it followed, the old scale when it refused.
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
and the desktop's resize. A plain target against the same server stays at
`scale=1.0` throughout and never sends the pseudo-encoding.

Reproduce it:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 24
# meanwhile, on the host
swaymsg output HEADLESS-1 scale 2 ; sleep 7 ; swaymsg output HEADLESS-1 scale 1
```

## Following the client, measured

A 2x browser connecting to an output at scale 1, with `resize = true`:

```
resize  3456x1802  scale=1.0  -> 3456x1802 CSS px     connect: the framebuffer, unlabelled
resize  3456x1802  scale=2.0  -> 1728x901 CSS px      OutputScale @ 2 answering the declaration: relabel
resize  3456x1766  scale=2.0  -> 1728x883 CSS px      asked once, at points × 2; the rect follows
```

The first report says 1x; the gateway declares 2x and holds its resize. wayvnc
sets the output's scale, the compositor's head change produces the second
report at 2x for the same pixels, and only then is the window asked for in
points × 2: one mode change on the host, one desktop drawn. A 1x browser
against the same output at scale 2 runs the mirror sequence, measured as
`3456x1766 @ 1` (connect), `@ 2` (first report), `@ 1` (the answer), then
`1728x883 @ 1` asked once; on a 1x browser the first report's label is a
canvas the browser holds for one round trip. The Toggle Display
Scale launcher entry on the host still works and is followed like any other
scale change; the browser's density is re-declared only when it changes.

Reproduce it with the host at `scale 1`:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 10
```
