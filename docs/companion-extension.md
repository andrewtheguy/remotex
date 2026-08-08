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

## Detection is asynchronous, and monotonic

`NATIVE_HOST` is read once at module load: the app's preload runs before any script in
the document and cannot appear later. The window kind is read once for the same sort of
reason. The companion can promise neither — its content script has to read its
whitelist out of `chrome.storage` first, and a site can be added to that whitelist
mid-session. So this is a store, not a constant:

```
probing ──► connected ──► absent        (never back to probing)
   └──────► absent
```

`probing` is not the same answer as `absent`, and the difference is load-bearing. The
one behaviour that hangs off the phase is the focus-driven clipboard read in
`useRemoteDesktop.ts`, which stands down while `probing`: starting it and stopping it
a quarter of a second later would push the same text twice and put the browser's
clipboard permission prompt on screen for nothing. When the phase settles to `absent`
the effect re-runs, and its own trailing call covers the delay.

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
    whitelist.ts              PURE — the matcher, and the most testable piece here
    settings.ts               chrome.storage.local
    messages.ts               ToWorker / ToContent / ToOffscreen + type guards
    geometry.ts               re-exports apps/viewer/src/main/geometry.ts
    resize.ts                 PURE window arithmetic
  src/worker/                 stateless router, ensureOffscreen, per-window icon, resize
  src/content/                the app-window and whitelist gates, and the page bridge
  src/offscreen/              the clipboard poller
  src/popup/                  the state card and Resize to display
  src/options/                the whitelist and the two feature toggles
  scripts/build.ts            Bun.build, mirroring apps/viewer/scripts/build.ts
  tests/
```

Toolchain mirrors `apps/viewer` exactly: TypeScript, `Bun.build`, biome,
`tsc --noEmit`, `bun test tests`.

### Permissions

Static `http://*/*` and `https://*/*` host permissions, gated by a whitelist the user
maintains. Not `optional_host_permissions` — the trade is stated below.

`http://` is in the list for `http://localhost` and `http://127.0.0.1`, which are
secure contexts. It is not a second path for insecure origins: this client refuses to
start outside a secure context, so the content script does nothing on one either.

No `tabs` permission (host permissions already grant `tab.url`), no
`externally_connectable`, no `web_accessible_resources` — the last so the extension's
presence cannot be probed by URL from an arbitrary page.

### The whitelist

```
entry   := [ scheme "://" ] hostpat [ ":" port ]
scheme  := "http" | "https"          absent ⇒ either
hostpat := labels | "*." labels | ipv4 | "[" ipv6 "]"
port    := 1..65535                  absent ⇒ any port
```

- Hosts are compared **label-wise on `split(".")`, never as substrings**, so
  `example.com` matches neither `notexample.com` nor `example.com.evil.net` nor a URL
  with the host in its query.
- `*.corp.example.com` matches subdomains **and** the apex — Chrome's own match-pattern
  convention, so it behaves the way anyone who has written a manifest expects.
- A wildcard needs at least two labels after `*.`, so `*.com` and bare `*` are parse
  errors. Not a public-suffix check; just the cheap rule that stops the whitelist
  becoming `<all_urls>` by the back door.
- A path is a parse error rather than a silent truncation: the client is a SPA whose
  path changes under the content script.
- Stored in `chrome.storage.local`, not `sync`. A list of internal gateway hostnames
  is not something to put in a Google account.

The content script's gate is two checks, in this order: **an app window** — the same
`display-mode` allow-list the client uses, so both ends of the bus agree by
construction — and then the whitelist, read straight out of `chrome.storage`. Neither
needs a service worker, which Chrome may have killed, and together they decide whether
the page ever learns the extension exists. The worker re-checks `sender.tab.url` on
every inbound message, so a content script left running in a window whose site was just
removed cannot still relay.

A whitelist edit reaches open windows through `chrome.storage.onChanged`, which fires
in content scripts too. An app window's content script keeps that one listener even
while un-whitelisted — it is invisible to the page — and flips: off→on posts `hello`
and installs the page listener, on→off posts `bye` and removes it. No reload, no
re-injection. That is what makes editing the list from a normal browser window while
the shim stays open work at all. A tab's content script registers nothing, because
nothing about a tab can change into a case it serves.

**Nothing is posted before both gates pass.** A `hello` on every page would tell every
site on the internet that this user runs a remote-desktop extension, and which version.

