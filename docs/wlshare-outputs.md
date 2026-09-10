# Switching outputs over VNC with wlshare

How a wlroots-based Wayland desktop with more than one monitor tells the gateway
what it has, and how the browser's display picker comes to name those outputs —
so a two-screen session is a choice in the floating menu rather than a second
target on a second port.

One RFB framebuffer is one output. wlshare captures one at a time and never
composes several into one picture, so the second monitor is not somewhere off the
edge of the desktop: it is not in the session at all until the client asks for it.
Standard RFB has no way to ask. `ExtendedDesktopSize` describes screens *inside*
one framebuffer, which is a different thing entirely, and a plain VNC server has
nothing else to say. So this is a second private extension beside
[the density one](wlshare-density.md), in the same shape and discovered the same
way: the client lists a pseudo-encoding, and the server that knows it answers.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare). Its
`outputs.rs` already tracks every `wl_output` and every wlr-output-management
head — names, modes and exact scales — because that is where the density report
comes from; this extension lists them and takes one back.

## Configuration

Nothing. wlshare is a plain `vnc` target, the same one
[the density doc](wlshare-density.md#configuration) configures, and the extension
is discovered on the connection:

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
host = "127.0.0.1"
port = 5900
username = "me"
password = "…"
resize = true
```

The gateway lists the pseudo-encoding in every generic `SetEncodings`, after the
density one and after everything that decides pixels, so it never weighs on
encoding preference. A server that does not know it ignores it, as RFB requires,
sends no list, and its client shows no picker.

## The wire

One pseudo-encoding and one message type, private and unregistered.

- **Pseudo-encoding** `0x574c534f`, the ASCII bytes `WLSO`, one past the density
  extension's `WLSH`.
- **Message type** `0xE1` in both directions, outside every registered client and
  server message type.
- **Scales** are the density extension's 16.16 unsigned fixed point.

### Server → client: OutputList

Sent as the answer to a `SetEncodings` carrying the pseudo-encoding — the only way
support is announced — again whenever the compositor's outputs, one of their
labels, or the shared one changes, and again as the answer to every
`SelectOutput`.

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE1` |
| 1 | U8 | padding |
| 2 | U16 | count |
| 4 | U32 | the shared output's id |

then `count` entries:

| Offset | Type | Field |
|---|---|---|
| 0 | U32 | id |
| 4 | U16 | width, pixels |
| 6 | U16 | height, pixels |
| 8 | U32 | scale, 16.16 fixed |
| 12 | U8 | flags — bit 0: headless |
| 13 | U8 | name length |
| 14 | U8[] | name, UTF-8 |

The id is the output's `wl_output` global, unique for as long as the output
exists and opaque to the client. The name is the compositor's own — `DP-2`,
`HEADLESS-1` — which is what the person at that desk sees in their own display
settings. Entries are ordered by name, so the menu's order does not depend on the
order the compositor announced them, and an output whose name or mode has not
arrived yet is not listed: it cannot be labelled, and capturing it would produce
nothing. The headless flag marks an output the compositor made rather than a
monitor somebody is sitting at — the only kind wlshare ever resizes or rescales.

### Client → server: SelectOutput

| Offset | Type | Field |
|---|---|---|
| 0 | U8 | `0xE1` |
| 1 | U8[3] | padding |
| 4 | U32 | id |

Eight bytes. Honoured from the client holding the desktop, and **answered with an
`OutputList` either way**: after the switch, or at once with the list as it stands
when the id names an output the compositor no longer has, or the one already being
shared. A request is never left without an answer, which is what lets the browser
keep no display state of its own.

What wlshare does with one: stop the capture, point the virtual pointer at the new
output (`zwlr_virtual_pointer` takes its output when it is made and never again,
so what the client holds is released and the pointer remade), take the new size
into the framebuffer blank, report the geometry, and capture again. The client is
sent nothing until a frame of the output it asked for has arrived — no pixels of
the screen it just left. A different size then reaches it as an
`ExtendedDesktopSize` rectangle with the server as the reason; a same-sized output
carries no rectangle at all and arrives as a full repaint.

## What the gateway does with it

`src/vnc.rs` reads the list into the same `DisplayState` the Apple dialect fills,
so everything downstream is already built: a `ServerMsg::Displays` control message
on every change, the display panel in the floating menu, and a `selectDisplay`
coming back. The engine forwards that as a `SelectOutput` — with a non-incremental
update request behind it, since a same-sized switch has no rectangle to repaint
through — and moves the checkmark only when a list comes back saying the server
moved it. A selection that named an output the list does not have is dropped
before the wire.

- **Labels.** `DP-2`, and under it the points that output occupies with the
  density that earns it more pixels: `1728×883 at 2x`, `1920×1080` at 1x. The
  same wording the Apple picker uses, because it means the same thing.
- **Nothing is optimistic.** The panel is a list plus a checkmark, both from the
  server. A switch the compositor refuses leaves the menu agreeing with what is on
  the canvas rather than with what was clicked.
- **Density follows the switch.** The browser re-declares its own density after
  the checkmark moves, and wlshare answers with an `OutputScale` as it always
  does — applying it on a headless output, and reporting the scale as it is on a
  monitor whose mode belongs to the person in front of it.
- **Resize follows the same rule.** Switching to a real monitor means resize
  requests are answered *prohibited* from then on; switching back to a headless
  output makes them work again. Neither is new: it is the rule wlshare already
  had, now reachable in one session.

## Not yet measured

Unlike the density extension, this one has no measured trace in this repo yet: it
was written against a single-output session and the two-monitor sequence — the
list, a switch, the `ExtendedDesktopSize` that follows it — has not been captured
with `tests/ws_probe.py` against a real two-monitor host. Run it there before
treating the sequence above as measurement rather than as what the two ends
intend:

```sh
uv run tests/ws_probe.py --port <gateway port> --target <name> --user <user> \
  --display 1728x1117@200 --viewport 1728x883 --seconds 20
```
