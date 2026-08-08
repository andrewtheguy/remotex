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

It is not built yet; this is its design.

## App windows only

**The extension does nothing in an ordinary tab.** The content script exits, the page
posts nothing, and `frontend/src/companion.ts` settles to `absent` on the first render
without a handshake. That is the simplification the whole design rests on: one window
kind to serve, no capability that is present in one and missing in the other, and no
state where half of it works.

A tab is not left unserved so much as served by the client itself. Full screen plus
Keyboard Lock (`frontend/src/immersive.ts`) gives a tab the chords, the close guard in
`useRemoteDesktop.ts` gives it the leave-site dialog, and the clipboard falls back to
the page's own focus-driven sync. Nothing there is worse for the extension declining to
participate; the answer to "I want more" is the app window, which is one menu item away.

## The shim window

Make one with *Install page as app…* from the Chrome menu, which also leaves a
Start-menu or desktop shortcut, or launch it directly:

```
chrome.exe --app=https://gateway.example/
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
neither a keyboard lock nor an extension changes that. The close guard in
`useRemoteDesktop.ts` is the answer to the first of them: the browser's own leave-site
dialog, armed while a session is live.

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

## Detection is asynchronous, and it can change back

`NATIVE_HOST` is read once at module load: the app's preload runs before any script in
the document and cannot appear later. The window kind is read once for the same sort of
reason. The companion can promise neither. Nothing tells a page synchronously whether a
content script was injected into it, so the only way to find out is to ask and wait, and
a bfcache restore makes the question worth asking twice. So this is a store, not a
constant, and every settled answer can be unsettled:

| from | on | to | |
|---|---|---|---|
| `probing` | `hello` | `connected` | |
| `probing` | the deadline | `absent` | 1.5 s of silence |
| `connected` | `bye` | `absent` | this site's host access was revoked |
| `absent` | `hello` | `connected` | granted, and injected without a reload |
| `absent` | `pageshow` | `probing` | a bfcache restore, which re-arms the deadline with it |
| `connected` | `pageshow` | `connected` | a hello goes out, but there is nothing to re-probe |

Only a `pageshow` returns anything to `probing`, and it re-arms the deadline as it
does — so there is no path to a phase that waits for an answer nothing will settle.

Both middle rows are a site's host access changing under a live window, which is a thing
Chrome lets the user do at any moment from its own site-access UI as much as from this
extension's popup. What `bye` does **not** cover is the extension being disabled or
reloaded outright: that takes its content script's context with it before anything can
be said, so a page can be left believing in a companion that has gone until it is
reloaded.

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

A tab never enters `probing` at all. It is `absent` from the first read, so that
stand-down costs nothing where there is nothing to wait for.

The handshake is symmetric — both sides say `hello`, and the page says it again on
`pageshow`, because a bfcache restore replays no content-script injection. Either side
can be first and neither assumes it.

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

## What the extension will contain

```
apps/companion/
  src/manifest.json           MV3; version injected from Cargo.toml at build time
  src/shared/
    contract.ts               type-imports frontend/src/companion.contract.ts
    messages.ts               ToWorker / ToContent / ToOffscreen + type guards
    origin.ts                 PURE — a tab URL to the origin pattern to ask for
    geometry.ts               re-exports apps/viewer/src/main/geometry.ts
    resize.ts                 PURE window arithmetic
  src/worker/                 stateless router, grants, ensureOffscreen, icon, resize
  src/content/                the app-window gate and the page bridge
  src/offscreen/              the clipboard poller
  src/popup/                  the state card, the site switch and Resize to display
  scripts/build.ts            Bun.build, mirroring apps/viewer/scripts/build.ts
  tests/
