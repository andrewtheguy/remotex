# Architecture

remotex is a single-user web gateway for RDP and VNC desktops. Macs can use
their built-in Screen Sharing service as VNC targets or the optional
`remotex-agent` companion over `rxa`. A Rust backend owns the remote protocol
session and exposes one browser protocol to a React SPA.

## Data path

```
React SPA
   │  /api: authentication, targets, session claim
   │  /ws: JSON control/input + binary image tiles
   ▼
axum server ── session slot ── protocol engine
                                  ├─ RDP via IronRDP
                                  ├─ VNC via built-in RFB client
                                  │    └─ includes macOS Screen Sharing
                                  └─ rxa via optional remotex-agent on macOS
```

RDP and VNC are decoded in the gateway, then emitted as WebP tiles. The optional
macOS agent already emits browser-ready WebP tiles, so the gateway relays them
without re-encoding.

## Constraints

- There is one active session slot. A new browser may take it over, evicting the
  previous browser; concurrent or shared sessions are not supported.
- Remote credentials stay in the server-side TOML config.
- The browser knows only the common remotex protocol, never RDP, RFB, or `rxa`.
- Protocol engines use broadly supported baseline features rather than
  server-specific workarounds.

## Backend

The main responsibilities are:

| Module | Responsibility |
|---|---|
| `server.rs`, `auth.rs` | HTTP routes, SPA serving, login sessions |
| `session.rs` | the single slot, target selection, takeover, detach/reattach |
| `ws.rs`, `protocol.rs` | browser WebSocket bridge and wire types |
| `rdp.rs` | IronRDP connection, framebuffer, input, optional resize |
| `vnc.rs` | RFB 3.8 client, framebuffer, cursor, input, optional resize |
| `rxa.rs` | encrypted Mac-agent connection and tile pass-through |
| `keymap.rs` | DOM key codes to RDP scancodes or X11 keysyms |

Each engine implements the same input/frame-channel boundary and runs
independently of the browser WebSocket.

## Session lifecycle

Authentication answers whether a browser may use the service. The session slot
separately records which browser owns the desktop and which target is active.

1. The browser authenticates with `POST /api/auth/login`.
2. `POST /api/session` claims the slot. A conflicting claim returns `409`
   unless it is a reclaim or forced takeover.
3. `/ws?session=<token>` attaches to the slot. An idle slot reports the target
   picker; an active slot reports the connected target and requests a repaint.
4. `connect` starts the selected engine. `disconnect` stops it and returns to
   the picker.
5. Losing the WebSocket detaches the browser. The RDP, VNC, or `rxa` engine
   remains available for a 60-second reattach grace period and discards frames
   while detached. If no browser returns, the engine stops and the slot returns
   to the picker.

A forced takeover closes the previous WebSocket and preserves the engine and
selected target when the new browser attaches within the same grace period. If
an engine ends, the slot returns to the picker.

Login tokens are stored in memory with a sliding expiry and delivered through
an `HttpOnly`, `SameSite=Strict` cookie. Session and target endpoints, including
the WebSocket upgrade, require a valid login. The cookie is marked `Secure`
only when `x-forwarded-proto` reports HTTPS, allowing direct HTTP use on a
trusted local network. A server restart logs browsers out.

## Browser protocol

`src/protocol.rs` and `frontend/src/protocol.ts` define the two sides.

Server-to-browser traffic uses JSON for control messages and binary frames for
screen updates. Control messages cover picker/connected state, desktop size, the
remote's display list, cursor shape, clipboard text, and errors.

Every binary frame is a **batch** of records, little-endian throughout:

```text
u8 kind = 0x02 | u8 flags = 0 | u16 record count | records

TILE     op 0x01: u8 format | u16 slot | u16 x | u16 y | u16 w | u16 h
                  | u32 len | payload[len]
TILE_REF op 0x02: u16 slot | u16 x | u16 y
```

One frame carries however many updates were ready at once, so a repaint costs
one client event, one round of decoding and one paint rather than one of each per
tile. The record count earns its two bytes: records are self-delimiting, so
without it a truncated frame would parse cleanly as a complete but smaller batch.
A non-zero `flags` is *rejected* rather than ignored, which is what makes the byte
usable for an additive change later.

The payload format is WebP (`format` 3), and it is the only value the byte takes:
one container covers the lossless screen content the gateway's engines encode and
the lossy tiles the macOS agent classifies, so the choice between them never
reaches the wire. The byte survives as the seam a second codec would arrive
through. `format` 1 and 2 were PNG and JPEG, retired together in protocol
version 4 — a client built against 3 rejects a v4 frame rather than mis-decoding
it, and the `protocolVersion` check refuses the session before that can happen.

