# A virtual-display remote session for sway

Console-style remote control of a physical sway machine: every physical display
is replaced by one resizable virtual display for the length of the session, the
gateway renders it at the browser's density with nothing to configure, and the
person at the keyboard takes control back through a Linux virtual console. This
is a design, not a measurement: nothing here is implemented, and the claims
marked *unverified* are the ones the first stage of the plan exists to test.
The reference host is the Intel Mac running Debian and sway (`ssh macintel`),
whose headless wayvnc is the measured case in
[`generic-vnc-hidpi.md`](generic-vnc-hidpi.md).

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
browser and gateway except that the density label is now true without a manual
toggle, and a session can end with a reason the browser can show.

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
protocol, so the remote user is unaffected (*unverified on macintel*).

**Pointers are disabled** by identifier, from the snapshot taken before the
virtual pointer existed. A pointer has no part in the escape, and a stray local
click into the remote session is the one leak the keymap does not close. Never
disable by `type:pointer`, which would also catch wayvnc's device.

**Restore** re-enables the pointers and removes the session keymap.
`get_inputs` does not report which file a keyboard's map came from, so the
keymap is restored by re-issuing the `input` lines of the user's sway config,
with `swaymsg reload` as the blunt fallback that also re-applies every output
from config.

### Sway commands, in order

```sh
# start
swaymsg -t get_outputs; swaymsg -t get_inputs; swaymsg -t get_workspaces   # snapshot
swaymsg create_output                                                      # HEADLESS-1
swaymsg output HEADLESS-1 enable scale 2 mode --custom 3456x1766 position 0 0
swaymsg 'workspace 1; move workspace to output HEADLESS-1'                 # each workspace, every panel
swaymsg focus output HEADLESS-1
swaymsg input 1452:641:Apple_Internal_Keyboard xkb_file /run/user/1000/<daemon>/escape.xkb
swaymsg input 1452:641:Apple_Internal_Trackpad events disabled             # each physical pointer
swaymsg output eDP-1 disable                                               # each physical output

# resize: wayvnc's existing path, wlr-output-management custom mode + scale

# end
swaymsg output eDP-1 enable mode 2560x1600 scale 2 position 0 0 transform normal   # per snapshot
swaymsg 'workspace 1; move workspace to output eDP-1'                      # per snapshot
swaymsg input 1452:641:Apple_Internal_Trackpad events enabled
swaymsg input 1452:641:Apple_Internal_Keyboard xkb_layout us              # the config's own input lines
swaymsg output HEADLESS-1 unplug                                           # or disable
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

- sway started with the headless backend loaded beside DRM, so `create_output`
  works on a physical machine. The expected form is
  `WLR_BACKENDS=libinput,drm,headless` in the session's environment
  (*unverified on macintel*).
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
| `src/config.rs` | Accept `subtype = "sway"` under `protocol = "vnc"`. `width` and `height` are points and pin the session size when `resize = false`. The density declaration is not offered on this subtype, because the wire answers. |
| `src/vnc.rs` | Add the pseudo-encoding to `SetEncodings`. Hold framebuffer requests until the idle `SessionLayout` arrives, the way the engine already holds them until wayvnc declares resize support. Refuse the session with a clear error if the first framebuffer update arrives without the ack: the server is not the daemon. Send `SetSessionDisplay` from the viewport instead of `SetDesktopSize`. Take `Resize.scale` from the latest `SessionLayout`. |
| `src/protocol.rs` | Carry the end reason to the browser so it can say "The person at the machine took control" rather than a generic disconnect. Reuse the existing session-ended shape if it already has a reason slot. |
| frontend | Copy for the end reasons. No new controls: no density toggle on this subtype, no resize toggle anywhere, per the product rules. |

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

- **Headless beside DRM.** Does sway on macintel accept `create_output` when
  started with both backends, and does the created output behave like the fully
  headless case wayvnc was measured against? First thing to test, before any
  code.
- **The escape keymap.** That sway still performs the VT switch from a
  per-device `xkb_file` map whose other keys are `NoSymbol`, and that the
  virtual keyboard's own map is untouched by it. If per-device maps cannot
  carry the switch, the fallback is leaving the physical keyboard's map alone
  and accepting that keys typed before the chord reach the remote session.
- **Restore on an inactive VT.** Under the vt policy the panels are re-enabled
  while sway does not hold the VT. Expected: wlroots applies the change on
  reactivation. If it is dropped instead, restore re-issues the output commands
  on the return switch, which the watcher also sees.
- **Unplugging the output.** `output … unplug` exists for headless outputs in
  recent sway; if macintel's version lacks it, restore falls back to disabling
  the output and reusing it next session.
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

1. **Controller with stock wayvnc.** Prove headless beside DRM on macintel.
   Consolidation onto the headless output and the console watcher, driven by
   wayvnc client connect and disconnect. Snapshot and restore, crash recovery.
   The existing gateway with the manual density toggle.
2. **The dialect.** The neatvnc hook and the two messages. Mode and scale
   applied together on request. `subtype = "sway"` in the gateway, density from
   the wire. End reasons shown in the browser.
3. **Hardening.** Container e2e with three headless outputs. Takeover lock, lid
   and hotplug handling, a swayidle inhibitor. Extended Clipboard on the server.
   Upstream what neatvnc and wayvnc will take.

Stage 1 is the whole risk. If sway cannot give a resizable headless output
beside a live DRM panel, the design changes shape and nothing in stages 2 and 3
should be started.
