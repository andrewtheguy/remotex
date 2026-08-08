# The companion extension

`remotex.app` ([docs/macos-viewer.md](macos-viewer.md)) is macOS-only. Everywhere
else the client is a browser tab, and a browser tab cannot do two of the things that
shell does:

- **read and write the system clipboard while the window is unfocused or minimised.**
  A page may only `navigator.clipboard.readText()` while focused, and may only write
  with a user gesture behind it.
- **resize its own window** to the remote's framebuffer.

`apps/companion/` is a Chrome MV3 extension supplying exactly those two, on a
per-host whitelist. It is not built yet; this is its design.

## The scope rule

**The extension does only what the browser cannot.** Anything a page can do for
itself stays in the page, because there is one client and it is the page a browser
loads — a capability implemented in an extension as well is a second implementation
of the same thing, drifting from the day it lands.

That rule already decided two features out of it. Full screen and Keyboard Lock are
plain web APIs, so `frontend/src/immersive.ts` owns them and they work in stock
Chromium with nothing installed; `beforeunload` is a page event, and only the page
knows whether a session is live, so the close guard is in `useRemoteDesktop.ts`. The
spike this design comes from put both in a content script, and only because it had no
client to put them in.

## `NATIVE_HOST` stays false

`NATIVE_HOST` does not mean "there is something native here". It means *a shell owns
the window chrome*, and the client answers by hiding its floating menu, dropping the
login screen and handing the panel actions out for a menu bar to drive. The extension
owns no chrome. Under it the client looks and behaves exactly as it does in any
browser.

So the extension gets its own seam, and it is much narrower — two files, both already
in the tree:

| | |
|---|---|
| `frontend/src/companion.contract.ts` | The wire types. React-free, so the extension can type-import it with no `frontend/node_modules` installed — the same rule and the same CI reason as `nativeHost.contract.ts`. |
| `frontend/src/companion.ts` | The store, the hooks and the three guards on the message bus. |

`NativeState` is reused verbatim rather than trimmed. `RemoteDesktop.tsx` builds it
once per render for the menu bar; a second, smaller state object would be a second
thing to keep in step for the sake of fields the popup happens not to read yet.

What is deliberately absent is every other `NativeCommand` variant — `openClipboard`,
`selectDisplay`, `setAudio`, `takeOver`, `sendKeyCombo`. Each exists because the shell
*hides* the floating menu. Under the extension that menu is on screen, and a popup
offering the same buttons would be a second UI for the same controls.

Resize is on neither side of the seam. The page reports the framebuffer in
`NativeState.size` because the menu bar already needed it, and the extension measures
the window from its own content script — a resize is arithmetic over two things it
already has.

## Detection is asynchronous, and monotonic

`NATIVE_HOST` is read once at module load: the app's preload runs before any script in
the document and cannot appear later. The companion cannot promise that. Its content
script has to read its whitelist out of `chrome.storage` first, and a site can be
added to that whitelist mid-session. So this is a store, not a constant:

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
  src/worker/                 stateless router, ensureOffscreen, per-tab icon, resize
  src/content/                the whitelist gate and the page bridge
  src/offscreen/              the clipboard poller
  src/popup/  src/options/
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

Read at both ends, which is not duplication. The **content script** reads storage
directly and gates itself: that is the authoritative check, it needs no service worker
(Chrome may have killed it), and it decides whether the page ever learns the extension
exists. The **worker** re-checks `sender.tab.url` on every inbound message, so a
content script left running in a tab whose site was just removed cannot still relay.

A whitelist edit reaches open tabs through `chrome.storage.onChanged`, which fires in
content scripts too. The content script keeps that one listener even while
un-whitelisted — it is invisible to the page — and flips: off→on posts `hello` and
installs the page listener, on→off posts `bye` and removes it. No reload, no
re-injection.

