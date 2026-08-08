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
| `absent` | `hello` | `connected` | a late one still counts |
| `absent` | `pageshow` | `probing` | a bfcache restore, which re-arms the deadline with it |
| `connected` | `pageshow` | `connected` | a hello goes out, but there is nothing to re-probe |

Only a `pageshow` returns anything to `probing`, and it re-arms the deadline as it
does — so there is no path to a phase that waits for an answer nothing will settle.

**`connected` is where it stops, and there is no goodbye.** A window either has a
content script for its whole life or never had one, since the hosts are match patterns
in the manifest; and the two ways an extension goes away mid-life — disabled, or
reloaded from `chrome://extensions` — tear its content script's context down without
giving it a turn to speak. So a page whose companion is disabled under it goes on
believing in one until it is reloaded. That is the cost of a seam with no teardown
message, and it is smaller than carrying a message nothing can ever send.

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
    geometry.ts               re-exports apps/viewer/src/main/geometry.ts
    resize.ts                 PURE window arithmetic
  src/worker/                 stateless router, ensureOffscreen, per-window icon, resize
  src/content/                the app-window gate and the page bridge
  src/offscreen/              the clipboard poller
  src/popup/                  the state card and Resize to display
  scripts/build.ts            Bun.build, mirroring apps/viewer/scripts/build.ts
  tests/
```

There is no options page and no `chrome.storage`. The extension holds no settings of
its own; see below.

Toolchain mirrors `apps/viewer` exactly: TypeScript, `Bun.build`, biome,
`tsc --noEmit`, `bun test tests`.

### The host list is the manifest

The gateways this extension serves are `content_scripts[0].matches` and
`host_permissions` in `manifest.json`, and nowhere else. Chrome's own match patterns,
Chrome's own matcher, applied by Chrome before any of this code runs.

That is not a shortcut around a whitelist; it *is* the whitelist, and it is a strictly
better one. A stored list would need a grammar, a matcher careful enough to compare
hosts label-wise rather than as substrings, an options page to edit it, a
`chrome.storage.onChanged` path to push edits into open windows, and a re-check in the
worker for a content script still running on a site that had just been removed — every
line of it reimplementing, at runtime and less well, a decision Chrome makes at load
time. It would also mean broad `http://*/*` and `https://*/*` host permissions and a
content script in every renderer on the machine, since a runtime list can only narrow
what the manifest already granted.

With the list in the manifest the extension has no ambient access at all. It exists in
the renderers of the hosts named in it and in no others, so there is no page that could
learn it is installed, and nothing to gate.

```json
"host_permissions": ["https://gateway.example.com/*"],
"content_scripts": [{ "matches": ["https://gateway.example.com/*"], … }]
```

`http://` patterns are allowed for `http://localhost` and `http://127.0.0.1`, which are
secure contexts. That is not a second path for insecure origins: this client refuses to
start outside one, so a content script on an insecure origin would find nothing to talk
to.

Two properties of Chrome's patterns are worth knowing before writing one. `*.host` is
the pattern that matches subdomains **and** the apex, so `https://*.corp.example.com/*`
covers `corp.example.com` too. And **a match pattern cannot express a port**: a gateway
on `https://gateway.example.com:8443` is written `https://gateway.example.com/*`, which
covers every port on that host. On a personal gateway that is a host already trusted
completely; where it is not, one `location.port` check at the top of the content script
is three lines and needs no grammar.

