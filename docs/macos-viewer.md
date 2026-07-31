# remotex.app

`remotex.app` is a native macOS 26 client that **carries** its own gateway and can
also be pointed at one. It owns target selection, session recovery, tile decoding,
Metal rendering, input, clipboard synchronization, and audio playback; it contains no
`WKWebView` and shares protocol behavior, not implementation, with the browser client.

The two gateways are the app's first question, asked on the `home` screen at every
launch with the last answer preselected:

| | the embedded gateway | a remote gateway |
|---|---|---|
| where it runs | in this bundle, started at launch | wherever it was installed |
| address | an ephemeral loopback port it picks | typed, and remembered once it answers |
| credential | a bearer token nobody types | a username and password, once per session |
| who reaches the target | this Mac | that gateway |
| **Configuration…** | edits this instance's config | not shown — it is that gateway's |

Neither is a default the app can pick for somebody. The embedded gateway is the right
answer for targets this Mac can reach directly — nothing to install, nothing to sign
in to. It is the wrong one when the link to the target is slow, because then every
tile crosses that link and the gateway belongs at the far end with this app talking to
*it*. Only the user knows which case they are in.

Below the choice, the two are the same program: `/api/config`, `/api/targets`,
`/api/session` and `/ws` are one route table with one set of shapes. The single
difference is which header carries the credential — see `GatewayCredential`.

The client has no RDP or VNC implementation. Engine-specific behavior is
reported by the gateway, with resize policy as the only client-side branch.
Whether the remote is a Mac is likewise discovered from the gateway's
`remoteOs` message and affects only keyboard conventions.

The bundle holds two executables:

| path | what it is |
|---|---|
| `Contents/MacOS/remotex-viewer` | the Swift app (`CFBundleExecutable`) |
| `Contents/MacOS/remotex-gateway` | a copy of the `remotex` gateway binary |

Two files in one directory cannot share a name, and the suffix also makes it obvious
which process is which in Activity Monitor and to `pgrep`. `CFBundleIdentifier` stays
`dev.remotex.viewer`, because TCC grants and saved window state are keyed on it.

## The embedded gateway

`remotex serve-embedded --instance-dir <dir>` is not a deployment: it serves the one
client that started it and dies with it. Everything a deployment would configure is
therefore decided in code — see `src/embedded.rs` and `config::Audience`:

| | |
|---|---|
| address | `127.0.0.1`, one socket |
| port | `0`, read back off the socket after binding |
| web UI | none. No `ServeDir`, no index handler; `/` is a 404 |
| login | refused. `/api/auth/*` answers 403 |
| credential | one bearer token, minted per launch, held only in memory |

### The two pipes

Opposite directions, unrelated jobs.

**The child's stdout → the app, once.** One JSON line, printed after the socket is
bound so the port in it is a fact rather than an intention:

```json
{"port":49213,"token":"…"}
```

That is how the app learns both the port and the token. stdout carries nothing else
(logging goes to stderr), and the pipe's read end belongs to the app alone — so the
token is not in `argv`, where `ps` would show it to every process, not in the
environment, inherited by anything either side spawns, and not in a file, which would
outlive the process that made it.

**The app → the child's stdin, never.** Nothing is written on it in either direction.
The app holds the write end for as long as it lives; when it ends, the kernel closes
that end, the gateway's blocking read returns end-of-file, and the gateway exits.

### Shutdown, in three layers

1. **The liveness pipe** is what the guarantee rests on. It fires whether the app
   quit cleanly, crashed, was Force Quit or took a `kill -9`, because no code of ours
   has to run for the kernel to close a descriptor. macOS has no `PR_SET_PDEATHSIG`,
   and unlike a `getppid` poll this leaves no window in which an orphan is still
   listening. `aGatewayIgnoringSignalsStillDiesWithThePipe` asserts it against a
   child that traps `SIGTERM`.
