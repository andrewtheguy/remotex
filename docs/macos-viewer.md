# remotex.app

A macOS window around the client, built with Electron and living in
[`apps/viewer`](../apps/viewer).

It is a **shell**, not a second client. Everything below the title bar is the same
SPA a browser loads, from the same `frontend/dist`, talking to a gateway over the
same sockets it uses in a browser — the session's, and `/ws/audio` with its own
queue beside it. The app owns the window, the menu bar, the pasteboard and the
gateway process, and nothing else: no claim, no session, no framebuffer, no wire
format. The protocol belongs to the client and the gateway, which is why there is no
version pair between the app and either of them.

What it adds is only what a page genuinely cannot do for itself:

- **Every ⌘ chord the menu bar would have taken reaches the guest**, ⌘W and ⌘Q
  included. A browser tab never sees them. What macOS keeps for itself — ⌘Space,
  ⌘Tab, the screenshot chords — no app sees, here included; **Send Keys** is the
  answer to those.
- **A clipboard that keeps syncing while the window is unfocused or minimised.**
  `navigator.clipboard.readText()` is refused unless the document is focused.
- **A real menu bar**, and sound started from it — a menu press is not a gesture in
  the page, so the client's `AudioContext` needs `--autoplay-policy` to start.
- **Resize to Display**, which fits the window to the remote rather than the remote
  to the window.
- **No context menu at all**, because the remote surface's right-click is the
  guest's.

