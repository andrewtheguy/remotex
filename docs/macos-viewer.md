# remotex.app

`remotex.app` is a native macOS 26 client, and it is a **shell**. It starts the
gateway in its own bundle, shows the SPA, and owns everything around it: the menu
bar, keyboard capture, pasteboard synchronization, and the window.

Inside the window is an embedded **Chromium** and nothing else. The page is
`frontend/dist` — the same build a browser loads — copied into the bundle and
opened as `remotex://app`, with `Contents/MacOS/remotex-gateway` behind it on
loopback for everything the page asks for. So the client has one implementation,
in one language, running on the engine it is fastest on, and this app is what a
browser cannot be:

| what it adds | why a page cannot |
|---|---|
| every ⌘ chord, ⌘Q and ⌘W included | a browser keeps them; `preventDefault()` does not reach them |
| `NSPasteboard` in both directions | reading on a timer and writing without a gesture are both refused |
| **Resize to Display** | a page cannot size the window it is in |
| a real menu bar | — |
| sound with no click in the page first | `--autoplay-policy=no-user-gesture-required` is the embedder's to pass |
| no right-click menu at all | a page can `preventDefault` its own `contextmenu`; the browser's answer to the native event is the embedder's |

There is one gateway, in this bundle. A gateway elsewhere is reached with a
browser, which is what a browser is for.

The bundle holds the engine, two executables and the client:

| path | what it is |
|---|---|
| `Contents/MacOS/remotex-viewer` | the Swift app (`CFBundleExecutable`) |
| `Contents/MacOS/remotex-gateway` | a copy of the `remotex` gateway binary |
| `Contents/Resources/web` | the built SPA: Chromium serves `index.html` from here as `remotex://app`, and the gateway serves the same directory |
| `Contents/Frameworks/Chromium Embedded Framework.framework` | the engine, ~317 MB of it |
| `Contents/Frameworks/remotex-viewer Helper*.app` | five wrappers around one `remotex-cef-helper` binary — Chromium's subprocesses. The names are CEF's to derive, not ours to choose |

`CFBundleIdentifier` remains `dev.remotex.viewer`; TCC grants and saved window
state are keyed to it.

## The embedded gateway

`remotex serve-embedded --instance-dir <dir> --web-root <dir>` serves only its
parent app and dies with it. `src/embedded.rs` and `config::Audience` fix its
server settings:

| | |
|---|---|
| address | `127.0.0.1`, one socket |
| port | `0`, read back off the socket after binding |
| web UI | the SPA in this bundle, named by `--web-root`. The window does not use it; a browser opened on this port does |
| login | refused. `/api/auth/login` and `/api/auth/logout` answer 403 |
| credential | a random token minted per launch, presented as the `remotex_session` cookie |

`--web-root` is passed rather than derived because nothing about a bundle's
layout is the gateway binary's to know: `default_static_dir()` finds an installed
prefix or a cwd-relative checkout, and inside `Contents/MacOS` neither applies.

### The origin

The window loads `remotex://app/index.html`, which `crates/remotex-cef/src/scheme.rs`
answers out of `Contents/Resources/web`. The gateway serves that same directory,
but the page in this app never asks it for the document — only for `/api/*` and
`/ws`.

This is about **`localStorage`**, which is keyed by origin. A gateway on an
ephemeral port is a different origin at every launch, so the client's three
remembered preferences were written to a bucket nothing would ever read again: the
Command-translation override and both "if compatible" defaults came back off at
every launch, and quitting was the only way to see it. `remotex://app` is the same
origin forever, however the port lands.

The scheme is registered **standard, secure, CORS-enabled and fetch-enabled**, in
*every* process — a renderer that has not been told would disagree with the browser
process about what the page's origin even is. Each of the four is load bearing:

| option | what it buys |
|---|---|
| standard | an origin at all, rather than an opaque one. This is what `localStorage` is keyed by |
| secure | a secure context, which is what WebCodecs requires — the client decodes audio and video through it |
| CORS-enabled | the page may make cross-origin requests, which every call to the gateway is |
| fetch-enabled | `fetch` may be used for them, which is what `gateway.ts` uses |

It costs one thing, paid in the gateway: `remotex://app` is not
`http://127.0.0.1:<port>`, so the page calls its own gateway **cross-origin**:

