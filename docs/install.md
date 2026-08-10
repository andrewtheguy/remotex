# Installing remotex

## Native packages

Install a native package from the
[latest release](https://github.com/andrewtheguy/remotex/releases/latest). The
package manager owns the gateway executable, frontend bundle, and config
example. It does not own the live config, so an upgrade or removal never
replaces or deletes credentials.

### Debian and Ubuntu (`.deb`)

Releases provide `remotex-linux-amd64.deb` and
`remotex-linux-arm64.deb`:

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-linux-amd64.deb
sudo apt install ./remotex-linux-amd64.deb
```

Use the `arm64` filename on an arm64 host. The package installs:

```text
/usr/bin/remotex
/usr/share/remotex/web/
/usr/share/doc/remotex/remotex.toml.example
```

### Fedora, RHEL, and other RPM distributions (`.rpm`)

Releases provide `remotex-linux-amd64.rpm` and
`remotex-linux-arm64.rpm`:

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-linux-amd64.rpm
sudo dnf install ./remotex-linux-amd64.rpm
```

Use the `arm64` filename on an arm64 host. The package uses the same `/usr/bin`
and `/usr/share` layout as the `.deb`. `sudo rpm -i` and a distribution's other
RPM frontend work too, but `dnf` is preferred because it resolves dependencies.

### macOS (`.pkg`)

The gateway package is arm64:

```sh
curl -fsSLO https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-macos-arm64.pkg
sudo installer -pkg remotex-macos-arm64.pkg -target /
```

It installs:

```text
/usr/local/bin/remotex
/usr/local/share/remotex/web/
/usr/local/share/doc/remotex/remotex.toml.example
```

The package is unsigned and not notarized. A browser download is quarantined,
so fetch it with `curl` as shown and install it from the terminal. This `.pkg`
is the browser gateway. The release's `.dmg` is the separate **remotex.app**
viewer described in [`macos-viewer.md`](macos-viewer.md).

## First configuration

The config contains the web-login hash and target credentials. Create it as the
account that will run `remotex serve`, mode `0600`. The package ships only the
public example from which to create it.

On Linux:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/remotex/remotex.toml.example /etc/remotex/remotex.toml
remotex gen-passwd admin
${EDITOR:-vi} /etc/remotex/remotex.toml
```

On macOS:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /usr/local/etc/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/local/share/doc/remotex/remotex.toml.example \
  /usr/local/etc/remotex/remotex.toml
remotex gen-passwd admin
${EDITOR:-vi} /usr/local/etc/remotex/remotex.toml
```

Paste the generated `admin:$2b$...` value into `[server].site_passwd` and
replace the example `[[targets]]` entry with the remote desktop to reach. Start
the gateway in the foreground:

```sh
remotex serve
```

For a Mac target, configure `protocol = "vnc"`, `subtype = "ard"`, and the Mac
account's username and password. The gateway connects directly to macOS Screen
Sharing; nothing is installed on the target Mac.

## Upgrade

Download the new asset and hand it to the same package manager:

```sh
sudo apt install ./remotex-linux-amd64.deb
sudo dnf upgrade ./remotex-linux-amd64.rpm
sudo installer -pkg remotex-macos-arm64.pkg -target /
```

Use only the command for the host platform. Package files are replaced in
place. The live config remains untouched because it is outside every package
manifest.

## Uninstall

On Debian or Ubuntu:

```sh
sudo apt remove remotex
```

On an RPM distribution:

```sh
sudo dnf remove remotex
```

On macOS:

```sh
sudo rm -f /usr/local/bin/remotex
sudo rm -rf /usr/local/share/remotex /usr/local/share/doc/remotex
sudo pkgutil --forget com.andrewtheguy.remotex.gateway
```

All three leave the live config behind. Remove `/etc/remotex` on Linux or
`/usr/local/etc/remotex` on macOS separately only when the credentials and
configuration should be deleted too.

## Unsupported-package fallback

The quick installer is only for a Linux distribution that can run the release
binary but supports neither `.deb` nor `.rpm`. It downloads the release tarball,
verifies its SHA-256 digest, and installs under `/opt/remotex`:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh | bash
```

`PREFIX` and `BINDIR` change its install locations:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh |
  PREFIX="$HOME/.local/opt/remotex" BINDIR="$HOME/.local/bin" bash
```

Pass a release tag as its first argument to install a specific version:

```sh
curl -fsSL https://andrewtheguy.github.io/remotex/install.sh |
  bash -s -- v0.0.144
```

The quick installer keeps its own versioned layout and rollback mechanism. It
is not part of the native package upgrade or removal flow.

## Build release packages

Build the tarball input, then the native package for the current host:

```sh
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

Linux builds both `.deb` and `.rpm`; macOS builds `.pkg`. See
[`packaging/README.md`](../packaging/README.md) for the release workflow.
