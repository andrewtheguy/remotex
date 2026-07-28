# macOS viewer

`remotex-viewer.app` is a macOS 26 client that speaks the gateway's HTTP and
WebSocket protocol directly. There is no `WKWebView` and no web content: the
viewer owns login, target selection, the session socket, tile decoding,
framebuffer rendering, pointer and keyboard input, and `NSPasteboard`.

It shares the *protocol* with the SPA, not an implementation. The SPA remains the
browser client and is unaffected by anything here.

There is no RDP, VNC, or RXA branch in the viewer beyond one thing: how the
remote can be resized (see [Resize](#resize)). Everything else is derived from
the gateway's control messages, so a difference in behaviour belongs to an engine
adapter and is tested as backend conformance.

Whether the remote is a Mac is discovered, not configured. Each engine settles it
as it connects and sends `{"type":"remoteOs","macos":…}`: `rxa` is macOS by
construction, `rdp` never is, and `vnc` reads it off the RFB handshake (Apple's
Screen Sharing announces protocol revision 003.889 and offers Apple's security
types). The viewer acts on that one bit and nothing else — a third-party VNC
server on a Mac reads as not-macOS, which costs a keyboard convention, not
correctness.

## Protocol and compatibility

The viewer ships as its own artifact, so it can be older or newer than the
gateway it is pointed at. `GET /api/config` carries a `protocolVersion`
(`PROTOCOL_VERSION` in `src/protocol.rs`), which the viewer checks before opening
a session and refuses on mismatch, with the reason shown on the login screen.
`ProductInfoTests` pins the Swift constant against the Rust one so the two cannot
drift silently.

`PROTOCOL_VERSION` covers `ClientMsg`, `ControlMsg`, and the tile frame layout. A
purely additive control message does not earn a bump: clients are required to
ignore tags they do not know, and the viewer does (`ServerMessage.unsupported`).

Two contract tests guard the boundary from the other side.
`WireContractTests` reads both message enums out of `src/protocol.rs` and
compares them against the tags the viewer handles, so a message added in Rust
fails a Swift test rather than becoming a silently skipped frame. The
`ServerMessage` decoding tests reuse the exact JSON literals that
`src/protocol.rs`'s own tests pin.

## Getting in

Two steps, because they answer different questions.

**Server.** An address, and a **Continue** button that validates it: parse, then
`GET /api/config` for the branding and the protocol check, then
`GET /api/auth/status`. Nothing is contacted before that button — reaching a
gateway is something the user asks for and gets an answer to, not something that
happens behind a launch spinner. A malformed address is refused without a request
at all. The address is remembered only once it has answered, so a typo does not
become what the next launch starts from.

**Login.** Credentials only. The gateway was already validated, so a failure here
can only be about who you are, which is why the address is shown but not editable
— **Change** goes back to step one. Logging out returns here rather than to the
server step: it is the credentials being given up, not the address.

This step is also the *only* place the address can be changed, and that link is
the only way to it — a **Change Gateway** menu item was enabled on this screen and
nowhere else, which made it a second name for a button already on screen.
Everything past this step belongs to one gateway — the login cookie is scoped to
that host, the claim token was minted by it, the socket is attached to it — so
changing the address from the picker or the desktop was a log out that did not say
so, and `changeGateway` refuses from anywhere but here. From there the step to
take first is Log Out, which lands back here.

Step two is skipped when the cookie is still good, so the common case is one
button. `HTTPCookieStorage.shared` outlives the app; the gateway's auth sessions
do not outlive *it*, so its restart ends them and any 401 drops back to login
rather than into a retry loop.

## Session lifecycle

Two independent things: the **login cookie** (may I use this gateway) and the
**claim token** (do I own the one session slot).

`SessionStateMachine` is the claim/attach/reconnect lifecycle as a pure value,
transcribed from the web client's. Reconnects are automatic with capped backoff
(`min(1000·2^n, 15000)` ms); `busy` (a claim answered 409) and `takenOver` (close
4001) wait for the user, because resolving either evicts whoever is on the
desktop now. Close 4000 and an ordinary drop take the same path, since the answer
to both is to claim again.

One rule is easy to get wrong: the backoff resets on **any control message**, not
on the socket opening. A slot that accepts the upgrade and drops it immediately
would otherwise retry at full speed forever.

Every interruption clears the framebuffer and releases held input. Clearing is
cheap to do because the gateway repaints in full whenever a client attaches.

## Rendering

One `MTLTexture` at exactly the remote's size, with the drawable pinned to the
same size, so nothing is scaled in the renderer. Tiles are written in with
`replaceRegion`; there is no delta encoding, so each tile overwrites its
rectangle outright. A remote larger than the window scrolls; it is never scaled to
fit, and zoom is out of scope.

The framebuffer *view* is laid out at the remote's own point size — its pixels
over the density `resize` reports — so the layer rasterizes the drawable for
whichever display the window is on. The picture is therefore scaled by the ratio
between the remote's density and the host display's, automatically and in both
directions:

| guest | host | result |
|---|---|---|
| 1x | Retina | magnified 2x, soft — there are no more pixels to have |
| Retina | 1x | reduced to half, downsampled and sharp |
| equal | equal | one texel per device pixel, nothing resampled |

Dragging the window between a Retina display and a 1x one switches between those
by itself: the desktop keeps its physical size, nothing is re-derived, and
`layer.contentsScale` is the only thing that changes. Laying the view out in the
host's backing scale instead is what would break this — a 1x remote would come out
at half its physical size on a Retina Mac rather than magnified.

Two ordering facts the port depends on, both covered by tests because neither can
be inferred by reading:

- `TileDecoder` does **not** flip the image. A bitmap context's memory is raster
  order, so the usual translate/scale flip inverts the buffer rather than
  correcting it.
- The shader flips `uv.y`, because clip space grows upward while the texture's
  row 0 is the top of the desktop.

Neither can be corrected downstream: a strip is placed into the texture by its
own `y`, so an inverted buffer or sampler puts every band in the wrong place
rather than turning the picture upside down.

Frames are handled strictly in arrival order. One loop reads the socket and fully
handles each frame — including awaiting the tile decode — before asking for the
next. Parallelising the decode would let a `resize` overtake the tiles queued
behind it and blit stale pixels into a freshly allocated texture.

## Resize

Three behaviours, chosen from the `connected` message. `ViewportPolicy` holds all
three so they cannot spread into the model as protocol checks.

| target | behaviour |
|---|---|
| `vnc` | follows the window continuously, debounced and deduped |
| `rdp` with `resize` | only on **Remote → Resize to Window**; a resize forces a Deactivation-Reactivation |
| `rxa` with `resize` | only on **Remote → Resize to Window**, and only while the display being shared is one the agent *made* |
| `rxa` otherwise | sends nothing, ever |

There is no resolution menu, for any target: a remote's resolution belongs to the
machine running it, and the rows above are the cases where that machine hands the
decision over. `rxa` is the narrowest, and the only one whose answer changes
mid-session — a Mac's own panel is never resized from here, so switching displays
from **Remote → Display** turns the item on and off. `ViewportPolicy` starts an
rxa session ignoring viewports and learns the rest from the `displays` list; see
`docs/mac-agent-architecture.md`.

A size mismatch has two directions, and **Remote → Resize to Window** is only one
of them. Below it, **Resize to Display** takes the other: the window is sized so
the desktop fits it exactly, and nothing goes on the wire. Which of the two is the
one that can move is decided by a single fact — a target that takes a size from
here gets the first, and every other target gets the second. For an rxa target
with `resize` that fact is answered per display, so the pair flips as the user
switches between the Mac's own screens and the agent's. Neither is
enabled before there is something to act on. **Resize to Display** waits for the
desktop to have a remote size to fit the window to, and **Resize to Window** for a
measured viewport to report; until then both are greyed, and after that exactly one
is. Both stay in the menu either way, one greyed, because which direction a target
allows is worth reading off the pair rather than inferring from an item that is not
there. The arithmetic is `RemoteGeometry.windowFrame`, taken as a delta on
the room the scroll view gives the document so the title bar and insets need no
accounting, anchored at the top-left, and held inside the screen's visible frame —
a 1x 3840×2160 remote is 3840×2160 points, and the answer there is the largest
window that fits with the scrollbars that implies. A full-screen window is left
alone.

## The window's chrome, and the strip above the desktop

While a desktop is showing, the toolbar gives way to it. In a window that is worth
8pt; in full screen it is the whole strip, because macOS keeps the title bar pinned
for as long as a toolbar is shown and auto-hides it as soon as none is — so a
full-screen desktop reaches the top of the screen, and the chrome returns on a trip
to the top edge. View Only and Clipboard are on the **Remote** menu as well as the
toolbar, which is what makes the toolbar's copies expendable.

The remote surface sits *inside* the safe area. Spanning the window instead put
40pt of black scroll-view background behind the title bar: it reads as part of the
picture and is not one, because a title bar drags the window and hands the content
nothing — clicks aimed at a guest's own menu bar landed in it and did nothing,
while a *drag* there moved the window. The browser has no chrome over its canvas,
which is why it never showed this.

In a window the title bar's own 32pt cannot be given back. Reclaiming it means
dropping `.titled`, and a window without it **cannot become the key window**
(measured: `canBecomeKey` is false even with `.resizable`), which would leave the
viewer unable to take a keystroke. Full screen is the answer for a remote whose top
edge matters; a smaller remote resolution, centred with margin, is the other.

The **Display** menu is not one. It lists the remote's screens, one checkable
item each, and picking one sends a `selectDisplay` — which screen to look at,
never what size it should be. Only `rxa` fills it: RDP and VNC each deliver a
single framebuffer spanning every remote screen, so for those the menu carries
one disabled item saying there is nothing to choose from. It stays in the bar
either way, because a menu bar whose items come and go is harder to learn than
one item that is sometimes greyed. The viewer holds no display state: the
checkmark follows the `active` in the gateway's `displays` message, so a
selection the remote refused leaves the menu agreeing with the canvas.

Viewport reports are clamped into `u16` before they are sent. The gateway
*rejects* an out-of-range value rather than clamping it, and only logs the
rejection, so an unclamped report would silently stop resizing anything.

What gets measured, and when, is two AppKit facts that read backwards:

- The trigger is the **scroll view's** `frameDidChange`. `NSClipView`'s
  `boundsDidChange` sounds like the right signal and is not: it fires on a
  *scroll*, where the origin moves and no size changes, and stays silent through a
  window resize, where its frame changes and its bounds size follows. Watching it
  meant a VNC target never followed the window at all.
- The measurement is the **scroll view's** size, not the clip view's. A legacy
  scroller — the style macOS uses once a mouse is attached — takes 17pt off the
  clip view when the remote overflows. Reporting that would resize the remote
  smaller, hide the scrollers, report the full size again, and flip between the
  two forever. The stable alternative is a desktop up to 17pt wider than the
  visible area, which scrolls.

Nothing is reported before the first layout. `RemoteGeometry` floors a report at
1 because the gateway rejects a zero, and an engine that follows the window would
take that literally. Nothing is reported from the picker either: the surface
exists there — the framebuffer has to survive a trip to the picker and back — but
there is no engine to resize.

The report that sizes a freshly started engine is the one from `connected`, and it
necessarily repeats a size already measured, so **both** dedupes have to be
cleared there: `ViewportPolicy`'s, and the queue's. The queue's is otherwise reset
only on a new socket, and a target switch keeps the socket it has — so without it
the second target of a session never resized to the window.

## Keyboard

An AppKit local event monitor consumes `keyDown`, `keyUp`, and `flagsChanged`
before application menu equivalents, while a connected desktop is painting. A
`keyDown` override would not do: the menu bar consumes key equivalents before the
responder chain, so Command chords would never reach the remote. macOS virtual
keycodes map to the same physical DOM `code` values the protocol uses.

It cuts the other way too, and decides what the **Remote** menu may carry: while
the desktop is painting and focused, every Command chord AppKit delivers goes
to the remote, so a key equivalent on one of those items fires only
on the screens where nothing is captured — and types into the guest on the one
where the item usually matters. No item on that menu carries one, by rule. The
four that did (Refresh, Log Out, Change Gateway, Connect to Gateway) were there to
drive the app from the keyboard in a test, which is not reason enough to ship a
chord whose meaning depends on which screen is up. The picker's own ⌘1…⌘9 target
picks stay: nothing is captured there, and they are printed on the rows.

The **Edit** menu is the one exemption, and `ViewerMenus` builds it rather than
SwiftUI: Command-C, Command-V, Command-X and Command-A are not built into
`NSTextField` or `NSTextView` — on macOS they are Edit menu key equivalents, and
the responder chain is only offered `copy:`/`paste:` because a menu item sent
them. Stripping the standard menus took the whole mechanism with it, so every text
field in the app answered Command-V with a beep. It does not reopen the problem
the rule is about: a focused desktop takes the chord in the monitor before the
menu bar is offered it, and with no text field in the responder chain the item is
disabled. The sweep skips this menu by object identity, not by title.

Installing it is not a launch-time step, and looked like one until it was measured:
SwiftUI rebuilds the whole menu bar from its own model of it when the first window
comes up, and a menu this app inserted is not in that model. The bar carried Edit
for about a second after launch and then went back to `View` in its place, so the
fix for the beeping read as no change at all. `ViewerMenus.ensureEditMenu` puts the
menu back whenever the bar no longer holds the one the delegate is holding, off the
same change notifications the sweep runs from — the bar the app hands out is not
the last one it gets, and both rules have to outlive a rebuild.

For a non-Mac remote, standard Mac Command shortcuts map to remote Control
shortcuts. A bare Command taps remote Meta, and other Command chords are sent as
remote Meta chords. For a Mac remote, Command remains Meta for every chord, so
Command-V arrives as Command-V rather than Control-V. Which applies follows the
discovered `remoteOs` bit. The default-on **Enable macOS Keyboard Overrides**
item in the **Remote** menu disables Command shortcut translation globally — also
the fix if a Mac is ever not recognised as one.

The protocol has no release-everything message, so `PressedInput` tracks what is
held and sends one release per code. Focus loss, window deactivation, a target
switch, a socket close, a takeover, and teardown all go through that one path;
`SessionStateMachine` emits it as an action rather than leaving it to call sites.
macOS-global shortcuts that never reach the application, including Command-Tab
and Command-Space, remain local.

## Pointer

Receiving any `cursor` message means the viewer owns pointer rendering from then
on. Until one arrives the local pointer is a transparent cursor rect, because an
engine that sends none is compositing its own pointer into the framebuffer and
two pointers are worse than one. A `cursor` with a null image means the remote
hid its shape, and a plain arrow stands in — on a remote desktop an invisible
pointer is worse than a generic one.

Hotspots arrive in remote pixels and are divided into points. The black margin
around a remote smaller than the window keeps the ordinary arrow.

Scroll deltas invert on both axes: AppKit is positive-up, DOM `deltaY` is
positive-down. Trackpad deltas pass through as the point-like values a browser
reports; a notched wheel's line deltas are scaled by 100, which is what the Mac
agent divides by to recover one scroll line.

## Clipboard

While a connected target advertises `clipboard`, native code polls the general
pasteboard's change count:

- a local text change sends the ordinary `clipboard` message;
- an unsolicited remote push writes to `NSPasteboard`;
- echo guards keep either direction from bouncing the same value back.

`requested` is the consent boundary and is load-bearing. The reply to a
**Clipboard…** fetch fills the panel and never touches `NSPasteboard`; only an
unsolicited push mirrors. Copy is how the user opts in. The wire carries no
request id — the synchronizer mints its own so a reply arriving after a close or
a second fetch cannot land in the wrong panel.

The ceiling is 64 KiB in either direction, refused rather than truncated: the
first 64 KiB of a copy could not be told from all of it, so an oversized remote
clipboard is reported as its size instead.

Command-V queues the current pasteboard value before the translated remote
Control-V events.

Programmatic pasteboard reads follow macOS's Paste from Other Apps permission.
The target's `clipboard = true` remains the server-side security boundary.

## Networking

Plain HTTP is allowed. A gateway is commonly reached directly over a private
network, so `Info.plist` carries `NSAllowsArbitraryLoads` — not the
`…InWebContent` variant, which only ever exempted WebKit, and ATS treats `ws://`
exactly as it treats `http://`.

The login cookie is attached to the `/ws` upgrade by hand, with
`httpShouldHandleCookies` off. `HTTPCookieStorage` matches a `Secure` cookie only
against an `https` scheme, and behind a TLS-terminating proxy the gateway does set
`Secure` while the socket URL's scheme is `wss` — so relying on implicit
attachment drops it. `require_auth` runs before the upgrade, so the symptom would
be a bare 401.

`maximumMessageSize` is raised to 16 MiB. The default is 1 MiB and going past it
fails the whole socket rather than dropping one frame. Measured with `--probe`, a
3204×1758 rxa desktop's largest strip was around 100 KB, so this is headroom for
a wider or worse-compressing desktop rather than a fix for something observed.

## Build and test

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
open -n dist/remotex-viewer.app
```

`--probe` attaches to a gateway from the command line and prints what arrives,
which is how the two socket-level assumptions above were settled:

```sh
REMOTEX_PROBE_USERNAME=… REMOTEX_PROBE_PASSWORD=… \
  dist/remotex-viewer.app/Contents/MacOS/remotex-viewer \
  --probe --gateway http://127.0.0.1:52380 --probe-target mac --probe-seconds 90
```

Idling past 60 seconds is the check that `URLSessionWebSocketTask` answers the
gateway's protocol pings — it does — since the gateway kills the engine after
that long without a pong.

See [`../packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for installation, signing, permissions, and development launch arguments.
