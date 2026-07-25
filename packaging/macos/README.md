# remotex-agent (macOS)

The screen agent remotex connects to when a target is `protocol = "rxa"`. It
replaces reaching the Mac over Screen Sharing, whose credential prompt reappears
on every disconnect. Here the pre-shared key *is* the credential, so a dropped
connection reconnects silently and never asks for a login.

Requires **macOS 14 or later**.

## Install

Take `remotex-agent-<version>-macos-arm64-unsigned.dmg` from the
[latest release](https://github.com/andrewtheguy/remotex/releases) — it ships
alongside the gateway tarballs, built from the same commit — then:

1. Open the image.
2. **Drag `remotex-agent.app` onto the `Applications` folder** beside it.
3. Clear the quarantine flag, because the bundle is ad-hoc signed rather than
   notarized:

   ```sh
   xattr -dr com.apple.quarantine /Applications/remotex-agent.app
   ```

4. Open it from `/Applications` — in the Finder, or `open
   /Applications/remotex-agent.app`.
5. Eject the image.

Install it from `/Applications`, not from the mounted image: opening it off the
image registers a login item pointing at a mount point, and that path is gone as
soon as you eject (see **Reinstalling over an existing copy** for what a stale
registration does).

The `xattr` line is Gatekeeper, not paranoia: a downloaded bundle without a
Developer ID signature is refused until the quarantine attribute is gone. The
same ad-hoc signature is why the two permissions below are asked for again after
an upgrade — the grants are keyed to the code identity, and it changes with every
build. A notarized build needs neither the `xattr` line nor the re-approval; the
release just does not have the signing secrets yet.

That first open does everything an install script would have:

- writes `~/Library/Application Support/remotex-agent/config.toml` (mode 0600)
  with a freshly generated pre-shared key, if it is not already there;
- registers itself with `SMAppService`, so it starts now and at every login and
  appears in **System Settings → General → Login Items**.

There is no Dock icon and no window — it is a background agent. What it does
have is a **menu bar item**, and that is the entire interface: there are no
subcommands, and nothing below needs a terminal.

## The menu bar item

| | |
|---|---|
| 🖥 | idle — running, nobody connected |
| 👁 | a gateway is connected and watching this screen |
| ⚠️ | a required permission is missing; nothing will work until it is granted |

Opening it shows the connected gateway's address, how long it has been attached,
and the address the agent is listening on. Then:

| | |
|---|---|
| **Copy Pre-Shared Key** | the key, straight to the clipboard |
| **Settings…** | listen address, display and pre-shared key, in one dialog |
| **Reveal Config in Finder** | where the config file is |
| **Open Log** | `~/Library/Logs/remotex-agent.log` |
| **Enable Screen Recording…** / **Enable Accessibility…** | only while one is missing; each opens the right Privacy pane, which is otherwise four levels down a settings tree |
| **Start at Login** | the `SMAppService` registration, as a toggle |
| **Quit remotex-agent** | really quits — see below |

Quit really quits: the embedded LaunchAgent uses `KeepAlive` /
`SuccessfulExit: false`, so a deliberate exit stays exited while a crash is
still restarted. The agent comes back at your next login, or whenever you open
it from `/Applications` again.

### One settings dialog, and Save restarts

**Settings…** holds all three settings at once:

| | |
|---|---|
| Listen address | `address:port` to wait for the gateway on. `0.0.0.0` is every interface |
| Display | which screen to share, when more than one is attached |
| Pre-shared key | editable, so a key can be pasted in — plus **Regenerate**, which fills the field with a fresh one |

Nothing is written until **Save**, so **Regenerate** followed by **Cancel**
changes nothing. Saving a change **restarts the agent immediately** — that is
how a new port, display or key takes effect — which drops any connection in
progress. The gateway reconnects on its own.

So a key change is two steps in either order, with a gap between them: put the
new key in the gateway's `remotex.toml`, and save it here. Nothing can connect
until both are done.

### Opening it a second time

Opening the app while a copy is already running puts up a panel saying so and
pointing at the menu bar, then exits — the running agent keeps the port and the
session. It is worth knowing that this is what "nothing happened" means, because a
background app has no window to fail in: any startup problem it cannot get past
(an unusable config file, a port it cannot bind) gets a panel of its own for the
same reason.

A second open still re-registers the login item on its way past, which is what
makes the repair in **Reinstalling over an existing copy** work while the old copy
is running.

### Over SSH there is no interface

A status item needs a window server, which an SSH session does not have. Pass
`--no-menu` there — and note that with no menu there is nothing to read the key
or change a setting with, which is why that flag is for development. The config
file is plain TOML if you are stuck without a screen.

## Then two permissions, and one key

Open the menu bar item, choose **Copy Pre-Shared Key**, and paste it as `psk` on
the matching `[[targets]]` entry in the gateway's `remotex.toml`:

```toml
[[targets]]
name = "mac"
protocol = "rxa"
host = "mac.local"
psk = "rxa..."
```

Both permissions are needed in **System Settings → Privacy & Security**, under
`remotex-agent`:

| Permission | Without it | Once granted |
|---|---|---|
| Screen Recording | the screen never paints; the gateway reports the reason | **restart the agent** — macOS only gives this to a fresh launch |
| Accessibility | the session looks perfectly healthy and silently ignores every click and keystroke | works immediately |

The second is the one that wastes an afternoon — with only Screen Recording
granted, everything appears to work except that nothing responds. So neither is
treated as an option: the agent asks for the missing one at startup and offers to
open the right pane, the menu bar icon shows ⚠️ until both are on, and the menu
carries an **Enable …** item for whichever is missing. When both are granted,
none of that appears.

Note the third column. Ticking Accessibility fixes a running agent on the spot,
and the ⚠️ clears within a second. Ticking Screen Recording does not: the running
process keeps being refused, so the warning stays until you **Quit** and open the
agent again — which is also what macOS's own "quit and reopen" prompt is telling
you.

macOS provides no way to grant these programmatically. Because the bundle is
code-signed with a stable identifier, you only grant them once; they survive
upgrades.

The menu bar is also the only place worth *reading* them from. Both permissions
are attributed to whatever launched the process, so the binary run from a terminal
reports your *terminal's* permissions — the same binary says "NOT granted" from a
shell and "granted" a second later when macOS launches it as the app. The agent's
own log is the other honest answer:

```sh
grep permissions: ~/Library/Logs/remotex-agent.log | tail -2
```

## Where things are

| | |
|---|---|
| Config | `~/Library/Application Support/remotex-agent/config.toml` (**Reveal Config in Finder**) |
| Log | `~/Library/Logs/remotex-agent.log` (**Open Log**) |
| Port | 52381 by default (**Settings…**) |

The config is written by the menu, and rewritten whole on every change — so
comments you add to it by hand will not survive the next one. It is mode 0600,
because the key in it is the entire credential.

## Uninstall

1. Switch **Start at Login** off in the menu bar item, then **Quit**.
2. `rm -rf /Applications/remotex-agent.app`

Step 1 is the part that matters: trashing the bundle without unregistering
leaves a dangling entry in Login Items. The config file is left behind either
way, since it holds the key — delete
`~/Library/Application Support/remotex-agent` to remove that too, and clear
`remotex-agent` from the two Privacy & Security lists by hand.

## Reinstalling over an existing copy

Replace the bundle by **opening the new one**, not just copying it into place.
The Login Items registration is a Background Task Management record that points
at the bundle it was made from; delete that bundle and drop a new one at the
same path and the record goes stale, launchd fails to spawn with `EX_CONFIG`,
and nothing appears in the log because the binary never runs. **Start at Login**
still shows a tick, because launchd's registration is intact — only the thing it
points at is gone.

The fix, and the way to avoid it: replace the app from the new image (the Finder
will offer to), then open `/Applications/remotex-agent.app` once — that
re-registers it and repairs the record. If it is already broken, opening the new
copy and switching **Start at Login** off and on again does the same.

## No login-window support

The agent runs in your GUI session, because both permissions require a window
server connection that a LaunchDaemon does not have. So it is not running at the
login window and cannot be: if nobody is logged in on the Mac, there is nothing
for the gateway to reach. This is a property of the design, not a bug.

## Building from source

```sh
packaging/macos/build-agent-app.sh   # -> dist/remotex-agent-<version>-...dmg
packaging/macos/build-agent-app.sh --no-dmg   # -> dist/remotex-agent.app
```

The first form leaves only the image: the bundle is built, signed and copied into
it, then removed from `dist/`, so there is no second copy to install from by
mistake. Use `--no-dmg` when you want the bundle itself.

Needs Xcode — the capture bindings build a small Swift bridge.

You do not have to, though: every release carries the built image as
`remotex-agent-<version>-macos-arm64-unsigned.dmg` (see **Install** above). The
release workflow runs this same script on a macOS runner, so a release bundle and
a local one differ only in the signature.

Signing prefers `$CODESIGN_IDENTITY`, then a `Developer ID Application`
identity, then `Apple Development`, then ad-hoc. Prefer a real identity: the
two TCC grants are keyed to the signed code identity, and ad-hoc changes it on
every build, so macOS asks for both permissions again each time.

### Notarizing for distribution

A `.app` downloaded from a release is quarantined, and Gatekeeper will refuse
it unless it is notarized. That needs a **Developer ID Application** certificate
and a one-time notarytool profile:

```sh
xcrun notarytool store-credentials remotex-notary \
  --key AuthKey_XXXX.p8 --key-id <KEY_ID> --issuer <ISSUER_UUID>

packaging/macos/build-agent-app.sh --notary-profile remotex-notary
```

The ticket is stapled into the bundle, so it validates offline.

### Signing from CI or over SSH

`codesign` needs the signing key's *partition list* to permit it. Keychain
Access's "Allow all applications to access this item" does **not** set that, so
from any session that cannot show UI — CI, or SSH / VS Code Remote into a Mac —
signing fails with `errSecInternalComponent` however the keychain is unlocked.
Either run the script at the Mac's own console, or import a `.p12` into a
throwaway keychain by setting:

| Variable | |
|---|---|
| `MACOS_CERT_P12` | base64 of a `.p12` exported **with its private key** |
| `MACOS_CERT_PASSWORD` | that `.p12`'s export password |
| `MACOS_KEYCHAIN_PASSWORD` | any string; scopes the temporary keychain |

The script creates the keychain, imports, runs `security
set-key-partition-list`, and deletes it again on exit.
