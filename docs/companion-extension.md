# The companion extension

`remotex.app` ([docs/macos-viewer.md](macos-viewer.md)) is macOS-only. Everywhere else
the client is a page in Chrome, and how well it works is decided almost entirely by
what kind of window that page is in.

| | |
|---|---|
| A tab | Works. The browser keeps Ctrl+W, Ctrl+T, Ctrl+N and the rest for itself, and there is no system clipboard while the window is unfocused. |
| **An app window** | **Better.** Chrome reserves no keys in one, so the chords reach the remote, and this is where the companion extension runs. |
| An app window, full screen | Best. The same, with the whole screen. |

`apps/companion/` is a Chrome MV3 extension that adds the two things even an app window
cannot do for itself:

- **the system clipboard while the window is unfocused or minimised.** A page may only
  `navigator.clipboard.readText()` while focused, and may only write with a user
  gesture behind it.
- **resizing its own window** to the remote's framebuffer. `window.resizeTo` is refused
  for a window the user opened rather than a script.

This is its design. The code is [`apps/companion/`](../apps/companion/README.md); what
has never been done is the manual half of [Testing](#testing), which needs a person and
a real remote.

## App windows only

**The extension does nothing in an ordinary tab.** The content script installs no
listeners and posts nothing, and `frontend/src/companion.ts` settles to `absent` on the
first render without a handshake. That is the simplification the whole design rests on:
one window kind to serve, no capability that is present in one and missing in the other,
and no state where half of it works. Both sides are *waiting* rather than finished,
though — see [below](#installing-as-an-app-does-not-reload-the-page): a tab that is
installed as an app becomes an app window without reloading, and both ends arm then.

A tab is not left unserved so much as served by the client itself. Full screen plus
Keyboard Lock (`frontend/src/immersive.ts`) gives a tab the chords — automatically, on
any full screen the page can observe, including the browser's own ⌃⌘F — and the
clipboard falls back to the page's own focus-driven sync. Nothing there is worse for
the extension declining to participate; the answer to "I want more" is the app window,
which is one menu item away.

## The shim window

Make one with *Install page as app…* from the Chrome menu, which also leaves a
Start-menu or desktop shortcut, or launch it directly:

```
chrome.exe --app=http://gw-a.remotex.localhost:52380/
```

It has no tab strip and no omnibox, and `frontend/src/appWindow.ts` recognises it by
its `display-mode`.

**It reserves no keys.** One line in Chromium's `browser_command_controller.cc` sits
ahead of every case in `IsReservedCommandOrKey`:

```cpp
// In Apps mode, no keys are reserved.
```

In a tab, Chrome acts on Ctrl+W, Ctrl+T and Ctrl+N itself, before the renderer, and
the page never sees the keydown. In an app window each one is delivered to the page
first and only reaches the browser if the page lets it through. The client already
calls `preventDefault()` on every key its desktop surface sees, so **this needs no
fullscreen, no Keyboard Lock, and on Windows and Linux no code at all**. The one place
a Mac needs more is `macKeys.ts`, whose table of Command chords to translate leaves the
six browser chords out precisely because they never arrive; the app window puts them
back.

That early return is cross-platform, and it is what the whole configuration rests on.
The per-key table in
`tmp/programs_for_reference/chrome_extension_spike/PWA_KEYS.md` was measured against
macOS, whose Cocoa menu redispatch is its own thing — the ranking of which keys are
free, which are page-first and which are hopeless still wants one pass on Windows.

**It still has extensions.** An app window shows extension icons beside its three-dot
menu — 1Password's locked/unlocked state is right there — so the action icon and its
popup work exactly as they do in a browser window. That is what makes the icon worth
building at all: it is the enabled indicator in the one configuration this runs in, and
its popup is where Resize to display lives.

**What no window of any kind gets**: Alt+F4, Alt+Tab, the Windows key, Ctrl+Alt+Del,
and on a Mac ⌘Tab, ⌘Space and the screenshot chords. Those belong to the OS, and
neither a keyboard lock nor an extension changes that. There is no answer to the first
of them: closing the window ends the session, and the client does not ask first.

`NATIVE_HOST` stays false in an app window, as it does in a tab. The floating menu is
the only chrome the client has here and it must stay on screen.

## The scope rule

**The extension does only what the browser cannot.** Anything a page can do for itself
stays in the page, because there is one client and it is the page a browser loads — a
capability implemented in an extension as well is a second implementation of the same
thing, drifting from the day it lands.

That rule decided the keys out of it twice over. In an app window the chords are the
page's for the asking, and in a tab full screen and Keyboard Lock are plain web APIs.
`beforeunload` is a page event and only the page knows whether a session is live, so
the close guard is the page's too. The spike this design comes from put all of it in a
content script, and only because it had no client to put it in.

## `NATIVE_HOST` stays false

`NATIVE_HOST` does not mean "there is something native here". It means *a shell owns
the window chrome*, and the client answers by hiding its floating menu, dropping the
login screen and handing the panel actions out for a menu bar to drive. The extension
owns no chrome, and neither does an app window — it has a title bar and nothing else.
Under both the client looks and behaves exactly as it does in any browser.

So the extension gets its own seam, and it is much narrower — two files, both already
in the tree:

| | |
|---|---|
| `frontend/src/companion.contract.ts` | The wire types. React-free, so the extension can type-import it with no `frontend/node_modules` installed — the same rule and the same CI reason as `nativeHost.contract.ts`. |
| `frontend/src/companion.ts` | The store, the hooks and the three guards on the message bus. |

`NativeState` is reused verbatim rather than trimmed. `RemoteDesktop.tsx` builds it
once per render for the menu bar; a second, smaller state object would be a second
thing to keep in step for the sake of fields nothing reads yet.

What is deliberately absent is every other `NativeCommand` variant — `openClipboard`,
`selectDisplay`, `setAudio`, `takeOver`, `sendKeyCombo`. Each exists because the shell
*hides* the floating menu. Here that menu is on screen, and a command for what it
already does would be a second UI for the same control.

Resize is on neither side of the seam. The page reports the framebuffer in
`NativeState.size` because the menu bar already needed it, and the extension measures
the window from its own content script — a resize is arithmetic over two things it
already has, and the popup is where it is asked for.

## Detection is asynchronous, and it can be re-asked

`NATIVE_HOST` is read once at module load: the app's preload runs before any script in
the document and cannot appear later. The companion can promise nothing of the sort.
Nothing tells a page synchronously whether a content script was injected into it, so the
only way to find out is to ask and wait; a bfcache restore makes the question worth
asking twice; and **the window kind itself can change under a running document**. So
this is a store, not a constant:

| from | on | to | |
|---|---|---|---|
| `probing` | `hello` | `connected` | |
| `probing` | the deadline | `absent` | 1.5 s of silence |
| `absent` | installed as an app | `probing` | the tab was reparented into an app window |
| `absent` | `pageshow` | `probing` | a bfcache restore, which re-arms the deadline with it |
| `connected` | `pageshow` | `connected` | a hello goes out, but there is nothing to re-probe |

Only a `pageshow` returns anything to `probing`, and it re-arms the deadline as it
does — so there is no path to a phase that waits for an answer nothing will settle.
*Re-arms*, not *arms another*: a second restore inside the first deadline invalidates
it, or the older one would come due against the newer question and answer `absent` on
its behalf, early by however long ago it was asked.

**There is no goodbye**, and there is nothing left for one to say. The extension serves
one host, named in its manifest, and asks for nothing at runtime — so it has no moment
where it learns it is leaving. An extension disabled, reloaded, or withheld from
`chrome://extensions` takes its content script's context with it before anything could
be sent, which is the same hole a `bye` never covered. A page can therefore be left
believing in a companion that has gone until it is reloaded, and reloading is the
gesture anybody would already reach for.

`probing` is not the same answer as `absent`, and the difference is load-bearing. The
focus-driven clipboard read in `useRemoteDesktop.ts` stands down while `probing`:
starting it and stopping it a quarter of a second later would push the same text twice
and put the browser's clipboard permission prompt on screen for nothing. When the phase
settles the effect re-runs, and its own trailing call covers the delay.

The phase is not the whole condition, though — `capabilities.clipboard` is the other
half of it, and both behaviours read both. The page stands its reader down, and hands a
remote copy over instead of writing it, only under a companion that says it is doing
the polling. One that reports `clipboard: false` changes nothing about the page at all:
it reads and writes for itself, exactly as it does with none installed. Nothing in the
extension turns that off today — there is no options page to turn it off from — but the
seam is the wrong place to assume it, because the page's behaviour has to follow what
the extension says it does rather than the fact that it answered.

A tab does not enter `probing` at all. It is `absent` from the first read, so that
stand-down costs nothing where there is nothing to wait for — until it stops being a
tab.

### Installing as an app does not reload the page

*Install page as app…* **reparents the live document** into the new window
(Chromium's `ReparentWebContentsIntoAppBrowser`) rather than loading it again. So a
window kind read once at load is read as `browser`, and stays wrong for as long as the
window is open: the installed window insists it is a tab, the seam never wakes, the
Command chords are not claimed, and the client's own Help card goes on advising an
install that has already happened. Closing it and launching the app again was the only
cure, because that is what finally produced a fresh document.

Nothing dispatches an event for it — the display mode simply changes underneath the
document — so both ends of the bus watch the three `(display-mode: …)` media queries and
**latch**: `frontend/src/appWindow.ts` for the page, and the same three queries in the
content script for the extension. Latch, not track, because full screen reports
`display-mode: fullscreen`, and a window that stopped counting as an app window on the
way into immersive would take the chord table down with it mid-session. The answer
therefore moves once, from tab to app window, and never back.

Both ends arm on the same signal, and neither has to be first: the handshake is
symmetric — both sides say `hello`, and the page says it again on `pageshow`, because a
bfcache restore replays no content-script injection.

## The wire

`window.postMessage`, with three guards on every inbound message and an explicit
`targetOrigin` on every outbound one:

```
e.source === window          drops an iframe or an opener
e.origin === location.origin drops a cross-origin poster
source: "remotex-ext"        drops every other conversation on this bus
```

There is no `externally_connectable`: the content script is a mandatory relay, and no
page can reach the service worker directly.

## What the extension contains

```
apps/companion/
  src/manifest.json           MV3; version injected from Cargo.toml at build time
  src/shared/
    contract.ts               type-imports frontend/src/companion.contract.ts
    messages.ts               ToWorker / ToContent / ToOffscreen + type guards
    origin.ts                 PURE — the one served host, as a pattern and a predicate
    geometry.ts               re-exports apps/viewer/src/main/geometry.ts
    resize.ts                 PURE window arithmetic
    version.ts                PURE — a Cargo version to a Chrome one
  src/worker/                 stateless router, ensureOffscreen, icon, resize
  src/content/                the app-window gate and the page bridge
  src/offscreen/              the clipboard poller, over the viewer's synchronizer
  src/popup/                  the state card and Resize to display
  scripts/build.ts            Bun.build, mirroring apps/viewer/scripts/build.ts
  icons/                      two SVGs, committed PNGs, and the script between them
  tests/
```

There is no options page and no `chrome.storage`. The extension holds no settings of
its own, and the one host it serves is in the manifest, see below.

Toolchain mirrors `apps/viewer` exactly: TypeScript, `Bun.build`, biome,
`tsc --noEmit`, `bun test tests`.

### One host, hard-coded

The whole access model is two lines of `manifest.json`:

```json
"permissions": ["offscreen", "clipboardRead", "clipboardWrite"],
"host_permissions": ["http://*.remotex.localhost/*"],
"content_scripts": [{ "matches": ["http://*.remotex.localhost/*"], ... }]
```

There is no grant flow, no popup switch, no `chrome.permissions` call anywhere and no
`optional_host_permissions` — with none declared, there is no origin this extension can
come to hold that is not written above. Install it and it works, on that host, on every
port. That is the setup, and there is no step where anybody turns a site on.

`.remotex.localhost` is where a development gateway already puts a browser.
`[server].dev_subdomain = "gw-a"` redirects any loopback name — `127.0.0.1`, `::1`,
`localhost`, another gateway's label — to `http://gw-a.remotex.localhost:<port>/`,
keeping the port, so each gateway on a machine has a cookie origin of its own
(`src/server.rs`, and `packaging/etc/remotex.toml.example` for the key). Setting that
key is the extension's whole installation procedure beyond loading the folder.

The cost is stated plainly: **a gateway reached at any other address gets no companion
at all**. A LAN name, a reverse proxy's hostname, `localhost` without the redirect —
none of them are this extension's, and the icon says so on every one. What that buys is
that nothing has to be granted, reconciled, stored, remembered across an update, or
asked about in a permission prompt. For a personal client whose app-window configuration
is a development one, that trade was worth taking.

Two properties of Chrome's match patterns are load-bearing here. `*.host` matches
subdomains **and** the apex, so the bare `remotex.localhost` is covered as well as every
label under it — `isCompanionUrl` in `shared/origin.ts` says the same, because a
predicate that disagreed with the manifest would call a window Chrome had injected into
"not ours". And **a pattern cannot express a port**, so every port on these names is
covered. What RFC 6761 reserves is that the name resolves to loopback and never leaves
this machine — not that this project is what answers there. Any local process that binds
a port answers to `anything.remotex.localhost:<that port>` too, so the host is a
**routing rule, not a credential**: it says where the bridge may run, and nothing about
who is on the other end. See [Costs](#costs-stated).

`http://` only. The gateway has no TLS listener and the redirect always sends a browser
to `http://`; a `.localhost` name is a secure context by the same rule the client's
preflight relies on, so `https://` would be a second pattern to keep in step for a URL
nothing produces.

The content script is **declared, not registered**. With one host there is nothing to
reconcile — no `scripting` permission, no `registerContentScripts`, no worker start-up
pass over `permissions.getAll()`, and no injection into the current window to make a
grant take effect without a reload. Chrome registers it from the manifest and keeps it
registered.

Its own gate is then one check: **an app window**, using the same `display-mode`
allow-list the client uses, so both ends of the bus agree by construction. It needs no
service worker, which Chrome may have killed, and it decides whether the page ever
learns the extension exists.

No `storage`, because there is nothing to store. No `tabs` — the host permission is what
makes `tab.url` readable, and it is readable for exactly the windows this extension
serves, which is also what makes `servedTabs()` right by construction. No
`externally_connectable` and no `web_accessible_resources`, the last so the extension
cannot be probed by URL even from the gateway's own page.

### The popup, and Resize to display

Triggered from the popup only. The page's floating menu gains nothing: it would be a
control that exists in one browser configuration and not another, and the popup is
reachable from the app window anyway.

The popup is a state card and one button. There is nothing to turn on in it: a window is
on the served host or it is not, and on one that is not, the popup's entire content is
the sentence saying so and naming the host — the correct rendering, not an empty state.

The **card** is `NativeState` as last reported — which target, the framebuffer as
`1920 × 1080 @2x`, whether the clipboard bridge is on — and the button is **Resize to
display**, disabled unless a size has been reported and `capabilities.resize` is set.

`apps/viewer/src/main/geometry.ts` is already exactly this arithmetic — pure, importing
nothing, tested, and already carrying the rule that matters most here: the window is
fitted to the framebuffer, and the framebuffer is never scaled to the window. It is
re-exported rather than copied.

The one subtlety is browser zoom. `innerWidth` is CSS pixels *at the current zoom*,
while `outerWidth` and `chrome.windows.update` are device-independent pixels, which
zoom does not touch. So a `w/scale` CSS-pixel framebuffer needs `w/scale * zoom` DIPs
of content, and the chrome is `outerWidth - innerWidth * zoom`. At any zoom but 1 the
desktop is not at 100% however the window is sized. Nothing corrects that silently: an
app window delivers Ctrl+0 to the page, and letting it through resets the zoom.

A 3840×2160 remote on a 1080p laptop gets a work-area-sized window and scrolls.

### Clipboard

An offscreen document (`reason: CLIPBOARD`) is the one context that can touch the
clipboard without focus; `navigator.clipboard.*` throws *"Document is not focused"*
there, so both directions go through the `execCommand` textarea trick. Text only —
images read back as an empty string, which is treated as "no text", not as "clipboard
cleared".

`apps/viewer/src/main/clipboard.ts` already has the poller, the size cap and three
echo guards, one of which the spike lacked: a newer local value wins, so the remote's
clipboard cannot stomp something the user copied here a moment ago. Share the class;
the only change it needs is `utf8Bytes` using `TextEncoder` instead of `Buffer`, which
does not exist in an offscreen document.

The page's own guards (`lastFromRemoteRef` / `lastToRemoteRef`) are the same loop seen
from the other side and are untouched.

### The toolbar icon

Two variants, on and off, painted per `tabId` from `chrome.tabs.onUpdated` (on
`loading` *and* whenever `changeInfo.url` is set — that is how a SPA's `pushState` shows
up), `onActivated` and `windows.onFocusChanged`. Per-tab icon state is reset by Chrome
on navigation, so `onUpdated` is required rather than an optimisation.

A `tab.url` the worker cannot read is a window this extension has no permission for,
which is the honest off state and costs no lookup to determine. The two off cases it has
to tell apart in its title are "this window is not a gateway of ours", which is an
address and nothing in the popup can change, and "this is not an app window", which the
client's Help card explains.

The icon is **cosmetic and best-effort**. The gate is the content script's own check and
is always right; nobody should make the icon authoritative.

No badge in the normal case. A badge that is always there says nothing.

## Testing

Deterministic and worth having: the app-window gate and its latch (a tab promoted to an
app window tells its subscribers exactly once; an app window that goes full screen stays
one), `isCompanionUrl` in
`shared/origin.ts` (the apex, a deeper label, a port, `https`, a host that merely
contains the suffix, `chrome://`, `file://` — table-driven, and the one piece that has
to agree with a match pattern Chrome enforces), the resize arithmetic, the message
guards, `iconStateFor`, the worker's router over a fake `chrome`, and a
`manifest.test.ts` asserting the permission arrays and the content script against
literals — a test that goes red the day someone adds one, and the place that pins
`optional_host_permissions` being *absent* and `host_permissions` being the one pattern
and nothing else.

Nothing goes in `tests/playwright/`. Every assertion an installed extension offers is
out of scope by that suite's own rules — a toolbar icon is pixels, key delivery is
synthetic input, a clipboard poll is timing — and loading an unpacked extension needs
`launchPersistentContext`, which the single-worker `logInAndConnect` / `returnToPicker`
harness is not built for and which would leak a profile between specs. It also cannot
open an app window, which is the only configuration the extension runs in.

So the irreducible half is manual, and all of it belongs in a shim window: Ctrl+W and
Ctrl+T reaching the remote with no fullscreen; Alt+F4 closing the window with no
dialog; copy while minimised; the echo loops in both directions; resize from the popup
at 1×, HiDPI, a `scale: 2` Retina remote and 125% zoom; and killing the service worker
from `chrome://extensions` mid-session, then confirming the next clipboard change still
arrives; and **the install itself** — *Install page as app…* from a tab with a live
session, confirming the seam comes up, the Help card's install row goes away and the
chords start reaching the remote **in the window Chrome just opened**, with nothing
closed and nothing reloaded. Plus three negatives: open the same gateway in an ordinary
tab and confirm the icon says off and the seam never wakes; open the same gateway at
`127.0.0.1` and confirm the redirect is what carries you onto the served host; and open
an unrelated site and confirm no content script runs in it at all.

## Distribution

**Load unpacked, for personal use. That is the whole of it** — no `.crx`, no policy
pinning, no Web Store listing, and nothing in the design that exists to satisfy a
reviewer.

Chrome cannot load a zip, a URL or a release; `Load unpacked` takes a **directory**, and
it re-reads that same absolute path on every browser start. So a GitHub release asset is
only ever transport, and the unzipped folder is the installation:

```sh
unzip -d ~/Applications/remotex-companion remotex-companion-<version>.zip
# chrome://extensions → Developer mode → Load unpacked → that folder
```

**No file of the extension's is edited and no permission is granted** — there is nothing
to click in `chrome://extensions` beyond loading the folder. The one thing that is
configured is on the other side: the gateway's own config must set
`[server].dev_subdomain`, which is what puts a loopback browser on
`http://<label>.remotex.localhost:<port>/`. Open that in an app window and the companion
is already there.

Updating is unzipping the next release over the same folder and pressing Reload. A
versioned folder instead is a new extension ID, which now costs nothing but is still
worth knowing: with the host in the manifest there is no per-profile state left to lose.

Two things that cannot be fixed and should not be discovered later: there is **no
auto-update**, because self-updating means a `.crx` with an `update_url`; and Developer
mode stays on, which Chrome nags about at startup on Windows.

The same directory loads in Edge, Brave, Opera and Vivaldi, all of which have app
windows of their own. Not Firefox: no `chrome.offscreen`, no app windows, and no
Keyboard Lock for the tab path either.

The `companion` job in `.github/workflows/release.yml` builds that zip, and its second
reason for existing is worth more than the asset: it runs `bun run check` over
`apps/companion` on a runner with no `frontend/node_modules` and no
`apps/viewer/node_modules`, which is the exact breakage `companion.contract.ts`'s
no-React rule and `geometry.ts`'s no-imports rule exist to prevent, and which nothing
else catches.

## Costs, stated

1. **Only `*.remotex.localhost` is served**, and a gateway reached at any other address
   gets no companion at all — no LAN name, no reverse proxy, not even a plain
   `localhost` without `[server].dev_subdomain` set. That is the price of the whole
   grant flow going away, and the icon says so on every window it does not serve.
2. **A match pattern cannot express a port**, so this covers every port on those names.
   Two gateways on one machine are two ports, so that much is wanted — but it is the
   whole of the check, and the check is a routing rule rather than a credential. **Any
   local process** that binds a port answers to `anything.remotex.localhost:<port>`, and
   a page it serves, opened in an app window, is handed the same clipboard bridge a
   gateway gets: the system clipboard while unfocused, in both directions. Nothing
   pairs, authenticates or asks. The bar is local code execution plus the user opening
   that address as an app, which for a personal development client was judged
   acceptable; a gateway-issued token exchanged over the page seam is what would close
   it, and there isn't one.
3. **The host is in the installed copy.** Changing it means editing `manifest.json` in
   the loaded folder or shipping a build that says something else — which is the cost
   the old per-grant design was paying its extra code to avoid, taken deliberately now
   that there is exactly one name to hard-code.
4. **Nothing works in a tab**, and that is deliberate rather than a gap to close later.
   The icon is what says so there, and the client's Help card is what says how to fix
   it; neither is load-bearing, so somebody can still end up wondering why a tab is
   quiet.
5. **Browser zoom** breaks "100%" regardless of window size. The arithmetic corrects
   for it and the popup says so; the client never scales anything to compensate.
6. **The shim's key behaviour is read from Chromium source and measured on macOS.**
   The reserved-key early return is cross-platform, but the per-key table in
   `PWA_KEYS.md` is not; a pass on Windows is owed.
7. **The toolbar icon is best-effort**, as above.
