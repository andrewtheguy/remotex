# Installing remotex

## Platform packages

Linux and Windows install a native package from the
[latest release](https://github.com/andrewtheguy/remotex/releases/latest).
macOS installs the same release binary and frontend through the Homebrew formula
in this repository. The live config remains outside the versioned payload, so
an upgrade or removal never replaces or deletes credentials.

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
/usr/share/doc/remotex/remotex.example.toml
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

### macOS (Homebrew)

The macOS release and formula are arm64-only. This repository is an opt-in tap;
it does not need to be listed in Homebrew's official formula index:

```sh
brew trust --formula andrewtheguy/remotex/remotex
brew tap andrewtheguy/remotex https://github.com/andrewtheguy/remotex
brew install andrewtheguy/remotex/remotex
```

The first command grants trust to this formula alone. It does not trust other
formulae or commands that may be added to the repository.

The formula installs the gateway CLI and web client into its Homebrew keg. Its
post-install step creates a starter config once at:

```text
$(brew --prefix)/etc/remotex/remotex.toml
```

The formula also defines a Homebrew service that runs `remotex serve`, not the
multi-instance TUI. Installation does not start it.

### Windows (`.msi`)

Windows x86-64, from an administrator's PowerShell:

```powershell
Invoke-WebRequest https://github.com/andrewtheguy/remotex/releases/latest/download/remotex-windows-x86_64.msi -OutFile remotex-windows-x86_64.msi
msiexec /i remotex-windows-x86_64.msi
```

The package is unsigned, so SmartScreen asks before it runs. It opens the usual
install wizard — folder, confirm, and a finish page once it is done — and
installs the same tree the Unix packages do, under `%ProgramFiles%\remotex`, and puts `bin`
on the machine `PATH`, so `remotex` works in a shell opened after the install:

```text
C:\Program Files\remotex\bin\remotex.exe
C:\Program Files\remotex\share\remotex\web\
C:\Program Files\remotex\share\doc\remotex\remotex.example.toml
```

The gateway reads its config from `%ProgramData%\remotex\remotex.toml`. Add
`/qn` for an unattended install. The multi-instance control plane
(`remotex tui`) is not available on Windows; the command exists and says so.

## First configuration

The config contains the web-login hash and target credentials. Keep it mode
`0600` and owned by the account that runs `remotex serve`. Linux and Windows
packages ship only the public [`remotex.example.toml`](../remotex.example.toml)
from which to create it; the macOS formula copies that example once during its
post-install step.

On Linux:

```sh
sudo install -d -m 700 -o "$(id -un)" -g "$(id -gn)" /etc/remotex
sudo install -m 600 -o "$(id -un)" -g "$(id -gn)" \
  /usr/share/doc/remotex/remotex.example.toml /etc/remotex/remotex.toml
remotex gen-passwd admin
${EDITOR:-vi} /etc/remotex/remotex.toml
```

On macOS:

```sh
remotex gen-passwd admin
${EDITOR:-vi} "$(brew --prefix)/etc/remotex/remotex.toml"
```

On Windows, from a PowerShell opened after the install, where only the account
that runs the gateway may read the file:

```powershell
New-Item -ItemType Directory -Force "$env:ProgramData\remotex" | Out-Null
Copy-Item "$env:ProgramFiles\remotex\share\doc\remotex\remotex.example.toml" "$env:ProgramData\remotex\remotex.toml"
icacls "$env:ProgramData\remotex\remotex.toml" /inheritance:r /grant:r "${env:USERNAME}:F"
remotex gen-passwd admin
notepad "$env:ProgramData\remotex\remotex.toml"
```

Paste the generated `admin:$2b$...` value into `[server].site_passwd` and
replace the example `[[targets]]` entry with the remote desktop to reach. On
Linux or Windows, start the gateway in the foreground:

```sh
remotex serve
```

On macOS, start the non-TUI gateway now and at each login:

```sh
brew services start andrewtheguy/remotex/remotex
brew services info andrewtheguy/remotex/remotex
```

The service logs to `$(brew --prefix)/var/log/remotex/stdout.log` and
`stderr.log`. Run `brew services run andrewtheguy/remotex/remotex` instead when
the gateway should run now without being registered for future logins.
Without `sudo`, Homebrew registers a per-user LaunchAgent. To run at boot while
still dropping to the current account, use this start command instead:

```sh
sudo brew services start --sudo-service-user="$(id -un)" \
  andrewtheguy/remotex/remotex
```

For a Mac target, configure `protocol = "vnc"`, `subtype = "ard"`, and the Mac
account's username and password. The gateway connects directly to macOS Screen
Sharing; nothing is installed on the target Mac.

## Upgrade

Update through the platform package manager:

```sh
sudo apt install ./remotex-linux-amd64.deb
sudo dnf upgrade ./remotex-linux-amd64.rpm
brew upgrade andrewtheguy/remotex/remotex
brew services restart andrewtheguy/remotex/remotex
msiexec /i remotex-windows-x86_64.msi
```

Use only the commands for the host platform. The live config remains untouched.
The stable release workflow updates the formula's release URL and SHA-256 after
it publishes the matching macOS tarball; `brew update` receives that commit.

## Uninstall

On Debian or Ubuntu:

```sh
sudo apt remove remotex
```

On an RPM distribution:

```sh
sudo dnf remove remotex
```

On macOS, stop and unregister the LaunchAgent before removing the formula:

```sh
brew services stop andrewtheguy/remotex/remotex
brew uninstall andrewtheguy/remotex/remotex
brew untap andrewtheguy/remotex
brew untrust --formula andrewtheguy/remotex/remotex
```

If the service was started at boot with `sudo`, stop it with the same command
prefix: `sudo brew services stop andrewtheguy/remotex/remotex`.

On Windows, remove remotex from **Apps & features**, or from an administrator's
PowerShell with the package file or without it:

```powershell
msiexec /x remotex-windows-x86_64.msi
Get-Package remotex | Uninstall-Package
```

None of these touch the live config. Remove `/etc/remotex` on Linux,
`$(brew --prefix)/etc/remotex` on macOS or `%ProgramData%\remotex` on Windows
separately only when the credentials and configuration should be deleted too.

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
is not part of a platform package manager's upgrade or removal flow.

## Build release artifacts

Build the tarball for the current host:

```sh
bash packaging/build-tarball.sh
```

On Linux, turn that tarball into both native packages:

```sh
bash packaging/build-native-packages.sh
```

The macOS tarball is the Homebrew formula's payload; the stable release workflow
updates `Formula/remotex.rb` with its exact release URL and SHA-256 after
publishing it. See [`packaging/README.md`](../packaging/README.md) for the release
workflow.

## Apple High Performance audio (build it yourself)

The Mac's system audio on an `ard-high-performance` target is behind a Cargo
feature that no release binary, package or container image includes: the stream is
AAC-ELD, and the only portable decoder for it is Fraunhofer's fdk-aac, whose licence
is not OSI-approved. To have it, build the gateway from source with the feature:

```sh
bun install --cwd frontend
cargo build --release --features apple-hp-audio
```

The build downloads a prebuilt static fdk-aac archive the same way it already
downloads FreeRDP, libopus and libvpx — no C++ toolchain is needed. Then set
`audio = true` on the target. A gateway built without the feature refuses that
key at startup and says so. The Mac sends its audio by UDP to the gateway's
address on a port it chooses (the VNC port's number, as measured), so the gateway
must be reachable from the Mac by UDP; behind a NAT or a firewall the session runs
without sound and the log says why.
