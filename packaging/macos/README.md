# remotex-agent (macOS)

The screen agent remotex connects to when a target is `protocol = "rxa"`. It
replaces reaching the Mac over Screen Sharing, whose credential prompt reappears
on every disconnect. Here the pre-shared key *is* the credential, so a dropped
connection reconnects silently and never asks for a login.

Requires **macOS 14 or later**.

## Install

Take `remotex-agent-<version>-macos-arm64-unsigned.zip` from the
[latest release](https://github.com/andrewtheguy/remotex/releases) — it ships
alongside the gateway tarballs, built from the same commit — then:

```sh
unzip remotex-agent-*-macos-arm64-unsigned.zip
xattr -dr com.apple.quarantine remotex-agent.app   # ad-hoc signed, so quarantined
cp -R remotex-agent.app /Applications/
open /Applications/remotex-agent.app
```

The `xattr` line is Gatekeeper, not paranoia: a downloaded bundle without a
Developer ID signature is refused until the quarantine attribute is gone. The
same ad-hoc signature is why the two permissions below are asked for again after
an upgrade — the grants are keyed to the code identity, and it changes with every
build.

That single open does everything an install script would have:

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

Opening it shows the connected gateway's address, how long it has been attached,
and the address the agent is listening on. Then:

| | |
|---|---|
| **Pre-Shared Key…** | shows the key, copies it, or mints a new one |
| **Copy Pre-Shared Key** | the same copy, without the panel |
| **Listen Address…** | `address:port` to wait for the gateway on |
| **Display** | which screen to share, when more than one is attached |
| **Reveal Config in Finder** | where the config file is |
| **Open Log** | `~/Library/Logs/remotex-agent.log` |
| **Screen Recording** / **Accessibility** | ticked when granted; each opens the right Privacy pane, which is otherwise four levels down a settings tree |
| **Start at Login** | the `SMAppService` registration, as a toggle |
| **Quit remotex-agent** | really quits — see below |

Quit really quits: the embedded LaunchAgent uses `KeepAlive` /
`SuccessfulExit: false`, so a deliberate exit stays exited while a crash is
still restarted. The agent comes back at your next login, or whenever you open
it from `/Applications` again.

### Settings apply at the next start

Changing the listen address, the display or the key writes the config file and
nothing more — the running agent keeps serving what it was launched with. The
menu says `⚠︎ Saved changes apply after a restart` while the two disagree, and
**Quit** followed by opening the app again is the restart.

That matters most for the key: after regenerating, the agent still
authenticates with the *previous* one until it restarts, so put the new key on
the gateway and restart the agent together.

### Over SSH there is no interface

A status item needs a window server, which an SSH session does not have. Pass
`--no-menu` there — and note that with no menu there is nothing to read the key
or change a setting with, which is why that flag is for development. The config
file is plain TOML if you are stuck without a screen.

## Then two permissions, and one key

Open the menu bar item, choose **Pre-Shared Key…**, and copy it. Paste it as
`psk` on the matching `[[targets]]` entry in the gateway's `remotex.toml`:

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

That is the only place worth reading them from. Both permissions are attributed
to whatever launched the process, so the binary run from a terminal reports your
*terminal's* permissions — the same binary says "NOT granted" from a shell and
"granted" a second later when macOS launches it as the app. The agent's own log
is the other honest answer:

```sh
grep permissions: ~/Library/Logs/remotex-agent.log | tail -2
```

## Where things are

| | |
|---|---|
| Config | `~/Library/Application Support/remotex-agent/config.toml` (**Reveal Config in Finder**) |
| Log | `~/Library/Logs/remotex-agent.log` (**Open Log**) |
| Port | 52381 by default (**Listen Address…**) |

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

The fix, and the way to avoid it:

```sh
cp -R remotex-agent.app /Applications/
open /Applications/remotex-agent.app          # re-registers, repairing the record
```

If it is already broken, open the new bundle and switch **Start at Login** off
and on again.

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

You do not have to, though: every release carries the built bundle as
`remotex-agent-<version>-macos-arm64-unsigned.zip` (see **Install** above). The
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
