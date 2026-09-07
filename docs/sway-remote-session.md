# A virtual-display remote session for sway

Console-style remote control of a physical sway machine: every physical display
is replaced by one resizable virtual display for the length of the session, the
gateway renders it at the browser's density with nothing to configure, and the
person at the keyboard takes control back through a Linux virtual console.
Stage 1 of the plan, the compositor-side choreography with a stock wayvnc, is
measured; see [Stage 1, measured on macintel](#stage-1-measured-on-macintel).
The wire extension and the daemon are still design, and the claims marked
*unverified* are the ones no measurement has reached. The reference host is the
Intel Mac running Debian and sway (`ssh macintel`), whose headless wayvnc is the
measured case in [`generic-vnc-hidpi.md`](generic-vnc-hidpi.md).

## Decision

**Keep RFB as the wire and add one private message pair.** The remote session
is a sway headless output, which is the same virtual-display idea Apple's High
Performance mode and the Windows console session use. wayvnc already captures
it, resizes it on request, injects input and syncs the clipboard, and the
gateway's generic VNC engine already speaks everything wayvnc offers. What
neither side has is a way to carry pixel density and session state, and a
controller on the sway host that folds every physical display into the virtual
one and gives the local user a way back. Those are the two new pieces.

The protocol choice is the small decision. The compositor-side choreography is
the work, and it is identical under any protocol, so the protocol is chosen to
minimise new client and server code — exactly the way the gateway treats Apple
RFB 003.889: standard RFB plus a few measured private messages behind a
`subtype`. The result is `subtype = "sway"` on `protocol = "vnc"`, and a session
daemon on the sway host that is wayvnc plus a small neatvnc patch plus a session
controller. It also closes the roadmap's open item on automatic density for
generic VNC, because the server finally answers with a scale.

### Alternatives

| | VNC dialect | Custom protocol | RDP server |
|---|---|---|---|
| New gateway code | One subtype, two messages | A fourth engine: transport, auth, decode, input, clipboard, resize | None for the wire; the FreeRDP client already declares density and resizes |
| New server code | Session controller, console watcher, neatvnc hook | Everything wayvnc already does, again, plus the controller | An RDP server for wlroots. None exists; libfreerdp-server gives framing, not capture or input |
| Resize and density | Resize exists; density is the new message | First class, but built from nothing | Free on the wire, expensive on the server |
| Clipboard | Exists on both ends | New on both ends | CLIPRDR exists in the library; wlr-data-control glue is new |
| Off-the-shelf clients for debugging | Any VNC viewer sees the picture | None | Any RDP client |
| Wire surface to maintain | Small, ours | All of it, ours | Large, not ours |

A custom protocol buys nothing the display-switching story needs, because the
gateway re-encodes every tile to VP9 anyway. RDP would be the pick if a wlroots
RDP server existed; writing one is the largest of the three jobs and the display
consolidation would still be custom.

## Scope

v1 must:

- Present the whole sway desktop on **one virtual display** whose size follows
  the browser window continuously, like Apple dynamic resolution and Windows
  display control.
- **Consolidate every physical display** into that one virtual display, however
  many are connected, the way Apple High Performance mode does. Their workspaces
  move onto it and they are disabled for the length of the session.
- Render that display at the **browser's density**, so a Retina window gets a 2x
  desktop with nothing to configure. Density and size change together, in one
  request.
- **Keyboard, pointer and clipboard** both ways. Text only for the clipboard.
- Give the person at the machine an **escape hatch that does not depend on the
  compositor showing anything**: a switch to a Linux virtual console. Whether the
  switch itself ends the session, or a login on that console does, is a host-side
  policy.
- Restore the desktop exactly as it was on any exit, including a daemon crash.

v1 will not:

- Carry audio, microphone or camera. The gateway's audio and camera sockets are
  not touched.
- Offer more than one virtual display, a mirror of the physical layout, or a
  curtain notice on the physical panels. One session, one output, and the panels
  are simply off.
- Serve two clients at once. A second connection replaces the first, matching
  the gateway's own takeover rule.
- Work on compositors other than sway. The controller speaks sway IPC by design.

## Architecture

```
Browser SPA ──► Gateway ──────────► Session daemon on the sway host ──► sway
 viewport in     VNC engine          wayvnc: capture, input, clipboard    HEADLESS-1  the session, resizable
 points+density  subtype = "sway"    neatvnc + private message hook       eDP-1, DP-1, …  disabled for the session
 VP9 tiles       RSA-AES, ZRLE       session controller (sway IPC)        tty3  the escape hatch
                                     console watcher + escape keymap
```

The gateway reaches the daemon the way it reaches wayvnc today: a TCP port on
the host, tunnelled over SSH for macintel. Authentication stays RSA-AES against
the daemon's key, which the gateway already speaks. Nothing changes between
browser and gateway except that the density label is now the server's word
rather than a fixed 1x, and a session can end with a reason the browser can show.

The daemon is one process because the session controller must act on connect
and disconnect and must know whether a resize was granted. Splitting it from the
VNC server would need a second channel with its own reachability and failure
modes, which is the objection [`roadmap.md`](roadmap.md#automatic-density-on-generic-vnc)
already records.

## Session choreography

A session starts when the first display request arrives, not on connect: until
then the daemon has no geometry to build the output from. It ends when the
client disconnects, when the local user takes over, or when another client
replaces it. Every end runs the same restore.

```
Gateway                     Daemon                              sway
  │ RFB handshake, RSA-AES, SetEncodings (+ SWAY pseudo-encoding)
  │──────────────────────────►│
  │◄──────────────────────────│ SessionLayout state=idle
  │ SetSessionDisplay 1728×883 pt @ 2.0
  │──────────────────────────►│ snapshot outputs, inputs, workspaces ─►│
  │                           │ create_output; HEADLESS-1 scale 2 mode 3456×1766 ─►│
  │                           │ move every workspace to HEADLESS-1; focus it ─►│
  │                           │ escape keymap on physical keyboards; pointers off ─►│
  │                           │ disable eDP-1, DP-1, … ─►│
  │◄──────────────────────────│ SessionLayout state=active 3456×1766 @ 2.0
  │◄──────────────────────────│ ExtendedDesktopSize rect 3456×1766, then frames
  │ SetSessionDisplay 1400×900 pt @ 2.0   (window resized)
  │──────────────────────────►│ HEADLESS-1 mode 2800×1800 ─►│
  │◄──────────────────────────│ SessionLayout active, ExtendedDesktopSize rect, frames
  │                           │      local user presses Ctrl+Alt+F3; sway switches VT
  │                           │ /sys/class/tty/tty0/active changes (takeover = "vt")
  │◄──────────────────────────│ SessionLayout state=ended reason=local-takeover
  │◄──────────────────────────│ close
  │                           │ restore snapshot; unplug HEADLESS-1; swaylock (policy) ─►│
  │                           │      user returns to sway's VT; desktop is back on the panels
```

| State | Enter | Leave | Wire |
|---|---|---|---|
| Idle | Daemon start, or restore complete | First `SetSessionDisplay` from an authenticated client | Ack with `SessionLayout idle` after `SetEncodings` |
| Active | Output created, workspaces moved, physical outputs disabled | Disconnect, local takeover, replacement, daemon shutdown, compositor refusal | `SessionLayout active` after every applied request |
| Ending | Any leave from Active | Restore finished | `SessionLayout ended` with a reason, then close |

**Restore is the invariant.** Before touching anything the daemon writes a
snapshot under `$XDG_RUNTIME_DIR`: every output's enabled state, mode, scale,
position and transform; every input's identifier and event state; every
workspace's output. Restore replays it. On start, if a snapshot file exists, the
daemon restores before it listens, so a crash mid-session leaves the desktop
recoverable by restarting the daemon. A `--restore` subcommand does the same by
hand.

## Displays and the escape hatch

The session replaces the physical displays rather than covering them. On start
the controller creates the headless output, moves every workspace from every
physical output onto it, focuses it, and then disables each physical output. The
panels go dark, the desktop is exactly one virtual display, and how many panels
there were is a fact only the snapshot remembers. On any end the snapshot puts
each panel back with its mode, scale, position and transform, and returns each
workspace to the output it came from.

**Why disable rather than curtain.** A curtain surface has one real job:
holding the local keyboard so keystrokes do not land in the remote session.
Linux already has a better answer. The virtual console switch is handled by sway
on the physical keyboard, works while every output is disabled, and once the
user is on a console the kernel owns the keyboard and nothing typed there can
reach the compositor at all. That is stronger isolation than any overlay, and it
needs no drawing code.

### The escape hatch

The person at the machine presses `Ctrl+Alt+F3`. sway sees the chord on the
physical keyboard, translates it to `XF86Switch_VT_3`, and asks logind to
switch. The daemon notices the switch by watching `/sys/class/tty/tty0/active`,
which names the active console and is pollable, so no D-Bus is needed;
`sd_login_monitor` is the alternative if a systemd session is the better
signal. What happens next is a host-side policy:

| Policy | The VT switch itself | Ending the session | Coming back | Resembles |
|---|---|---|---|---|
| `takeover = "vt"` | Ends the session and restores the panels at once | The chord is the whole gesture | `Ctrl+Alt+F1` back to sway's VT; the desktop is already on the panels. `swaylock` first when `takeover_lock = true` | Apple: press a key, sharing ends |
| `takeover = "login"` | Nothing; the remote session continues, the panels stay off | Log in on the console and run `<daemon> end` | Same chord back; the login was the credential check | Windows: sign in locally to take the console |

Under the login policy a local console session and the remote session coexist
safely: the console keyboard never reaches sway, and the headless output keeps
rendering because it is not bound to DRM. Under the vt policy the restore may
run while sway's VT is inactive; wlroots is expected to apply the output changes
when sway regains the VT (*unverified*).

### Local input while the panels are off

**Keyboards stay enabled**, because sway can only switch VT from a key it
receives: `events disabled` stops libinput from delivering anything, the chord
included. Instead each physical keyboard gets a session keymap through
`input <identifier> xkb_file`: a full XKB map in which every key is `NoSymbol`
except the modifiers and `F1` to `F12`, which carry `XF86Switch_VT_n` under
Ctrl+Alt. Typed keys reach nothing; the chord still works. The same map can put
`XF86Switch_VT_3` on a bare `Esc` so that one key is the gesture, as on a Mac.
wayvnc's virtual keyboard carries its own keymap over the virtual-keyboard
protocol, so the remote user is unaffected: measured, its layout stays
`English (US)` while the physical keyboards carry the session map.

**Pointers are disabled** by identifier, from the snapshot taken before the
virtual pointer existed. A pointer has no part in the escape, and a stray local
click into the remote session is the one leak the keymap does not close. Never
disable by `type:pointer`, which would also catch wayvnc's device.

**Restore** re-enables the pointers and removes the session keymap with
`input <identifier> xkb_file -`, which unsets the file and returns the device
to the layout the config gives it.

### Sway commands, in order

```sh
# start
swaymsg -t get_outputs; swaymsg -t get_inputs; swaymsg -t get_workspaces   # snapshot
swaymsg create_output                                                      # HEADLESS-n
swaymsg -- output HEADLESS-n scale 2                                       # one property per command:
swaymsg -- output HEADLESS-n mode --custom 3456x1766@60Hz                  # combined with enable, sway 1.10
swaymsg 'workspace 1; move workspace to output HEADLESS-n'                 # reports success and does nothing
swaymsg focus output HEADLESS-n                                            # each workspace, every panel
swaymsg input 1452:594:Apple_Inc._Apple_Internal_Keyboard_/_Trackpad xkb_file /run/user/1000/<daemon>/escape.xkb
swaymsg input 1452:594:bcm5974 events disabled                             # each physical pointer
wayvncctl output-set HEADLESS-n; until wayvncctl -j output-list shows it captured
swaymsg output LVDS-1 disable                                              # each physical output
swaymsg output HEADLESS-n position 0 0

# resize: wayvnc's existing path, wlr-output-management custom mode + scale

# end
swaymsg output LVDS-1 enable; mode 1280x800@60.223Hz; scale 1; position 0 0; transform normal   # per snapshot, one command each
swaymsg 'workspace 1; move workspace to output LVDS-1'                     # per snapshot
swaymsg input 1452:594:bcm5974 events enabled
swaymsg input 1452:594:Apple_Inc._Apple_Internal_Keyboard_/_Trackpad xkb_file -   # drops the session map
wayvncctl output-set LVDS-1; until it shows captured
swaymsg output HEADLESS-n unplug
swaylock -f                                                                # takeover_lock = true
```

Order matters twice. Workspaces move before the panels are disabled, so the
migration is explicit rather than sway's own choice of surviving output. On
restore the panels come back before the workspaces, so each has somewhere to
go. The mode and scale on resize go through wlr-output-management, the path
wayvnc already uses and the one that returns the granted-or-refused answer
synchronously. Everything else is IPC, which is the only way to create an
output, move a workspace, or touch input devices.

## Wire extension

Standard RFB 3.8 carries everything but density and session state. The dialect
adds one pseudo-encoding and one message type used in both directions. Numbers
are private and unregistered, which is fine because both ends are ours; a stock
wayvnc ignores the pseudo-encoding and never sees the message, because the
client waits for the ack.

- **Pseudo-encoding** `0x53574159`, the ASCII bytes `SWAY`, sent in the
  client's `SetEncodings` beside the standard list.
- **Message type** `0xE0` in both directions, outside every registered client
  and server type.
- **Ack.** The server answers a `SetEncodings` carrying the pseudo-encoding with
  `SessionLayout state = idle`. That is the only way support is announced — the
  same pattern as ContinuousUpdates.
- **Scale** is 16.16 unsigned fixed point. The v1 gateway sends 1.0 or 2.0,
  quantised as on RDP; the field allows sway's fractional scales later without a
  wire change.

### Client → server: SetSessionDisplay

Sent once the ack has arrived, before the first framebuffer request, and again
on every viewport change while `resize = true`. The first one starts the
session. Size is in points; the daemon derives the pixel mode as points × scale.

| Offset | Type | Field | Meaning |
|---|---|---|---|
| 0 | U8 | message-type | `0xE0` |
| 1 | U8 | padding | 0 |
| 2 | U16 | width | points |
| 4 | U16 | height | points |
| 6 | U32 | scale | 16.16 fixed; `0x0002_0000` is 2.0 |

Ten bytes. The standard `SetDesktopSize` is not sent on this subtype; the
daemon answers it with status 1, prohibited, if a client tries.

### Server → client: SessionLayout

Sent as the ack, after every applied request, and once before the connection
closes. The pixel size is the framebuffer's; the scale is the label the gateway
attaches to it.

| Offset | Type | Field | Meaning |
|---|---|---|---|
| 0 | U8 | message-type | `0xE0` |
| 1 | U8 | state | 0 idle · 1 active · 2 ended |
| 2 | U8 | reason | 0 none · 1 client closed · 2 local takeover · 3 compositor refused · 4 daemon shutting down · 5 replaced by another client |
| 3 | U8 | takeover | 0 vt · 1 login — the policy in force, so the browser can say how the session may end |
| 4 | U16 | width | pixels |
| 6 | U16 | height | pixels |
| 8 | U32 | scale | 16.16 fixed |

Twelve bytes. The framebuffer size change itself still arrives as neatvnc's
server-initiated `ExtendedDesktopSize` rectangle, which the gateway already
handles. Ordering rule: `SessionLayout` precedes the rectangle it describes,
and the gateway labels every framebuffer with the scale of the most recent
`SessionLayout`. A rectangle whose pixels disagree with that message is applied
anyway and logged; it means the compositor changed the mode on its own.

### What stays exactly as it is

- Version 3.8 handshake, RSA-AES security types 5 and 129, the daemon's key
  fingerprint logged, not verified.
- ZRLE, zlib, Hextile, RRE, Raw and CopyRect in the gateway's existing
  preference order; ContinuousUpdates and Fence.
- The Cursor pseudo-encoding. Pointer and key events through wayvnc's virtual
  devices, with the gateway's existing keysym path.
- Clipboard: whatever neatvnc advertises. The gateway takes Extended Clipboard
  when offered and Latin-1 cut text otherwise. UTF-8 beyond Latin-1 is an
  Extended Clipboard addition on the neatvnc side, listed under open questions.

## The daemon

A fork of wayvnc carrying three additions. The binary name is open.

1. **neatvnc hook.** neatvnc closes a connection on an unknown message type and
   has no API to send one. The patch adds a callback for client message `0xE0`
   with its fixed length, and a call to write a server message to one client.
   Small enough to offer upstream; useful even if it stays local.
2. **Session controller.** Owns the state machine above, the snapshot file, and
   a sway IPC connection found through `SWAYSOCK` or
   `/run/user/$UID/sway-ipc.*.sock`. Runs the commands listed above. On a resize
   it calls wayvnc's existing output-management path with mode and scale
   together and waits for the compositor's done event before answering.
3. **Console watcher and escape keymap.** Polls `/sys/class/tty/tty0/active`
   and, under the vt policy, treats any change away from sway's console as a
   takeover. Writes the session XKB map at start and hands it to each physical
   keyboard. Exposes a control socket with `end`, `status` and `restore`, which
   the login policy and the tests both use.

Configuration on the host:

```ini
# ~/.config/<daemon>/config
address = 127.0.0.1
port = 5900
enable_auth = true
username = andrew
password = …
rsa_private_key_file = …

session_output = HEADLESS      # prefix of the created output
takeover = vt                  # vt | login
takeover_lock = false          # run swaylock after a takeover
escape_key = esc               # esc | none; plain Esc also switches to the console
```

Host prerequisites:

- sway 1.10 or later, which carries a headless backend beside DRM on its own:
  `create_output` works on the stock session with no `WLR_BACKENDS`.
- The wlroots protocols wayvnc already needs: screencopy or
  image-copy-capture, virtual keyboard and pointer, data-control,
  output-management. Nothing more; there is no drawing on the physical side.
- A free virtual console with a getty, so the login policy has somewhere to log
  in. Read access to `/sys/class/tty/tty0/active`, which is world-readable.
- `swaylock` present when `takeover_lock` is set. The daemon refuses the config
  otherwise.

## Gateway changes

| Where | Change |
|---|---|
| `src/config.rs` | Accept `subtype = "sway"` under `protocol = "vnc"`. `width` and `height` are points and pin the session size when `resize = false`. |
| `src/vnc.rs` | Add the pseudo-encoding to `SetEncodings`. Hold framebuffer requests until the idle `SessionLayout` arrives, the way the engine already holds them until wayvnc declares resize support. Refuse the session with a clear error if the first framebuffer update arrives without the ack: the server is not the daemon. Send `SetSessionDisplay` from the viewport instead of `SetDesktopSize`. Take `Resize.scale` from the latest `SessionLayout`. |
| `src/protocol.rs` | Carry the end reason to the browser so it can say "The person at the machine took control" rather than a generic disconnect. Reuse the existing session-ended shape if it already has a reason slot. |
| frontend | Copy for the end reasons. No new controls: no density or resize toggle, per the product rules. |

Nothing touches the tile path, the shadow, the encoders, or the audio and
camera sockets. The engine keeps its one read loop, one input path and one tile
path for every VNC dialect.

## Testing

- **Wire unit tests** in Rust for both messages, encode and decode, against
  hand-written byte strings, following the rule that a wire-format test gets its
  own parser.
- **Container e2e** beside `vnc_tiles_e2e`: a sway running fully headless with
  the daemon and three headless outputs. Two stand in for physical panels, so
  the consolidation onto one session output, the disabling, the pointer and
  keymap changes and the restore can all be asserted from
  `swaymsg -t get_outputs`, `get_inputs` and `get_workspaces`, no DRM needed.
  The test checks the state after restore equals the snapshot.
- **Resize and density** asserted through `tests/ws_probe.py`: after a viewport
  change the probe expects a `Resize` whose pixel size is points × scale and
  whose scale is the requested one.
- **Takeover** without a real console: the watcher's file is a
  `--vt-active-file` argument, so a test writes a new console number into a
  temp file and asserts the reason on the wire and the restore. The login policy
  is covered by the control socket's `end`. No synthetic keypress anywhere.
- **Real device**: macintel, entered in `tmp/test_uat.toml` as a `sway` subtype
  target, run by hand with eyes on the physical panel for what only eyes can
  see: that it goes dark, that the chord lands on a console, and that the
  desktop is back on the panel on return.

## Open questions

- **wayvnc 0.9.1 crashes on the choreography.** Half the connects measured so
  far killed the server with SIGSEGV while the headless output was being
  created and configured beside it; see
  [the blocking finding](#the-blocking-finding-wayvnc-091-crashes-on-the-choreography).
  Stage 1 with a stock wayvnc is not usable until the backtrace says why.
- **The escape keymap's switch.** The per-device `xkb_file` map loads and
  leaves the virtual keyboard's layout alone; whether sway performs the VT
  switch from it on a physical keypress is unmeasured. If it does not, the
  fallback is leaving the physical keyboard's map alone and accepting that keys
  typed before the chord reach the remote session.
- **Restore on an inactive VT.** Under the vt policy the panels are re-enabled
  while sway does not hold the VT. Expected: wlroots applies the change on
  reactivation. If it is dropped instead, restore re-issues the output commands
  on the return switch, which the watcher also sees.
- **Lid and hotplug.** A lid close or a monitor unplugged during a session
  changes the physical set the snapshot describes. Restore should enable what
  is present and skip what is gone, and a panel that appears mid-session should
  be disabled on arrival so the session stays one display.
- **swayidle.** An idle timeout that disables outputs or locks the screen must
  not fire against the session output. Probably an inhibitor for the session's
  length.
- **Extended Clipboard in neatvnc.** Latin-1 cut text is what works today.
  Non-Latin text needs the extension on the server side; a separate change,
  not v1.
- **Fractional scale.** The wire allows it; the gateway quantises to 1x or 2x
  like RDP. Whether the tile grid tolerates 1.5x is a later measurement.
- **Upstreaming.** The neatvnc hook and possibly the session controller as a
  wayvnc feature. Decide after v1 works locally.

## Plan

1. **Controller with stock wayvnc.** Headless beside DRM on macintel,
   consolidation onto the headless output, snapshot and restore, driven by
   wayvnc client connect and disconnect, through the existing gateway at 1x.
   Measured, see below; the console watcher and crash
   recovery are written and not yet exercised, and the wayvnc crash stands in
   the way of using it.
2. **The dialect.** The neatvnc hook and the two messages. Mode and scale
   applied together on request. `subtype = "sway"` in the gateway, density from
   the wire. End reasons shown in the browser.
3. **Hardening.** Container e2e with three headless outputs. Takeover lock, lid
   and hotplug handling, a swayidle inhibitor. Extended Clipboard on the server.
   Upstream what neatvnc and wayvnc will take.

Stage 1 was the whole risk, and the compositor half of it is retired: sway
gives a resizable headless output beside a live DRM panel. The risk that
replaced it is the wayvnc crash, and stage 2 should start with the backtrace.

## Stage 1, measured on macintel

Measured 2026-09-07 on the reference host: Debian 13, sway 1.10.1, wlroots
0.18.2, wayvnc 0.9.1 and neatvnc 0.9.1 from the distribution, greetd starting
sway on VT 2, one internal panel `LVDS-1` at 1280×800 scale 1. The controller is
`sway-remote-session/sway-remote-session` in this repository: a dependency-free
Python script run through uv, driven by `wayvncctl event-receive`, with a
control socket for `status`, `end` and `restore`. The host runs it as the
`sway_remote_session` role of the `ansible-macintel` playbook, a user unit
beside wayvnc's, with the size, scale and policy on its command line.

### What works

- **Headless beside DRM needs nothing.** sway 1.10 always adds a headless
  backend to its multi-backend, so `swaymsg create_output` succeeds on the stock
  session. The output arrives as `HEADLESS-n` at 1920×1080, scale 1, right of
  the panel, and `output HEADLESS-n unplug` removes it. `n` climbs with every
  creation for the compositor's lifetime, so the daemon finds the new output by
  name difference, never by a fixed name.
- **Custom mode and scale apply, one property per command.** `scale 2` and
  then `mode --custom 3456x1766@60Hz` give a 1728×883 logical output that
  wayvnc lists as 3456×1766. Combined into one `enable scale 2 mode --custom …`
  command, sway 1.10.1 answers success and changes nothing. A refresh in the
  custom mode is optional; without it the mode reports 0 Hz.
- **Consolidation.** Workspaces move with `workspace <name>; move workspace to
  output HEADLESS-n`; after `output LVDS-1 disable` sway reports the panel as
  inactive with a null mode and scale, and exactly one output is active.
- **Inputs.** The escape map compiles under xkbcomp and loads through a
  per-device `xkb_file` on the internal keyboard and on the other keyboard-class
  devices (IR receiver, power and sleep buttons, video bus); `get_inputs` then
  names their layout after the map while wayvnc's `0:0:wlr_virtual_keyboard_v1`
  keeps `English (US)`. `xkb_file -` drops the map. `events disabled` by
  identifier stops the touchpad; the virtual pointer is
  `0:0:wlr_virtual_pointer_v1`, so vendor and product `0:0` identify wayvnc's
  devices and the snapshot excludes them.
- **wayvnc follows.** `wayvncctl output-set` switches the capture and
  `wayvncctl -j output-list` shows `captured: true` on the target once it has;
  `output-set` returns before that, so the controller waits for the flag before
  it disables the panel and before it unplugs the session output. `wayvncctl -j
  event-receive` delivers `client-connected` and `client-disconnected` with a
  `connection_count`, `capture-changed` with an `output` field, and
  `output-added` and `output-removed`, one JSON object per line.
- **Through the gateway.** The generic VNC engine on a `resize = true` target
  followed wayvnc's server-initiated resizes, 1280×800 to 1920×1080 to
  3456×1766, and showed the session at the manual 1x label. The gateway's first
  `SetDesktopSize` goes out at connect, while the capture is still on the DRM
  panel, and wayvnc answers it prohibited because that output is not headless.
  The deferred start in stage 2 removes the race; in stage 1 it is harmless.
- **Restore.** Every session end, including the ones wayvnc's crash caused,
  put the panel back at its mode, scale, position and transform, the workspace
  on it, the touchpad on and the keyboards on their configured layout, and the
  controller's comparison against the snapshot found no difference.

### What the host does to a session

- The desktop role's `sway-lid watch` re-enables the internal panel whenever it
  is inactive with the lid open, within a second of the session disabling it.
  The role now honours a hold file, `$XDG_RUNTIME_DIR/sway-remote-session/active`,
  that the controller writes for the session's length.
- `swayidle` powers every output off after five minutes idle and locks after
  ten. Remote input counts as activity, so neither fires under a live viewer;
  when it does, wayvnc pauses its capture of the powered-off output until the
  next input.
- The `wayvnc` user unit restarts the server three seconds after any exit, so
  a crash costs the viewer a reconnect.

### The blocking finding: wayvnc 0.9.1 crashes on the choreography

In three of six connects wayvnc died with SIGSEGV about 200 ms after
`client-connected`, while the controller was creating the headless output and
setting its scale and mode, before the capture switch and before the panel was
touched. The three that survived ran the identical sequence. Each time the
gateway saw the server close the connection, the controller saw the control
socket refuse and then `wayvnc-shutdown`, and the restore ran clean. The one
connect made without an early client `SetDesktopSize` did not crash, which is
one sample. There is no backtrace yet: the user unit's core limit is zero, so
`systemd-coredump`, installed on the host for this, logged the signal without a
core. The next step is `LimitCORE=infinity` on the wayvnc unit plus
`wayvnc-dbgsym` and `libneatvnc0-dbgsym` from `trixie-debug`, or a source build
with symbols. The stage-2 daemon is a wayvnc fork, so the fix lands where the
crash is.

Until then a stock wayvnc cannot carry stage 1: a connect that kills the server
half the time is not a session. The role stays deployed and enabled on macintel
with its unit stopped; `systemctl --user start sway-remote-session` re-arms it.

### Not measured

- The VT takeover: `sudo chvt 3` during a session drives the console watcher
  without a keypress, and `chvt 2` back drives the re-run of the restore on an
  inactive VT.
- Controller crash recovery (the unit restarts it and it restores from the
  snapshot before listening) and `sway-remote-session end`, the login policy's
  gesture. Both are written and untested.
- The escape map's VT switch on a physical keypress, and the panel going dark
  and coming back: eyes only.
- Fractional scale on the headless output, and any external monitor.
