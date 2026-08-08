# Installing remotex

## Install

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
```

The installer selects the current OS and architecture, verifies the release
SHA-256 digest, and installs under `/opt/remotex`. It may use `sudo` for the
prefix and `/usr/local/bin/remotex` link.

Generate a web-login credential, then open the installed config:

```sh
remotex gen-passwd admin
${EDITOR:-vi} /opt/remotex/etc/remotex.toml
```

Paste the generated `admin:$2b$...` value into `[server].site_passwd` and
replace the example `[[targets]]` entry with the remote desktop you want to
reach. Then start the gateway in the foreground:

```sh
remotex serve
```

The full config template is at
`/opt/remotex/current/share/doc/remotex/remotex.toml.example`. Keep the live
config readable only by the service user because it contains credentials.

For a Mac target, the gateway connects directly to its built-in Screen Sharing
over VNC; no companion software is required. Configure it as a `vnc` target with
`subtype = "ard"` and the Mac account's username and password. That subtype is
Apple Screen Sharing's Standard mode over RFB 3.8. It selects Apple Remote
Desktop authentication and the Mac's physical displays, so the connection lands
at the user's own screen rather than a separate login-window session.

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
  bash -s -- v0.0.130
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

The script removes the installation, launcher, and config.

## Build a tarball

```sh
bash packaging/build-tarball.sh
```

See [`packaging/README.md`](../packaging/README.md) for its contents and release
workflow.
