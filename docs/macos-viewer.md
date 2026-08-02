# remotex.app

`remotex.app` is a native macOS 26 client. It can start its bundled gateway or
connect to a remote one. It owns the gateway session — target selection, the
claim, reconnection, takeover — plus the menu bar, keyboard capture, pasteboard
synchronization, and the window.

The remote surface inside that window is a `WKWebView`, and only that. See
[The canvas](#the-canvas): the app hands it wire frames and it draws them,
sharing the browser client's decoding rather than reimplementing it. Everything
above the surface is native; nothing about the session is the page's.

The two gateways are the app's first question, asked on the `home` screen at every
launch with the last answer preselected:

| | the embedded gateway | a remote gateway |
|---|---|---|
| where it runs | in this bundle, started at launch | wherever it was installed |
| address | an ephemeral loopback port it picks | typed, and remembered once it answers |
| credential | a bearer token nobody types | a persisted login cookie obtained from a username and password |
| who reaches the target | this Mac | that gateway |
| **Configuration…** | edits this instance's config | not shown — it is that gateway's |

Use the embedded gateway when this Mac directly reaches the targets. Across a
slow link, run the gateway near the targets and choose the remote gateway.

Both choices use the same `/api/config`, `/api/targets`, `/api/session`, and `/ws`
contracts. Only the credential header differs; see `GatewayCredential`. Any
other behavioral difference between them is a bug.

The client has no RDP or VNC implementation. Engine-specific behavior is
reported by the gateway, with resize policy as the only client-side branch.
Whether the remote is a Mac is likewise discovered from the gateway's
`remoteOs` message and affects only keyboard conventions.

The bundle holds two executables and the remote surface:

| path | what it is |
|---|---|
| `Contents/MacOS/remotex-viewer` | the Swift app (`CFBundleExecutable`) |
| `Contents/MacOS/remotex-gateway` | a copy of the `remotex` gateway binary |
| `Contents/Resources/canvas` | the canvas page — see [The canvas](#the-canvas) |

`canvas` is not the SPA and not a web UI: the embedded gateway still serves
nothing, and an `index.html` in this bundle would mean it had started to (which
`release.yml` checks).

`CFBundleIdentifier` remains `dev.remotex.viewer`; TCC grants and saved window
state are keyed to it.

## The embedded gateway

`remotex serve-embedded --instance-dir <dir>` serves only its parent app and dies
with it. `src/embedded.rs` and `config::Audience` fix its server settings:

| | |
|---|---|
| address | `127.0.0.1`, one socket |
| port | `0`, read back off the socket after binding |
| web UI | none. No `ServeDir`, no index handler; `/` is a 404 |
| login | refused. `/api/auth/*` answers 403 |
| credential | a random bearer token minted per launch, held only by the app and gateway in memory |

### Process pipes

After binding, the gateway writes exactly one JSON line to stdout:

```json
{"port":49213,"token":"…"}
```

The app is the token's only client-side holder. Logging goes to stderr. The
private stdout pipe keeps the token out of `argv`, the environment, and
persistent files.

The app keeps the write end of the gateway's stdin open without writing. When the
app exits, the kernel closes it; the gateway reads EOF and exits.

### Shutdown, in three layers

1. **The liveness pipe** handles clean quit, crash, Force Quit, and `kill -9`
   without requiring app cleanup code. `aGatewayIgnoringSignalsStillDiesWithThePipe`
   proves this with a child that traps `SIGTERM`. macOS has no
   `PR_SET_PDEATHSIG`; unlike `getppid` polling, the pipe leaves no interval in
   which an orphan still listens.
2. `SIGTERM`, the ordinary graceful stop.
3. `applicationWillTerminate` closes the pipe and terminates the child, then kills it
   after a grace period. Synchronous, because the process may be gone the moment it
   returns.

### The instance directory

The instance directory is `--instance-dir <path>` when supplied, otherwise
`~/Library/Application Support/<CFBundleName>` (mode `0700`). The shipped bundle
uses `remotex`; variants use their bundle name. All app state is beneath this
directory; `/opt/remotex` is never consulted.

| file | |
|---|---|
| `remotex.toml` | the only thing a user edits, mode `0600` |
| `gateway.log` | the gateway's stderr, appended across launches |
| `viewer.json` | client preferences |

Preferences use `viewer.json`, not `UserDefaults`, so `--instance-dir` isolates
them during QA. A `UserDefaults` suite remains in the user's Preferences directory
regardless of the supplied instance directory.

A first launch writes a commented zero-target template, which is valid; the picker
states that there is nothing to connect to.

### Configuration

**Remote › Configuration…** edits `remotex.toml` and can reveal it in Finder.

Save runs the bundled gateway's `check-config --embedded`. Failure preserves the
editor text, displays the gateway error, and writes nothing. Success writes
atomically and restarts the gateway.

`[server]` is refused in this file, and having no targets at all is not an error — it
is what a first launch has, and the picker says so in words.

A top-level `branding` sets the target-list heading, window title, and launch
screen. It is the one shared spelling for embedded and served gateways;
`[server].branding` does not exist.

### About

Because `commandsReplaced` removes the standard app menu, `RemoteCommands`
restores **About** explicitly. It uses the configured branding and shows
`CFBundleShortVersionString`, the wire protocol version, and the instance
directory, with Reveal in Finder.

## Running more than one instance

**A second instance is a second app.** One command stamps it out:

```sh
packaging/macos-viewer/make-instance-bundle.sh remotex-work ~/Pictures/work.png
```

This creates `~/Applications/remotex-work.app`, an ad-hoc-signed copy with its
own bundle identifier, name, icon, and default instance directory.

### Why a separate bundle

LaunchServices supplies no arguments on double-click, and `open` without `-n`
discards `--args` when reactivating an app. Reading the instance directory from
`CFBundleName` (`InstanceDirectory.defaultURL`) therefore makes each variant
independently launchable. A wrapper
could carry `--instance-dir`, but the Dock would show the base app rather than the
instance. Keep `--instance-dir` only as a QA override.

### What a variant costs

- **13 MB, and it goes stale.** Re-run the script after updating `remotex.app`;
  it replaces the bundle without touching its instance directory.
- **Nothing else.** No entitlements, no notarization, no TCC. The shipped bundle is
  ad-hoc signed itself (`codesign -dv` → `Signature=adhoc`), and the viewer holds no TCC
  grants for a change of code identity to break: it captures keys with a *local*
  `NSEvent` monitor, which needs no Accessibility permission.

Do not edit `remotex.app` in place; use the script so the copy is re-signed.

### Naming the window too

The bundle name reaches the Dock, ⌘-Tab and the menu bar. The *window* is named by the
config:

```toml
branding = "Work"
```

The bundle name identifies the app; `branding` identifies its content.

Each instance starts with an independent empty configuration and an isolated
port, log, and preference file.

## Protocol compatibility

Before opening a session, the client requires `GET /api/config`'s
`protocolVersion` to match `PROTOCOL_VERSION` in `src/protocol.rs`. A mismatch is
reported on the launch screen. For an embedded gateway it indicates a broken build.

The version covers client messages, control messages, and binary frame layouts.
Unknown additive control messages are ignored, but a change that makes an older
peer fail without a useful error requires a version bump.

Contract tests protect both sides:

- `ProductInfoTests` compares the Swift protocol version with the Rust constant;
- `WireContractTests` compares the Rust message tags with the tags handled by
  the viewer;
- `ServerMessage` tests reuse the JSON literals pinned by the Rust protocol
  tests.

## Entry and session lifecycle

`ViewerScreen` has `home`, `login`, `launching`, `picker`, and `desktop` states.

The embedded branch is `home` → `launching` → `picker`. `launching` shows a
spinner or gateway stderr with **Configuration…**, **Change Gateway…**, and
**Try Again**. An unexpected gateway exit returns there without an automatic
restart loop.

The remote branch is `home` → `login` → `picker`, skipping `login` for a valid
stored cookie. `home` verifies reachability and protocol compatibility. A `403`
from `/api/auth/status` identifies an embedded gateway and is rejected instead
of presenting an unusable login form.

**Change Gateway…** returns to `home`; for a remote gateway it also logs out and
releases the session slot.

Authentication and session ownership stay separate, as before:

- the credential authorizes this client to the gateway;
- the claim token owns the program's one active session slot.

A remote `401` drops the stored cookie and returns to `login`. An embedded token
is valid for the lifetime of the process that minted it, so an embedded `401`
means the process no longer recognizes its launch token; the app restarts it once,
then returns to `launching` rather than looping.

`SessionStateMachine` implements claim, attach, reconnect and takeover as a pure state
machine. Network reconnects use capped exponential backoff up to 15 seconds. A busy
slot and a session taken over by another client wait for an explicit user decision
because resolving either case may evict the current owner.

The reconnect backoff resets after a control message proves that the connection
is usable, not merely when the WebSocket opens. Any interruption clears the
framebuffer and releases held input; the gateway requests a full repaint when a
client attaches again.

## The canvas

The remote surface is a web page — `frontend/src/viewer`, built by
`bun run build:viewer` into `Contents/Resources/canvas` — shown in a `WKWebView`.
It draws tiles to a 2D canvas, wears the remote cursor as a CSS cursor, decodes
Opus and H.264 with WebCodecs, and scrolls an oversized desktop with the browser's
own scrollbars. It shares `protocol.ts`, `tilePainter.ts`, `cursorCss.ts`,
`videoDecoder.ts` and `audioPlayer.ts` with the browser client, so the wire format
has one implementation and both clients read it.

It owns nothing else. There is no session, no gateway socket and no claim in the
page; it is handed frames and it reports pointer input.

### The loopback bridge

`CanvasServer` is an `NWListener` bound to `127.0.0.1` on an ephemeral port, with
a random path prefix minted per launch — the same split as the embedded gateway's
bearer token, and for the same reason: the port is not a secret and the token is.
It serves the page and one held-open `GET /<token>/frames`.

The document's origin is therefore `http://127.0.0.1:<port>`, which is what makes
this work at all. Loopback is potentially trustworthy, so the page is a **secure
context** and WebCodecs is available against *any* gateway — embedded or remote,
HTTP or HTTPS. A `file:` URL or a custom scheme is not reliably trustworthy in
WebKit, and without a secure context `AudioDecoder` is simply absent and remote
sound disappears with nothing in any log to say why. The page reports
`isSecureContext` and whether it found a decoder in its first message; the app
raises an alert if either is wrong, because both fail silently on their own.

Everything from the app rides that one stream, in order:

```text
[u32 be length][u8 kind][payload]        length counts the kind byte
  kind 0x00 — a JSON control command
  kind 0x01 — a gateway binary frame, its own 0x02/0x03 kind byte included
```

Ordering is the point of using one channel for both. Tiles carry no delta state
and overwrite their rectangles, so a `resize` that overtook the tiles queued ahead
of it would paint stale pixels into a freshly sized canvas; an `audioFormat` that
arrived after the packets it configures configures nothing. The commands are
`resize`, `clear`, `cursor`, `audioFormat`, `audioStop` and `input`.

Binary frames are forwarded byte for byte. The 256-slot tile cache, the image
decode, `cacheReset`, the audio decoder and the H.264 video decoder are all the
page's, which is why `render_type = "video"` reached this client as a frontend
change rather than as a second implementation of a format only one side reads.

A page that stops draining the stream costs frames rather than memory:
`CanvasServer` drops binary envelopes once ~2 MB is unwritten and asks the gateway
for a full repaint when the backlog clears, since nothing else would ever repaint
what went missing. That guard meets `render_type = "video"` badly and recovers
anyway: the access units after a dropped one refer to a frame the decoder never
saw, so it errors, and the repaint the re-prime asks for is the IDR that starts it
again. The visible cost is one `videoState` alert for a stream that is about to
work — which is why the alert is worth having only for a page that is genuinely
wedged, and why the drop is logged.

The page reports back over one `WKScriptMessageHandler`: `ready`, `pointer`,
`button`, `wheel`, `cacheReset`, `audioState` and `videoState`. It holds no
protocol state —
the app builds every `ClientMsg` and `PressedInput` still answers for releasing
what was held on a target switch, a takeover or a dropped socket. Keyboard events
never reach the page at all; see [Keyboard and pointer input](#keyboard-and-pointer-input).

A reloaded page reattaches its stream, and the app re-primes it from scratch:
the current size, the current cursor, and a `refresh` for the pixels.

### Presentation

The canvas bitmap is the remote's framebuffer, pixel for pixel, and its CSS box
is that divided by the remote's own density:

```text
CSS size = framebuffer pixels / remote backing scale
```

The window's screen then rasterizes that box at its own backing scale. A remote
larger than the window scrolls; the viewer does not zoom or fit the framebuffer
to the window.

`REMOTEX_VIEWER_DEV_URL` points the web view at `bun run dev` instead of the
bundled page, keeping the stream — the same shape as `REMOTEX_DEV_BACKEND` for
the SPA.

## Display and resize behavior

`ViewportPolicy` separates gateway-granted resize permission from whether the
viewer actively reports window changes.

Permission comes from the `connected` message:

| Target | May resize |
|---|---|
| RDP or VNC with `resize` | yes |
| Any other case | no — no viewport request is ever sent |

The three View menu items are one decision:

- **Auto Resize** sends debounced, deduplicated window changes. Its remembered
  value is shared with the picker's *Auto-resize the remote to the window, if
  compatible* toggle. It defaults off until chosen and is ignored for targets
  without resize permission. See `ViewerPreferences.autoResizeByDefault`.

  > **Known limitation on RDP.** When the default is on, the client reports its
  > window from the `connected` handshake — before RDP's Display Control channel
  > has finished opening. The gateway drops a size request that arrives that early
  > (`Asked::NotReady`) and, unlike a density change, does not retry it, so on an
  > RDP target the desktop stays at its connect size until the next window change
  > lands after the channel is up. Turning **Auto Resize** off and on, or nudging
  > the window, applies it. VNC is unaffected. This is a server-side gap
  > documented at the `Viewport` arm in `src/rdp.rs`; the browser client has the
  > same symptom for the same reason.
- **Resize to Window** asks the remote to adopt the viewer's available size, once.
- **Resize to Display** changes the local window so the current remote desktop
  fits at its point size; it sends nothing to the gateway.

All three remain in the menu and are disabled when they do not apply. The two
one-shots are disabled while **Auto Resize** is on: one is what it does
continuously, and the other cannot fit a window to a desktop that is already
fitting itself to the window.

Apple Screen Sharing Standard mode (`subtype = "ard"`, RFB 3.8) fills the Display
menu with physical screens and *All Displays*; choosing one narrows the framebuffer
to that screen's own pixels.
`subtype = "ard-high-performance"` (experimental — see
[apple-vnc-889.md](apple-vnc-889.md)) instead requests one virtual display at the
configured size. It disables the remote physical displays and moves the Mac's
windows to that virtual display. With `resize = true`, viewport reports replace
its mode through Apple dynamic resolution. Apple's client supports up to two
virtual displays; Remotex always requests one. RDP and generic VNC expose one
combined framebuffer, so the menu reads *No Displays to Choose From*.

The checkmark moves only when the Mac confirms the selection in a display layout.
A mixed-density combined framebuffer has no valid scale and is shown at its pixel
size; a selected Retina display uses its reported scale and renders at 100%. See
[`apple-vnc-889.md`](apple-vnc-889.md).

### Viewport measurement

The viewer reports the web view's own bounds, as its frame changes. That is the
whole measurement: a page scrolls inside those bounds and cannot change them, so
the scrollbar-driven resize oscillation the native surface had to discount
cannot arise. Nothing about the viewport goes through the page.

Reports are not sent before initial layout or while the target picker is active.
Each axis is clamped to `1...u16.max`. Starting a new target clears both the
policy and outbound-queue deduplication so the first `connected` event can resend
an already measured viewport.

### Window chrome

The desktop toolbar is hidden while a remote is displayed. The remote surface
stays inside the window's safe area so the title bar never overlaps interactive
remote content. The window remains titled because an untitled window cannot
become key and accept keyboard input; full screen is the chrome-free mode.

## Keyboard and pointer input

While a connected desktop is focused, an AppKit local event monitor consumes
`keyDown`, `keyUp`, and `flagsChanged` before application menu equivalents.
Remote-menu commands therefore have no keyboard shortcuts. macOS-global
shortcuts such as Command-Tab and Command-Space remain local because the
application never receives them.

The monitor is why keyboard input is the one thing that did **not** move to the
canvas page. It sits outside the web view and swallows what it consumes, so
WebKit never sees a key event — which is what lets ⌘Q and ⌘W reach the guest
instead of this application. The page has no keyboard handling in it at all.

The Edit menu remains available for text fields and supplies the standard
copy/paste/cut/select-all actions through the responder chain. `ViewerMenus`
restores it when SwiftUI rebuilds the main menu.

Keyboard translation depends on `remoteOs`:

- for a non-Mac remote, standard Command shortcuts become remote Control
  shortcuts, a bare Command taps remote Meta, and other Command chords remain
  Meta chords;
- for a Mac remote, Command remains remote Meta for every chord.

The default-on **Enable macOS Keyboard Overrides** preference disables the
Command-to-Control translation when turned off. `PressedInput` tracks every
pressed code and releases them on focus loss, window deactivation, target
switch, socket closure, takeover, or teardown.

Pointer input, by contrast, is the page's: it is the only side that knows where
the canvas sits after a scroll, so it maps and clamps positions to remote pixels
itself (`remotePoint` in `frontend/src/viewer/input.ts`) and reports them. A
press is preceded by its own position, so a click never lands where the pointer
used to be. Wheel deltas come from the DOM already signed and already carrying
their `deltaMode`, so nothing converts them.

Before the first `cursor` message the pointer is hidden, because an engine may
already composite its own into the framebuffer. After one, the page wears the
remote shape as a CSS cursor; a null image uses a drawn arrow so the pointer
remains visible. Hotspots arrive in remote pixels and are scaled by whatever the
desktop is currently drawn at.

## Clipboard

For a connected target with `clipboard`, the viewer polls
`NSPasteboard.changeCount`:

- local text changes send the ordinary `clipboard` message;
- unsolicited remote changes write to `NSPasteboard`;
- echo guards prevent either direction from bouncing the same value back.

A response to an explicit **Clipboard…** fetch fills the panel but does not
write to the local pasteboard. The panel's Copy action is the consent boundary.
The synchronizer uses a local request token so a late response cannot populate a
closed or replaced panel.

Clipboard values are capped at 64 KiB in either direction and refused rather
than truncated. Command-V queues the current local pasteboard value before
sending the translated remote paste chord. Programmatic reads follow macOS's
**Paste from Other Apps** permission, while `clipboard = true` remains the
boundary — set per target on the gateway currently in use.

## Audio

**Remote → Enable Audio** is available when `connected.audio` is true. The
gateway owns the wire format and bounded audio queue; the app owns the
*subscription* (`AudioControl`) and the canvas page owns decoding and playback.

Like **Auto Resize**, the toggle writes a **remembered** default — the same value
the picker's *Play the remote's sound, if compatible* toggle edits
(`ViewerPreferences.audioByDefault`). When it is on, a pick or takeover of a
target that carries audio subscribes on its own, without the menu being touched;
a target with no audio starts silent, and a *silent reattach* — a reconnect the
user did not ask for — is left as it was rather than re-seeded, so a mid-session
mute survives a dropped socket. On the macOS side there is no browser-style
gesture requirement, so the subscription is asserted straight from `connected`.

While sound is playing the window title gains a trailing `🔊` (`windowTitle`) —
the one persistent surface that can show it, since the toggle is a menu item. The
browser does the same on its tab title, but at the front, where a truncated-from-
the-right tab title keeps it visible.

Decoding is WebCodecs, through the browser client's own `audioPlayer.ts` and
`audioSchedule.ts` — the same 0.1-second start cushion and 0.3-second latency
ceiling, and the same trim when the ceiling is exceeded. The `audioFormat`
control message is forwarded to the page whole, `OpusHead` included, because that
is where the pre-skip is. Nothing on the Swift side reads the codec: it has no
decoder to choose and would only be a second opinion about one.

`mediaTypesRequiringUserActionForPlayback` is set to nothing, so unlike a browser
tab there is no gesture requirement and the subscription is asserted straight
from `connected`. This is also the one place the secure-context requirement in
[The canvas](#the-canvas) is load-bearing: without it there is no `AudioDecoder`,
and the failure is silence.

An ordinary reconnect reasserts the subscription; a target switch clears it.

A page that cannot play what arrived reports it, which raises the alert and
unsubscribes — packets decoded by nothing are bytes spent on nothing. Neither
side can distinguish a quiet remote from an RDP server that never opens its audio
channel; the gateway log carries that diagnosis.

## Networking

The embedded gateway uses plain HTTP on loopback, and remote gateways may also use
HTTP. Because ATS treats `ws://` as `http://`, the bundle sets
`NSAllowsArbitraryLoads`. That covers the canvas page's own loopback origin too,
which is plain HTTP for the same reason.

Every request, including `/api/config` and the WebSocket upgrade, carries the
credential. `/api/config` is public, but sending the credential uniformly avoids
route-specific authentication assumptions. `require_auth` runs before the upgrade,
so a missing credential produces HTTP 401 before a socket exists.

The headers are not interchangeable:

| gateway | header |
|---|---|
| embedded | `Authorization: Bearer <token>` |
| remote, signed in | `Cookie: remotex_session=<token>` |
| remote, not yet | none. The public routes are what the `home` screen asks |

`httpShouldHandleCookies` is always off. The client manually stores the session
cookie in `viewer.json` (mode `0600`) because `HTTPCookieStorage` drops a `Secure`
cookie when matching it against `wss` rather than `https` behind a TLS-terminating
proxy, and matches hosts without considering the port, which would mix same-host
gateway logins. Manual persistence also keeps the login across app restarts.

On upgrade, the query `session` token owns the slot; the header credential
authorizes the client. They are independent.

`URLSessionWebSocketTask.maximumMessageSize` is set to 16 MiB. Exceeding the
limit ends the socket rather than dropping one frame.

### Local network permission

macOS 15 and later refuse an app's connections to anything off this machine until
local network access is allowed, which covers the embedded gateway: the permission
belongs to the responsible app bundle, and `remotex-gateway` is a child of
`remotex.app`. A fresh install therefore fails on its first target, and the sheet
that asks is the user's to answer.

The refusal is `EHOSTUNREACH`, exactly what an address with no route gives, and
nothing on the gateway side can tell the two apart — there is no API that returns
the permission state, which TN3179 still says in as many words. So the gateway does
not try to. `engine::tcp_connect` adds one clause to the error naming the
permission, on macOS only, and leaves the address standing as the other
possibility. It does not wait, retry, or conclude.

Note for QA: `tccutil reset LocalNetwork` cannot undo a decision — local network
privacy is a Network Extension filter, not TCC — and TN3179 records that macOS
offers no reset. Toggling the app off and on under System Settings > Privacy &
Security > Local Network and relaunching is the practical way back. The row is in
the second, alphabetical group, between **Input Monitoring** and **Microphone**.

## Build and QA

Run the tests, build the packaged app, and launch QA against a throwaway instance:

```sh
(cd frontend && bun run check && bun test src)
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh
open -n dist/remotex.app --args --instance-dir "$PWD/tmp/app-instance"
```

The build script builds the canvas page itself, so `bun` is required for it. The
first line is separate because it is the page's *own* checks — a bundle whose
page builds but whose tile painter is wrong looks fine until pixels land.

`--instance-dir` is the only GUI-launch argument and is the whole of the isolation:
config, log, and preferences are all under the directory it names, so QA cannot
touch `~/Library/Application Support/remotex`. Delete the QA directory for a clean
run. Gateway selection belongs to the `home` screen, so a launcher can isolate an
instance but cannot choose its gateway.

Always validate the packaged `.app`; `swift run`, standalone `swift build`, and
the executable under `.build` bypass bundle menus, `Info.plist` behavior, and the
bundled gateway (`Bundle.main.url(forAuxiliaryExecutable:)` finds nothing, so the app
comes up saying it is incomplete).

For socket-level diagnostics, `--probe` starts the embedded gateway and prints
received control and frame information. It takes no address or credentials — it is a
diagnostic for the embedded path only, and a remote gateway's socket is that
deployment's own to measure:

```sh
dist/remotex.app/Contents/MacOS/remotex-viewer \
  --probe --instance-dir "$PWD/tmp/app-instance" \
  --probe-target mac --probe-seconds 90
```

The bundled gateway is also a full `remotex` binary, which is how an instance's
configuration is checked from a terminal:

```sh
dist/remotex.app/Contents/MacOS/remotex-gateway check-config --embedded \
  --config ~/Library/Application\ Support/remotex/remotex.toml
```

Automated tests cover message decoding, frame parsing, arrival ordering,
geometry, the bridge's JSON both ways, and the loopback listener over a real
socket. What the page does with what it is handed — the slot table, the draw
order, the envelope reassembler, pointer mapping — is checked in `bun test`,
where the code is. Audio playback and anything about pixels still require manual
QA; the Web Inspector is available on the canvas in a debug build.

The in-process tone harness (`cargo test --lib serve_a_test_tone -- --ignored`)
serves a login gateway. Test it in the app by choosing **Somewhere Else**, entering
the printed loopback address, and signing in with the printed credentials. It
checks the client playback path with a scripted engine. To include RDP negotiation,
configure an `audio = true` RDP target; verify start/stop/resume, and use a source
that announces left and right channels when checking stereo order.

See
[`packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for signing, packaging, permissions, and development launch details.
