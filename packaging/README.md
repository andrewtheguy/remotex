# Packaging

Native packages are the release install contract. Linux ships both `.deb` and
`.rpm`; macOS ships `.pkg`. The distro-agnostic tarball remains the layout and
frontend input for container builds and the payload used by the
unsupported-platform quick installer. Containers replace its native binary with
a build that excludes the `embedded-gateway` default feature.

## Native layouts

Linux package managers own the conventional FHS paths directly:

```text
/usr/bin/remotex
/usr/share/remotex/web/
/usr/share/doc/remotex/remotex.example.toml
```

The macOS package owns the corresponding local prefix:

```text
/usr/local/bin/remotex
/usr/local/share/remotex/web/
/usr/local/share/doc/remotex/remotex.example.toml
```

The Windows package (`.msi`) owns the same tree under the 64-bit Program Files
directory and puts its `bin` on the machine `PATH`:

```text
C:\Program Files\remotex\bin\remotex.exe
C:\Program Files\remotex\share\remotex\web\
C:\Program Files\remotex\share\doc\remotex\remotex.example.toml
```

There is no package wrapper, version directory, active-version symlink, or
package-managed rollback. The package manager replaces and removes its files.

The live config is deliberately outside the manifests:
`/etc/remotex/remotex.toml` on Linux,
`/usr/local/etc/remotex/remotex.toml` on macOS and
`%ProgramData%\remotex\remotex.toml` on Windows. The operator creates it from the
example with mode `0600` and ownership of the account that runs the gateway.
That keeps both upgrades and removals away from stored credentials.

## Scripts

| Path | Purpose |
|---|---|
| `build-tarball.sh` | build the gateway and assemble the common release payload |
| `build-native-packages.sh` | consume that payload and build `.deb` + `.rpm` or `.pkg` |
| `build-windows-msi.ps1` | build the gateway on Windows and the `.msi` from `windows/remotex.wxs` (WiX 5) |
| `verify-windows-msi.ps1` | install that `.msi`, run the installed gateway, remove it, check nothing is left |
| `build-container-binary.sh` | build and verify a gateway with all default features disabled |
| `uninstall-macos-pkg.sh` | remove the installed `.pkg` by its receipt and forget it |
| `install.sh` | install the tarball fallback under a relocatable prefix |
| `uninstall.sh` | remove that fallback installation or one fallback version |
| `Dockerfile` | build an image from an extracted release tarball |

The repository-root `install.sh` downloads and verifies a release before
calling the tarball's `packaging/install.sh`. That path is retained only for a
Linux distribution that supports neither native package format.

## Local build

```sh
cd frontend && bun install --frozen-lockfile && cd ..
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

The native builder requires `dpkg-deb` and `rpmbuild` on Linux, or `pkgbuild` on
macOS. On Windows, in PowerShell 7 with WiX on `PATH`
(`dotnet tool install --global wix --version 5.0.2`):

```powershell
pwsh -File packaging\build-windows-msi.ps1
pwsh -File packaging\verify-windows-msi.ps1   # elevated: installs and removes it
```

Outputs are:

```text
dist/remotex-linux-amd64.deb
dist/remotex-linux-amd64.rpm
dist/remotex-macos-arm64.pkg
dist/remotex-windows-x86_64.msi
```

Arm Linux runners use `arm64` in the asset names. The tarballs retain their
existing versioned filenames because the quick installer selects and verifies
them by release version.

## Releases

`.github/workflows/release.yml` creates a draft, builds the frontend once, then
builds native packages and tarballs for Linux x86-64, Linux arm64, and macOS
arm64, and the MSI for Windows x86-64. The release is published only after the packages and common artifacts
succeed.

Container images take their layout and frontend from the Linux tarballs, then
replace `bin/remotex` with the separately built container gateway. The build
script, release smoke test, and Dockerfile all reject a binary that exposes
`tui`, `serve-embedded`, or `check-config --embedded`. The tarballs therefore remain
build plumbing and fallback payloads even though native packages are what users
are directed to install.