```

There is no options page and no `chrome.storage`. The extension holds no settings of
its own; the list of sites it serves is Chrome's, see below.

Toolchain mirrors `apps/viewer` exactly: TypeScript, `Bun.build`, biome,
`tsc --noEmit`, `bun test tests`.

### The host list is Chrome's grants

The extension ships with **no host access at all** and no static content script. What it
declares is that it may ask:

```json
"optional_host_permissions": ["http://*/*", "https://*/*"],
"permissions": ["scripting", "activeTab", "offscreen", "clipboardRead", "clipboardWrite"]
```

Nothing is granted by that. A site is added by opening the popup on it and turning it
on, which calls `chrome.permissions.request({ origins })` from the click — Chrome asks,
in Chrome's own words — and on a grant the worker registers a content script for that
origin and injects it into the open window once, so the seam comes up without a reload.
Turning it off is `permissions.remove` and `unregisterContentScripts`, and the site can
equally be revoked from `chrome://extensions` or the icon's own right-click menu, which
the worker hears through `permissions.onRemoved` and answers with a `bye`.

**Chrome stores the grants, and stores them outside the installed directory** — in the
profile, keyed by extension ID. That is the whole reason this design is worth its extra
code: an unpacked extension has no auto-update, so every release is a folder overwritten
by hand, and a list living in `manifest.json` would be overwritten with it. This one
survives, and installing needs no file edited at all.

`chrome.permissions.getAll()` **is** the host list. There is no second copy to keep in
step, no `chrome.storage`, no options page, and no matcher of ours — a grant is what
Chrome consults when it decides to inject, and the popup reads the same call back to
draw its switch. The worker re-registers from `getAll()` on `onInstalled` and
`onStartup` rather than trusting `persistAcrossSessions` across an update: the grant is
the durable thing, and registration is derived from it.

Two properties of Chrome's origin patterns matter before asking for one. `*.host`
matches subdomains **and** the apex, so `https://*.corp.example.com/*` covers
`corp.example.com` too. And **a pattern cannot express a port**: a gateway on
`https://gateway.example.com:8443` is asked for as `https://gateway.example.com/*`,
which covers every port on that host. Deriving that pattern from the tab's URL is
`shared/origin.ts`, which is pure and is where the port is dropped and a non-`http(s)`
URL is refused.

`http://` is in the optional list for `http://localhost` and `http://127.0.0.1`, which
are secure contexts. It is not a second path for insecure origins: this client refuses
to start outside one, so a content script on an insecure origin would find nothing to
talk to.

The content script's own gate is one check: **an app window**, using the same
`display-mode` allow-list the client uses, so both ends of the bus agree by
construction. It needs no service worker, which Chrome may have killed, and it decides
whether the page ever learns the extension exists.

No `storage`, because there is nothing to store. No `tabs` — `activeTab` gives the
popup the URL of the tab it was opened on, which is the only one it needs, and a granted
site gives `tab.url` for the rest. No `externally_connectable` and no
`web_accessible_resources`, the last so the extension cannot be probed by URL even from
the gateway's own page.

### The popup, and Resize to display

Triggered from the popup only. The page's floating menu gains nothing: it would be a
control that exists in one browser configuration and not another, and the popup is
reachable from the app window anyway.

The popup is a switch, a state card and one button.

The **switch** is the site's host access, and it is the only place a site is ever added.
`activeTab` gives the popup the URL of the window it was opened on, `shared/origin.ts`
turns that into the pattern to ask for, and the click is the user gesture
`chrome.permissions.request` requires. Off calls `permissions.remove`. On a window with
no grant that switch is the entire popup — the correct rendering, not an empty state.

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
up), `onActivated`, `windows.onFocusChanged` and `permissions.onAdded`/`onRemoved`.
Per-tab icon state is reset by Chrome on navigation, so `onUpdated` is required rather
than an optimisation.

A `tab.url` the worker cannot read is a site with no grant, which is the honest off
state and costs no lookup to determine. The two off cases it has to tell apart in its
title are "this site has not been turned on" — the thing the popup fixes — and "this is
not an app window", which the popup cannot fix and the client's Help card explains.

