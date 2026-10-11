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
nothing else to say. So this is a private extension beside
[the density one](wlshare-density.md), with the same shape and asked for the
same way: a `wlshare` target lists a pseudo-encoding, and wlshare answers.

The server side is [wlshare](https://github.com/andrewtheguy/wlshare). Its
`outputs.rs` already tracks every `wl_output` and every wlr-output-management
head — names, modes and exact scales — because that is where the density report
comes from; this extension lists them and takes one back.

## Configuration

Nothing beyond the subtype. It is the `wlshare` target
[the density doc](wlshare-density.md#configuration) configures:

```toml
[[targets]]
name = "workstation"
protocol = "vnc"
subtype = "wlshare"
host = "127.0.0.1"
port = 5900
username = "me"
password = "…"
```

The gateway lists the pseudo-encoding in a `wlshare` target's `SetEncodings`,
after the density one and after everything that decides pixels, so it never
weighs on encoding preference. A plain `vnc` target does not list it, is sent no
list, and its client shows no picker: a wlshare server behind one stays on the
output it opened with.

## The wire

The extension is wlshare's, and its messages and their layouts are in wlshare's
own [`docs/architecture.md`](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#the-outputs-extension):
the pseudo-encoding `WLSO` and message type `0xE1` in both directions,
`OutputList` from the server and `SelectOutput` from the client. What the
gateway leans on:

- **The list is the announcement.** It answers the `SetEncodings` that lists the
  pseudo-encoding, and comes again whenever the compositor's outputs, one of
  their labels, or the shared one changes.
- **An entry is a label.** The id is opaque. The name is the compositor's own —
  `DP-2`, `HEADLESS-1` — which is what the person at that desk sees in their own
  display settings, and the entries are ordered by name, so the menu's order
  does not depend on the order the compositor announced them.
- **Every `SelectOutput` is answered with an `OutputList`**, after the switch or
  at once with the list as it stands, which is what lets the browser keep no
  display state of its own.
- **A switch sends nothing of the screen left behind.** The client is sent
  nothing until a frame of the output it asked for has arrived. A different size
  then reaches it as an `ExtendedDesktopSize` rectangle with the server as the
  reason; a same-sized output carries no rectangle at all and arrives as a full
  repaint.
- **That first frame does not wait on damage**, and is taken whole
  ([Capture](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#capture)).
  Both matter to a gateway: the second monitor of an idle desk may not change
  for minutes, so a switch that waited would produce no rectangle, no resize and
  no frames until somebody moved the mouse.

## What the gateway does with it

`src/vnc.rs` reads the list into the same `DisplayState` the Apple dialect fills,
so everything downstream is already built: a `ServerMsg::Displays` control message
on every change, the display panel in the floating menu, and a `selectDisplay`
coming back. The engine forwards that as a `SelectOutput` — with a non-incremental
update request behind it, since a same-sized switch has no rectangle to repaint
through — and moves the checkmark only when a list comes back saying the server
moved it. A selection that named an output the list does not have is dropped
before the wire.

- **Labels.** Each menu entry shows the compositor name, such as `DP-2`, followed
  by its logical size and scale: `1728×883 at 2x` or `1920×1080 at 1x`. This is
  the same representation used by the Apple display picker.
- **Selection acknowledgement.** A click sends `SelectOutput`, but the checkmark
  changes only when the next `OutputList` names that output as active. If the ID
  is no longer selectable when wlshare handles it, wlshare sends the current list
  without switching, so the menu continues to describe the canvas actually being
  shown. The gateway rejects IDs absent from its most recent list before sending a
  request.
- **Density declaration.** Once an `OutputList` confirms a switch, the gateway
  repeats the browser's density declaration for the new output when the session
  follows the window or when a touch client declares density at a kept size. A
  pointer client at a kept size declares none. The declaration includes the
  browser window's pixel size even when the scale is unchanged: a headless output
  can adopt that size, while a physical monitor retains its user-owned mode and
  reports the resulting `OutputScale`. If an earlier density request is still
  outstanding, the new declaration waits for its response.
- **Framebuffer size.** A differently sized output is announced with an
  `ExtendedDesktopSize` rectangle whether or not the session lets the browser
  request resizes. The gateway advertises both RFB size pseudo-encodings on every
  VNC target so wlshare has a negotiated way to change the framebuffer geometry.
  After a server-side size change, wlshare disconnects at the next update if the
  client advertised neither, rather than sending pixels with dimensions the
  client cannot learn. That is wlshare's implementation policy, not a general
  RFB-server requirement.
- **Empty lists.** If the compositor loses its last output, the gateway forwards
  an empty list so a newly attached browser removes stale display choices.
- **Resize permission.** Even when the session permits browser-driven resize,
  wlshare answers a request on a physical monitor as prohibited: it reconfigures
  headless outputs only. Switching to a headless output makes resizing eligible
  again, but it still requires wlr-output-management and compositor acceptance.

## Two outputs at once

A desk with exactly two outputs is listed with a third entry, *All Displays*
(alpha), which is what such a desk starts on, at its first list and whenever its
outputs become two unless one output alone was picked: the first output stays on the canvas and the second opens at
`/display/2` in a tab of its own, as an RDP target's or a High Performance Mac's
second virtual display does. The entry is the gateway's, not wlshare's, and no
target key asks for it: the outputs are the compositor's, two monitors on a
desk or two headless outputs it was started with (`WLR_HEADLESS_OUTPUTS=2`, or
sway's `create_output`).

wlshare shows a connection one output, so the tab is a second connection
([A display beside](https://github.com/andrewtheguy/wlshare/blob/main/docs/architecture.md#a-display-beside)).
What the gateway does:

- **The canvas goes to the first output.** Starting on *All Displays*, or
  choosing it, while the canvas is on the second sends a `SelectOutput` for the first, and the entry is
  checked, with the second marked for its tab, once the list comes back saying
  so. With the canvas already there the list is answered from the gateway and
  wlshare is asked nothing.
- **The tab's socket opens the second connection**, with the target's login and
  `0xB5` as its ClientInit byte, and wlshare shows it the output the first is
  not on. It is a session of its own into that socket: wlshare's VP9 passed, with
  a walk of the tab's own link, the cursor, the size and the density. On a
  session that follows the window the tab's window sizes a headless second
  output at the density of the screen that window is on, which the tab states
  on its display socket as the session's page does on its own; a monitor keeps
  its mode.
- **It lists nothing of the session's.** No output list, no clipboard, no sound,
  camera or microphone: those are the first connection's.
- **Choosing one output ends it** before wlshare is asked for that output, and
  so does a list that is no longer two outputs, or the tab closing. wlshare ends
  it on its own side when the first connection selects its output or leaves.

Checked 2026-10-04 with `tests/ws_probe.py --select 0xffffffff --tab` against a
headless sway with `HEADLESS-1` and `HEADLESS-2`, and against one with two
monitors, 1920×1080 and 1024×768: each tab was sent its own output's size and
pictures while the canvas kept the first, and wlshare logged the second
connection as `showing output … beside`. `tests/wlshare_e2e.rs` runs the same
against a container.

## Measured

Measured 2026-09-09 on `macintel`, a sway session with two physical outputs —
`HDMI-A-1` at 1920×1080 and `LVDS-1` at 1280×800, both at scale 1 — through
`tests/ws_probe.py` over an SSH tunnel to wlshare on its loopback port. That
session's wlshare has `resize = false`, since it shares physical panels.

Connect, then switch to the laptop panel:

```
resize  1920x1080  scale=1.0        connect: the output the session is on
displays  active=0x32               HDMI-A-1 1920×1080, LVDS-1 1280×800
-> selectDisplay 0x34
displays  active=0x34               the checkmark moves when the server says it moved
resize  1280x800   scale=1.0        the ExtendedDesktopSize rect for the new output
                                    then the frames of the new screen
```

The ids are the `wl_output` globals, `0x32` and `0x34` here. Switching back runs
the mirror sequence and repaints at 1920×1080.

The pointer goes with the capture. A `mouseMove` to the centre of the new
1280×800 framebuffer moved sway's focused output to `LVDS-1`, and the same move
after switching back moved it to `HDMI-A-1` — the virtual pointer is remade
against the output being shared, so absolute positions land on the screen the
client is looking at rather than on the one it left.

Three more, measured the same way:

- **The output already shared.** Answered with the list, no resize, and the frames
  keep coming: a second click on the checkmark costs a message and nothing else.
- **An id the list does not have.** Dropped by the engine before the wire — the
  gateway logs `ignoring a selection of unknown display 153` — so the server is
  never asked to bind to something that is gone.
- **An output arriving or leaving mid-session.** `swaymsg output LVDS-1 disable`
  shrank the list to one entry (a client then shows no picker at all, which is
  the rule for a list of one), and re-enabling it brought the output back under a
  **new** id: the global is unique for as long as that output exists, not across
  its life.

An idle-output switch does not depend on later input. In the 2026-09-09 probe,
`selectDisplay` was followed by the new output's resize and a complete framebuffer
update without any pointer event. The run completed within five seconds, but that
duration was an observation, not a protocol or scheduler bound.

The same probe against an older wlshare build received neither a resize nor a
frame until the pointer moved. That server waited for damage instead of capturing
the selected output once in full. The regression condition is therefore the event
sequence — selection, resize when needed, then a complete update without input —
rather than a pixel colour or the five-second timing from one run.

The choice outlives the client. wlshare keeps the output the last client asked
for — a reconnect opens on that one, not on the configured default — until the
daemon restarts.

One thing worth knowing when testing an idle host: with the monitors asleep
(`swaymsg output * power off`, which swayidle does on a timer), wlr-screencopy
fails every frame and no pixels arrive at all. That is not this extension, and it
is not the switch — it is the same for one monitor as for two — but it does make
a switch look like it did nothing. `swaymsg "output * power on"` first.