2. `SIGTERM`, the ordinary graceful stop.
3. `applicationWillTerminate` closes the pipe and terminates the child, then kills it
   after a grace period. Synchronous, because the process may be gone the moment it
   returns.

### The instance directory

`--instance-dir <path>`, or `~/Library/Application Support/<CFBundleName>` (mode
`0700`) — `remotex` for the shipped bundle, and its own name for a variant stamped out
by `make-instance-bundle.sh`, which is what lets a second bundle be a second
installation with no argument to pass. Everything this launch reads or writes is under
it, and **nothing under `/opt/remotex` is ever consulted** — a Mac can run the server
install and this app at once without either changing what the other does.

| file | |
|---|---|
| `remotex.toml` | the only thing a user edits, mode `0600` |
| `gateway.log` | the gateway's stderr, appended across launches |
| `viewer.json` | client preferences |

Preferences live here rather than in a `UserDefaults` suite because the directory is
the unit of isolation: a suite lives in the user's own `Preferences` whatever the rest
of the app was told, which is the trap the old `--settings` flag existed to work
around.

A first launch writes a commented template with no targets, which is a valid
configuration for this app — the picker simply says there is nothing to connect to
yet.

### Configuration

**Remote › Configuration…** edits `remotex.toml` in a sheet: the TOML in a monospaced
editor, Reveal in Finder, Cancel, and Save.

### About

**remotex › About remotex** — named after the instance's `branding`, so a second
instance's item says its own name — states the three things that identify a running
app, none of which is visible anywhere else:

- **the version**, from `CFBundleShortVersionString`, plus the wire protocol number
  the gateway is checked against;
- **the instance directory**, with Reveal in Finder.

It exists as an explicit item because `commandsReplaced` takes the standard
application menu down whole (see `RemoteCommands`): About is not restored unless it is
put back, and until it was, a running app could not say which build it was.

Save validates first, by running the bundled gateway's `check-config --embedded` on
the candidate text. So what the editor accepts is by construction what the gateway
starts on, and there is no second idea of what a config means: a refusal keeps the
sheet open with the text intact, shows the gateway's own complaint verbatim, and
writes **nothing**. A clean save writes atomically and restarts the gateway, so a new
target is in the picker by the time the sheet closes.

`[server]` is refused in this file, and having no targets at all is not an error — it
is what a first launch has, and the picker says so in words.

A top-level `branding` names the instance: the heading above the target list, the
window title and the launch screen. It is the one key this file shares with a served
gateway's, and the only place either sets it — `[server].branding` no longer exists,
because a key in that block could not name an app whose config has no such block.

## Running more than one instance

**A second instance is a second app.** One command stamps it out:

```sh
packaging/macos-viewer/make-instance-bundle.sh remotex-work ~/Pictures/work.png
```

That writes `~/Applications/remotex-work.app` — a copy of `remotex.app` with its own
`CFBundleIdentifier`, its own `CFBundleName` and its own icon, ad-hoc re-signed. Double
-click it. There is no flag to pass, no launcher to keep, and nothing to remember.

### Why a whole bundle, and not a launcher

Because the argument has nowhere to come from. LaunchServices hands a double-clicked
app no arguments, and `open` without `-n` reactivates the running copy and *silently
discards* `--args` — so `--instance-dir` can only arrive via a wrapper that shells out,
and then the thing in the Dock is remotex rather than the instance. That is the trap the
Chrome `--user-data-dir` launchers hit.

So the instance directory is read from the bundle instead:
`~/Library/Application Support/<CFBundleName>` (`InstanceDirectory.defaultURL`). The
shipped bundle is named `remotex`, so its instance is exactly where it always was, and a
variant named `remotex-work` gets its own with nothing passed to it.

This is what Chrome's **Create Shortcut** does — a separate bundle per profile, with its
own identifier and icon, in `~/Applications` — except that Chrome ships a few-KB
`app_mode_loader` shim that talks to the one installed browser, because duplicating 200
MB per shortcut would be absurd. remotex.app is 13 MB, so a plain copy is cheaper than
the machinery to avoid one.

