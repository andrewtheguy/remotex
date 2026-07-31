# macOS agent architecture

`remotex-agent` is the optional macOS endpoint for `protocol = "rxa"`. It
captures one display in the logged-in user's session, encodes changed regions,
and accepts input from the remotex gateway. Long-lived identity keys let the
gateway reconnect without returning to macOS Screen Sharing's login gate.

Installation and operation are documented in
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Components

```text
remotex gateway                         macOS
src/rxa.rs                              crates/rxa-agent
    │                                      │
    └──── Noise-encrypted rxa over TCP ────┤
                                           ├─ ScreenCaptureKit capture
                                           ├─ WebP tile encoder
                                           ├─ Core Graphics input injection
                                           └─ menu bar UI and login item

crates/rxa-proto: identity keys, handshake, framing, messages, key mapping
```

- `crates/rxa-proto` defines the wire types, framing, identity keys, and key
  mapping shared by both endpoints.
- `crates/rxa-agent` contains the macOS capture, encoding, input, clipboard,
  settings, and menu bar implementation.
- `src/rxa.rs` adapts the agent connection to the gateway's common
  browser/session interface. Tiles are relayed without decoding.

## Transport and identity

The agent listens on TCP port 52381 by default. Connections use
`Noise_IK_25519_ChaChaPoly_BLAKE2s`, with the RXA protocol version in the Noise
prologue. Each endpoint has one long-lived X25519 keypair:

- the gateway stores `[rxa].private_key`, and each target stores that Mac's
  `agent_public_key`;
- the agent stores `private_key`, and keeps the gateways it will answer in
  `authorized_gateways` beside its config — one `<rxgp key> <name>` per line, `#`
  comments and blank lines allowed, mode 0600
  (`crates/rxa-agent/src/authorized.rs`).

The asymmetry is why the pattern is `IK` rather than `KK`. The gateway knows
exactly which Mac it is dialing, so the responder's static key is pinned; the Mac
does not know which of its authorized gateways is calling, so the initiator's
static key travels encrypted inside message 1. The agent decrypts it, looks it up,
and refuses the dial before sending message 2. What an unlisted gateway can tell
from that is exactly two things: something answered port 52381, and its key was
refused. What it never gets is any RXA traffic — no `Hello`, no display list, and
no answer to whether anybody is watching the screen. An agent with an empty list
listens and refuses everything, which is what a first launch looks like.

Being on the list is also what lets the agent say *which* gateway is connected: the
entry's name is what the menu bar leads with ("Sharing this screen with home
server (192.168.1.5, 12m)") and what a refused second gateway is told holds the
session. It is a label and nothing more — nothing is decided by it.

Keys use a role-specific prefix followed by the base64url key and a CRC16:

| Prefix | Key |
|---|---|
| `rxgs` / `rxgp` | gateway private / public |
| `rxas` / `rxap` | agent private / public |

Every config field accepts only its expected role and key kind. Only public keys
need to move between machines; private keys are never displayed. The agent can
import an existing private identity from its settings dialog or from standard
input with `--import-private-key`.

Noise transport frames contain length-prefixed `rxa-proto` messages. The gateway
speaks first, with its claim on the session slot; the agent's `Hello` is the
answer to it (see Lifecycle and recovery below):

- gateway to agent: the session claim, attach/detach control, input, display
  selection, private display size and density, clipboard requests and writes, and
  heartbeat pings;
- agent to gateway: `Hello` or a `Busy` refusal, display list and active display,
  display size, WebP tiles, cursor shape, clipboard data, errors, and heartbeat
  pongs.

The gateway translates these messages into the same browser protocol used by
RDP and VNC.

## Display model

One session shares one whole display. The agent reports every available display
and identifies the active one. A session starts on the display macOS currently
calls main; the browser or viewer may select another display by its
`CGDirectDisplayID`.

Display selection and display sizing are separate:

| Display | Selection | Density | Size |
|---|---|---|---|
| Mac-owned physical or virtual display | client chooses | reported by the Mac | controlled by the Mac or its host |
| Agent-created private display | client chooses | follows the client's host display | changes only on an explicit resize request |

