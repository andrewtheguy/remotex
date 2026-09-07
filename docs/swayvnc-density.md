# Pixel density over VNC with swayvnc

How a sway desktop behind wayvnc tells the gateway what scale its framebuffer is
drawn at, so a `scale 2` output is shown as a sharp 2x desktop rather than half a
desktop stretched up. Standard RFB carries pixels and nothing else
([`generic-vnc-hidpi.md`](generic-vnc-hidpi.md)); this is one private extension
on top of it, in the shape Apple's display layout already gives the gateway: the
*server* reports the density, and the label the browser sees is the wire's word.
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
target's. Against a stock wayvnc the request goes unanswered and the session
continues as generic RFB at 1x, with one warning on stderr.

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
screen changes density (`hostDisplay`). It carries the density the browser
would like the desktop rendered at, quantized to 1x or 2x like every other
engine's request ([`protocol::render_density`]). This version of wayvnc records
and logs it; acting on it is the next step, below.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE0` |
| 1 | U8[3] | padding |
| 4 | U32 | scale, 16.16 fixed |

Eight bytes.

## What the gateway does with it

`src/vnc.rs` keeps the extension's state per connection as `Density`: `Off`
on every other target, `Asked` from the handshake, `Reported` after the first
message, `Unanswered` if pixels arrive before any report.

- **Label.** Every generic `DesktopSize` and `ExtendedDesktopSize` rectangle is
  applied with the last reported scale; before the first report, or on any other
  target, that is `UNSCALED`. A report whose size is the current framebuffer's
  relabels it at once — the same pixels shown at a new density are a new canvas
  — and a report naming a size the framebuffer does not have yet is the label
  for the rectangle about to arrive.
- **Resize.** With `resize = true` the window's points are asked for as
  `points × scale` pixels, so the logical desktop is the window. The first
  request waits for the report, because a request in the wrong pixels is a
  desktop redrawn twice; a target the server never answers sends it at 1x on
  the first framebuffer update. Every report that changes the scale re-asks for
  the window in the new pixels, unless the report already names them.
- **Declare.** The browser's density goes to the server on the first report and
  on every change.

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
and the desktop's resize, and are what the next step removes. A plain target
against the same server stays at `scale=1.0` throughout and never sends the
pseudo-encoding.

Reproduce it:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --viewport-after-resize --seconds 24
# meanwhile, on the host
swaymsg output HEADLESS-1 scale 2 ; sleep 7 ; swaymsg output HEADLESS-1 scale 1
```

## Next: following the client

With the declared density on the server, the next version of the wayvnc patch
sets the captured output's scale to it when the two differ — the live
`swaymsg output <name> scale <n>` the host's Toggle Display Scale launcher entry
runs today, applied through wlr-output-management together with the mode the
resize already sets — and the browser's density then drives the desktop's with
nothing to configure, as RDP does. Nothing on the gateway side changes for it:
the report of the new scale and the re-asked size are the path measured above.
The gateway never applies a density the server has not reported.