| header | why |
|---|---|
| `Access-Control-Allow-Origin: remotex://app` | names the one caller. Never echoed from the request |
| `Access-Control-Allow-Credentials: true` | what lets the `remotex_session` cookie travel. Without it the call succeeds *unauthenticated*, which reads as a mysterious 401 rather than as a CORS error |
| `Vary: Origin` | so no cache serves one origin's answer to another |

`shell_origin_cors` (`src/server.rs`) answers this only where both halves of an
embedded gateway hold — `allow_shell_origin` **and** `GatewayAuth::Token` — because
the credential is what the second header puts at stake. A served gateway is
reachable from a network and its cookie is behind a typed password. The named
origin is strictly safer than the `null` this replaced: `null` is also every
sandboxed frame on the web, and `remotex://app` is nobody else. WebSockets do no
preflight and carry the cookie on the upgrade, so `/ws` needs nothing beyond this.

There is a second gate in front of all of it, and it is Chromium's rather than
ours. Chromium 151 checks **Local Network Access** — a request from a public origin
to a loopback or private address needs a permission — and `remotex://app` is a
public origin talking to `127.0.0.1`. With no browser UI to ask and no permission
handler to answer, such a request is neither sent nor refused: it hangs. What that
looked like was a black window with an empty `#root`, no console error and no
failed request, because the client renders nothing until `/api/auth/status`
settles. `--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets`
turns it off, and the reasoning is in `crates/remotex-cef/src/app.rs`: the page,
the address and the gateway all ship in this one bundle, so a prompt could only
ever be answered one way.

The client reaches all of it through `frontend/src/gateway.ts`, which is the only
module that knows an origin: `window.__remotexGateway`, injected into `index.html`
by the scheme handler as it is served — before any of the client's own script — or
`location.origin` in a browser. `gatewayFetch` sets `credentials: "include"`, which
a cross-origin `fetch` needs and a same-origin one does not mind.

The bundle stays a **classic, deferred** script, which is `classicScriptTag` in
`frontend/vite.config.ts`. A module script would now work — `remotex://app` is a
real origin and CORS-enabled — but there is no reason for one build to behave
differently from the other, and `defer` is still what keeps the script from running
inside `<head>` and throwing "Root element not found". One build serves both
clients, and there is nowhere to put a second: this directory is `remotex://` and
`http://` at the same time.

### The cookie

The app puts the launch token in Chromium's cookie jar before the first load — a
document that arrives first arrives unauthenticated. The write is queued on
Chromium's IO thread and every read of the jar goes through that same queue, so the
load cannot overtake it.

It is written `SameSite=None; Secure`, and neither half is optional. `remotex://app`
and `http://127.0.0.1:<port>` are different sites, so a `Lax` cookie is simply not
sent on the page's own `fetch` and `ws://` calls, and the request then arrives
*unauthenticated* — a mysterious 401 rather than anything about cookies. `None`
requires `Secure`, and Chromium allows a `Secure` cookie here because a loopback
address is a trustworthy origin even over plain HTTP.

The expiry is a `cef_basetime_t`, which is microseconds since **base::Time's** epoch
and not the Unix one; the two are 369 years apart. A Unix timestamp handed over as
one is a date in the seventeenth century, and a cookie that expired in 1657 is not
refused loudly — it is accepted, dropped, and the jar is empty afterwards, which
reads exactly like a cookie that was never set. `expiry_in_days` counts from CEF's
own `basetime_now()` for that reason.

A cookie and not the `Authorization` header this app used to send, because the
requests that matter are not the app's. The page issues its own `fetch` calls and
opens its own `ws://` sockets, and neither can be given a header from outside the
document. `require_auth` therefore reads the same cookie on both kinds of
gateway and differs only in what makes the value valid: a session lookup for a
login gateway, a constant-time compare against the launch token here.

`/api/auth/status` answers on both, because the same SPA asks it first on both.
An answer of "no" means the gateway and the app disagree about the token, which
no login form can fix — the page reports it and the app takes the screen back.

Chromium keeps this instance's profile — the `localStorage` the client's three
remembered preferences live in, and the cookie jar above — under
`<instance-dir>/chromium`, named by `cache_path` and `root_cache_path` at startup.
That is what makes `--instance-dir` isolate preferences the way it isolates the
config and the log, and it is better than the app-container store it replaced: the
state is under the directory that names the instance rather than beside it.