A display choice lasts only for the session and is not written to agent config.
Clients use the `active` flag from the agent's display list as the authority for
their checkmark or selected row.

Switching displays restarts capture. If the new display cannot be captured, the
agent restores the previous one. The new size is ordered before tiles in the new
coordinate space, and input mapping is recalculated before input resumes.

The agent periodically re-measures the selected display. A mode change emits
`AgentMsg::DisplaySize`, including the backing scale, through the same ordered
path as tiles. If a host-driven display reconfiguration invalidates the
ScreenCaptureKit stream, capture restarts with backoff.

## Capture and encoding

ScreenCaptureKit supplies frames and dirty rectangles. The agent snaps each
dirty rectangle outward to a fixed 320×64 grid, deduplicates the cells in that
frame, and hashes their source pixels. A cell whose pixels have not changed is
not encoded.

All tile payloads are WebP. A per-tile classifier chooses lossless encoding for
flat or text-like content and lossy encoding for photographic content without
changing the wire format.

```text
SCStream callback ──▶ raw-frame queue ──▶ encoders ──▶ ordered queue ──▶ socket
```

The capture callback copies dirty pixels but never encodes or waits. Both queues
are bounded. If capture outruns the encoder, the agent drops the pending frame
and requests a later full repaint instead of accumulating stale work.

Cells within one captured frame may encode concurrently, but frames never
interleave and their cells are emitted in source order. This is required because
tiles overwrite their rectangles: an older tile arriving after a newer one
would leave stale pixels. Display-size messages use the same queue so they
cannot overtake related tiles.

Cursor shapes are read separately from the framebuffer and sent with their
hotspot at the representation closest to the selected display's backing scale.

## Private virtual display

The optional **Add a private 2x display** setting creates an additional display
with the private `CGVirtualDisplay` API. It does not replace or modify the Mac's
other displays. The agent creates it at startup, releases it on exit, and
continues normally if creation fails.

The configured `virtual_display_initial_size` is a point size with an 800×600
minimum. It is the initial mode and also fixes the display's permanent size
envelope: `maxPixels` and `sizeInMillimeters` cannot be changed after creation.
Requests above the envelope are clamped. Sufficiently small modes can fall
outside macOS's HiDPI density range and become 1x; the agent reports the scale
macOS actually applied.

The private API requires defensive checks:

- the mode is configured at its point size with HiDPI enabled;
- `SCContentFilter.pointPixelScale` is not used to determine the private
  display's capture scale;
- `CGDisplayBounds` supplies the point size; the density is **measured from
  pixels** and never read from the display's mode (see below);
- creation verifies that the display is online and active;
- configuration changes are asynchronous, so the agent waits for
  `CGDisplayBounds` to settle before releasing the display lock.

### Density

Both clients report the backing scale of the screen containing their window.
For the agent-created display only, the agent applies the corresponding 1x or 2x
density while preserving the display's point size. Mac-owned displays are never
changed in response to this report.

Changing density affects the pixels behind the desktop, not its point-space
layout. Clients still render every remote at the point size reported by the
gateway.

Three properties of the private API make this harder than it reads, all three
measured on the test VM and all three load-bearing:

- **The creating process is told the wrong mode.** For its own virtual display,
  `CGDisplayCopyDisplayMode` in the agent reports the mode it asked for rather
  than the one macOS is scanning out, and `NSScreen.backingScaleFactor` — derived
  from the same mode — agrees with it. A display whose framebuffer is provably
  3200×2000 reads as 1x in the agent and 2x in any other process. It is not a
  stale cache: reconfiguration callbacks arrive and the reading does not change.
  So the density comes from a one-point framebuffer capture, which is the only
  reading that is true in that process. Mac-owned displays still use their mode.
