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

There is no dock icon and no window — it is a background agent.

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

Check the result:

```sh
/Applications/remotex-agent.app/Contents/MacOS/remotex-agent --status
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

## No login-window support

The agent runs in your GUI session, because both permissions require a window
server connection that a LaunchDaemon does not have. So it is not running at the
login window and cannot be: if nobody is logged in on the Mac, there is nothing
for the gateway to reach. This is a property of the design, not a bug.

## Building from source

```sh
packaging/macos/build-agent-app.sh          # -> dist/remotex-agent.app
CODESIGN_IDENTITY="Apple Development: …" packaging/macos/build-agent-app.sh
```

Needs Xcode (the capture bindings build a small Swift bridge). Signing prefers
`$CODESIGN_IDENTITY`, then the first `Apple Development` identity in the
keychain, then ad-hoc.