**Nothing is posted before the whitelist matches.** A `hello` on every page would tell
every site on the internet that this user runs a remote-desktop extension, and which
version.

### Resize to display

Triggered from the extension popup only. The page's floating menu gains nothing: it
would be a control that exists in one browser configuration and not another.

`apps/viewer/src/main/geometry.ts` is already exactly this arithmetic — pure,
importing nothing, tested, and already carrying the rule that matters most here: the
window is fitted to the framebuffer, and the framebuffer is never scaled to the
window. It is re-exported rather than copied.

The one subtlety is browser zoom. `innerWidth` is CSS pixels *at the current zoom*,
while `outerWidth` and `chrome.windows.update` are device-independent pixels, which
zoom does not touch. So a `w/scale` CSS-pixel framebuffer needs `w/scale * zoom` DIPs
of content, and the chrome is `outerWidth - innerWidth * zoom`. At any zoom but 1 the
desktop is not at 100% however the window is sized, so the popup says so and offers a
Reset zoom button — surfaced, never silently corrected.

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

Two variants, painted per `tabId` from `chrome.tabs.onUpdated` (on `loading` *and*
whenever `changeInfo.url` is set — that is how a SPA's `pushState` shows up),
`onActivated`, `windows.onFocusChanged` and `storage.onChanged`. Per-tab icon state is
reset by Chrome on navigation, so `onUpdated` is required rather than an optimisation.

The icon is **cosmetic and best-effort**. The gate is the content script's whitelist
check and is always right; nobody should make the icon authoritative.

No badge in the normal case. A badge that is always there says nothing.

## Testing

Deterministic and worth having: the whitelist matcher (table-driven, and the most
valuable file in the tree), the resize arithmetic, the message guards, `iconStateFor`,
the worker's router over a fake `chrome`, and a `manifest.test.ts` asserting the
permission array against a literal — a test that goes red the day someone adds one.

Nothing goes in `tests/playwright/`. Every assertion an installed extension offers is
out of scope by that suite's own rules — a toolbar icon is pixels, key delivery is
synthetic input, a clipboard poll is timing — and loading an unpacked extension needs
`launchPersistentContext`, which the single-worker `logInAndConnect` / `returnToPicker`
harness is not built for and which would leak a profile between specs.

The irreducible half is manual: copy while minimised, the echo loops in both
directions, the icon flipping in an already-open tab when the whitelist is edited,
resize at 1×, HiDPI, a `scale: 2` Retina remote and 125% zoom, and killing the service
worker from `chrome://extensions` mid-session.

## Distribution

Load unpacked for personal use. Generate a key once and commit only the derived public
`"key"` field into the manifest — without it the extension ID changes on every
unpacked reload, which loses `chrome.storage.local`, which is the whitelist. A `.crx`
pinned through `ExtensionSettings` policy is the managed path; an unlisted Web Store
listing is the low-friction one, and the broad host permission is what a reviewer will
ask about.

The same package loads in Edge, Brave, Opera and Vivaldi. Not Firefox: no
`chrome.offscreen`, and no Keyboard Lock.

## Costs, stated

1. **Static broad host permissions run a content script in every http/https renderer.**
   Mitigated as far as it can be — it posts nothing and registers no page listener
   before the whitelist matches, so it is invisible and non-fingerprintable — but it is
   still code everywhere, and it is the reason a Web Store reviewer will ask questions.
   `optional_host_permissions` plus `chrome.scripting.registerContentScripts` gives an
   identical whitelist UX with no ambient access; because the matcher is a pure module
   the switch stays cheap. Note that Chrome match patterns cannot express a port, so
   `https://host:8443` would have to register as `https://host/*` and be narrowed by
   our own matcher afterwards. The grammar is designed so that remains possible.
2. **Browser zoom** breaks "100%" regardless of window size. The arithmetic corrects
   for it and the popup says so, rather than the client scaling anything.
3. **The toolbar icon is best-effort**, as above.