The icon is **cosmetic and best-effort**. The gate is the content script's own check and
is always right; nobody should make the icon authoritative.

No badge in the normal case. A badge that is always there says nothing.

## Testing

Deterministic and worth having: the app-window gate, `originPatternFor` in
`shared/origin.ts` (a URL with a port, a wildcard host, `chrome://`, `file://`, a
gateway behind a path — table-driven, and the one piece where a mistake grants more
than was meant), the reconciliation of registered scripts against
`permissions.getAll()`, the resize arithmetic, the message guards, `iconStateFor`, the
worker's router over a fake `chrome`, and a `manifest.test.ts` asserting the permission
arrays against literals — a test that goes red the day someone adds one, and the place
that pins `host_permissions` being *absent* and `optional_host_permissions` being the
two broad patterns and nothing else.

Nothing goes in `tests/playwright/`. Every assertion an installed extension offers is
out of scope by that suite's own rules — a toolbar icon is pixels, key delivery is
synthetic input, a clipboard poll is timing — and loading an unpacked extension needs
`launchPersistentContext`, which the single-worker `logInAndConnect` / `returnToPicker`
harness is not built for and which would leak a profile between specs. It also cannot
open an app window, which is the only configuration the extension runs in.

So the irreducible half is manual, and all of it belongs in a shim window: Ctrl+W and
Ctrl+T reaching the remote with no fullscreen; Alt+F4 raising the leave-site dialog
instead; copy while minimised; the echo loops in both directions; resize from the popup
at 1×, HiDPI, a `scale: 2` Retina remote and 125% zoom; a site granted from the popup
and picked up **without a reload**, and revoked from `chrome://extensions` so the page
sees the `bye`; and killing the service worker from `chrome://extensions` mid-session,
then confirming the next clipboard change still arrives. Plus two negatives: open the
same gateway in an ordinary tab and confirm the icon says off and the seam never wakes,
and open an unrelated site and confirm no content script runs in it at all.

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

Then click the toolbar icon on the gateway and turn the site on. **No file is edited at
any point**, which is the point: nothing in the installation is yours to preserve.

Updating is unzipping the next release over the same folder and pressing Reload. Same
path, so the same extension ID — Chrome derives an unpacked extension's ID from the
directory path — so the granted sites are still there. Unzip to a versioned folder
instead and it is a new extension with nothing granted, which is worth knowing before
doing it once.

Two things that cannot be fixed and should not be discovered later: there is **no
auto-update**, because self-updating means a `.crx` with an `update_url`; and Developer
mode stays on, which Chrome nags about at startup on Windows.

The same directory loads in Edge, Brave, Opera and Vivaldi, all of which have app
windows of their own. Not Firefox: no `chrome.offscreen`, no app windows, and no
Keyboard Lock for the tab path either.

The release job that builds that zip earns its place for a second reason: it runs
`bun run check` over `apps/companion` on a runner with no `frontend/node_modules`, which
is the exact breakage `companion.contract.ts`'s no-React rule exists to prevent and
which nothing else catches. It is not in `.github/workflows/release.yml` yet, because a
job that builds a directory that does not exist fails the release.

## Costs, stated

1. **A match pattern cannot express a port**, so granting a gateway on a non-default
   port grants every port on that host. On a personal gateway that host is trusted
   completely already; where it is not, one `location.port` check in the content script
   narrows it.
2. **`optional_host_permissions` is `http://*/*` and `https://*/*`**, which is as broad
   as a declaration gets, and `chrome://extensions` says so in those words. What it
   grants is nothing: no site is reachable and no content script exists anywhere until
   the user turns one on, and each is one origin. The narrower alternative is a fixed
   list in the manifest, which cannot be added to without editing the installed copy and
   loses every entry on update — the cost this design was chosen to avoid.
3. **The grants live in the Chrome profile**, so they follow the browser rather than the
   installation, and unzipping an update to a *different* path is a different extension
   ID with nothing granted.
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
