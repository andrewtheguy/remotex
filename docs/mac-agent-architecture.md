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
                                           └─ menu bar UI and SMAppService

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
`Noise_KK_25519_ChaChaPoly_BLAKE2s`, with the RXA protocol version in the Noise
prologue. Each endpoint has one long-lived X25519 keypair and pins the other's
public key:

- the gateway stores `[rxa].private_key`, and each target stores that Mac's
  `agent_public_key`;
- the agent stores `private_key` and one `gateway_public_key`.

Both static keys are known before the handshake, so authentication completes
inside Noise. An agent without `gateway_public_key` listens but refuses all
connections.

Keys use a role-specific prefix followed by the base64url key and a CRC16:

| Prefix | Key |
|---|---|
| `rxgs` / `rxgp` | gateway private / public |
| `rxas` / `rxap` | agent private / public |

Every config field accepts only its expected role and key kind. Only public keys
need to move between machines; private keys are never displayed. The agent can
import an existing private identity from its settings dialog or from standard
input with `--import-private-key`.

Noise transport frames contain length-prefixed `rxa-proto` messages:

- agent to gateway: display list and active display, display size, WebP tiles,
  cursor shape, clipboard data, errors, and heartbeat pongs;
- gateway to agent: attach/detach control, input, display selection, private
  display size and density, clipboard requests and writes, and heartbeat pings.

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
- `CGDisplayCopyDisplayMode` supplies the active pixel and point dimensions;
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

### Explicit resize

The private display accepts a size only when all of the following are true:

1. the RXA target has `resize = true`;
2. the private display is the active shared display;
3. the user invokes **Resize to window** in the browser or viewer.

The agent does not continuously follow window changes. Reconfiguring a live
display moves its windows and is intentionally an explicit operation.

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

The agent polls `NSPasteboard.changeCount` because AppKit provides no clipboard
change notification. It reads contents only after the counter changes and only
while a gateway has enabled clipboard watching for a target with
`clipboard = true`. Clipboard reads are subject to macOS's **Paste from Other
Apps** permission.

## Lifecycle and recovery

The application registers its embedded LaunchAgent through `SMAppService` and
runs in the logged-in user's GUI session. The menu bar shell is created before
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

Only one authenticated gateway connection may be active. A new connection
replaces the current one only after completing its handshake, so an
unauthenticated socket cannot evict a session. Handshakes run outside the accept
path with a 20-second timeout.

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

- The program has one active session and one authenticated gateway connection.
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
