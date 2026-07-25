# remotex-agent (macOS)

The screen agent remotex connects to when a target is `protocol = "rxa"`. It
replaces reaching the Mac over Screen Sharing, whose credential prompt reappears
on every disconnect. Here the pre-shared key *is* the credential, so a dropped
connection reconnects silently and never asks for a login.

Requires **macOS 14 or later**.

## Install

```sh
cp -R remotex-agent.app /Applications/
open /Applications/remotex-agent.app
```

That single open does everything an install script would have:

- writes `~/Library/Application Support/remotex-agent/config.toml` (mode 0600)
  with a freshly generated pre-shared key, if it is not already there;
- registers itself with `SMAppService`, so it starts now and at every login and
  appears in **System Settings → General → Login Items**.

There is no Dock icon and no window — it is a background agent. What it does
have is a **menu bar item**, which is where everything below can also be done
without a terminal.

## The menu bar item

| | |
|---|---|
| 🖥 | idle — running, nobody connected |
| 👁 | a gateway is connected and watching this screen |

Opening it shows the connected gateway's address and how long it has been
attached, and offers:

- **Copy Pre-Shared Key** — the same value as `--show-psk`
- **Open Log**
- **Screen Recording** / **Accessibility** — ticked when granted, and each opens
  the right Privacy pane, which is otherwise four levels down a settings tree
- **Start at Login** — the `SMAppService` registration, as a toggle
- **Quit remotex-agent**

Quit really quits: the embedded LaunchAgent uses `KeepAlive` /
`SuccessfulExit: false`, so a deliberate exit stays exited while a crash is
still restarted. The agent comes back at your next login, or whenever you open
it from `/Applications` again.

Over SSH there is no window server to put a status item in; pass `--no-menu`
there.

## Then two permissions, and one key

Get the key to put on the gateway:

```sh
/Applications/remotex-agent.app/Contents/MacOS/remotex-agent --show-psk
```

Paste it as `psk` on the matching `[[targets]]` entry in the gateway's
`remotex.toml`:

```toml
[[targets]]
name = "mac"
protocol = "rxa"
host = "mac.local"
psk = "rxa..."
```

Then open **System Settings → Privacy & Security** and enable `remotex-agent`
under **both**:

| Permission | Without it |
|---|---|
| Screen Recording | the screen never paints; the gateway reports the reason |
| Accessibility | the session looks perfectly healthy and silently ignores every click and keystroke |

The second is the one that wastes an afternoon — with only Screen Recording
granted, everything appears to work except that nothing responds.

macOS provides no way to grant these programmatically. Because the bundle is
code-signed with a stable identifier, you only grant them once; they survive
upgrades.

Check the result **in the menu bar**, where Screen Recording and Accessibility
are ticked when granted.

Do not trust `--status` for this. Both permissions are attributed to whatever
launched the process, so running the binary from a terminal reports your
*terminal's* permissions, not the agent's — the same binary says "NOT granted"
from a shell and "granted" a second later when launched as the app:

```sh
# Everything except the two permission lines is accurate here.
/Applications/remotex-agent.app/Contents/MacOS/remotex-agent --status

# The truth about the permissions, from the agent as macOS actually runs it.
grep permissions: ~/Library/Logs/remotex-agent.log | tail -2
```

## Where things are

| | |
|---|---|
| Config | `~/Library/Application Support/remotex-agent/config.toml` |
| Log | `~/Library/Logs/remotex-agent.log` |
| Port | 52381 by default (`listen` in the config) |

## Uninstall

```sh
/Applications/remotex-agent.app/Contents/MacOS/remotex-agent --unregister
rm -rf /Applications/remotex-agent.app
```

`--unregister` takes it out of Login Items; without it, moving the bundle to the
Trash leaves a dangling entry there. The config file is left behind, since it
holds the key — delete
`~/Library/Application Support/remotex-agent` to remove that too, and clear
`remotex-agent` from the two Privacy & Security lists by hand.

## Reinstalling over an existing copy

Replace the bundle by **opening the new one**, not just copying it into place.
The Login Items registration is a Background Task Management record that points
at the bundle it was made from; delete that bundle and drop a new one at the
same path and the record goes stale, launchd fails to spawn with `EX_CONFIG`,
and nothing appears in the log because the binary never runs. `--status` still
cheerfully reports the login item as enabled, because launchd's registration is
intact — only the thing it points at is gone.

The fix, and the way to avoid it:

```sh
cp -R remotex-agent.app /Applications/
open /Applications/remotex-agent.app          # re-registers, repairing the record
```

If it is already broken, `--unregister` and then open the app again.

## No login-window support

The agent runs in your GUI session, because both permissions require a window
server connection that a LaunchDaemon does not have. So it is not running at the
login window and cannot be: if nobody is logged in on the Mac, there is nothing
for the gateway to reach. This is a property of the design, not a bug.

## Building from source

```sh
packaging/macos/build-agent-app.sh          # -> dist/remotex-agent.app
```

Needs Xcode — the capture bindings build a small Swift bridge.

Without a Mac to build on, the **Mac Agent (unsigned)** workflow
(`.github/workflows/mac-agent.yml`) runs the same script on a GitHub runner and
uploads the bundle as an artifact — arm64, ad-hoc signed. For testing only: the
download is quarantined (`xattr -dr com.apple.quarantine remotex-agent.app`) and
an ad-hoc identity asks for both permissions again on every install.

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
