# remotex.app

`remotex.app` is a native macOS 26 client that carries its own gateway. It starts
that gateway on an ephemeral loopback port at launch, authenticates to it with a
token nobody types, and shuts it down when the app quits — so there is no server to
install, no address to enter, and no login. It owns target selection, session
recovery, tile decoding, Metal rendering, input, clipboard synchronization, and
audio playback; it contains no `WKWebView` and shares protocol behavior, not
implementation, with the browser client.

The client has no RDP, VNC, or RXA implementation. Engine-specific behavior is
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

`--instance-dir <path>`, or `~/Library/Application Support/remotex` (mode `0700`).
Everything this launch reads or writes is under it, and **nothing under
`/opt/remotex` is ever consulted** — a Mac can run the server install and this app at
once without either changing what the other does.

| file | |
|---|---|
| `remotex.toml` | the only thing a user edits, mode `0600` |
| `gateway.log` | the gateway's stderr, appended across launches |
| `viewer.json` | client preferences |

Preferences live here rather than in a `UserDefaults` suite because the directory is
the unit of isolation: a suite lives in the user's own `Preferences` whatever the rest
of the app was told, which is the trap the old `--settings` flag existed to work
around.

A first launch writes a commented template and mints this instance's `[rxa]`
`private_key`. That key is not a choice — it is the gateway's name on the wire, and
without it the one protocol written for this app could not be configured without a
terminal.

### Configuration

**Remote › Configuration…** edits `remotex.toml` in a sheet: the TOML in a monospaced
editor, Reveal in Finder, Cancel, Save, and this instance's `rxa` public key with a
Copy button — the value a Mac agent needs in its `authorized_gateways`.

### About

**remotex › About remotex** — named after the instance's `branding`, so a second
instance's item says its own name — states the three things that identify a running
app, none of which is visible anywhere else:

- **the version**, from `CFBundleShortVersionString`, plus the wire protocol number
  the gateway is checked against;
- **the instance directory**, with Reveal in Finder;
- **the `rxa` public key**, with Copy — the same `GatewayKeyRow` the picker and the
  configuration panel show, because pairing a Mac is a reason to want it without
  editing anything.

It exists as an explicit item because `commandsReplaced` takes the standard
application menu down whole (see `RemoteCommands`): About is not restored unless it is
put back, and until it was, a running app could not say which build it was.

### The gateway key, in three places

`GatewayKeyRow` is the only value in the app that has to *leave* it — an agent answers
no gateway missing from its `authorized_gateways` — so it appears wherever somebody
would look for it:

- **the target picker's footer**, beside Configuration…. The screen a first launch
  lands on and the one a target switch comes back to, so this is the unburied copy;
- **About**, as part of what this instance is;
- **the configuration panel**, beside the target being paired.

The row is always present, in all three: when there is no `[rxa].private_key` it says
so, rather than disappearing and reading as a feature the app does not have. Deriving
the key costs a gateway process (`rxa-pubkey`), so `GatewayConfigStore` remembers a
successful answer and retires it on a save — which is the one thing that can change
the identity. A failed read is not cached, because bootstrapping is what mints the key
and a `nil` remembered before it ran would stick for the life of the app.

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

An instance is a directory, so a second one is a second directory. Two run side by
side with no coordination: separate configs, separate gateways on separate ephemeral
ports, separate `rxa` identities, nothing shared.

The obstacle is only ever *launching* them. Double-clicking `remotex.app` passes no
arguments — LaunchServices does not — and `open` without `-n` reactivates the copy
already running and silently discards `--args`. Both halves are the same trap the
Chrome `--user-data-dir` launchers hit, and the answer is the same: a small launcher
app per instance.

### 1. A launcher of its own

`packaging/macos-viewer/instance-launcher.applescript` is the template. Edit the app
path and the instance name in it, then compile it into an app:

```sh
osacompile -o ~/Applications/remotex\ Work.app \
  packaging/macos-viewer/instance-launcher.applescript
```

It runs one line — `open -n /Applications/remotex.app --args --instance-dir <dir>` —
and exits, so what stays in the Dock is remotex itself. The launcher is a real bundle:
give it any name, drop an icon on its **Get Info** window, pin it, find it in
Spotlight.

Nothing stops you doing the same from a `.command` file or a Shortcuts.app *Run Shell
Script* action. What you must **not** do is duplicate `remotex.app` and edit the copy:
the bundle is signed with a hardened runtime, so any change to it invalidates the
signature and breaks the TCC grants, which are keyed on the code identity.

### 2. Name it

Both instances are otherwise called "remotex" everywhere. Give each config a heading:

```toml
branding = "Work"
```

That reaches the window title, the picker heading and the launch screen — everywhere
the *window* identifies itself.