Cursor shapes are the exception: they stay PNG on the JSON control channel, where
a few hundred bytes a handful of times a session buys nothing from a codec change,
and where the macOS agent's shapes arrive already encoded by AppKit.

`slot` is the tile cache. `TILE` means "draw this, and keep it in slot N";
`TILE_REF` means "draw what you have in slot N here", in seven bytes instead of a
payload. `NO_SLOT` (0xFFFF) means "draw this and do not keep it" — used for
payloads too large to be worth a slot. A client's cache is a fixed array of
`SLOT_COUNT` (256) entries and it never evicts: the gateway names the slot to
overwrite. That asymmetry is deliberate, because a content-addressed cache would
need both ends running an identical eviction policy over an identical cost metric,
and they cannot have one — the gateway knows encoded bytes, a client knows what
its decoder made of them. Both clients keep the encoded payload and re-decode on a
reference.

A client that cannot decode a tile it was told to keep, or that meets a reference
to a slot it does not hold, sends `cacheReset`. That is a separate message from
`refresh` for a reason: `refresh` is routed to the engine, which would repaint
into an unchanged slot table, send the same references back, and miss again.

Browser-to-server traffic is JSON:

- session control: connect or disconnect;
- input: mouse movement/buttons, wheel, and DOM keyboard codes;
- display control: viewport size and a display selection;
- a full repaint request, and a tile-cache reset;
- clipboard: send text to the remote, or request the remote's text.

Pointer motion is coalesced at the client: while the socket has bytes still
queued, only the newest move is kept. Anything that is not a move flushes the
held one first, because a click has to follow the move that positioned it.

Viewport reports affect only engines configured for resize, and there is no
other way for a client to ask for a size — no menu of resolutions, for any
protocol. RDP resize is explicit from the UI; VNC resize follows the browser
when the server advertises the extension. `rxa` is explicit too and narrower
still: what it resizes is the private display the agent can create, never one of
the Mac's own screens, so the control appears only while that display is the one
being shared. A remote's resolution is otherwise set on the remote and reaches
the client only as a `resize`.

Choosing *which* of a remote's displays to view is a separate matter, and one a
client does decide: an engine that can offer a choice sends a `displays` list and
acts on `selectDisplay`. Only `rxa` can — RDP and VNC each deliver a single
framebuffer spanning every remote screen — so those clients show no picker.
Switching changes nothing about resolution: the size that follows is the size the
chosen display was already at. What it does change is whether a resize may be
asked for at all, since only the agent's own display can take one.

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine, so a new canvas does not
depend on updates seen by the previous client. A client may also send it to
recover a canvas that has gone wrong: the viewer offers it as **Remote →
Refresh**, and the SPA does not — it has no equivalent command and never sends
the message.

The clipboard is per-target opt-in (`clipboard = true`, supported by every
engine) and works two ways at once. The backend owns the data: the VNC engine
forwards `ServerCutText` as it arrives and also buffers it, the RDP engine asks
for the text as soon as the remote announces a copy, and the Mac agent watches
its pasteboard and pushes changes. On top of that the browser can always
request the current text, which is what a browser attaching mid-session does,
having missed every push so far. Each cached value carries when remotex last
observed the remote clipboard change; fetching it later preserves that activity
time instead of replacing it with the fetch time. Replies to that explicit
request are marked separately from unsolicited changes: only the latter drive
automatic remote-to-local clipboard sync. Opening or revealing the panel is a
read operation, and its Copy button is the explicit local write.

In the browser every arrival feeds the clipboard panel, while unsolicited
changes also feed the local OS clipboard where the Clipboard API is available.
Automatic sync is best effort by design — `navigator.clipboard` is absent on a
non-secure origin, the usual LAN deployment over plain HTTP, and Safari will not
read the clipboard without a paste gesture. The panel needs no permission: it
opens on a concealed CRC32/length/activity-time summary, reveals the fetched
text into its editable box on request, and offers explicit Send and Copy
actions. The feature therefore degrades to manual rather than breaking. One
transfer is capped at 64 KiB in each direction.

The gateway sends a WebSocket protocol ping every five seconds. Browsers answer
with a protocol pong in their networking stack, so background-tab JavaScript
timer throttling does not affect liveness. A connection with no pong for about
60 seconds is expired and its engine stops immediately because the missing-pong
wait has already consumed the reattach grace period. An orderly WebSocket close
starts a fresh 60-second reattach window. These frames are transport-level and
do not appear in the JSON browser protocol.

