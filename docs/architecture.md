# Architecture

A single-user web gateway to remote desktops: one Rust binary (an axum web
server with **server-side protocol engines**) plus one Vite/React SPA. The
browser speaks a single uniform protocol no matter what the target speaks —
RDP, VNC and `rxa` sessions are indistinguishable to the frontend.

Macs are reached over **`rxa`**, a protocol built for this one client, served by
a small agent that ships alongside the gateway (`crates/rxa-agent`). It exists
because Apple's Screen Sharing re-prompts for a login on every reconnect; see
[Engines → rxa](#rxa-srcrxars).

This document describes the system as built. Remaining work lives in
[`roadmap.md`](roadmap.md).

## Data path

```
Browser — full-screen canvas SPA (frontend/)
   │
   │  WebSocket /ws — the uniform protocol (src/protocol.rs, protocol.ts):
   │    server → browser: screen tiles as binary frames (PNG or JPEG),
   │                      resize/error as JSON text
   │    browser → server: input events + viewport reports as JSON text
   ▼
axum server (src/server.rs) ── /ws bridge (src/ws.rs)
   │
   │  the session slot (src/session.rs): claim/attach/detach/takeover; the
   │  engine is spawned per *session*, not per WebSocket, and survives
   │  detach. One spawn path, dispatch on the target's protocol — each
   │  engine implements the same run(config, input_rx, frame_tx) contract
   ▼
 ┌───────────────────────┐   ┌────────────────────────┐   ┌──────────────────────┐
 │ rdp::run (src/rdp.rs) │   │ vnc::run (src/vnc.rs)  │   │ rxa::run (src/rxa.rs)│
 │ IronRDP client        │   │ built-in RFB 3.8 client│   │ Noise + tile relay   │
 └───────────┬───────────┘   └───────────┬────────────┘   └──────────┬───────────┘
             │ RDP (TLS/NLA)             │ RFB (raw)                 │ rxa (Noise/TCP)
             ▼                           ▼                           ▼
        RDP server                  VNC server                 remotex-agent      (LAN)
                                                               (crates/rxa-agent,
                                                                on the Mac)
```

The `rxa` engine is the one place tiles are **not** decoded server-side: the
agent encodes PNG/JPEG on the Mac and the gateway relays those bytes untouched.
That is a deliberate exception to the tenet below, and the reasoning is in
[Engines → rxa](#rxa-srcrxars).

## Design tenets

- **Server-side decode for every protocol.** The backend owns the protocol
  session and the framebuffer; the browser only draws tiles. This keeps one
  transport to optimize (backend → browser is the bottleneck link — the
  targets are LAN, the browser may be on weak WAN), enables session
  resume/takeover, and makes "add a protocol" mean "write another
  engine", not "ship another in-browser decoder".

  `rxa` is the deliberate exception, and it does not weaken the tenet: the
  agent is *ours*, so it emits the browser's own tile format directly. Nothing
  is decoded in the browser that was not already (PNG and JPEG are native), and
  the gateway is spared decoding a Retina desktop only to re-encode it.
- **Single session, permanently.** This is a single-user program with one
  active session slot. Session takeover (a new browser force-claims the slot
  and evicts the previous holder) and detach/reattach exist;
  concurrent sessions, session sharing, or a session broker are permanently
  out of scope.
- **Baseline protocol, no per-implementation workarounds.** Guacamole-style:
  speak the subset every server must support, and spend the cleverness on the
  link we control.
- **One config file.** TOML only (`[server]` + `[[targets]]`), no environment
  variables, no `.env`. Credentials stay server-side and never reach the
  browser.

## Backend modules

```
src/
  main.rs            entry: CLI dispatch + serve
  lib.rs             library surface (shared with the integration tests)
  cli.rs             clap CLI (serve --config, gen-passwd, gen-psk)
  config.rs          TOML config ([server] + [[targets]] profiles)
  auth.rs            web login (site_passwd credential + auth sessions)
  server.rs          axum router (/api/*, /ws, disk-served SPA + fallback)
  ws.rs              WebSocket <-> session bridge
  session.rs         the session slot (claim/attach/connect/disconnect/
                     takeover, with a picker state) and the engine seam:
                     spawns rdp::run, vnc::run or rxa::run for the target
  rdp.rs             RDP engine (IronRDP): connect + active loop
  vnc.rs             VNC engine (built-in RFB client, raw-only + resize)
  rxa.rs             macOS agent engine: Noise handshake, tile pass-through,
                     silent reconnect
  engine.rs          helpers shared by the engines (host_port, clamp_u16)
  keymap.rs          DOM KeyboardEvent.code -> RDP scancode / X11 keysym
  protocol.rs        wire messages (ClientMsg / ServerMsg / Tile)
  error.rs           AppError

crates/
  rxa-proto/         the rxa wire protocol, shared by both sides so it cannot
                     drift: PSK, Noise handshake, framing, messages, and the
                     DOM-code -> macOS-keycode table. Cross-platform, and unit
                     tested on Linux because the agent crate never builds there
  rxa-agent/         the macOS agent binary (see below). macOS-only, and
                     excluded from the workspace's default-members so a bare
                     cargo build/clippy/test on Linux never reaches for it
```

Each engine runs on a dedicated thread with a current-thread tokio runtime
(IronRDP's futures are not `Send`; one shared spawn path keeps the seam
uniform). The engine lives as long as the remote session: it is spawned only
once the browser picks a target (`ClientMsg::Connect`, after the post-login
picker) — not merely on attach — and ends when the remote host disconnects, not
when the browser does.

## The session slot

`SessionManager` (src/session.rs) decouples the engine session (backend ↔
remote host) from the browser attachment (backend ↔ WebSocket), with
takeover/reclaim claim rules on top of a persistent engine. The slot also holds
the **selected target**: none is the post-login *picker* state (authenticated,
holding the slot, no connection started), a target is a live desktop. The
selection is slot state, so a takeover inherits it — the new browser lands on
the picker or the desktop exactly where the previous holder was. There is still
one active session at a time; the picker is just the slot's idle state.

- **Claim** — `POST /api/session` (`{force?, sessionId?}`) mints the slot
  token. While another browser's WebSocket is attached the claim answers
  `409` unless `force` (takeover) or `sessionId` is the current token (the
  same browser reclaiming after a network drop). Claiming evicts the
  previously attached WebSocket — its socket closes with code **4001** —
  but never the engine.
- **Attach** — `/ws?session=<token>` joins the slot (a stale token closes
  with code **4000**; the browser claims again). Attach does *not* start an
  engine: it reports the current state to the browser — `picker` when idle,
  or `connected` when an engine is running (a reattach then injects
  `ClientMsg::Refresh`, making the engine re-announce the desktop size and
  repaint fully — RDP repacks its server-owned `DecodedImage`, VNC issues a
  non-incremental update request; the VNC server is one LAN hop away, so
  duplicating the framebuffer server-side would buy nothing).
- **Connect** — `ClientMsg::Connect {target}` (the picker's pick) starts the
  engine for that `[[targets]]` profile; the browser is told `connected` and
  the engine paints.
- **Disconnect** — `ClientMsg::Disconnect` ("switch target") tears the engine
  down and returns the slot to the picker without dropping the WebSocket. An
  engine that ends on its own (remote hung up, or a connect failure after its
  `error`) does the same — the browser lands back on the picker rather than a
  dropped socket, with any error shown there.
- **Detach** — the WebSocket went away; the engine keeps running and its
  frames are dropped until the next attach. Closing the browser therefore
  *detaches* from the desktop rather than ending it; the remote session ends
  only when the remote host ends it.

Browser input is routed through the manager (keyed by the attachment id), so it
always reaches the engine that is live *now* and is simply dropped in the picker
state — no stale engine handle to manage across connect/disconnect.

One slot, permanently: takeover replaces the attached browser, never adds
one — concurrent sessions, sharing, and brokers stay out of scope.

## Web login

Everything session-related refuses unauthenticated requests before they reach
the session layer (src/auth.rs):

- **Credential** — `[server].site_passwd` holds `username:bcrypt_hash`
  verbatim (TOML needs no escaping for bcrypt's alphabet; no base64 wrapping).
  Required; generated with `remotex gen-passwd <username>`
  (hidden prompt on a TTY, reads a line when piped).
- **Login** — `POST /api/auth/login` (`{username, password}`) verifies via
  bcrypt (off the async workers) and sets the `remotex_session` cookie:
  `HttpOnly; SameSite=Strict; Path=/`, plus `Secure` only when
  `x-forwarded-proto: https` says a TLS proxy is in front (Safari drops
  Secure cookies set over plain HTTP). Tokens live in an in-memory map with
  a sliding 6-hour TTL — a restart logs every browser out, harmless for a
  single-user program. `POST /api/auth/logout` invalidates the caller's
  token; `GET /api/auth/status` answers `{authenticated}` for the SPA's
  mount-time check.
- **Guards** — middleware refuses `/api/targets`, `/api/session`, and the
  `/ws` upgrade (the handshake itself 401s) without a live token. Public:
  `/api/health`, `/api/auth/*`, and the SPA shell — it renders the login
  screen and holds no secrets.

The auth session ("may this browser talk to the server?") is independent of
the session slot ("which browser owns the desktop?"): takeover evicts the
other browser's WebSocket but never logs it out. In the frontend, `App.tsx`
mounts the session only once authenticated (mounting claims the slot), shows
the login screen otherwise — with the app version at the bottom, injected
from Cargo.toml via a Vite define — and returns to it when a claim answers
401. Once mounted, the browser holds the slot and lands on the **target
picker** (`TargetPicker.tsx`, fed by `GET /api/targets`); picking a profile
starts its session and switches to the desktop. Logging out is the **Log out**
button in the floating menu (`FloatingMenu.tsx`) — a draggable ☰ FAB that
toggles a toolbar drawer; it ends the browser's login, not the engine. Its
**Switch target** button disconnects the engine (`ClientMsg::Disconnect`) back
to the picker without logging out.
The drawer also sends browser-swallowed **special keys** (F5, Ctrl+W, Alt+F4…)
and **modifier taps** via `sendKeyCombo`, and shows the touch-gesture
cheat-sheet. Its **Soft
keyboard** button opens an on-screen keyboard panel (`SoftKeyboardPanel.tsx`) —
a compact docked layout with a symbol/nav screen toggle and sticky-modifier
badges on narrow viewports, a draggable floating PC-keyboard grid at ≥800px;
every soft key is expressed as a DOM `code` and routed through the same `key`
messages via `sendKeyCombo`, so it reuses the whole input path with no
keysym-only detour. (Its **Clipboard** section remains a placeholder.)

## The wire protocol (browser ↔ backend)

Defined in `src/protocol.rs`, mirrored in `frontend/src/protocol.ts`.

**Server → browser.** Split by weight: screen tiles are **binary
WebSocket frames** — a 10-byte little-endian header (kind, format, x, y, w, h)
followed by a PNG-compressed RGB payload; dirty rectangles taller than
`STRIP_ROWS` (64) are split into strips. Control messages stay JSON text with
a `type` tag: `resize` (the remote desktop size changed), `error` (the engine
failed — the session then returns to the picker, so the browser shows it
there), and the session-slot status `picker` / `connected {name}` telling the
browser which post-login state it is in. Measured ~10x smaller than the old
base64-in-JSON baseline on a full-screen paint; per-session byte totals are
logged on disconnect.

**Browser → server.** JSON text frames. Session control acts on the slot, not
an engine: `connect {target}` (pick a target from the picker) and `disconnect`
(switch back to it). The rest is engine input: `mouseMove`, `mouseButton`,
`wheel`, `key` (DOM `KeyboardEvent.code`, plus a `caps` flag carrying the
browser's authoritative CapsLock lock state so the backend never has to infer
it), `viewport` — the browser's viewport in
device pixels, i.e. the size it *wants* the remote desktop to be (engines
that can drive the remote size act on viewport reports; the rest ignore
them) — and `refresh`, a full-repaint request. `refresh` is normally
injected server-side by the session layer on reattach, but a
browser may also send it to recover a corrupted canvas.

## Engines

### RDP (src/rdp.rs)

IronRDP client: TLS/NLA per the target's `security` mode, active-stage loop
decoding into a `DecodedImage`, dirty regions repacked to RGB strips and sent
as tiles. Input is injected as fast-path PDUs (`keymap::scancode` maps DOM
codes).

**On-request resize (opt-in via `resize = true`).** With the opt-in, the
connector negotiates the Display Control Virtual Channel; a `viewport` report
becomes a `DISPLAYCONTROL_MONITOR_LAYOUT` request (`ActiveStage::encode_resize`,
sizes adjusted to even width and the 200–8192 range). The server answers by
deactivating the session (`DeactivateAll`), which drives a fresh
Deactivation-Reactivation Sequence to learn the renegotiated desktop size; the
framebuffer is rebuilt at that size and a `resize` is sent to the browser.
Unlike VNC, the browser only reports the viewport when the user asks (the
floating menu's "Resize to window") — reactivation is heavier than VNC's
`SetDesktopSize`, so automatic viewport reports are suppressed. Without the
opt-in the desktop keeps its connect-time size (`width`/`height` from the
target profile) and the frontend keeps its scrollbars.

### VNC (src/vnc.rs)

A minimal built-in RFB client (RFC 6143), Guacamole-style baseline:

- **Protocol 3.8.** Anything announcing at least 3.8 is answered with 3.8
  (macOS Screen Sharing greets with 3.889, RealVNC with 4.x — both accept a
  3.8 client). Older servers are rejected.
- **Security None or classic VncAuth** (DES over the 16-byte challenge with
  the RFB bit-reversed key convention). VncAuth is chosen when the target has
  a password, otherwise None. Apple/RealVNC proprietary types are not spoken.
- **Raw encoding only.** The one encoding every VNC server must support. The
  backend↔VNC hop is LAN, so VNC's clever wire encodings buy nothing there.
- **Forced pixel format:** 32bpp true-colour BGRX little-endian, repacked
  server-side to RGB and PNG-encoded in the same strips as RDP.
- **Input:** pointer events carry the tracked button mask + position (wheel =
  press/release of buttons 4–7); keys map DOM `code` → X11 keysym via
  `keymap::keysym`, resolved against the live Shift state so the *shifted*
  keysym is sent (`A`, `!`) rather than the base symbol — VNC servers force
  the exact keysym requested, so sending the unshifted symbol while Shift is
  held drops the Shift. CapsLock case is applied the same way from the `key`
  message's `caps` flag (letters only, XORed with Shift) and the CapsLock key
  is never forwarded, keeping the server's Lock modifier off to avoid
  re-casing ambiguity. (RDP needs none of this: it forwards scancodes and the
  host tracks its own modifier state.)

**Pointer shapes (always on).** The Cursor pseudo-encoding (`-239`) is
advertised unconditionally: a server that supports it stops compositing the
pointer into the framebuffer and sends the shape instead (pixels + a 1-bit
mask, the rect's x/y being the hotspot), which the engine folds into an RGBA
PNG and forwards as a `cursor` control message for the browser to draw. This
is what puts a pointer on the screen for servers that never composited one —
macOS Screen Sharing draws no cursor into the framebuffer at all, so without
this the desktop arrives with no pointer anywhere on it. Receiving a `cursor`
message at all is what tells the browser it owns pointer rendering; engines
that composite (RDP, and VNC servers that ignore the pseudo-encoding) send
none and the browser keeps its own pointer hidden. A 0×0 rect is the server
hiding the pointer — still browser-owned, so the frontend substitutes a plain
arrow rather than leaving nothing on screen (Xtigervnc reports exactly this
for a root window with no cursor set). The latest shape is cached per session
and replayed on `Refresh`, since the server only resends it when it changes
and a browser attaching later would otherwise get no pointer.

**Dynamic resize (opt-in via `resize = true` on the target).** The
engine advertises the DesktopSize/ExtendedDesktopSize pseudo-encodings and
turns browser `viewport` reports into `SetDesktopSize` requests, so
TigerVNC-family servers (Xtigervnc, x0vncserver, …) re-render at the
browser's size and the scrollbars disappear. `SetDesktopSize` is only sent
after the server declares support with its first ExtendedDesktopSize rect; a
report arriving earlier is stashed and replayed then. Any size change —
requested or server-initiated — is forwarded to the browser as `resize` and
followed by a full framebuffer request, since a resize invalidates the
contents. Servers without the extension (and targets without the opt-in)
keep the fixed connect-time size — acceptable per the no-workarounds rule.

Deliberately out of the VNC baseline: clipboard (`ServerCutText` is drained
and dropped), Bell, and non-raw encodings.

### rxa (src/rxa.rs)

The way remotex reaches a **Mac**. VNC still works against Apple's Screen
Sharing, but it was an ongoing pain point: sessions dropped, and a disconnect
forced a fresh **login** every time — the credential prompt is a property of
Apple's server, so there was nothing to fix on our side of the RFB connection.
Screen Sharing also ignores `SetDesktopSize` and never composites a cursor
(the reason the Cursor pseudo-encoding path above exists at all). RealVNC is
stable and does not re-prompt, but its free tier has no LAN direct-connect.

So `rxa` (remote**X** **a**gent) stops speaking VNC to the Mac. A small agent
(`crates/rxa-agent`) runs there and the gateway dials it with a **pre-shared
key**. Because the PSK lives in the config file, a reconnect is a two-message
cryptographic handshake with **no interactive login, ever** — which is
precisely the RealVNC property that was missing.

**Transport.** TCP + `Noise_NNpsk0_25519_ChaChaPoly_BLAKE2s` (`snow`), with the
protocol version bound in as the Noise prologue so a mismatch fails cleanly at
the handshake instead of desynchronising later. The PSK alone provides mutual
authentication: no certificates, no CA, nothing that expires. `NN` adds an
ephemeral DH on top, so a recorded session stays unreadable even if the PSK
later leaks. Everything both sides must agree on lives in `crates/rxa-proto`,
in the same repo, so the two halves cannot drift.

The PSK carries a **CRC**, so a mistyped key is a config-parse error naming the
checksum rather than a handshake that mysteriously never completes once the
gateway starts dialling.

**Framing treats Noise as a byte stream.** A Noise transport message caps at
65535 bytes and a full-screen keyframe is far larger, so rather than inventing a
chunking flag, `frame.rs` splits transparently into ≤65519-byte Noise messages
and application framing sits on top as plain `u32 LE length + u8 type + body`.
Chunking never reaches the message definitions, and the whole layer is testable
against an in-memory duplex. One consequence is load-bearing: the read and write
halves each need their own nonce counter, so the framing holds a
`snow::StatelessTransportState` rather than the stateful transport. With the
stateful one, a reader blocked waiting for a tile would block the writer trying
to send input.

**Pixels pass through.** The agent captures with ScreenCaptureKit, takes the
**native dirty rects** macOS reports, and encodes each tile as PNG or JPEG
*on the Mac*, choosing per tile — PNG for flat UI and text (smaller *and*
sharper), JPEG for photographic content. The gateway relays those bytes into
`Tile::encoded` without decoding a pixel, which is why `Tile` carries a
`format` byte. Nothing new lands in the browser: `createImageBitmap` decodes
JPEG natively.

The classifier has to run on every tile, so it samples a strided subset and
counts distinct colours quantised to 5 bits per channel: few colours means UI,
many means photo. On a real desktop that lands where you would expect — text and
window chrome as PNG, wallpaper gradients as JPEG. Dirty rects are split at
`STRIP_ROWS = 64`, the same as the other engines, so a full repaint does not
become one enormous message and the browser can start painting early.

Two capture details are the classic ways to get this wrong, and both are
handled explicitly. `CVPixelBuffer`'s **`bytesPerRow` is not `width * 4`** —
rows are read at the reported stride, or the image shears. And a Retina display
**captures at pixel dimensions that differ from the point dimensions**
`CGEventPost` wants, so both are kept and converted at the input boundary.

**The pipeline is bounded and coalescing, with one encoder thread.**

```
SCStream callback ──▶ raw tiles ──▶ encoder ──▶ out ──▶ pump ──▶ socket
  (dispatch queue)     (bounded)    (thread)   (bounded)  (tokio)
```

The capture callback never encodes and never blocks — blocking
ScreenCaptureKit's dispatch queue stalls capture itself. When the link cannot
keep up the sink drops the frame and sets the full-repaint flag, so falling
behind degrades into one later, coarser repaint instead of a queue of stale
tiles. And there is deliberately **one** encoder thread rather than a pool: a
pool lets two frames' tiles finish out of order, and the same region is commonly
dirty in consecutive frames, so an older tile could land on top of a newer one
and leave stale pixels until something else redraws them. Ordering is worth more
than the parallelism until measurement says otherwise.

**Input** is `CGEvent*` + `CGEventPost`. Keys go through
`rxa_proto::keymap::mac_keycode` with modifier flags tracked from what the
browser reports; remotex sends `caps` as an authoritative flag on every key
event, so the agent never has to infer lock state. Pointer coordinates arrive as
framebuffer pixels and must become global display points — divide by the
capture's backing scale, offset by the display origin. That conversion is the
most likely "clicks land in the wrong place" bug in the agent, so it is a pure
function tested at 1×, 2× and offset corners.

**Two TCC permissions, and neither can be obtained by simply using the API.**
Screen Recording for `SCStream`, Accessibility for `CGEventPost`. Asking is not
politeness: `SCShareableContent` fails with something that reads like a refusal
but also happens when the question was never asked — and until it *is* asked the
agent does not appear in the Screen Recording list at all, so there is nothing
for the user to switch on. `CGEventPost` is worse; it never fails. Without
Accessibility the screen paints, the session looks perfectly healthy, and every
click and keystroke is silently discarded. So the agent calls
`CGRequestScreenCaptureAccess` and `AXIsProcessTrustedWithOptions` explicitly at
startup, and reports where both stand.

Both grants are keyed to the **signed code identity**, which is why signing is
not optional: an ad-hoc signature has no stable identity, so every rebuild
re-prompts. Both also require a window server connection, which exists only
inside the user's GUI session — hence a LaunchAgent, never a LaunchDaemon, and
hence no login-window support (see [`roadmap.md`](roadmap.md)).

**Reconnect is the point.** This engine deliberately differs from RDP and VNC
in one way:

- an **initial** connect failure is fatal and reported, exactly like the other
  engines — a wrong host or a wrong PSK must be visible immediately in the
  picker, not buried in a retry loop;
- an **established** session that drops retries silently with capped backoff
  (1 s → 15 s), forever. On reconnect it repaints, and announces a size only if
  the Mac came back a *different* one — a `resize` costs the frontend its canvas
  contents, so the usual unchanged-desktop reconnect must not send one. The
  browser sees frames pause and resume; it never bounces back to the picker, and
  there is never a credential prompt.

Application-level ping/pong on an idle timer catches the half-open TCP
connection that `SO_KEEPALIVE` would take minutes to notice — the exact shape a
Wi-Fi drop takes. Input buffered during an outage is discarded rather than
replayed: a mouse position from eight seconds ago is worse than no event.

**Cursor shapes** ride the existing `cursor` control channel, so the frontend's
`paintCursor` — built for VNC — needs no changes at all.

**Scope.** Screen, keyboard/mouse and cursor shapes. Out: clipboard, audio, and
dynamic resize — the agent captures the Mac's own resolution and `resize = true`
is rejected on an `rxa` target. That last one is a design decision rather than a
gap: unlike RDP, which resizes its own isolated session, a Mac has one console
session, so the only thing there is to resize is the physical display of
whoever is sitting at it (see [`roadmap.md`](roadmap.md)).

**The agent's menu bar item is its entire interface** — no Dock tile, no
windows, and no CLI beyond three launch flags (`--config`, `--no-register`,
`--no-menu`). It reports whether a gateway is attached and to which address,
copies the pre-shared key, opens one settings dialog holding all three settings
(listen address, display, key), reveals the config, opens the log, toggles the
login item, and quits. Having the item at all is what makes the agent's state
observable to the person whose screen is being shared, which for software of this
kind is not a nicety.

The two TCC grants are **not** in that list of settings, because they are not
settings: the agent does nothing useful without either, so they are health. The
icon carries a third state for "a permission is missing" — ahead of "connected",
since a gateway attached to an agent that cannot capture or inject is the case
most worth warning about — a panel asks for the missing one once per launch and
offers to open the pane, and the menu grows an **Enable …** row only while one is
missing. Nothing about them appears when both are granted.

The two are read on deliberately different schedules, because they take effect
differently. Accessibility applies the moment it is ticked — `CGEventPost` starts
landing with no restart — so it is polled once a second until it is granted and
then left alone. Screen Recording is granted to a *launch*: the TCC state flips
immediately but ScreenCaptureKit goes on refusing the running process, which is
why macOS itself offers to quit and reopen the app. So it is read once at startup
and believed for the rest of the run; re-reading it would report "granted" over a
session that cannot capture a pixel.

Quit is why the embedded LaunchAgent sets `KeepAlive` to `SuccessfulExit: false`
rather than `true` — under a plain `true` launchd would restart the agent seconds
after the user asked it to stop, while the narrower form still recovers from a
crash. The menu also means AppKit owns the main thread: `menubar::run` takes it,
and the cursor poll runs as a timer on that run loop.

Two consequences of putting everything there, both deliberate:

- **A failed launch gets a panel too.** A background app has no window to fail
  in, so a startup that gives up silently is a double-click that does nothing at
  all — no icon, no error, and no way to find out short of running the binary in a
  terminal. So the listener is bound on the main thread, before any of the
  threading is set up, and "the port is already taken" (the common case: the app
  was opened while a copy was running) or "that config cannot be loaded" says so
  on screen and exits **0** — zero because launchd's `KeepAlive` would otherwise
  restart the agent into the same failure and put the same panel back up every ten
  seconds.
- **`NSAlert` is the whole panel toolkit** (`panels.rs`). A settings *window* for
  three fields would be a window controller, a nib and a Dock tile's worth of
  behaviour the agent spent real effort not having; an alert with an accessory
  view is three fields. Panels activate the app first: an accessory app is never
  active, and a modal it opens without activating lands *behind* everything —
  invisible, and unreachable from the menu that opened it.
- **A saved change is applied by restarting into it** (`settings.rs`). Editing
  validates and writes the file, atomically and at 0600, and then the agent
  re-execs itself if anything actually changed. Rebinding a listener under a live
  connection, swapping the key the current gateway authenticated with, restarting
  the capture stream on another display: three piles of machinery replaced by one
  `exec`, which cannot leave a change half-applied. Not "quit and let launchd
  restart us" (`KeepAlive` is `SuccessfulExit: false`, so Quit can mean Quit) and
  not "spawn a copy and exit" (the copy loses a race with the port it is about to
  bind). The re-exec keeps the PID, the launchd job and the code identity the TCC
  grants are keyed to; the gateway sees a dropped connection and reconnects, which
  it is already built to do. The config the process was launched with is kept
  beside the saved one, so if the `exec` ever fails the menu can say "restart to
  apply" instead of showing a setting that is not in force.

Reading the permissions is a menu item and not a subcommand for a reason that
outlasts taste: macOS credits a TCC grant to whatever launched the process, so a
shell asking on the agent's behalf gets the shell's answer.

Installing, signing and the permission grants are covered in
[`packaging/macos/README.md`](../packaging/macos/README.md). Why the design is
shaped this way, and what the machine disagreed with, is in
[`mac-agent-plan.md`](mac-agent-plan.md); what it would take to reach a
logged-out Mac, and why that is not planned, is in
[`roadmap.md`](roadmap.md).

## Frontend

Vite + React 19 + TypeScript, managed with Bun (`frontend/`). The files that
matter:

- `protocol.ts` — TS mirror of the wire protocol (binary tile parsing).
- `App.tsx` — the auth gate: login screen vs the session.
- `Login.tsx` — the login form, with the app version pinned at the bottom.
- `useRemoteDesktop.ts` — the one hook: session claim + WebSocket lifecycle,
  the picker-vs-desktop `mode` (from the server's `picker`/`connected` status)
  with `connect(name)` / `switchTarget()`, tile rendering, input capture,
  viewport reporting, the touch view transform (fit-to-width × pinch zoom + pan).
- `TargetPicker.tsx` — the post-login target picker: lists `GET /api/targets`
  and starts the picked session; shows a failed connect's error.
- `touchGestures.ts` — the mobile touch gesture engine.
- `RemoteDesktop.tsx` — the session shell: the picker or the full-screen canvas
  + input overlay + the connection-status overlay + the floating menu.
- `FloatingMenu.tsx` — the draggable ☰ FAB and toolbar drawer (Switch target,
  Log out, special-key/modifier combos, the soft-keyboard toggle, plus a
  clipboard placeholder).
- `SoftKeyboardPanel.tsx` / `softKeyboard.ts` — the on-screen keyboard panel
  and its layout tables (compact docked screens + the ≥800px PC grid).

**Connection flow.** The hook claims the session slot, opens the
WebSocket with the token, and reconnects automatically with capped backoff
after any drop (network, server restart) — no page reload. The per-tab token
(sessionStorage) makes a reconnect a *reclaim*, so it never trips the takeover
prompt. Once attached, the server's `picker`/`connected` status drives the
`mode`: the picker (pick a target → `connect`), or the desktop (`switchTarget`
→ `disconnect` returns to the picker). Two states wait for the user instead of
retrying: **busy** (another browser holds the slot; "Take over" force-claims)
and **taken over** (this tab was evicted with close code 4001; "Take it back"
force-claims). A fatal engine error is no longer a dead-end state — the socket
stays up, the session returns to the picker, and the error shows there. The
reconnect backoff resets once the socket attaches (any status message), so a
slot that closes right after connecting can't hot-loop.

**Full-screen canvas.** The canvas fills the browser viewport and
renders at **1:1 device pixels**: the backing store stays at the remote pixel
size, the CSS size is remote ÷ `devicePixelRatio` — no scaling, no
letterboxing. A remote desktop larger than the viewport overflows into native
scrollbars. A re-armed `matchMedia` listener re-derives the CSS size when
`devicePixelRatio` changes (monitor moves, browser zoom), and the CSS size
snaps to the viewport when the remote matched it so fractional-dpr rounding
can't spawn phantom scrollbars.

**Viewport reporting.** On connect and on window-resize/dpr changes
(debounced 250ms, deduped) the browser sends `viewport` = viewport size ×
`devicePixelRatio`. Where the engine can act on it (VNC with `resize = true`
against a TigerVNC-family server) the desktop follows the window and the
scrollbars disappear. RDP with `resize = true` reports the viewport only on
request: the `connected` status carries the target's protocol and `resize`
flag, and for RDP the automatic reporters are suppressed (`manualResizeRef`) —
the floating menu's "Resize to window" button pushes one report, because RDP's
reactivation is heavier than VNC's resize.

**Mobile.** Pinch-zoom-capable touch devices
(`navigator.maxTouchPoints >= 2`) diverge from the desktop model in two ways,
both with conservative touch bounds. *Sizing:* the viewport report uses
CSS pixels (no dpr — a phone's 3× dpr would mint an enormous desktop),
floored per axis at a constant 1024×768; the constant floor (rather than
geometry found on connect) means a phone connecting to a desktop a previous
session left too tall repairs it on connect, since the engine outlives the
browser here. *Display and input:* native scrolling is off; `applyCanvasCss`
positions the canvas by fit-to-width scale × pinch zoom (1–4×) plus a clamped
pan (`translate3d`), and `touchGestures.ts` drives it — a trackpad model
where the cursor is a persistent position (drawn by the remote, or by the
browser when the engine sends shapes): one-finger tap clicks at the cursor,
one-finger drag moves it
(edge-panning the view), double-tap-and-hold holds the left button with a
second finger assisting, two-finger tap right-clicks, pinch zooms, two-finger
drag pans, three-finger swipe scrolls axis-locked. Gesture wheel ticks are
sign-only `wheel` messages; the input overlay covers the whole viewport (the
disconnect bar is z-lifted above it), and hybrid mouse input maps through
the canvas rect so it tracks the zoom/pan.

PNG tiles decode asynchronously (`createImageBitmap`), so all incoming
messages run through one promise queue: draws land in arrival order and a
resize can't jump past queued tiles. Input is captured on a transparent
overlay exactly covering the canvas; held keys/buttons are released on blur
so nothing sticks on the remote.

**Pointer.** By default the browser's own cursor is hidden — the remote
composites one into the framebuffer. When the engine sends `cursor` messages
instead (see the VNC section), `paintCursor` takes over: the hardware pointer
wears the shape as a CSS `cursor` (no lag — the compositor moves it), and the
touch gesture layer's virtual cursor gets an image element positioned at its
remote coordinate, never drawn below 1:1 so it stays findable when the desktop
is zoomed out to fit a phone. A null shape (remote hid the pointer) falls back
to an arrow painted into a canvas — PNG, not SVG, since Safari rejects SVG
cursors. Pointer state lives in refs and is pushed straight to the DOM:
pointer motion must never re-render.

## Configuration

One global TOML file (`--config <path>`, or `<prefix>/etc/remotex.toml` in the
installed layout — see [`install.md`](install.md) and `packaging/`). A
`[server]` block (bind host/port, static dir, the required `site_passwd`
web-login credential) plus `[[targets]]` profiles:
protocol, host/port, credentials, RDP-only `width`/`height`/`security`, the
VNC `resize` opt-in, and the rxa-only `psk`. Every profile is served; the
browser picks one from the post-login picker (there is no `--target` selector —
one pathway to a target). See `packaging/etc/remotex.toml.example`.

Per-protocol fields are validated at parse time rather than ignored: an `rxa`
target without a `psk` — or with one whose checksum fails — is a startup error
naming the typo, not a handshake that mysteriously never completes. A `psk` on
a non-`rxa` target is refused too, since silently ignoring it would leave
someone believing a target was authenticated by a key it never uses.

## Testing

- **Unit tests** live with the code (protocol encoding, RFB handshake pieces,
  VncAuth vectors, input translation, keymaps, config parsing).
- **E2E tests** (`tests/`): protocol-level tests against the real axum server
  (`protocol_e2e.rs` — claim/attach flows, the picker state, connect,
  switch-target, takeover eviction (including takeover of the picker), and
  detach/reattach run against a scripted in-process RFB server, so the
  session-slot semantics are covered deterministically without containers),
  and container-backed happy paths — `rdp_tiles_e2e.rs` against a dummy xrdp,
  `vnc_tiles_e2e.rs` against a dummy TigerVNC (full-desktop paint, dynamic
  resize, and detach/reattach repaint through a real server). Containers run
  under podman or docker. For a **remote** engine over SSH — the only option on
  a Mac that cannot run one locally (inside a VM there is no nested
  virtualization, so `podman machine` fails outright) — set
  `CONTAINER_CONNECTION` to the connection name and
  `REMOTEX_TEST_CONTAINER_HOST` to the address the published port is reachable
  on, since a remote engine publishes on its own loopback. **Never a headless
  browser** — browser automation is flaky by policy.
- **The rxa engine** is covered container-free by `rxa_e2e.rs`, which drives
  the real server against an in-process fake agent speaking the real Noise
  handshake: a JPEG tile arrives byte-for-byte as `format = 2`, the cursor
  lands on the control channel, a dropped link reconnects and repaints without
  an error or a picker bounce (and without resizing an unchanged canvas), a
  wrong PSK is reported rather than retried, and browser input reaches the agent
  in order and untranslated with `viewport` swallowed.
  `rxa-proto` carries the protocol's own suite (PSK, handshake, framing across
  the Noise chunk boundary, per-variant message roundtrips, keycodes), all of
  which runs on Linux — deliberately, since the agent crate never builds there.
- **The agent** can only be tested on macOS, and its capture and injection
  paths additionally need the two TCC grants, so they are verified by hand from
  the running agent — the only thing that can answer, since macOS credits a
  permission to whatever launched the process. The menu bar item is where that
  answer shows up, by *absence*: an **Enable …** row appears only while a grant
  is missing and nothing about them appears once both are granted (Accessibility
  polled once a second until it is on, Screen Recording read once at startup —
  see the rxa section above). Its GUI is hand-verified for the same kind of
  reason: `NSAlert` and `NSStatusItem` need a window server, so nothing under
  `cargo test` can open a menu.
