# Installing zdesk

## Native packages

Install a native package from the
[latest release](https://github.com/andrewtheguy/zdesk/releases/latest). The
package manager owns the gateway executable, config example and licence; the
web client is compiled into the executable. It does not own the live config, so
an upgrade or removal never replaces or deletes credentials.

Linux release binaries require glibc 2.39 or newer: Debian 13, Ubuntu 24.04,
or later releases, and RPM distributions with the same baseline. The package
format alone does not make the binary compatible with an older distribution.

### Debian 13+ and Ubuntu 24.04+ (`.deb`)

Releases provide `zdesk-linux-amd64.deb` and
`zdesk-linux-arm64.deb`:

```sh
curl -fsSLO https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-linux-amd64.deb
sudo apt install ./zdesk-linux-amd64.deb
```

Use the `arm64` filename on an arm64 host. The package installs:

```text
/usr/bin/zdesk
/usr/share/doc/zdesk/zdesk.example.toml
/usr/share/doc/zdesk/LICENSE
```

### RPM distributions with glibc 2.39+ (`.rpm`)

Releases provide `zdesk-linux-amd64.rpm` and
`zdesk-linux-arm64.rpm`:

```sh
curl -fsSLO https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-linux-amd64.rpm
sudo dnf install ./zdesk-linux-amd64.rpm
```

Use the `arm64` filename on an arm64 host. The package uses the same `/usr/bin`
and `/usr/share` layout as the `.deb`. `sudo rpm -i` and a distribution's other
RPM frontend work too, but `dnf` is preferred because it resolves dependencies.

### macOS (`.pkg`)

The gateway package is arm64:

```sh
curl -fsSLO https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-macos-arm64.pkg
sudo installer -pkg zdesk-macos-arm64.pkg -target /
```

It installs:

```text
/usr/local/bin/zdesk
/usr/local/share/doc/zdesk/zdesk.example.toml
/usr/local/share/doc/zdesk/LICENSE
```

The package is unsigned and not notarized. A browser download is quarantined,
so fetch it with `curl` as shown and install it from the terminal. The `.pkg`
contains the gateway CLI, with the web client inside it.

### Windows (`.msi`)

Windows x86-64, from PowerShell 7 (`pwsh`) run as administrator:

```powershell
Invoke-WebRequest https://github.com/andrewtheguy/zdesk/releases/latest/download/zdesk-windows-x86_64.msi -OutFile zdesk-windows-x86_64.msi
msiexec /i zdesk-windows-x86_64.msi
```

The package is unsigned, so SmartScreen asks before it runs. It opens the usual
install wizard — folder, confirm, and a finish page once it is done — and
installs the same tree the Unix packages do, by default under `%ProgramFiles%\zdesk`, and puts `bin`
on the machine `PATH`, so `zdesk` works in a shell opened after the install:

```text
C:\Program Files\zdesk\bin\zdesk.exe
C:\Program Files\zdesk\VERSION
C:\Program Files\zdesk\share\doc\zdesk\zdesk.example.toml
C:\Program Files\zdesk\share\doc\zdesk\LICENSE
```

The gateway reads its config from `%ProgramData%\zdesk\zdesk.toml`. Add
`/qn` for an unattended install. The multi-instance control plane,
`zdesk tui`, is included and keeps its instances under
`%LOCALAPPDATA%\zdesk\instances`.

## First configuration

The config contains the web-login hash and target credentials. Create it as the
account that will run `zdesk serve`, mode `0600`. The package ships only the
public [`zdesk.example.toml`](../zdesk.example.toml) from which to create it.
The same account owns the state directory, where an enabled `[meter]` keeps its
database by default.

On Linux:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/zdesk /var/lib/zdesk
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/zdesk/zdesk.example.toml /etc/zdesk/zdesk.toml
zdesk gen-passwd admin
${EDITOR:-vi} /etc/zdesk/zdesk.toml
```

On macOS:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /usr/local/etc/zdesk /usr/local/var/zdesk
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/local/share/doc/zdesk/zdesk.example.toml \
  /usr/local/etc/zdesk/zdesk.toml
zdesk gen-passwd admin
${EDITOR:-vi} /usr/local/etc/zdesk/zdesk.toml
```

On Windows, from PowerShell 7 (`pwsh`) opened after the default-path install, where only the account
that runs the gateway may read the directory — the config and the throughput database
with its SQLite files inherit that. If the wizard selected another folder, use its
`share\doc\zdesk\zdesk.example.toml` as the `Copy-Item` source instead:

```powershell
New-Item -ItemType Directory -Force "$env:ProgramData\zdesk" | Out-Null
icacls "$env:ProgramData\zdesk" /inheritance:r /grant:r "${env:USERNAME}:(OI)(CI)F"
Copy-Item "$env:ProgramFiles\zdesk\share\doc\zdesk\zdesk.example.toml" "$env:ProgramData\zdesk\zdesk.toml"
zdesk gen-passwd admin
notepad "$env:ProgramData\zdesk\zdesk.toml"
```

Paste the generated `admin:$2b$...` value into `[server].site_passwd` and
replace the example `[[targets]]` entry with the remote desktop to reach. Its
password is not written there: the target names it with `password_ref`, and
the config keeps it encrypted under your master password, which the gateway
reads from the file `[server].master_password_file` names. Write the master
password alone to a file beside the config, which only your account can read,
and name it there (`master_password_file = "master-password"`; a relative path
is taken from the config's directory). `password master` then prints the
`[master]` block to paste into the config, and `password encrypt` the value to
paste as `example = "…"` under `[passwords]`; both read the master password
from that file:

```sh
zdesk password master
zdesk password encrypt
```

See [Target passwords](guide.md#target-passwords). Start the gateway in the
foreground:

```sh
zdesk serve
```

Then open <http://localhost:52380>. [Using zdesk](guide.md) says how each
kind of target is set up and how the page is reached from another machine.

## Container

```sh
docker run -d --name zdesk -p 52380:52380 \
  -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro \
  ghcr.io/andrewtheguy/zdesk:latest
```

Set `[server].listen = "0.0.0.0:52380"` in the mounted config, or pass the same
address as `-e ZDESK_LISTEN=0.0.0.0:52380`. With `[meter].enabled` set, mount
a volume at `/opt/zdesk/var` too, or the records go with the container. Images
are published for Linux amd64 and arm64 with `latest` and `v<version>` tags. The
image has no `zdesk tui`.

Generate the required web-login credential with:

```sh
docker run --rm -it ghcr.io/andrewtheguy/zdesk:latest gen-passwd admin
```

The master password comes from a file, as for any served gateway. Keep it in
a secret, name that file in the config, and write the config's passwords with
the same image, pasting what each prints:

```toml
[server]
master_password_file = "/run/secrets/zdesk_master"
```

```sh
# asked without echo, kept out of shell history, written 0600
(umask 077; trap 'stty echo' EXIT; trap 'exit 1' INT HUP TERM; stty -echo; printf 'Master password: '; IFS= read -r pw; echo; printf '%s' "$pw" > zdesk_master)
docker run --rm -it -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro \
  -v ./zdesk_master:/run/secrets/zdesk_master:ro \
  ghcr.io/andrewtheguy/zdesk:latest password master -c /opt/zdesk/etc/zdesk.toml
docker run --rm -it -v ./zdesk.toml:/opt/zdesk/etc/zdesk.toml:ro \
  -v ./zdesk_master:/run/secrets/zdesk_master:ro \
  ghcr.io/andrewtheguy/zdesk:latest password encrypt
```

Mount the same file read-only into the running gateway, as above or as a Docker,
Podman or Kubernetes secret. Under Helm the printed `[master]` block and values
belong in the values the chart renders the config from; they are ciphertext, so
only the master password needs to be a secret.

## Upgrade

Download the new asset and hand it to the same package manager:

```sh
sudo apt install ./zdesk-linux-amd64.deb
sudo dnf upgrade ./zdesk-linux-amd64.rpm
sudo installer -pkg zdesk-macos-arm64.pkg -target /
msiexec /i zdesk-windows-x86_64.msi
```

Use only the command for the host platform. Package files are replaced in
place. The live config remains untouched because it is outside every package
manifest.

## Uninstall

On Debian or Ubuntu:

```sh
sudo apt remove zdesk
```

On an RPM distribution:

```sh
sudo dnf remove zdesk
```

On macOS there is no package manager to ask, so the repository ships an
uninstaller that reads the installed receipt and removes exactly what the
package wrote:

```sh
curl -fsSLO https://raw.githubusercontent.com/andrewtheguy/zdesk/main/packaging/uninstall-macos-pkg.sh
sudo bash uninstall-macos-pkg.sh
```

`--dry-run` prints the removals without making them and needs no `sudo`. The
script forgets the receipt afterwards, and keeps any payload directory that
still holds a file the operator put there. Doing it by hand is the same steps
against the current layout:

```sh
sudo rm -f /usr/local/bin/zdesk
sudo rm -rf /usr/local/share/doc/zdesk
sudo pkgutil --forget com.andrewtheguy.zdesk.gateway
```

On Windows, remove zdesk from **Apps & features**.

None of these touch the live config or state. Remove `/etc/zdesk` and
`/var/lib/zdesk` on Linux, `/usr/local/etc/zdesk` and
`/usr/local/var/zdesk` on macOS, or `%ProgramData%\zdesk` on Windows
separately only when the credentials, configuration, and throughput history
should be deleted too.

## Build release packages

Build the tarball input, then the native package for the current host:

```sh
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

Linux builds both `.deb` and `.rpm`; macOS builds `.pkg`. See
[`packaging/README.md`](../packaging/README.md) for the release workflow.