The connection out to a remote gets the matching treatment, in one place for all
three protocols (`src/engine.rs`). Every engine socket is opened through the same
helper: `TCP_NODELAY`, a 20-second connect budget, a 30-second handshake budget,
and TCP keepalive tuned to notice silence — probing after 10 seconds idle, every
5 seconds, giving up after 3 unanswered probes, so a host that stops answering is
reported in about 25 seconds instead of the kernel default's couple of hours.
Without it a host that vanishes with no FIN — powered off, or cut off — leaves
the engine blocked on a read forever and the client holding a frozen desktop with
nothing to say. On Linux the socket also gets a 30-second `TCP_USER_TIMEOUT`,
because keepalive probes are only sent on an *idle* connection: the moment
somebody clicks at a desktop that has frozen, unacknowledged data makes the
retransmission budget own the socket instead, and that runs to about fifteen
minutes. macOS has no equivalent option.

What this proves is narrow: that the peer's kernel is still answering. For RDP and
VNC that is all there is, so a remote whose kernel answers while its server
process is wedged still reads as an idle desktop: a hung `Xvnc`, a `SIGSTOP`ped
server, a sleeping display, or a VM that was *suspended* rather than powered off
all keep answering, and the client cannot tell any of them from a desktop nobody
is touching. Neither protocol offers a way to ask better — RFB has no ping, and
IronRDP's Heartbeat PDU is server-to-client only — so switching target by hand is
the way out. A probe that would close the RFB half is in
[`roadmap.md`](roadmap.md).

RXA asks the agent process itself as well, which closes that half for it: a Mac
whose agent has wedged or gone is reported. What that still does not prove is that
pixels are flowing. The agent answers pings from its message loop while capture
delivers on its own queues, so a capture stream that is alive but producing nothing
leaves the link provably healthy and the picture frozen — rarer than the RDP and
VNC case, and narrower, but the same symptom.

## Engines

### RDP

IronRDP handles TLS and optional NLA/CredSSP. The engine maintains a decoded
framebuffer, converts dirty rectangles to RGB, and sends them to the browser as
WebP tiles at most 64 rows tall. Input uses fast-path PDUs after mapping DOM codes
to scancodes.

Before anything is encoded, the rectangle is compared against a *shadow copy* of
the pixels this client was last sent (`src/tiles.rs`). An update that changed
nothing is dropped; one that changed a little is sent as the part that changed.
That matters most here: the RDP pointer is composited into the framebuffer, so
every mouse event produces a damage rectangle, and this engine also repaints
regions that did not change. Measured on the dummy xrdp container, a scripted
240-position mouse sweep went from 115,747 bytes to 27,634 with the tile cache
behind it (`tests/rdp_bytes_probe.rs`).

The shadow tracks which pixels it actually knows, and a repaint (`refresh`) drops
that knowledge rather than assuming the client is showing black — both clients
keep their pixels when a `resize` repeats the size they already have, so assuming
black would withhold every region that is *now* black.

With `resize = true`, the Display Control Virtual Channel resizes the remote
desktop when requested from the browser. Otherwise the configured initial
width and height remain fixed.

With `clipboard = true`, the MS-RDPECLIP static virtual channel carries
`CF_UNICODETEXT` in both directions. RDP uses delayed rendering: a copy
announces only the available formats, and the text costs a second round trip.
The engine hides that from the browser by requesting the text as soon as the
remote announces it, so a remote copy arrives unprompted as it does for the
other engines; in the other direction the browser's text is advertised and held
until the remote actually pastes. Line endings are converted between CRLF and
LF at the boundary. Images, HTML and file transfer are out of scope, and a
server that never joins the channel leaves the clipboard inert rather than
ending the session.

### VNC

The built-in client speaks RFB 3.8 with None, classic VncAuth, or Apple's DH
security. It
requests raw 32-bit true-colour pixels, converts them to RGB, and supports the
Cursor pseudo-encoding. Rects go through the same shadow comparison as RDP's, so a
server that re-sends unchanged pixels — and they do — costs the browser link
nothing. `refresh` still asks the server for a non-incremental update rather than
answering from the shadow: the shadow holds what the *browser* was sent, which goes
stale across a detach because the session layer drops frames while nobody is
attached, and trading the server's ground truth for bytes on a LAN hop is not the
trade this is trying to make. This path can connect directly to macOS Screen Sharing;
the companion agent is not required for Mac targets.

A Mac target says so: `subtype = "ard"`, which selects Apple's DH authentication
(RFB security type 30) and makes the credentials the *macOS account's* rather
than the Screen Sharing password. What that buys is the Mac's own screen. A
password alone authenticates nobody in particular — macOS logs the connection as
`uid -2` — and macOS answers an anonymous viewer by creating a new login-window
session on a virtual display, so the client lands on a login screen that will not
take the account already signed in on the console, while that session carries on
unshared beside it. Named, the same connection resolves to that user's session.