Editing the list is editing that file in the installed copy and pressing Reload in
`chrome://extensions`. See [Distribution](#distribution). The one thing Chrome will not
do is inject into a window that is already open, so a window open across the edit is
reopened. That is the whole of what the `storage.onChanged` machinery bought.

The content script's remaining gate is one check: **an app window**, using the same
`display-mode` allow-list the client uses, so both ends of the bus agree by
construction. It needs no service worker, which Chrome may have killed, and it decides
whether the page ever learns the extension exists.

The rest of the manifest is three permissions — `offscreen`, `clipboardRead`,
`clipboardWrite` — and that is the whole list. No `storage`, because there is nothing to
store. No `tabs`, because host permissions already grant `tab.url` for the hosts that
matter. No `externally_connectable` and no `web_accessible_resources`, the last so the
extension cannot be probed by URL even from the gateway's own page.

### The popup, and Resize to display

Triggered from the popup only. The page's floating menu gains nothing: it would be a
control that exists in one browser configuration and not another, and the popup is
reachable from the app window anyway.

The popup is a state card and one button. The card is `NativeState` as last reported —
which target, the framebuffer as `1920 × 1080 @2x`, whether the clipboard bridge is on
— and **Resize to display**, disabled unless a size has been reported and
`capabilities.resize` is set. There is no host row and no switch: the popup opens on a
window the extension is already running in, so there is nothing there to decide.

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

The question it answers is now only the app-window one. A `tab.url` the worker cannot
read is a host the manifest does not name, and the toolbar icon is greyed everywhere by
default anyway; what is left to say is why the same gateway is quiet in an ordinary tab,
and the title says it.

The icon is **cosmetic and best-effort**. The gate is the content script's own check and
is always right; nobody should make the icon authoritative.

No badge in the normal case. A badge that is always there says nothing.

## Testing

Deterministic and worth having: the app-window gate, the resize arithmetic, the message
guards, `iconStateFor`, the worker's router over a fake `chrome`, and a
`manifest.test.ts` asserting the permission array against a literal — a test that goes
red the day someone adds one. It cannot assert the host patterns, which are the
installation's business rather than the repo's; what it can assert is that the
repository's copy names an example host and not a wildcard.

Nothing goes in `tests/playwright/`. Every assertion an installed extension offers is
out of scope by that suite's own rules — a toolbar icon is pixels, key delivery is
synthetic input, a clipboard poll is timing — and loading an unpacked extension needs
`launchPersistentContext`, which the single-worker `logInAndConnect` / `returnToPicker`
harness is not built for and which would leak a profile between specs. It also cannot
open an app window, which is the only configuration the extension runs in.

So the irreducible half is manual, and all of it belongs in a shim window: Ctrl+W and
Ctrl+T reaching the remote with no fullscreen; Alt+F4 raising the leave-site dialog
instead; copy while minimised; the echo loops in both directions; resize from the popup
at 1×, HiDPI, a `scale: 2` Retina remote and 125% zoom; a host added to the installed
manifest and picked up on Reload; and killing the service worker from
`chrome://extensions` mid-session. Plus one negative: open the same gateway in an
ordinary tab and confirm the icon says off and the seam never wakes.

## Distribution

**Load unpacked, for personal use. That is the whole of it** — no `.crx`, no policy
pinning, no Web Store listing, and nothing in the design that exists to satisfy a
reviewer.

The installation is a **copy of `dist/` living outside this repository**, and that is
what makes the manifest a reasonable place to keep a host list. The repository holds the
code and a manifest naming an example host; the copy holds the gateways, which are the
one part of this that is nobody else's business. `andrewtheguy/remotex` is public, and
internal hostnames do not belong in it.

```sh
cd apps/companion && bun run build
cp -R dist ~/Applications/remotex-companion      # or anywhere outside the repo
$EDITOR ~/Applications/remotex-companion/manifest.json
```

Then Load unpacked, once. Adding a gateway later is that same editor and the Reload
button — no rebuild, no repository change, no options page. Updating the *code* is a
rebuild and a re-copy, at which point the two host patterns are pasted back in; if that
ever becomes tiresome, a copy step that preserves them is a few lines, but not before it
does.

Nothing here needs a stable extension ID. The `"key"` field and its `.pem` exist to keep
`chrome.storage.local` across reloads, and this extension stores nothing.

The same directory loads in Edge, Brave, Opera and Vivaldi, all of which have app
windows of their own. Not Firefox: no `chrome.offscreen`, no app windows, and no
Keyboard Lock for the tab path either.

## Costs, stated

1. **A match pattern cannot express a port**, so a gateway on a non-default port is
   named by host and every port on that host is covered. On a personal gateway that host
   is trusted completely already; where it is not, one `location.port` check in the
   content script narrows it.
2. **The host list lives in the installed copy, not in the repository**, so it is not
   backed up by anything that backs up this repository, and a fresh install is two
   patterns retyped. That is the price of not publishing them.
3. **Nothing works in a tab**, and that is deliberate rather than a gap to close later.
   The icon is what says so there, and the client's Help card is what says how to fix
   it; neither is load-bearing, so somebody can still end up wondering why a tab is
   quiet.
4. **Browser zoom** breaks "100%" regardless of window size. The arithmetic corrects
   for it and the popup says so; the client never scales anything to compensate.
5. **The shim's key behaviour is read from Chromium source and measured on macOS.**
   The reserved-key early return is cross-platform, but the per-key table in
   `PWA_KEYS.md` is not; a pass on Windows is owed.
6. **The toolbar icon is best-effort**, as above.
