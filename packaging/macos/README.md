# remotex-agent for macOS

`remotex-agent` is an optional, RealVNC-like dedicated-agent alternative to
using macOS Screen Sharing as a VNC target. Its PSK authenticates reconnects
directly instead of returning to Screen Sharing's login gate. It shares the
logged-in user's Mac with a remotex gateway over the encrypted `rxa` protocol
and requires macOS 14 or later.

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
`~/Library/Application Support/remotex-agent/config.toml`, generates a
pre-shared key, and registers the app as a login item. The agent has no Dock
icon; use its menu bar item.

## Connect the gateway

Choose **Copy Pre-Shared Key** from the menu bar item and add an `rxa` target to
the gateway config:

```toml
[[targets]]
name = "mac"
protocol = "rxa"
host = "mac.local"
psk = "rxa..."
```

The agent listens on port 52381 by default. The PSK is the credential and must
match exactly on both sides.

## Permissions

Grant `remotex-agent` both permissions under **System Settings → Privacy &
Security**:

| Permission | Purpose | After granting |
|---|---|---|
| Screen Recording | capture the display | quit and reopen the agent |
| Accessibility | inject mouse and keyboard input | effective immediately |

The menu bar shows a warning and links to the relevant settings pane while a
permission is missing.

Permissions are tied to the app's signing identity. The ad-hoc-signed release
may need both grants again after an upgrade. A stable Developer ID signature
preserves the identity.

## Menu and settings

The status icon distinguishes idle, connected, and missing-permission states.
Its menu provides:

- connection and listen-address status;
- PSK copy;
- address, display, and PSK settings;
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
points the user to the menu bar. Startup errors are shown in a panel.

## Files

| Item | Path |
|---|---|
| App | `/Applications/remotex-agent.app` |
| Config | `~/Library/Application Support/remotex-agent/config.toml` |
| Log | `~/Library/Logs/remotex-agent.log` |

The config is mode `0600` and is rewritten by the settings UI, so manual
comments are not preserved.

## Upgrade

Replace the app in Applications, then open the new copy once. Opening it
refreshes the login-item registration, which can otherwise continue pointing
at the replaced bundle.

## Uninstall

1. Turn off **Start at Login** and choose **Quit**.
2. Move `/Applications/remotex-agent.app` to the Trash.
3. Optionally remove its config directory and its entries under Privacy &
   Security.

Unregister before removing the app to avoid leaving a stale Login Items entry.

## Limitations

The agent runs in the logged-in user's GUI session. It cannot share the macOS
login window or run when no user is logged in. It mirrors the selected physical
display and does not resize it to the browser viewport.

See [`docs/mac-agent-architecture.md`](../../docs/mac-agent-architecture.md)
for the capture, transport, and lifecycle design.

## Build from source

Xcode is required for the ScreenCaptureKit Swift bridge.

```sh
packaging/macos/build-agent-app.sh
packaging/macos/build-agent-app.sh --no-dmg
```

The first command creates the DMG; the second keeps the `.app` in `dist/`.

`icon.svg` is the source for the committed `AppIcon.icns`. Regenerate it after
changing the SVG:

```sh
brew install librsvg
packaging/macos/make-icon.sh
```

Signing selection is: `CODESIGN_IDENTITY`, a Developer ID Application identity,
an Apple Development identity, then ad-hoc signing.

### Notarization

Store a notarytool profile, then pass it to the build:

```sh
xcrun notarytool store-credentials remotex-notary \
  --key AuthKey_XXXX.p8 --key-id <KEY_ID> --issuer <ISSUER_UUID>

packaging/macos/build-agent-app.sh --notary-profile remotex-notary
```

For non-interactive signing, the build script accepts:

| Variable | Value |
|---|---|
| `MACOS_CERT_P12` | base64-encoded `.p12` containing the private key |
| `MACOS_CERT_PASSWORD` | export password |
| `MACOS_KEYCHAIN_PASSWORD` | password for the temporary keychain |

The script imports the certificate into a temporary keychain, configures the
partition list for `codesign`, and removes the keychain on exit.
