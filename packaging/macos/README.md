# remotex-agent for macOS

`remotex-agent` is an optional, RealVNC-like dedicated-agent alternative to
using macOS Screen Sharing as a VNC target. Its keypair authenticates reconnects
directly instead of returning to Screen Sharing's login gate. It shares the
logged-in user's Mac with a remotex gateway over the encrypted `rxa` protocol
and requires macOS 15.4 or later.

## Install

Download `remotex-agent-<version>-macos-arm64-unsigned.dmg` from the
[latest release](https://github.com/andrewtheguy/remotex/releases), then:

1. Drag `remotex-agent.app` to Applications.
2. Clear quarantine because the public build is ad-hoc signed:

   ```sh
   xattr -dr com.apple.quarantine /Applications/remotex-agent.app
   ```

3. Open `/Applications/remotex-agent.app`.

The first launch creates
`~/Library/Application Support/remotex-agent/config.toml`, mints this Mac's
keypair, and registers the app as a login item. The agent has no Dock icon; use
its menu bar item. It starts **unpaired**: it listens and refuses every
connection until it is told which gateway to answer.

## Pair it with the gateway

Two public keys are exchanged, one each way. Neither is a secret, and pairing
never moves a private key: each machine's stays in its own config file. The one
time a private key does travel is when you deliberately carry an identity to
another Mac — see [Moving an identity to another
Mac](#moving-an-identity-to-another-mac).

**1. This Mac's key onto the gateway.** Open **Settings…** from the menu bar item
and press **Copy** beside *This Mac's public key* (or run
`remotex-agent --public-key`, which works over SSH). Add an `rxa` target:

```toml
[rxa]
private_key = "rxgs..."      # this gateway's identity; `remotex gen-key`

[[targets]]
name = "mac"
protocol = "rxa"
host = "mac.local"
agent_public_key = "rxap..."
```

**2. The gateway's key into the agent.** Run `remotex rxa-pubkey` on the gateway
— it also logs its public key at startup — and paste the `rxgp...` value into
*Gateway public key* in the same Settings dialog.

The agent listens on port 52381 by default. One `[rxa].private_key` serves every
`rxa` target: it is the server's identity, not a per-Mac credential.

## Moving an identity to another Mac

A Mac's identity is its private key, so a reinstall or a replacement machine can
keep the public key the gateway already has instead of being re-paired. Use
**Import…** in Settings, or over SSH:

```sh
pbpaste | remotex-agent --import-private-key      # prints the resulting public key
```

The key is read from stdin rather than passed as an argument, so it stays out of
shell history and out of `ps`. Nothing else in the config changes, and the public
key it prints should match what the gateway already has.

## Permissions

Grant `remotex-agent` both permissions under **System Settings → Privacy &
Security**:

| Permission | Purpose | After granting |
|---|---|---|
| Screen Recording | capture the display | quit and reopen the agent |
| Accessibility | inject mouse and keyboard input | effective immediately |

The menu bar shows a warning and links to the relevant settings pane while a
permission is missing. Screen Recording is checked again when the menu opens;
after you enable it, the menu offers to quit the agent and tells you to reopen it
from Applications. Accessibility is detected while the agent remains open and
needs no restart.

### Signing identity and the grants

Permissions are tied to the app's signing identity, and this is the single
biggest source of "the agent is approved but still cannot capture the screen".

A **signed** build (Developer ID, or a free Apple Development certificate — good
enough for your own Mac) keeps one identity across rebuilds, so both grants are
given once and stay.

An **ad-hoc** build has no stable identity: macOS treats every build as a
different app. The grants do not carry over, and they do not re-prompt either —
System Settings keeps the old entry, matched by path, while the system refuses
the app behind it. After installing each ad-hoc build you must open **System
Settings → Privacy & Security** and, under **both** Screen Recording and
Accessibility, remove `remotex-agent` with **−** and add it back with **+**,
then reopen the agent. Every time.

> **Do not alternate between build sources.** Installing the GitHub release's
> ad-hoc `-unsigned.dmg` over a locally signed build, or the reverse, changes the
> code identity exactly as two ad-hoc builds would, and breaks the grants the
> same way. Pick one and stay on it; switching costs one round of the manual
> remove-and-re-add above.

Building from source signs with a keychain identity by default, so a local build
is the way to avoid all of this — see [Build from source](#build-from-source).

A third permission appears only if you use the clipboard bridge (`clipboard =
true` on the gateway's target). macOS asks before letting the agent read the
pasteboard, and after the first prompt the app is listed under **Paste from
Other Apps**, where **Allow** stops it asking again. Until then, expect a prompt
each time something is copied on the Mac while a session is connected. The menu
bar reports the current setting once macOS has one to report.

## Menu and settings

The status icon is the first part of the app created. It starts in the warning
state, before config I/O, login-item registration, socket setup, or permission
checks, then changes to idle or connected only after startup succeeds. A startup
or network-worker failure leaves the warning icon and diagnostic menu alive
instead of exiting into a launchd restart loop. Its normal menu provides:

- connection and listen-address status, and a **Not paired** row while no
  gateway key is set;
- settings: listen address, a read-only list of the displays this Mac can share,
  whether to add a private 2x display of the agent's own, that display's
  **initial** size, this Mac's public key with a **Copy** button, the gateway's
  public key to paste in, and **Regenerate identity** behind a confirmation;
- config and log shortcuts;
- permission shortcuts;
- the **Start at Login** toggle and **Quit**.

Saving settings restarts the agent, disconnecting the current gateway until it
reconnects. A deliberate quit remains stopped until the app is opened again or
the next login; crashes are restarted automatically.

If the browser closes or loses its network path, the gateway's shared session
heartbeat closes the agent connection after about 60 seconds. Capture then
stops and the menu returns to **No gateway connected**. Reopening the browser
during that grace period reuses the existing session.

Opening the app while it is already running keeps the existing process and
points the user to the menu bar. Startup errors are shown in a panel and remain
visible from the degraded menu bar item after that panel is dismissed.

## Files

| Item | Path |
|---|---|
| App | `/Applications/remotex-agent.app` |
| Config | `~/Library/Application Support/remotex-agent/config.toml` |
| Log | `~/Library/Logs/remotex-agent.log` |

The config is mode `0600` and is rewritten by the settings UI, so manual
comments are not preserved.

## Upgrade

Choose **Quit** from the menu bar item first, then replace the app in
Applications, then open the new copy once.

Quitting first is what makes the upgrade land, and skipping it is silent rather
than noisy. macOS will not launch a second copy of an app that is already
running: opening the new bundle over a running agent just *activates* the old
process, so nothing is upgraded, nothing is said, and the old version keeps
running until the next login. With the agent quit there is nothing to activate,
and opening the new copy starts the LaunchAgent job from it.

The job is the copy that matters — it is the one that comes back at login, that
the menu's Quit and a settings save restart, and that the Screen Recording and
Accessibility grants were issued to. So whichever order things happen in, only
one agent ends up running: a copy that finds the job already registered stands
down and asks launchd to start it from this bundle instead of competing for the
port (see `hand_over_to_launchd` in `main.rs`). Before that, a fresh install
could leave two processes fighting over 52381 and a modal alert nobody was there
to dismiss.

Scripted, with no GUI in the way, the whole upgrade is: replace the bundle, then

```sh
launchctl kickstart -k gui/$(id -u)/dev.remotex.agent
```

which restarts the job from whatever is on disk. Do not `bootout` first — that
unloads the job, and the kickstart then has nothing to find.

There is no need to uninstall anything. Unregistering the login item, which is
what Uninstall does, only means having to register it again.

Stay with the same kind of build you already have — see [Signing identity and
the grants](#signing-identity-and-the-grants). Upgrading a signed install with
an ad-hoc one (or the reverse) silently invalidates Screen Recording and
Accessibility, and the fix is manual.

## Uninstall

1. Turn off **Start at Login** and choose **Quit**.
2. Move `/Applications/remotex-agent.app` to the Trash.
3. Optionally remove its config directory and its entries under Privacy &
   Security.

Unregister before removing the app to avoid leaving a stale Login Items entry.

## Limitations

The agent is available only while a user is signed in to the Mac. It stops at
logout and cannot be used to sign in from the macOS login screen.
This is a limitation of remotex's current per-user installation, not of macOS
remote access generally. RealVNC Service Mode and RustDesk's installed service
support the login screen by adding system-level launch components. Equivalent
login-screen support is planned for remotex.

The agent mirrors one whole display at a time. Which display is chosen from the
remotex viewer or the browser, per session, not in the agent's settings — so
there is no display setting here, only the checkbox that decides whether the
private display exists and the `virtual_display_initial_size` its first
appearance uses.

A Mac's own screens are never resized from a client: their resolution is set on
the Mac. The private display is the exception, because nobody is sitting at it —
with `resize = true` on the gateway's target, **Resize to window** in either
client asks it to match the window it is being viewed in. It applies only while
that display is the one being shared, and each client says so its own way: the
viewer keeps both **Resize to Window** and **Resize to Display** in its Remote
menu at all times and greys out whichever does not apply, while the browser's
floating menu shows the button only when it does.

"Initial" is the whole of how the configured size behaves: it is what the private
display is created at the first time this Mac sees it (no smaller than 800x600),
and what it fixes permanently is the largest mode that display can ever render at
2x — which is also the ceiling a Resize to window clamps to.

Prefer Resize to window over the mode list in System Settings > Displays. macOS
will resize this display like any other screen, but it lists every size twice — a
HiDPI entry and a `(low resolution)` one — so half of what it offers is 1x at a
size that could have been 2x, and picking past the envelope comes back oversized
at a size nobody chose. Resize to window avoids both: it stays inside the
envelope and re-applies the density the display is in. It is not a guarantee of
sharpness, though — shrink the window far enough below the size the display was
created at and the mode leaves the HiDPI range, so the desktop comes back 1x at
the size that was asked for. Growing the window again brings 2x back.

Whatever the display ends up at, however it got there, macOS remembers it against
that display and restores it on the next launch — so a resize asked for from a
client sticks the same way one made in System Settings does, and editing the
setting will not move a display that has already been arranged. Its density needs
no help at all: whichever client is connected reports the screen it is on, and the
display matches it.

See [`docs/mac-agent-architecture.md`](../../docs/mac-agent-architecture.md)
for the capture, transport, and lifecycle design.

## Build from source

Xcode is required for the ScreenCaptureKit Swift bridge.

```sh
packaging/macos/build-agent-app.sh
packaging/macos/build-agent-app.sh --no-dmg
```

The first command creates the DMG; the second keeps the `.app` in `dist/`.

The build signs with a keychain identity by default — `$CODESIGN_IDENTITY` if
set, otherwise the first Developer ID Application certificate, otherwise the
first Apple Development one. It falls back to ad-hoc only when the keychain has
none, and says so loudly, because that costs the manual re-granting described
under [Permissions](#permissions). Note that a build signed this way has a
different identity from the ad-hoc GitHub release, so the first local build
after running the release needs both grants re-added once.

`icon.svg` is the source for the committed `AppIcon.icns`. Regenerate it after
changing the SVG:

```sh
brew install librsvg
packaging/macos/make-icon.sh
```

### Notarization

Store a notarytool profile, then pass it to the build:

```sh
xcrun notarytool store-credentials remotex-notary \
  --key AuthKey_XXXX.p8 --key-id <KEY_ID> --issuer <ISSUER_UUID>

CODESIGN_IDENTITY="Developer ID Application: Example (TEAMID)" \
   packaging/macos/build-agent-app.sh --notary-profile remotex-notary
```

For non-interactive signing, the build script accepts:

| Variable | Value |
|---|---|
| `CODESIGN_IDENTITY` | name of the imported Developer ID identity |
| `MACOS_CERT_P12` | base64-encoded `.p12` containing the private key |
| `MACOS_CERT_PASSWORD` | export password |
| `MACOS_KEYCHAIN_PASSWORD` | password for the temporary keychain |

The script imports the certificate into a temporary keychain, configures the
partition list for `codesign`, and removes the keychain on exit.