`--instance-dir` remains, and is now what it should always have been: the override a QA
run uses to point the *stock* bundle at a throwaway directory.

### What a variant costs

- **13 MB, and it goes stale.** The copy carries its own client and gateway binaries, so
  it keeps running the build it was stamped from. Re-run the script after updating
  `remotex.app`; it replaces the bundle, keeps the name and icon, and never touches the
  instance directory.
- **Nothing else.** No entitlements, no notarization, no TCC. The shipped bundle is
  ad-hoc signed itself (`codesign -dv` → `Signature=adhoc`), and the viewer holds no TCC
  grants for a change of code identity to break: it captures keys with a *local*
  `NSEvent` monitor, which needs no Accessibility permission. That is all the **agent**,
  which is a different program with a different problem.

What you must still **not** do is edit `remotex.app` in place. The script copies first
and re-signs the copy, which is a different operation from breaking the seal on the one
Apple's installer put there.

### Naming the window too

The bundle name reaches the Dock, ⌘-Tab and the menu bar. The *window* is named by the
config:

```toml
branding = "Work"
```

That reaches the window title, the picker heading and the launch screen. Worth setting
as well — one names the app, the other names what is on screen.

### What a new instance costs

- **Its own configuration.** A fresh directory starts with an empty targets list, so
  each instance is configured independently — one instance being set up says nothing
  about another.
- **Nothing else.** The gateways pick their own ports, and neither instance can see
  the other's config, log or preferences.

## Protocol compatibility

The check survives even though both halves now ship in one bundle, and it means
something different: a mismatch is a broken build rather than an old server, which is
exactly the kind of thing that must not present as a hang. Before opening a session
the client requests `GET /api/config` and requires its `protocolVersion` to match
`PROTOCOL_VERSION` in `src/protocol.rs`; a mismatch is reported on the launch screen.

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

`ViewerScreen` is `home`, `login`, `launching`, `picker`, `desktop`. The first is the
gateway choice above; the branches meet again at `picker`.

**The embedded branch** goes `home` → `launching` → `picker`. `launching` asks for
nothing: it shows a spinner, or the reason the gateway did not start with the
gateway's own stderr beneath it and **Configuration…** / **Change Gateway…** /
**Try Again** to act on it. A gateway that exits while the app is using it lands on
the same screen — deliberately not restarted automatically, since one that died on a
config it accepted will die again, and a silent retry loop would hide the output that
explains why.

**The remote branch** goes `home` → `login` → `picker`, and skips `login` when the
stored session cookie is still one the gateway knows. The address is validated on
`home` — reachable, and speaking a protocol version this build can — so a failure on
`login` can only be about the credentials. A gateway that answers `403` from
`/api/auth/status` is an embedded one somebody typed the address of; it is refused by
name rather than shown a login form that could never succeed.

**Change Gateway…** is the way back to `home` from anywhere, and it is also the log
out: a remote gateway is told, so its login and its session slot are released rather
than left for the reattach grace period.

Authentication and session ownership stay separate, as before:

- the credential authorizes this client to the gateway;
- the claim token owns the program's one active session slot.

What a `401` means is the one thing that differs between the branches after a session
has started. On a remote gateway the login has expired or been ended elsewhere, so the
`login` screen comes back and the stored cookie is dropped. On the embedded one there
is no login to offer: the token is good for as long as the process that minted it
lives, so a `401` means the gateway behind that port is not the one that issued it, and
the app restarts the gateway instead — once, then it shows the launch screen rather
than looping.

`SessionStateMachine` implements claim, attach, reconnect and takeover as a pure state
machine. Network reconnects use capped exponential backoff up to 15 seconds. A busy
slot and a session taken over by another client wait for an explicit user decision
because resolving either case may evict the current owner.

The reconnect backoff resets after a control message proves that the connection
is usable, not merely when the WebSocket opens. Any interruption clears the
framebuffer and releases held input; the gateway requests a full repaint when a
client attaches again.