### The popup, and Resize to display

Triggered from the popup only. The page's floating menu gains nothing: it would be a
control that exists in one browser configuration and not another, and the popup is
reachable from the app window anyway.

The popup is a state card and one button. The card is `NativeState` as last reported —
which target, the framebuffer as `1920 × 1080 @2x`, whether the clipboard bridge is on
— and **Resize to display**, disabled unless a size has been reported and
`capabilities.resize` is set. The host row shows the whitelist entry covering this
window, with a switch; where the only thing covering it is a wildcard, that is said
rather than the broad rule silently deleted.

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
up), `onActivated`, `windows.onFocusChanged` and `storage.onChanged`. Per-tab icon state
is reset by Chrome on navigation, so `onUpdated` is required rather than an
optimisation.

Off is the honest answer for the same window in a tab, which is where most of the icon's
work is: the difference between "this site is not whitelisted" and "this is not an app
window" is the whole of what someone needs told, and the title says which.

The icon is **cosmetic and best-effort**. The gate is the content script's two checks
and is always right; nobody should make the icon authoritative.

No badge in the normal case. A badge that is always there says nothing.

## Testing

Deterministic and worth having: the whitelist matcher (table-driven, and the most
valuable file in the tree), the app-window gate, the resize arithmetic, the message
guards, `iconStateFor`, the worker's router over a fake `chrome`, and a
`manifest.test.ts` asserting the permission array against a literal — a test that goes
red the day someone adds one.

Nothing goes in `tests/playwright/`. Every assertion an installed extension offers is
out of scope by that suite's own rules — a toolbar icon is pixels, key delivery is
synthetic input, a clipboard poll is timing — and loading an unpacked extension needs
`launchPersistentContext`, which the single-worker `logInAndConnect` / `returnToPicker`
harness is not built for and which would leak a profile between specs. It also cannot
open an app window, which is the only configuration the extension runs in.

So the irreducible half is manual, and all of it belongs in a shim window: Ctrl+W and
Ctrl+T reaching the remote with no fullscreen; Alt+F4 raising the leave-site dialog
instead; copy while minimised; the echo loops in both directions; resize from the popup
at 1×, HiDPI, a `scale: 2` Retina remote and 125% zoom; the whitelist edited from a
normal browser window reaching the open shim with no reload; and killing the service
worker from `chrome://extensions` mid-session. Plus one negative: open the same gateway
in an ordinary tab and confirm the icon says off and the seam never wakes.

## Distribution

**Load unpacked, for personal use. That is the whole of it** — no `.crx`, no policy
pinning, no Web Store listing, and nothing in the design that exists to satisfy a
reviewer.

One thing still has to be done properly. Generate a key once and commit only the derived
public `"key"` field into the manifest: without it the extension ID changes on every
unpacked reload, and a new ID is a new `chrome.storage.local`, which is the whitelist
gone. Keep the `.pem` out of the repo.

The same directory loads in Edge, Brave, Opera and Vivaldi, all of which have app
windows of their own. Not Firefox: no `chrome.offscreen`, no app windows, and no
Keyboard Lock for the tab path either.

## Costs, stated

1. **Static broad host permissions run a content script in every http/https renderer.**
   Mitigated as far as it can be — it posts nothing and registers no page listener
   before both gates pass, so it is invisible and non-fingerprintable — but it is still
   code everywhere. `optional_host_permissions` plus
   `chrome.scripting.registerContentScripts` gives an identical whitelist UX with no
   ambient access; because the matcher is a pure module the switch stays cheap. Note
   that Chrome match patterns cannot express a port, so `https://host:8443` would have
   to register as `https://host/*` and be narrowed by our own matcher afterwards. The
   grammar is designed so that remains possible.
2. **Nothing works in a tab**, and that is deliberate rather than a gap to close later.
   The icon is what says so there, and the client's Help card is what says how to fix
   it; neither is load-bearing, so somebody can still end up wondering why a tab is
   quiet.
3. **Browser zoom** breaks "100%" regardless of window size. The arithmetic corrects
   for it and the popup says so; the client never scales anything to compensate.
4. **The shim's key behaviour is read from Chromium source and measured on macOS.**
   The reserved-key early return is cross-platform, but the per-key table in
   `PWA_KEYS.md` is not; a pass on Windows is owed.
5. **The toolbar icon is best-effort**, as above.