The subtype is declared rather than inferred from which credential fields are
filled, because the two dialects want different ones and guessing is how a good
password ends up authenticating nobody. It therefore requires `username` and
`password`, rejects `vnc_password`, and rejects `resize` — macOS accepts the
resize negotiation and then ignores every request, so the key would promise a
control that does nothing, and there is no agent-made display behind this
protocol for it to mean something about. A plain `vnc` target is the mirror image: it takes
`vnc_password` only. Reaching a Mac as a plain target is allowed and warned about
once, in the log, at the moment it can still be changed.

With `resize = true`, it advertises DesktopSize/ExtendedDesktopSize and sends
`SetDesktopSize` after the server confirms support. Non-raw encodings are not
implemented. macOS Screen Sharing is the one server known to accept that
negotiation and then ignore the request: a Mac reached as a plain `vnc` target
never resizes, with no error, and the only way to change it is on the Mac itself.
(An `ard` target refuses the key at startup instead, so this is only reachable by
not declaring what the target is.)

With `clipboard = true`, `ServerCutText` is forwarded to the browser as it
arrives and also fills a per-session buffer that answers a later fetch, and a
browser send becomes `ClientCutText`.

Two encodings are possible, and which one applies is the server's choice. The
Extended Clipboard pseudo-encoding (`0xc0a1e5ce`) carries UTF-8 and is
advertised whenever the target opts in; a server that supports it answers with
a capability message, and from then on text moves through the lazy
notify/request/provide exchange, deflated, with CRLF line endings converted at
the boundary. TigerVNC does this. A server that stays silent — TightVNC, for
one — leaves the baseline latin-1 cut text in use, where anything outside
latin-1 becomes `?` on the way out and cannot be represented on the way in.
That limit is the server's, not the gateway's.

Pointer button state is tracked across RFB pointer events. Keyboard input maps
DOM codes to X11 keysyms using live Shift and browser-reported Caps Lock state.
The latest cursor shape is cached and replayed on refresh because servers send
it only when it changes.

### rxa

As an alternative to macOS Screen Sharing over VNC, the gateway can connect to
the optional `remotex-agent` with a Noise session authenticated by a long-lived
X25519 keypair on each end, each pinning the other's public key. This provides
RealVNC-like reconnect behavior: the keys are the connection credential, so a
reconnect does not return to Screen Sharing's login gate. The agent captures
and encodes the Mac display, and the gateway relays its tiles. Established
connections retry with capped backoff after transient failures and request a
repaint on recovery. Input generated while disconnected is discarded. Initial
connection and authentication failures return to the picker instead of
retrying indefinitely, and so does an established link that stays down for 30
seconds — long enough to hide a Wi-Fi roam or an agent restart, short enough that
a Mac which was switched off does not leave a frozen desktop on screen.

A client picks which of the Mac's displays to share, and the agent reports the
set it has. A Mac's own screens keep their mode: nothing on this wire asks a
physical panel to change resolution, and it is changed on the Mac, in System
Settings, with the agent reporting the new size when it sees it. The private
display the agent can create for itself is the exception — nobody is sitting at
it, so a client may ask for its size with `resize = true` on the target and that
display being the one shared. See
[`mac-agent-architecture.md`](mac-agent-architecture.md).

RXA has a separate application ping/pong between the gateway and agent to detect
a half-open agent TCP connection quickly and reconnect it — faster than the
socket keepalive every engine gets, and answered by the agent process rather than
its kernel, so it also catches a Mac that is reachable while the agent is wedged.
Because that ping goes out every five seconds, this engine's own socket is never
idle and its keepalive timer effectively never arms. Browser lifetime
remains owned by the shared session layer under the same rules as RDP and VNC.
When that layer ends an RXA engine, it closes the agent connection, stops
capture, and clears the agent's sharing status.

See [`mac-agent-architecture.md`](mac-agent-architecture.md) for the agent,
capture pipeline, protocol, and lifecycle.

## Frontend

The SPA has three states: login, target picker, and remote desktop. The desktop
uses a canvas for tiles and an overlay for input. It supports desktop mouse and
keyboard input, touch gestures, an on-screen keyboard, a clipboard panel, target
switching, takeover, and explicit RDP resize. Clipboard text arriving from the
server is mirrored into the local OS clipboard, and the local clipboard is sent
to the remote when the tab regains focus; both are skipped silently wherever the
Clipboard API is unavailable. The on-screen keyboard and the
clipboard panel are mutually exclusive: both dock to the bottom edge on mobile
and report their height so the canvas insets above them.