- **A live capture stream pins the density.** With a ScreenCaptureKit stream
  attached, `applySettings:` returns YES and the display stays 1x. The agent
  stops the stream, applies, then restarts it and announces the resulting
  geometry — for both densities and for every resize, since a resize re-applies
  the density in order to keep it.
- **HiDPI engages only at the creation size.** Asked to raise the density of a
  display smaller than its envelope, macOS keeps the framebuffer and halves the
  point size instead. A density rise is therefore applied at the creation size,
  and the size the display was in is restored afterwards by a shrink at the new
  density.

A density change is skipped entirely when the display already measures what was
asked for, so a client reporting its screen on every connect costs nothing.

### Client-driven resize

The private display accepts a size only when both of the following are true:

1. the RXA target has `resize = true`;
2. the private display is the active shared display.

Whether a client then resizes it once on request or continuously as its window
changes is the client's own choice, made per session and defaulting to on
request. The agent draws no distinction between the two: it applies whatever size
it is sent. Neither condition above is a client's to relax, and the second moves
during a session — switching to one of the Mac's own screens withdraws it, and the
clients' resize controls go dead until the private display is shared again.

A client reports its viewport in remote pixels. The gateway divides by the
scale it previously announced for that display and sends the resulting point
size to the agent. The agent clamps the request to the display's creation
envelope and reports the resulting size and scale through the normal
`DisplaySize` path.

### Persistent identity

The private display uses fixed vendor, product, and serial values. macOS stores
its arrangement, mode, and primary-display status against that identity and
restores them on later launches. Consequently:

- the configured size is only the initial size for a new identity;
- a client-requested resize can persist across agent restarts;
- the private display may be the Mac's main display if the saved arrangement
  says so;
- an arrangement that leaves the display offline must be repaired in System
  Settings.

The private API exposes no supported way to position the display, prevent it
from becoming primary, or clear macOS's remembered arrangement.

## Input and clipboard

`rxa-proto` maps browser DOM key codes to macOS virtual key codes. Mouse
coordinates are clamped to the selected display and injected with Core Graphics.
Screen Recording permission is required for capture and Accessibility
permission is required for input.

Three things travel with the input that the agent could not work out for itself,
all of them fields on the `CGEvent` it posts (`crates/rxa-agent/src/input.rs`):

- **the click count**, from `MouseEvent.detail` in the browser and
  `NSEvent.clickCount` in the macOS client, injected as `kCGMouseEventClickState`.
  That field is the whole of what makes a double-click a double-click on macOS:
  left unset, every injected click counted as the first one and nothing on any of
  the Mac's displays could be double-clicked. The count follows the press through
  a drag and is released with it, which is what makes dragging out of a
  double-click select by word;
- **the wheel unit**, from the DOM's `deltaMode` and from
  `NSEvent.hasPreciseScrollingDeltas`, which decides between
  `kCGScrollEventUnitPixel` and `kCGScrollEventUnitLine`. A trackpad's few-pixel
  deltas and a wheel's few-line notches are the same numbers and an order of
  magnitude apart in effect, so spending both as lines — which is what the agent
  did before the unit existed — turned a glide into a series of jumps. A page is
  the third unit and the one macOS does not have: `Injector::wheel` spends it as
  `kCGScrollEventUnitLine` scaled by `LINES_PER_PAGE`, a screenful being the only
  thing a page can mean here;
- **the button number**, for anything past left and right. Middle, back and
  forward are all `OtherMouse` events distinguished only by
  `kCGMouseEventButtonNumber`, and the DOM numbers the two side buttons 3 and 4
  exactly as macOS does. RDP and VNC have nowhere to put the side buttons and drop
  them at the gateway.

Two conversions have no client to ask and are made on the Mac. A move carries
`kCGMouseEventDeltaX/Y` — how far the pointer travelled, which is what an app
reading relative input sees instead of the position. And Shift turns a vertical
wheel horizontal, but only for a client whose own OS has not done it already: a
Mac client's delta arrives on the horizontal axis and is left alone, while a
Windows or Linux client's arrives vertical and is moved across, because macOS
applies that rule to hardware and not to what is posted.

