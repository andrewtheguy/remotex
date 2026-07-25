# Installing remotex

## Install

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
```

The installer selects the current OS and architecture, verifies the release
SHA-256 digest, and installs under `/opt/remotex`. It may use `sudo` for the
prefix and `/usr/local/bin/remotex` link.

Then configure and start the gateway:

```sh
remotex gen-passwd admin
$EDITOR /opt/remotex/etc/remotex.toml
remotex serve
```

The full config template is at
`/opt/remotex/current/share/doc/remotex/remotex.toml.example`. Keep the live
config readable only by the service user because it contains credentials.

For a Mac target, the gateway can connect directly to macOS Screen Sharing over
VNC; no companion software is required. The optional `remotex-agent` DMG offers
a dedicated-agent alternative whose PSK authenticates reconnects without
returning to Screen Sharing's login gate. See
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Installer options

Use `PREFIX` and `BINDIR` to change the install location:

| Variable | Default | Purpose |
|---|---|---|
| `PREFIX` | `/opt/remotex` | installation root |
| `BINDIR` | `/usr/local/bin` | launcher directory |

For example:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh |
  PREFIX="$HOME/.local/opt/remotex" BINDIR="$HOME/.local/bin" bash
```

Pass a release tag as the first argument to install a specific version:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh |
  bash -s -- v0.1.0
```

## Installed layout

```text
<prefix>/
├── etc/remotex.toml
├── versions/<version>/
├── current -> versions/<version>
└── .install.lock

<bindir>/remotex -> <prefix>/current/bin/remotex
```

The config is shared across versions. Each install atomically switches
`current` and retains the previous version.

## Upgrade and rollback

Run the installer again to upgrade. To roll back, point `current` to the
retained version:

```sh
ln -sfn versions/<previous> /opt/remotex/current
```

## Uninstall

From a checkout:

```sh
PREFIX=/opt/remotex BINDIR=/usr/local/bin bash packaging/uninstall.sh
```

The script removes the installation, launcher, and config. The optional macOS
companion agent has a separate uninstall procedure in
[`packaging/macos/README.md`](../packaging/macos/README.md).

## Build a tarball

```sh
bash packaging/build-tarball.sh
```

See [`packaging/README.md`](../packaging/README.md) for its contents and release
workflow.
