# remotex-agent for macOS

`remotex-agent` is an optional dedicated-agent alternative to macOS Screen
Sharing. It shares one display from the logged-in user's session over the
encrypted RXA protocol and uses identity keys for unattended reconnects. It
requires macOS 15.4 or later.

## Install

Download `remotex-agent-<version>-macos-arm64-unsigned.dmg` from the
[latest release](https://github.com/andrewtheguy/remotex/releases), then:

1. Drag `remotex-agent.app` to Applications.
2. Clear quarantine because the public build is ad-hoc signed:

   ```sh
   xattr -dr com.apple.quarantine /Applications/remotex-agent.app
   ```

3. Open `/Applications/remotex-agent.app`.

First launch creates
`~/Library/Application Support/remotex-agent/config.toml` and generates the Mac's
identity. The app has no Dock icon; use its menu bar item. It starts with no
gateways authorized and refuses every connection until one is.

A launch does **not** arrange to start at login. Tick **Start at Login** in the
menu (or run `remotex-agent --install-launchagent` once), which writes
`~/Library/LaunchAgents/dev.remotex.agent.plist` naming that copy of the app by
absolute path. Doing it explicitly, from the copy you mean, is what stops a second
copy — one opened from a mounted disk image, say — becoming the one macOS starts.
Installing from a disk image is refused for the same reason.

## Pair with the gateway

Pairing exchanges public keys only.

1. Open the agent's **Settings…** and copy **This Mac's public key**, or run
   `remotex-agent --public-key`. Put the `rxap...` value on the gateway:

   ```toml
   [rxa]
   private_key = "rxgs..."      # generated with `remotex gen-key`

   [[targets]]
   name = "mac"
   protocol = "rxa"
   host = "mac.local"
   agent_public_key = "rxap..."
   ```

2. Run `remotex rxa-pubkey` on the gateway, then add its `rxgp...` value to the
   agent's authorized list: **Settings… › Authorized gateways › Manage…**, one
   entry per line, the key followed by a name for that machine.

   ```text
   rxgp...  home server
   ```

   That name is only for this Mac — it is what the menu bar calls the gateway while
   it is connected. Lines starting with `#` are ignored, so an entry can be
   commented out and put back. The file itself is `authorized_gateways` beside the
   config, and can be edited there instead.

The agent listens on port 52381 by default. One gateway `[rxa].private_key`
identifies that gateway to every RXA target, so a second gateway means a second
line on this list — not a re-pairing. Several may be authorized; one holds the Mac
at a time, and a second has to take the session over from the client asking.

## Move an identity to another Mac

Importing the old private key lets a reinstalled or replacement Mac keep the
public key already configured on its gateways. Use **Import…** in Settings or:

```sh
pbpaste | remotex-agent --import-private-key
```

The command reads standard input so the private key does not appear in shell
history or `ps`. It prints the derived public key for comparison.

## Permissions

Grant both permissions under **System Settings → Privacy & Security**:

| Permission | Purpose | After granting |
|---|---|---|
| Screen Recording | capture the display | quit and reopen the agent |
| Accessibility | inject mouse and keyboard input | effective immediately |

The menu bar warns while either permission is missing and links to its settings
pane.

If the target has `clipboard = true`, macOS may also ask for **Paste from Other
Apps** permission. Choose Allow, then use the per-app setting under Privacy &
Security if permanent access is wanted.

### Signing identity and permissions

Screen Recording and Accessibility grants are tied to the app's signing
identity:

- Developer ID and Apple Development builds keep a stable identity across
  rebuilds.
- An ad-hoc build has a new identity every time. After replacing one, remove
  and re-add the app under both permission panes, then reopen it.

Do not alternate between the ad-hoc GitHub build and a locally signed build.
Changing identity invalidates the existing grants even when the bundle path is
unchanged.

## Menu, settings, and files

The menu bar shows connection and permission status and provides settings,
config and log shortcuts, **Start at Login**, and **Quit**. Settings include the
listen address, public keys, available displays, and the optional private 2x
display.

Saving settings restarts the agent and temporarily disconnects the gateway. A
deliberate quit remains stopped; launchd restarts crashes. Startup and worker
errors leave the menu available in a degraded state with diagnostics.

| Item | Path |
|---|---|
| App | `/Applications/remotex-agent.app` |
| Config | `~/Library/Application Support/remotex-agent/config.toml` |
| Log | `~/Library/Logs/remotex-agent.log` |

The config is mode `0600`. The settings UI rewrites it, so manual comments are
not preserved.

## Upgrade

1. Choose **Quit** from the current menu bar item.
2. Replace the app in Applications.
3. Open the new copy once.

Quitting first is required: opening a replacement while the old process runs
activates the old copy instead of launching the new one.

For a scripted upgrade, replace the bundle and restart the registered job:

```sh
launchctl kickstart -k gui/$(id -u)/dev.remotex.agent
```

Do not run `bootout` first; it unloads the job that `kickstart` needs. Keep the
same signing identity across the upgrade or re-grant Screen Recording and
Accessibility.

Stopping it for a test needs nothing cleverer than a kill. The job has no
`KeepAlive`, so the process stays stopped instead of being relaunched from whatever
bundle is on disk at that instant, and
`launchctl kickstart gui/$(id -u)/dev.remotex.agent` (no `-k`) starts it again. Kill,
do the thing, kickstart.

## Uninstall

1. Turn off **Start at Login** and choose **Quit**.
2. Move `/Applications/remotex-agent.app` to the Trash.
3. Optionally remove its config directory and Privacy & Security entries.

Turning **Start at Login** off first is what removes the LaunchAgent plist;
`remotex-agent --uninstall-launchagent` does the same from a terminal. Deleting the
app without it leaves a plist naming a binary that is no longer there.

## Display behavior and limitations

The agent is available only while a user is signed in. It stops at logout and
does not provide login-window access.

One session shares one whole display. The browser or viewer chooses the display;
the agent settings only decide whether its optional private display exists and
what initial size creates it.

Mac-owned displays are never resized from a client. With `resize = true`, the
private display accepts sizes from a client while it is active — once, on request
(**Resize to window**), or continuously, whichever that client is set to. Its
configured size is an initial size with an 800×600 minimum and also defines the
largest 2x mode available to later requests.

macOS remembers the private display's arrangement and mode. A client resize can
therefore survive an agent restart, and changing the configured initial size
does not move an identity that macOS has already seen. Very small modes can fall
back to 1x.

See
[`docs/mac-agent-architecture.md`](../../docs/mac-agent-architecture.md)
for transport, capture, private-display, and lifecycle details.

## Build from source

Xcode is required for the ScreenCaptureKit Swift bridge.

```sh
packaging/macos/build-agent-app.sh
packaging/macos/build-agent-app.sh --no-dmg
```

The first command creates the DMG; `--no-dmg` keeps only the app in `dist/`.

The build uses `$CODESIGN_IDENTITY` when set, otherwise the first Developer ID
Application certificate, then the first Apple Development certificate. It
falls back to ad-hoc signing only when no keychain identity is available.

`icon.svg` is the source for `AppIcon.icns`:

```sh
brew install librsvg
packaging/macos/make-icon.sh
```

### Notarization

Store a notarytool profile and pass it to the build:

```sh
xcrun notarytool store-credentials remotex-notary \
  --key AuthKey_XXXX.p8 --key-id <KEY_ID> --issuer <ISSUER_UUID>

CODESIGN_IDENTITY="Developer ID Application: Example (TEAMID)" \
  packaging/macos/build-agent-app.sh --notary-profile remotex-notary
```

For non-interactive signing:

| Variable | Value |
|---|---|
| `CODESIGN_IDENTITY` | imported Developer ID identity |
| `MACOS_CERT_P12` | base64-encoded `.p12` with the private key |
| `MACOS_CERT_PASSWORD` | export password |
| `MACOS_KEYCHAIN_PASSWORD` | temporary keychain password |

The build imports the certificate into a temporary keychain and removes that
keychain on exit.