The agent polls `NSPasteboard.changeCount` because AppKit provides no clipboard
change notification. It reads contents only after the counter changes and only
while a gateway has enabled clipboard watching for a target with
`clipboard = true`. Clipboard reads are subject to macOS's **Paste from Other
Apps** permission.

## Lifecycle and recovery

The agent runs in the logged-in user's GUI session. Starting at login is a plain
LaunchAgent at `~/Library/LaunchAgents/dev.remotex.agent.plist` naming the
executable by **absolute path**, written only when somebody asks — the menu's
Start at Login, or `--install-launchagent`. A launch registers nothing, so no copy
of the app can change what launchd starts by merely running; see
`crates/rxa-agent/src/loginitem.rs` for the bundle-relative registration this
replaced and what it cost. The menu bar shell is created before
configuration, permission checks, display setup, or the network runtime. A
handled startup or worker failure leaves that shell available with diagnostics
and Quit.

The job has no `KeepAlive`: launchd starts it at login and on an explicit
`kickstart`, and never on its own. A process that quits or dies stays gone until
the next login or until the user opens the app. That is deliberate — automatic
respawn recovered crashes at the cost of stale instances, because a death by
signal relaunched whatever bundle was on disk at that instant and it went on
holding the port.

Saving settings restarts the agent through `kickstart` so address, display, and
identity changes take effect together.

Only one session may be active. The slot is claimed, not seized: a connection
completes its handshake, sends `GatewayMsg::Claim` naming the session it is for,
and only then is judged (`state::decide`). Handshakes and claims run outside the
accept path with a 20-second timeout, so an unauthenticated socket — or an
authenticated one that goes quiet — cannot disturb the session in the slot.

Four outcomes, and which one applies depends on the claim's session id alone:

| claim | outcome |
|---|---|
| nobody holds the slot | granted |
| the holder is the session asking | reclaimed, silently |
| a different session, `force` set | handed over, because a person asked |
| a different session, no `force` | refused with `AgentMsg::Busy`, naming the holder and how long they have had it |

**Authentication and session ownership are separate layers**, as they are in SSH.
The keys decide whether a peer may ask at all; the session id decides whose turn
it is. Nothing about the slot is keyed on a public key or an address, which is what
allows a Mac with several gateways on its authorized list to have exactly one of
them holding it — and the session id is not a credential: an authenticated peer can
present any value, and the agent only ever compares it.

A gateway mints one session id per process, because a gateway instance has
exactly one session slot of its own. Every dial it makes therefore reclaims: a
dropped link, a target switched away and back, a browser takeover on the gateway,
and a half-open connection not yet reaped are all the same session returning, so
none of them costs the user a prompt. Only a genuinely different gateway is
refused, and its client offers the takeover.

Browser lifetime remains controlled by the gateway's shared session layer. A
missing browser pong ends the session after about 60 seconds; an orderly browser
disconnect allows the same reattach grace period. Ending the RXA engine closes
the agent connection and stops capture.

The gateway also sends an RXA ping every five seconds. A silent agent link is
reconnected after its 15-second deadline. An established link retries with
capped backoff for up to 30 seconds; recovery requests a full repaint, and input
received during the outage is discarded. Initial connection or authentication
failures are reported immediately. If an established link remains down past the
retry window, the browser returns to the target picker with the reason.

## Constraints

- The program has one active session. That is a limit on concurrency, not on who
  may connect: a second gateway is refused until a person takes the session over,
  and the slot is keyed on the claim's session id rather than on any key.
- The agent shares one whole display at a time.
- Mac-owned displays are never resized by a client.
- The private display changes size only through an explicit request and changes
  density only to match the connected client's screen.
- The agent runs only in a logged-in GUI session; it does not provide macOS
  login-window or unattended service access.
- Screen Recording and Accessibility grants are tied to the app's signing
  identity. Switching between differently signed builds requires granting them
  again.
- Pasteboard reads require macOS 15.4 or later, which sets the deployment
  target.
- Audio is not part of RXA.