`Ctrl+Alt+Shift+;` hides the floating button and its drawer, and shows them again.
It is caught on `window` in the capture phase and stopped there, because the remote
surface forwards every key it sees — a bubble-phase listener would toggle the
button and type the chord at the guest. Three modifiers because the chord it takes
is the guest's rather than the browser's: `Ctrl+Shift+;` is Excel's insert-time,
and `Ctrl+Alt` is AltGr on Windows and X11. Not persisted: a chrome-less desktop
with no visible way back should not survive a reload.

On a Mac host driving a non-Mac remote, the SPA translates Command chords the way
the macOS viewer does (see `frontend/src/macKeys.ts` and docs/macos-viewer.md):
Command plus A, C, F, P, S, V, X or Z becomes a remote Control chord, a bare
Command taps remote Meta, and any other Command chord is forwarded as a Meta
chord. Eight rather than the viewer's fourteen, because a web page never receives
Command-W, T, N, L or O — the browser keeps those — and Command-R is left alone
deliberately, since a leaked reload would drop the session. A **Mac keyboard**
toggle in the floating menu turns translation off, and it is inapplicable (and
disabled) for a Mac remote, which is what the `remoteOs` message decides. Command
releasing also flushes any translated key still held, because macOS browsers can
withhold `keyup` while Command is down.

Incoming image decodes are serialized so tiles and resize messages are applied
in wire order. A remote cursor shape is installed as a CSS cursor for mouse
input and rendered separately for touch input.

Each tab stores its session token in `sessionStorage`, allowing network
reconnects to reclaim the same slot without prompting for takeover. A busy slot
waits for an explicit takeover; an evicted tab waits for an explicit reclaim.

Desktop rendering presents the remote at its own size — `resize` carries the
density the remote draws at, and the canvas is that many framebuffer pixels per
CSS pixel — and uses scrollbars when the desktop is larger than the viewport. The
ratio between that density and the host display's is what scales the picture,
automatically and in both directions: a 1x guest on a Retina host is magnified 2x
and soft, a Retina guest on a 1x host is reduced to half and sharp, and equal
densities are 1:1 in device pixels with nothing resampled. A window dragged
between displays of different scale switches between those on its own and needs
no re-derivation — the browser rasterizes the same CSS size at the new density.
Viewport reports are in the remote's pixels for the same reason.
Touch devices use fit-to-width rendering with pinch zoom, pan, a virtual cursor,
and multi-finger gestures. View transforms affect presentation and input
coordinate mapping, not framebuffer resolution.

### Native macOS viewer

The optional macOS 26 viewer is a second client of the same protocol, not a shell
around this one. It speaks `/api/*` and `/ws` itself: its own login and target
picker, its own claim/attach/reconnect state machine, Metal framebuffer
rendering, AppKit input, and `NSPasteboard`. Nothing web is involved, and the SPA
is unaffected by it.

Because the two artifacts ship separately, `GET /api/config` carries a
`protocolVersion` (`PROTOCOL_VERSION` in `src/protocol.rs`) that the viewer
refuses to open a session against if it does not recognise. Additive control
messages do not bump it: clients must ignore tags they do not know.

The viewer has one protocol-engine branch, and only one: which of the three
resize behaviours a target uses. Everything else follows from the control
messages, so any other difference belongs to an engine adapter.

See [`macos-viewer.md`](macos-viewer.md) for its protocol, rendering, keyboard,
clipboard, and packaging design.

## Configuration and testing

Configuration is one global TOML file with `[server]` and `[[targets]]`
sections. See [`install.md`](install.md) and
[`packaging/etc/remotex.toml.example`](../packaging/etc/remotex.toml.example).

Protocol-specific fields are validated during startup. In particular, `rxa`
requires a checksum-valid `[rxa].private_key` and a checksum-valid
`agent_public_key` per target, each rejected if it is the wrong *kind* of key —
the role is in the prefix, so a gateway key pasted where an agent's belongs is
named rather than left to fail at the handshake. Incompatible fields are rejected
rather than silently accepted, `resize` on a `subtype = "ard"` target among
them.

Unit tests cover protocol, config, authentication, key mapping, and engine
helpers. Tests under `tests/` exercise the HTTP/WebSocket session flow and
protocol engines; RDP and VNC happy paths use containerized dummy servers, while
`rxa` uses an in-process fake agent, including an end-to-end check that closing
the browser releases the agent connection. Session-manager tests verify the
same detach deadline for all three protocols. Browser automation is
intentionally not used.
