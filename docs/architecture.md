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

RDP and VNC are decoded in the gateway, then emitted as PNG tiles. The optional
macOS agent already emits browser-ready PNG/JPEG tiles, so the gateway relays
them without re-encoding.

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
tiles. A tile contains a 10-byte little-endian header:

```text
u8 kind | u8 format | u16 x | u16 y | u16 width | u16 height | image bytes
```

Formats are PNG and JPEG. Control messages cover picker/connected state,
desktop size, cursor shape, clipboard text, and errors. Large dirty rectangles
are split into 64-row strips to bound individual WebSocket frames.

Browser-to-server traffic is JSON:

- session control: connect or disconnect;
- input: mouse movement/buttons, wheel, and DOM keyboard codes;
- display control: viewport size and full-refresh request;
- clipboard: send text to the remote, or request the remote's text.

Viewport reports affect only engines configured for resize. RDP resize is
explicit from the UI; VNC resize follows the browser when the server advertises
the extension; `rxa` ignores viewport size.

`refresh` re-announces the desktop size and requests a full repaint. The session
layer injects it after attaching to an existing engine so a new canvas does not
depend on updates seen by the previous browser.

The clipboard is pull-only and per-target opt-in (`clipboard = true`, supported
by VNC and `rxa`). The backend owns the data — the VNC engine buffers what the
remote last cut, the Mac agent reads its pasteboard when asked — and the browser
requests it explicitly, so nothing is retained client-side and nothing is
pushed. The browser never reads or writes the local OS clipboard: the panel is a
text box, so no Clipboard API permission or secure context is required. One
transfer is capped at 64 KiB in each direction.

The gateway sends a WebSocket protocol ping every five seconds. Browsers answer
with a protocol pong in their networking stack, so background-tab JavaScript
timer throttling does not affect liveness. A connection with no pong for about
60 seconds is expired and its engine stops immediately because the missing-pong
wait has already consumed the reattach grace period. An orderly WebSocket close
starts a fresh 60-second reattach window. These frames are transport-level and
do not appear in the JSON browser protocol.

## Engines

### RDP

IronRDP handles TLS and optional NLA/CredSSP. The engine maintains a decoded
framebuffer, converts dirty rectangles to RGB, and sends image strips to the
browser. Input uses fast-path PDUs after mapping DOM codes to scancodes.

With `resize = true`, the Display Control Virtual Channel resizes the remote
desktop when requested from the browser. Otherwise the configured initial
width and height remain fixed.

### VNC

The built-in client speaks RFB 3.8 with None or classic VncAuth security. It
requests raw 32-bit true-colour pixels, converts them to RGB, and supports the
Cursor pseudo-encoding. This path can connect directly to macOS Screen Sharing;
the companion agent is not required for Mac targets.

With `resize = true`, it advertises DesktopSize/ExtendedDesktopSize and sends
`SetDesktopSize` after the server confirms support. Non-raw encodings are not
implemented.

With `clipboard = true`, `ServerCutText` fills a per-session buffer the browser
fetches on request, and a browser send becomes `ClientCutText`. The text is
latin-1, as the baseline protocol defines it: characters outside latin-1 become
`?` on the way out. The Extended Clipboard pseudo-encoding, which would carry
UTF-8, is not negotiated.

Pointer button state is tracked across RFB pointer events. Keyboard input maps
DOM codes to X11 keysyms using live Shift and browser-reported Caps Lock state.
The latest cursor shape is cached and replayed on refresh because servers send
it only when it changes.

### rxa

As an alternative to macOS Screen Sharing over VNC, the gateway can connect to
the optional `remotex-agent` with a pre-shared-key Noise session. This provides
RealVNC-like reconnect behavior: the PSK is the connection credential, so a
reconnect does not return to Screen Sharing's login gate. The agent captures
and encodes the Mac display, and the gateway relays its tiles. Established
connections retry with capped backoff after transient failures and request a
repaint on recovery. Input generated while disconnected is discarded. Initial
connection and authentication failures return to the picker instead of
retrying indefinitely.

RXA has a separate application ping/pong between the gateway and agent to detect
a half-open agent TCP connection quickly and reconnect it. Browser lifetime
remains owned by the shared session layer under the same rules as RDP and VNC.
When that layer ends an RXA engine, it closes the agent connection, stops
capture, and clears the agent's sharing status.

See [`mac-agent-architecture.md`](mac-agent-architecture.md) for the agent,
capture pipeline, protocol, and lifecycle.

## Frontend

The SPA has three states: login, target picker, and remote desktop. The desktop
uses a canvas for tiles and an overlay for input. It supports desktop mouse and
keyboard input, touch gestures, an on-screen keyboard, a clipboard panel, target
switching, takeover, and explicit RDP resize. The on-screen keyboard and the
clipboard panel are mutually exclusive: both dock to the bottom edge on mobile
and report their height so the canvas insets above them.

Incoming image decodes are serialized so tiles and resize messages are applied
in wire order. A remote cursor shape is installed as a CSS cursor for mouse
input and rendered separately for touch input.

Each tab stores its session token in `sessionStorage`, allowing network
reconnects to reclaim the same slot without prompting for takeover. A busy slot
waits for an explicit takeover; an evicted tab waits for an explicit reclaim.

Desktop rendering is 1:1 in device pixels and uses scrollbars when the remote
desktop is larger than the viewport. Touch devices use fit-to-width rendering
with pinch zoom, pan, a virtual cursor, and multi-finger gestures. View
transforms affect presentation and input coordinate mapping, not framebuffer
resolution.

## Configuration and testing

Configuration is one global TOML file with `[server]` and `[[targets]]`
sections. See [`install.md`](install.md) and
[`packaging/etc/remotex.toml.example`](../packaging/etc/remotex.toml.example).

Protocol-specific fields are validated during startup. In particular, `rxa`
requires a checksum-valid PSK and rejects resize, and RDP rejects clipboard;
incompatible fields are rejected rather than silently accepted.

Unit tests cover protocol, config, authentication, key mapping, and engine
helpers. Tests under `tests/` exercise the HTTP/WebSocket session flow and
protocol engines; RDP and VNC happy paths use containerized dummy servers, while
`rxa` uses an in-process fake agent, including an end-to-end check that closing
the browser releases the agent connection. Session-manager tests verify the
same detach deadline for all three protocols. Browser automation is
intentionally not used.