A profile on disk is necessary and not sufficient — a stable [origin](#the-origin)
is the other half. Either one alone looks exactly like working until the app is quit
and launched again, which is how this shipped twice as fixed and was not.

`persist_session_cookies` is deliberately off, and it is worth being exact about
what that does and does not mean: it governs cookies with **no** expiry, and the
launch token is written with one — so the token *is* stored in this instance's
profile on disk, whatever that setting says. Nothing is done about it, because there
is nothing there to protect: the gateway that would accept it is gone by the next
launch, the shell overwrites it before the first page load, and the directory it sits
in is the `0700` one that already holds `remotex.toml`. Anyone who can read the jar
can read the credentials of every machine this app can reach.

The claim lives in `sessionStorage`, which is per browser whatever the profile does.

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

### Quitting Chromium

`remotex_cef_shutdown` closes every browser and **waits** before taking the engine
down, turning the pump by hand while it waits — a close finishes on the UI thread,
which is the thread already inside `terminate:`, so it cannot happen otherwise.

Shutting down with a browser still open is not a leak. CEF walks structures the
browser owns and the process dies on the way out of ⌘Q: a crash report every quit,
and a profile left marked unclean, so the following launch comes up in Chromium's
crash-recovery state. The fault is therefore visible one launch after the one that
caused it, which is what made it look like a startup problem.

If the browsers do not close inside a second, the engine is left standing rather
than shut down. The process is exiting either way; a dirty profile is a worse next
launch and a segfault is a worse one still.

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

There is no `viewer.json` any more. The three remembered preferences — the
Command-translation override and the two "if compatible" defaults — belong to the
client, in its own `localStorage`, in the per-instance data store described above.

A first launch writes a commented zero-target template, which is valid; the
picker states that there is nothing to connect to.

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
`CFBundleShortVersionString` and the instance directory, with Reveal in Finder.

There is no wire-protocol version in it. The client and the gateway are built
together and shipped in one bundle, so there is no pair of versions that could
disagree — the check that used to run before every session is gone with the
session this app used to own.

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
independently launchable. A wrapper could carry `--instance-dir`, but the Dock
would show the base app rather than the instance. Keep `--instance-dir` only as a
QA override.

### What a variant costs

- **13 MB, and it goes stale.** Re-run the script after updating `remotex.app`;
  it replaces the bundle without touching its instance directory.
- **No entitlements, no notarization, no TCC.** The shipped bundle is ad-hoc signed
  itself (`codesign -dv` → `Signature=adhoc`), and the viewer holds no TCC grants
  for a change of code identity to break: it captures keys with a *local* `NSEvent`
  monitor, which needs no Accessibility permission.
- **One permission that is not TCC.** Local network access is asked for per app,
  and a variant is a different app, so each one is asked once — see
  [Local network permission](#local-network-permission).

Do not edit `remotex.app` in place; use the script so the copy is re-signed.

### Naming the window too

The bundle name reaches the Dock, ⌘-Tab and the menu bar. The *window* is named by the
config:

```toml
branding = "Work"
```

The bundle name identifies the app; `branding` identifies its content.

Each instance starts with an independent empty configuration and an isolated
port and log.

## Entry and session lifecycle

`ViewerScreen` has two cases, and the second one is a web view.

`launching` is a spinner that occasionally becomes a message: the gateway's
stderr with **Configuration…** and **Try Again**. Everything else — the login the
gateway refuses, the target picker, the desktop, the takeover interstitial, the
reconnect backoff — belongs to the client, which is the same client a browser
runs and is documented in [`architecture.md`](architecture.md).

The app returns to `launching` from exactly three places: the gateway failing to
start, the gateway exiting while it was in use, and the page reporting that the
gateway would not take the launch token. All three are conditions no page can do
anything about.

## The bridge

One query function named `remotexNative`, and one `ExecuteJavaScript` call. That is
the whole app-to-client protocol; `frontend/src/nativeHost.ts` is its other half,
and `NativeBridgeTests` pins the JSON both ways.

The query function is CEF's message router, configured once in
`crates/remotex-cef/src/app.rs` and installed in every frame's V8 context as it is
created — which is why `NATIVE_HOST` can be read at module load and never be wrong.
The page posts and does not wait: the router's reply is a receipt, because a query
left unanswered leaks on both sides of the IPC, and everything the app has to say
goes the other way.

**`onSuccess` and `onFailure` are not optional**, even though nothing reads them.
A missing member of a JavaScript object reads back as a value of type `undefined`,
and the router's renderer half tests these two for "is a function" without first
testing for "is undefined" — which it does do for `persistent`. So a query without
them is rejected as malformed, and the rejection is silent in both directions: the
exception never reaches the page and the query never reaches the app. The symptom
is every menu in **View** and **Remote** permanently disabled, since the `state`
they derive from is the message being dropped. `nativeHost.test.ts` is what holds
the shape now; no browser test can, because in a browser `postToHost` returns
without doing anything, which is exactly what the broken version did here.

**Page → app** — `state`, `clipboardFromRemote`, `unauthenticated`.

`state` is one object carrying the mode, the connection status, whether a frame
has arrived, the remote's size and density, the capability flags, the display
list, and the keyboard-override verdict. Every menu title, tick and enabled state
is derived from it and from nothing else. Nothing in the app is ever set
optimistically: a tick moves when the client says the thing changed, not when the
item was pressed, so a menu cannot claim a capability the thing on screen does
not have.

It decodes field by field with defaults rather than through the synthesized
decoder, because a page part way through a navigation posts what it has — and the
answer to a missing field is "nothing is connected", not a decode failure that
leaves the menus describing the session before it.

**App → page** — `key`, `releaseInput`, `clipboardLocal`, and the menu commands:
`openClipboard`, `openDisplays`, `closePanel`, `resizeToWindow`, `setAutoResize`,
`selectDisplay`, `setAudio`, `setMacKeyOverrides`, `refresh`, `switchTarget`,
`takeOver`, `sendKeyCombo`.

Commands are JSON, encoded and never interpolated. Text copied off a remote
desktop reaches this app through the clipboard bridge and then goes into a
JavaScript call; a remote that copies a closing paren is not entitled to run
anything in this window.

The call is a `?.` chain, because the page installs its entry point when the
desktop mounts and removes it when that unmounts — a menu item pressed a moment
either side of a target switch has to find nothing and do nothing.

The document is `remotex://app/index.html`, a **secure context** because the scheme
was registered as one, so WebCodecs is available and Opus decodes. H.264 does not:
stock CEF ships without proprietary codecs, and a `render_type = "video"` target
therefore fails here through the client's own "this browser cannot decode…" path —
see [`known-issues.md`](known-issues.md). Nothing about the session passes through
this app either way: the page talks to the gateway itself, over the cross-origin
path [above](#the-origin).

Navigation anywhere else is refused, in Rust now rather than in Swift, and it is a
scheme-and-host test again (`client::permits`) — the thing a `file://` document
could not have, since a file URL has neither host nor port and every one of them
therefore matched every other. This matters because the window shows somebody
else's pixels and carries their clipboard strings, and there is no address bar to
notice with. Popups are refused for the same reason, in the same place.

There is **no context menu**. `on_before_context_menu` clears the model, which is
what stops Chromium showing one at all. A browser's menu — Back, Reload, View Page
Source — means nothing here, and a right-click is something the guest wants: the
desktop surface calls `preventDefault` on `contextmenu`, but that covers the canvas
only, and even there the menu is Chromium's answer to the *native* event rather than
to the DOM one. What a right-click ought to offer is on the menu bar, which is the
shell's and is already on screen.

`--autoplay-policy=no-user-gesture-required` is what lets **Remote › Enable Audio**
start sound: a menu press is not a user activation as far as the engine is
concerned, and without it the page's `AudioContext` would come up suspended with
nothing on screen to say so.

## Display and resize behavior

The client decides what it may ask for; the menu asks. Both permissions come from
the gateway's `connected` message and reach the menu bar in the bridge's `state`:

| Target | May resize | Window may drive it |
|---|---|---|
| Plain `vnc` with `resize` | yes | yes |
| RDP or an Apple subtype with `resize` | yes | no |
| Any other case | no | no |

The second column is `resize`, the operator's. The third is `autoResize`, and the
gateway decides that one on its own: RDP renegotiates with a
Deactivation-Reactivation Sequence and Apple High Performance replaces a virtual
display, and both have a fault in [`known-issues.md`](known-issues.md) that a
window drag reaches far more often than a menu item does.

The three View menu items are one decision:

- **Auto Resize** asks the client to follow the window. Where the mode is refused
  the item reads **Auto Resize (Not Applicable)** and greys, with **Resize to
  Window** live beneath it — greying alone would read as "this session cannot
  resize", which the item below disproves. The model refuses the command as well
  as greying the item.
- **Resize to Window** asks the remote to adopt the viewer's available size, once.
- **Resize to Display** changes the local window so the current remote desktop
  fits at its point size; it sends nothing to the gateway. The arithmetic is
  `RemoteGeometry.windowFrame` and the measurement is the container view's own
  bounds — a page scrolls inside them and cannot change them.

All three remain in the menu and are disabled when they do not apply. The two
one-shots are disabled while **Auto Resize** is on: one is what it does
continuously, and the other cannot fit a window to a desktop that is already
fitting itself to the window.

The viewport itself is measured and reported by the client, from the window it is
in. Nothing about it passes through this app.

The Display menu lists what the client reports. Apple Screen Sharing Standard
mode (`subtype = "ard"`) fills it with physical screens and *All Displays*;
`ard-high-performance` requests one virtual display; RDP and generic VNC expose
one combined framebuffer, so the menu reads *No Displays to Choose From*. The
checkmark moves only when the Mac confirms the selection.

### Window chrome

The desktop toolbar is hidden while a remote is displayed. The window remains
titled because an untitled window cannot become key and accept keyboard input;
full screen is the chrome-free mode. The title gains a trailing 🔊 while sound is
playing — the one persistent surface that can show it, since the toggle is a menu
item.

## Keyboard and pointer input

While a connected desktop is focused, an AppKit local event monitor consumes
`keyDown`, `keyUp`, and `flagsChanged` before application menu equivalents.
Remote-menu commands therefore have no keyboard shortcuts. macOS-global
shortcuts such as Command-Tab and Command-Space remain local because the
application never receives them.

The monitor is the reason this app exists around the page. It sits outside
Chromium's view and swallows what it consumes, so the engine never sees a key
event — which is what lets ⌘Q and ⌘W reach the guest instead of this application.
The page's own key listeners never fire here; the keys arrive over the bridge
instead.

What a chord *means* is the client's. `KeyboardCodes` maps a macOS virtual
keycode to a DOM `code` and sends it; `frontend/src/macKeys.ts` owns the
Command-to-Control translation, the bare-Command tap, and the record of what is
held. One implementation, shared with the browser, tested once — and the one
difference between the two hosts is a table: the six chords a browser never
receives (⌘L, ⌘N, ⌘O, ⌘R, ⌘T, ⌘W) are mapped only when the host is this app.

Capture is gated on a live desktop with a first frame, read out of the bridge's
`state`, and on the first responder being the surface — which is what keeps
typing in the clipboard panel from reaching the remote.

Command-V pushes the local pasteboard before the keystroke, where the chord will
arrive as a paste. Pointer input is the page's throughout.

The Edit menu remains available for text fields and supplies the standard
copy/paste/cut/select-all actions through the responder chain. `ViewerMenus`
restores it when SwiftUI rebuilds the main menu, and strips every key equivalent
outside it.

## Clipboard

For a connected target with `clipboard`, the viewer polls
`NSPasteboard.changeCount`:

- local text changes are sent to the client, which forwards them if its own echo
  guards agree;
- unsolicited remote changes arrive as `clipboardFromRemote` and are written to
  `NSPasteboard`;
- echo guards on both sides prevent either direction from bouncing the same value
  back, and a newer local value wins over an older remote one.

The *panel* is the client's — **Remote › Clipboard…** opens the page's own
clipboard panel, where Copy is still the only thing that writes a fetched value
to this Mac. Rebuilding it in AppKit would be two clipboard editors to keep in
step and two places for that consent boundary to live.

Clipboard values are capped at 64 KiB in either direction and refused rather
than truncated. Programmatic reads follow macOS's **Paste from Other Apps**
permission, while `clipboard = true` remains the boundary.

## Networking

The gateway uses plain HTTP on loopback. Because ATS treats `ws://` as `http://`,
the bundle sets `NSAllowsArbitraryLoads`; that covers the `fetch` calls and socket
upgrades the page makes to loopback from its `remotex://app` document. Chromium's
own gate on the same traffic is [Local Network Access](#the-origin), which is a
separate thing and is disabled by switch.

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
packaging/macos-viewer/stage-cef.sh          # only before a bare `swift test`
swift test --package-path apps/remotex-viewer
packaging/macos-viewer/build-viewer-app.sh
open -n dist/remotex.app --args --instance-dir "$PWD/tmp/app-instance"
```

The build script builds the SPA itself, so `bun` is required for it. The first
line is separate because it is the client's *own* checks — a bundle whose page
builds but whose tile painter is wrong looks fine until pixels land.

`stage-cef.sh` is what `Package.swift` links against: it builds `remotex-cef` and
puts `libremotex_cef.a` and CEF's `libcef_sandbox.dylib` under `target/cef-link`.
`build-viewer-app.sh` runs it itself; a bare `swift build` or `swift test` does not,
and fails to link without it. CEF itself is expected at `$CEF_PATH`, default
`~/.local/share/cef`; export one with
`cargo run -p export-cef-dir -- --force "$HOME/.local/share/cef"`.

An edit to the shell or the Chromium host does not need the whole build:
`packaging/macos-viewer/refresh-viewer-app.sh` replaces just those two in the
bundle that is already there, re-signs it inner-first, and takes seconds rather
than a minute. It is not a build — trust a result from the bare build script.

`--instance-dir` is the only GUI-launch argument and is the whole of the
isolation: config and log are under the directory it names, so QA cannot touch
`~/Library/Application Support/remotex`. Delete the QA directory for a clean run.

**Ask the app rather than clicking at it.** No AppleScript, no synthetic clicks, no
screenshot loops; four ways in, in the order they cost:

1. Run the bundled binary directly rather than through `open`, so its stderr is
   yours: `REMOTEX_CEF_TRACE=1 dist/remotex.app/Contents/MacOS/remotex-viewer
   --instance-dir "$PWD/tmp/app-trace" 2> trace.log`. The trace names the scheme
   requests, the browser, the cookie write, the load result, and every navigation
   the policy refused.
2. `REMOTEX_CHROMIUM_SWITCHES` replaces the switch list at startup, so trying one is
   a relaunch rather than a rebuild — including `--remote-debugging-port=9222`,
   which makes the page answerable over CDP from a `uv` script: the live DOM, the
   console and exception streams, `Network.*` events, and the cookie jar.
3. `REMOTEX_STARTUP_PAGE=grid` opens a page with no script in it at all. If the grid
   fills the window, the engine, the view and the compositor are sound and the fault
   is the client's — one relaunch, half the search gone.
4. **Remote › Developer Tools**, for the times a person is already looking.

Four things this engine specifically puts at risk, beyond the list above:

- **Preferences survive a relaunch** — see below.
- **Auth travels on a cold launch.** Delete the QA instance directory first; a
  cookie that never arrives surfaces as a 401, not as a CORS error.
- **⌘Q and ⌘W reach the guest.** The `NSEvent` monitor must still win over
  Chromium's view.
- **No orphaned helpers.** After Force Quit, `pgrep -f "remotex-viewer Helper"`
  must print nothing, and neither must `pgrep -f remotex-gateway`.

**Quit and launch again as part of every QA pass.** Change a remembered preference
— the Command-translation override, or either "if compatible" default — then ⌘Q and
reopen. Nothing within a single launch can tell a stored preference from one that
was written where it will never be read again, which is how this shipped as fixed
twice; the gateway's port is in `gateway.log` and differing between the two runs is
the point, not a problem.

Always validate the packaged `.app`; `swift run`, standalone `swift build`, and
the executable under `.build` bypass bundle menus, `Info.plist` behavior, and the
bundled gateway (`Bundle.main.url(forAuxiliaryExecutable:)` finds nothing, so the app
comes up saying it is incomplete).

The bundled gateway is also a full `remotex` binary, which is how an instance's
configuration is checked from a terminal:

```sh
dist/remotex.app/Contents/MacOS/remotex-gateway check-config --embedded \
  --config ~/Library/Application\ Support/remotex/remotex.toml
```

Automated tests cover the bridge's JSON in both directions, the menus' titles and
enablement against a recorded page, the pasteboard rules, the window-fitting
arithmetic, the menu-bar rules, and the gateway process contract. Everything
below the bridge is the client's, and is tested in `bun test` and
`tests/playwright` where the code is. Sound and anything about pixels still
require manual QA; **Remote › Developer Tools** opens Chromium's inspector on the
page, in the shipped app rather than only in a debug build — the shipped app is the
one there is to look at.

The in-process tone harness (`cargo test --lib serve_a_test_tone -- --ignored`)
serves a login gateway for testing the client's playback path in a browser. To
include RDP negotiation, configure an `audio = true` RDP target; verify
start/stop/resume, and use a source that announces left and right channels when
checking stereo order.

See
[`packaging/macos-viewer/README.md`](../packaging/macos-viewer/README.md)
for signing, packaging, permissions, and development launch details.