### 3. Give it a face

Drop an `icon.icns` (or `icon.png`) into the instance directory:

```text
~/Library/Application Support/remotex-work/
  remotex.toml
  icon.icns        ← this instance's Dock icon
```

The app loads it at launch and sets it as its own icon, so the Dock, ⌘-Tab and the
window's proxy icon all show it. A file rather than a config key: an instance either
has one or it does not, so there is nothing to validate and nothing to spell wrong.
`.icns` wins over `.png` when both are present, because it is the format macOS wants.

The launcher's icon and the instance's icon are different things and both are worth
setting — the first is what you click, the second is what you then see running.

### What a new instance costs

- **A new `rxa` identity.** A fresh directory mints its own `[rxa].private_key` on
  first launch, so every Mac agent it should reach needs *that* instance's public key
  on its `authorized_gateways` list — copy it from **About** or **Configuration…**,
  both of which show that instance's own key. The keys are
  genuinely different; one instance being paired says nothing about another.
- **A turn at the agent.** An agent serves one session at a time, so whichever
  instance reaches a shared Mac second is refused with a **Take over** button. That is
  the design, not a conflict to resolve.
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

There is one screen ahead of the target picker and it asks for nothing: `launching`
shows a spinner, or the reason the gateway did not start with the gateway's own
stderr beneath it and **Configuration…** / **Try Again** to act on it. A gateway that
exits while the app is using it lands on the same screen — deliberately not restarted
automatically, since one that died on a config it accepted will die again, and a
silent retry loop would hide the output that explains why.

Authentication and session ownership stay separate, as before:

- the bearer token authorizes this client to the gateway;
- the claim token owns the program's one active session slot.

A `401` no longer means "sign in again": the token is good for as long as the process
that minted it lives, so it means the gateway behind that port is not the one that
issued it. The app restarts the gateway instead.

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
slots. The viewer stores encoded WebP payloads in those slots and re-decodes a
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

Permission comes from the `connected` message and, for RXA, the active display:

| Target | May resize |
|---|---|
| RDP or VNC with `resize` | yes |
| RXA with `resize`, private display active | yes |
| Any other case | no — no viewport request is ever sent |

RXA permits it only for a display created by the agent; a Mac-owned display is
never resized by the viewer, and switching onto one withdraws the permission
mid-session.

The three View menu items are one decision:

- **Auto Resize** hands the remote's size to the window, which then follows every
  change, debounced and deduplicated. Off by default, per session: it is not
  remembered, and every connection starts manual.
- **Resize to Window** asks the remote to adopt the viewer's available size, once.
- **Resize to Display** changes the local window so the current remote desktop
  fits at its point size; it sends nothing to the gateway.

All three remain in the menu and are disabled when they do not apply. The two
one-shots are disabled while **Auto Resize** is on: one is what it does
continuously, and the other cannot fit a window to a desktop that is already
fitting itself to the window.

RXA is the only engine that reports individually selectable displays. The
Display menu sends `selectDisplay` and follows the `active` flag returned by the
gateway; it does not infer selection locally. RDP and VNC expose one framebuffer
and therefore no display choice.

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
Mac agent's line conversion.

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

The gateway is reached over plain HTTP on loopback, and ATS treats `ws://` as
`http://`, so the bundle uses `NSAllowsArbitraryLoads`.

Every request carries `Authorization: Bearer <token>`, including `/api/config`, which
needs no credential: a client that authenticates only the routes it believes are
guarded is one route away from a 401 nobody expected. The WebSocket upgrade carries it
too — `require_auth` runs before the upgrade, so omitting it is a bare 401 rather than
a socket that closes with a reason. `httpShouldHandleCookies` is off everywhere; there
are no cookies in this arrangement.

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

There is deliberately no way to point the app at another gateway — not on the command
line and not in the UI. The gateway it talks to is the one in its own bundle.

Always validate the packaged `.app`; `swift run`, standalone `swift build`, and
the executable under `.build` bypass bundle menus, `Info.plist` behavior, and the
bundled gateway (`Bundle.main.url(forAuxiliaryExecutable:)` finds nothing, so the app
comes up saying it is incomplete).

For socket-level diagnostics, `--probe` starts the same embedded gateway and prints
received control and frame information. It takes no address or credentials, because
there is no other gateway for it to reach:

```sh
dist/remotex.app/Contents/MacOS/remotex-viewer \
  --probe --instance-dir "$PWD/tmp/app-instance" \
  --probe-target mac --probe-seconds 90
```

The bundled gateway is also a full `remotex` binary, which is how an instance is
inspected from a terminal:

```sh
dist/remotex.app/Contents/MacOS/remotex-gateway rxa-pubkey \
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