## Rendering

The viewer maintains one `MTLTexture` at the remote framebuffer's pixel size.
Tiles overwrite their rectangles with `replaceRegion`, and a paused `MTKView`
redraws after every complete batch.

The gateway may replace a tile payload with a reference to one of 256 cache
slots. The viewer stores encoded PNG/JPEG payloads in those slots and re-decodes a
payload when it is referenced. The gateway chooses which slot to overwrite. A
missing slot or a cached payload that cannot decode sends `cacheReset` and drops
that record.

Frames are processed strictly in arrival order. The socket loop completes every
decode and upload for one frame before receiving the next, so a resize cannot
overtake tiles in the preceding coordinate space and older tiles cannot land
over newer ones.

`TileDecoder` leaves decoded rows in raster order. The Metal shader flips the
texture's vertical coordinate because Metal clip space and the desktop texture
use opposite vertical origins.

The framebuffer view is laid out at the remote's point size:

```text
point size = framebuffer pixels / remote backing scale
```

The window's screen then rasterizes that view at its own backing scale. A remote
larger than the available area scrolls; the viewer does not zoom or fit the
framebuffer to the window.

## Display and resize behavior

`ViewportPolicy` separates two questions: whether this session may resize the
remote, which the gateway answers, and whether the window drives it, which this
viewer does.

Permission comes from the `connected` message:

| Target | May resize |
|---|---|
| RDP or VNC with `resize` | yes |
| Any other case | no — no viewport request is ever sent |

The three View menu items are one decision:

- **Auto Resize** hands the remote's size to the window, which then follows every
  change, debounced and deduplicated. Off until set, and then **remembered**: it
  is one value with two editors — this menu item and the picker's *Auto-resize the
  remote to the window, if compatible* — so a choice made mid-session is the one
  the next connection starts from. It is applied to a new connection only where the
  target allows resize; where it does not, the remembered default silently does
  nothing, which is the picker caption's "if compatible". See
  `ViewerPreferences.autoResizeByDefault`.

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

**No engine fills the Display menu today.** RDP, plain VNC and both Mac subtypes each
expose a single framebuffer and send no display list, so the menu holds one disabled
item reading *No Displays to Choose From* and never anything else.

Picking one of a Mac's screens was attempted through Apple's own protocol revision
(`subtype = "ard-high-performance"`) and does not work: macOS replaces the Mac's real
displays with one synthesized display for the duration of such a session, so there is
nothing to enumerate. `subtype = "ard"` shares every real screen, in one framebuffer.
The `selectDisplay` / `Displays` wire and this menu remain in place, and the gateway
side is implemented and tested, waiting on a mechanism that reports a list — see
[`apple-vnc-889.md`](apple-vnc-889.md) and [`roadmap.md`](roadmap.md).

### Viewport measurement

The viewer observes the scroll view's `frameDidChange` and reports the scroll
view's size. It does not use `NSClipView.boundsDidChange`, which represents
scrolling rather than window resizing, or the clip view's size, which changes
when legacy scrollbars appear and can cause resize oscillation.

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

Before the first `cursor` message, the viewer hides the local pointer because an
engine may already composite its cursor into the framebuffer. After a cursor
message, the viewer renders the remote shape; a null image uses a local arrow so
the pointer remains visible. Hotspots arrive in remote pixels and are converted
to points.

AppKit scroll deltas are inverted on both axes to match DOM wheel direction.
Trackpad deltas pass through directly; line-based wheel deltas are scaled for the
gateway's line conversion.

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
boundary — set per target in the app's own configuration.

## Audio

**Remote → Enable Audio** is available when `connected.audio` is true. The
gateway owns the wire format and bounded audio queue; the viewer owns decoding
and playback scheduling.

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

The viewer decodes bare Opus packets with `AVAudioConverter` and
`kAudioFormatOpus`; it needs neither a container nor a vendored decoder.
`AVAudioConverter` does not apply the `OpusHead` pre-skip, so `OpusDecoder`
discards the reported priming frames itself.