The client hides its floating menu under `NATIVE_HOST` and the menu bar drives the
same actions. Its *panels* — the clipboard editor, the display list — stay the
client's: rebuilding those natively would be two of each to keep in step, and the
consent boundary (the panel's Copy button) would then exist in two places.

## The embedded gateway

The app carries its own gateway and runs it:

```
remotex-gateway serve-embedded --instance-dir <dir> --web-root <dir>
```

Those two arguments are the whole of it. `src/cli.rs` refuses `--port`,
`--gateway` and `--token` by design: the port and the secret are the gateway's to
decide, not the app's to pass.

Two pipes, in opposite directions, doing two unrelated jobs — the same pair
described from the other side in `src/embedded.rs`:

- **its stdout → the app**, once. After binding `127.0.0.1:0` it prints one line,
  `{"port":49213,"token":"…"}`, and never writes to stdout again. That is how the
  port and the token are learned, and why neither is guessed. The token is
  deliberately not argv (`ps`), not an environment variable (inherited) and not a
  file (outlives the process).
- **the app → its stdin**, never. Nothing is written on it in either direction. It
  stays open for as long as the app lives, so when the app ends — cleanly, crashed,
  Force Quit, `kill -9` — the kernel closes that end, the gateway reads
  end-of-file, and exits. **This is the layer the "no stray gateway" guarantee
  rests on**, because it needs no code of ours to run at the right moment.
  `SIGTERM` and `SIGKILL` sit on top of it, not instead of it.

Its stderr goes to `<instance>/gateway.log`, and the last 60 lines stay in memory
for the launch screen. A failure waits briefly for stderr to close before it is
reported: stdout's end-of-file can be delivered first, and a config error arriving a
moment late is the whole message.

`check-config --embedded` is the same code path the gateway starts on, which is what
lets the configuration editor delegate every question of validity to it. There is no
TOML parser in `apps/viewer` and there must never be one.

## The origin

The window loads `remotex://app/index.html` **from the bundle**, not from the
gateway. The reason is `localStorage`: the gateway listens on whatever port the
kernel hands it, so loading over `http://127.0.0.1:<port>` would put the client at a
new origin every launch, and the preferences it remembers would be dropped each
time — silently, with nothing on screen to say a preference had been lost.

The scheme is registered `standard`, `secure`, `corsEnabled` and `supportFetchAPI`,
and each of the four buys one thing: an origin `localStorage` can key on, a secure
context for WebCodecs, and the ability to reach loopback at all.

The cost is that the page then calls its own gateway cross-origin. `shell_origin_cors`
in `src/server.rs` answers exactly `remotex://app` — a literal constant, never
echoed — with `Access-Control-Allow-Credentials: true`, and only when
`allow_shell_origin` is set *and* the gateway authenticates by token. A served
gateway answers for no such origin.

The launch screen and the configuration editor are served from the same origin at
`/_shell/`, so the window never changes origin and there is no second
`BrowserWindow` for a screen that says one sentence.

## The launch sequence

Order matters, and most of it is not obvious:

1. Read `--instance-dir` out of argv, or take Electron's default `userData` —
   which already *is* `~/Library/Application Support/<CFBundleName>`. Read the
   default **before** overriding it.
2. Point the profile at `<instance>/electron`, then take the single-instance lock,
   so the lock lives in the instance's own profile and two instance directories
   coexist.
3. Append the Chromium switches and declare the scheme — both only possible before
   the app is ready.
4. Start the gateway; read its one line.
5. Put the token in the cookie jar and **await it** before loading anything.
6. Load `remotex://app/index.html`.

The cookie is `remotex_session` on `http://127.0.0.1:<port>`, `SameSite=None`
(the page and the gateway are genuinely different sites, and a `Lax` cookie is
simply not sent) and `Secure` (which `None` requires, and loopback is allowed to
be). Awaited before the load, or the client's first request arrives anonymous and
the gateway answers 401 with nothing on the wire to explain why.

## The keyboard

The whole design is one line:

```ts
window.webContents.setIgnoreMenuShortcuts(guestOwnsKeyboard(viewer));
```

**No item outside the App, Edit and Window menus carries a key equivalent.** While a
live, ready desktop has focus, the few that remain are suppressed, so ⌘Q, ⌘W and ⌘T
arrive in the page as ordinary `keydown` events and take the same path every other
key takes: one translator instance, one held-key set, one keyboard path, and it is
the client's (`macKeys.ts`, `capturesEveryChord`).

There is **no key monitor, no injected keys and no capture button**. `NativeCommand`
carries no `key` and no `releaseInput`, and `before-input-event` is not used —
its `preventDefault` kills page delivery as well as the menu shortcut, which is the
opposite of what is wanted.

Quit is not given away with the rest. `guestOwnsKeyboard` is false anywhere but a
live desktop, so ⌘Q quits from the picker, the launch screen, a panel's text box or
another app — and Quit is on the menu and the Dock tile regardless.
`NativeState.editing` is what hands the shortcuts back the moment a caret lands in
something typeable, so ⌘V pastes into the clipboard panel rather than into a guest
that cannot see the field.

macOS itself keeps ⌘Space, ⌘Tab, ⌘⇧3/4/5 and Ctrl-↑, and no app sees them.
**Remote ▸ Send Keys** is the way to send ⌥F4 and the bare modifier taps, and that
is what it is for — the keys the platform keeps, not the ones the shell hands over.

## Chromium switches

Replaced wholesale by `REMOTEX_CHROMIUM_SWITCHES`, so "is this switch the thing?" is
one relaunch away rather than a rebuild. Note *wholesale*: to add
`--remote-debugging-port=9222` you must re-spell the defaults beside it.

- `--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets`
  — Chromium ≥ 151 *hangs* a request from a public origin (which `remotex://app`
  is) to 127.0.0.1, with no error anywhere. The symptom is a black window and an
  empty `#root`. Inert on a build without the feature, which is why it stays either
  way.
- `--autoplay-policy=no-user-gesture-required` — menu-driven audio has no gesture
  in the page.
- `--use-mock-keychain`, `--password-store=basic` — otherwise macOS asks for the
  keychain in front of a window that has not painted, for a cookie jar whose only
  content is a token this launch minted.
- `--disable-pinch` — pointer clients show desktops at 100%.

## The instance directory

Everything one launch reads or writes, in one place: `remotex.toml` (0600),
`gateway.log`, and `electron/` (the profile — cookie jar and `localStorage`). The
directory is `0700`, because the config holds every target's credentials and a
directory anyone can list is one anyone can watch for a replacement.

Nothing under `/opt/remotex` is ever consulted. A Mac may run the server install and
this app at once and neither can change what the other does.

`--instance-dir` is the QA override, and QA should always use it:

```sh
open -n dist/mac-arm64/remotex.app --args --instance-dir "$PWD/tmp/app-qa"
```

The real instance is `~/Library/Application Support/remotex`. A second *installed*
instance is a second `productName` build from the same config — there is no
bundle-renaming script, because under Electron a renamed copy is a quarter of a
gigabyte and a re-sign of a framework and five helpers.

## Build and QA

```sh
cargo build --release                    # the gateway that goes in the bundle
(cd frontend && bun run build)           # the client it shows
(cd apps/viewer && bun run dist)         # remotex.app + the disk image
```

`bun run start` in `apps/viewer` runs it unpackaged against the same two build
outputs; `REMOTEX_GATEWAY_BIN` and `REMOTEX_WEB_ROOT` point it somewhere else. The
development fallback is the **release** gateway on purpose: an unpackaged run is a
manual QA run, and a debug build of the encoders is too slow to judge a remote
desktop by.

`bun run check` and `bun test tests`, both **from `apps/viewer/`**, are the gate —
the subshells above leave the caller where it started, so a `cd` of your own is the
first half of running them. The tests cover the handshake,
the log tail, the instance directory, the scheme's routing, the clipboard's echo
guards, the window arithmetic, the config store, the bundle's own paths, and every
menu title, tick and greyed item — all without an app running, because only
`main.ts`, `window.ts`, `scheme.ts` and `menu-install.ts` import electron and
everything else takes its dependency as a port.

### Asking the app what it is

Do not drive the UI to find out what the client is doing. Ask it:

```sh
REMOTEX_CHROMIUM_SWITCHES="--disable-features=LocalNetworkAccessChecks,LocalNetworkAccessChecksWebSockets \
  --autoplay-policy=no-user-gesture-required --use-mock-keychain --password-store=basic \
  --disable-pinch --remote-debugging-port=9222" \
  dist/mac-arm64/remotex.app/Contents/MacOS/remotex --instance-dir "$PWD/tmp/app-qa"
curl -s http://127.0.0.1:9222/json/list
```

`Runtime.evaluate` over that page's WebSocket answers the questions that matter —
`window.__remotexGateway`, `typeof window.remotexNative`, `window.isSecureContext`,
what is in `#root` — deterministically and without a screenshot. **Remote ▸
Developer Tools** is the same inspector from inside the shipped app.

A preload that fails to load is the failure to watch for, because it does not look
like one: the client's two globals are the only thing telling it which host it is
in, and without them it concludes it is an ordinary browser tab and renders the
login screen — for a gateway whose address it was never given. A window in which
nothing works and nothing looks broken. `preload-error` now goes straight to the
launch screen, and `distDirFor` carries the story of how it happened once already.

### What only eyes can answer

1. Windows guest, Notepad open: ⌘W closes the *guest's* window, not the shell's.
   ⌘T opens a guest tab. ⌘Q does not quit the shell.
2. Switch Target to reach the picker, then ⌘Q → the shell quits.
3. On the desktop ⌘C copies in the guest; in Remote ▸ Clipboard…'s text box ⌘V
   pastes the Mac's clipboard locally; back on the canvas ⌘V pastes in the guest.
4. Against a Mac guest, with the override reading "(Not Applicable)", ⌘C stays ⌘C.
5. ⌘Space still opens Spotlight; ⌘Tab still switches apps.
6. Copy on the Mac while the window is **minimised** → it arrives in the guest.
7. View ▸ Resize to Display fits the window to the desktop exactly.
8. Remote ▸ Enable Audio starts sound from a menu press.
9. Remote ▸ Configuration… refuses a `[server]` block in the gateway's own words
   and writes nothing.
10. `kill` the gateway from a terminal → the launch screen returns with the tail.
11. Quit, then Force Quit: `pgrep -f serve-embedded` prints nothing either time.
12. Turn a preference off, quit, **relaunch** → it survived. The stable origin and
    the persistent profile each look like they work alone; only a relaunch tells
    you.
13. A `render_type = "video"` H.264 target plays. Electron ships proprietary
    codecs, so the codec a stock Chromium refuses is not a problem here — a browser
    may still refuse it, which is the client's own banner and not the shell's
    concern.

## Signing

Ad-hoc by default, with electron-builder's own hardened-runtime entitlements
(`allow-jit`, `allow-unsigned-executable-memory`, `disable-library-validation`) —
which is the set that arrangement needs, so there are no entitlement files here.
`CODESIGN_IDENTITY` replaces the identity and drops `-unsigned` from the image's
name. Nothing is notarized: a downloaded image still has its quarantine bit cleared
by hand, and a Developer ID would make notarization and updates one conversation
rather than none.