Decoded buffers are scheduled explicitly on an `AVAudioPlayerNode`.
`AudioSchedule` uses the same 0.1-second start cushion and 0.3-second latency
ceiling as the browser. When the ceiling is exceeded, the viewer stops the
player, discards its queued audio, and restarts the timeline at the cushion.

An ordinary reconnect reasserts the subscription; a target switch clears it.
The output follows the Mac's default device, rebuilding the engine after
`AVAudioEngineConfigurationChange`.

The viewer can report local decoder or output failures. It cannot distinguish a
quiet remote from an RDP server that never opens its audio channel; the gateway
log carries that diagnosis.

## Networking

The embedded gateway is reached over plain HTTP on loopback, and a remote one may be
plain HTTP too; ATS treats `ws://` as `http://`, so the bundle uses
`NSAllowsArbitraryLoads`.

Every request carries this client's credential, including `/api/config`, which needs
none: a client that authenticates only the routes it believes are guarded is one route
away from a 401 nobody expected. The WebSocket upgrade carries it too — `require_auth`
runs before the upgrade, so omitting it is a bare 401 rather than a socket that closes
with a reason.

Which header depends on the gateway, and they are not interchangeable — `require_auth`
reads the cookie on a login gateway and the bearer on a token one, and neither looks at
the other:

| gateway | header |
|---|---|
| embedded | `Authorization: Bearer <token>` |
| remote, signed in | `Cookie: remotex_session=<token>` |
| remote, not yet | none. The public routes are what the `home` screen asks |

`httpShouldHandleCookies` is off everywhere, on both. The session cookie is held by
the client and set by hand, never by `HTTPCookieStorage`, for two reasons: that storage
matches a `Secure` cookie only against an `https` scheme, and behind a TLS-terminating
proxy the gateway does set `Secure` while the socket's scheme is `wss` — so the cookie
would be dropped for a 401 with nothing to explain it; and it matches by host while
**ignoring the port**, so two instances against two gateways on one host would share
one login and each would log the other out. The cookie is stored in the instance's
`viewer.json` (mode `0600`), which is what makes quitting the app not mean typing the
password again.

Two different tokens meet on the upgrade and are not interchangeable: the query's
`session` is the claim, deciding whose turn it is, and the header's is this client's
credential, deciding whether it may ask at all.

`URLSessionWebSocketTask.maximumMessageSize` is set to 16 MiB. Exceeding the
limit ends the socket rather than dropping one frame.

## Build and QA

Run the tests, build the packaged app, and launch QA against a throwaway instance:

```sh
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh --no-dmg
open -n dist/remotex.app --args --instance-dir "$PWD/tmp/app-instance"
```

`--instance-dir` is the only argument the app takes, and it is the whole of the
isolation: config, log and preferences are all under the directory it names, so a QA
run cannot touch what a real one keeps in
`~/Library/Application Support/remotex`. Clear the slate by deleting the directory.

`--instance-dir` remains the only argument the app takes. Pointing it at another
gateway is a UI decision, not a command-line one: type the address on the `home`
screen. There is deliberately no `--gateway` flag — an address that a launcher can
pass is one an instance can be silently launched with, and the instance directory is
what isolation is built on here.

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

Automated tests cover message decoding, frame parsing, tile ordering, geometry,
audio framing, schedule arithmetic, and Opus fixtures produced by the gateway.
Audio playback still requires manual QA.

The in-process tone harness (`cargo test --lib serve_a_test_tone -- --ignored`) is no
longer reachable from this app: it serves a *login* gateway on a fixed port, and the
app can only talk to the one it started for itself. Verify the harness in a browser;
for the app, configure an `audio = true` RDP target in its own configuration and use
that. Enable audio during a quiet phase and verify that the tone starts, stops, and
returns without another action. Use a source that announces its left and right
channels when checking stereo order.

See
[`packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for signing, packaging, permissions, and development launch details.
