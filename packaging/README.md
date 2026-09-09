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

No package installs a service either. `windows/remotex-service.ps1` is a
template the operator copies and applies by hand, deliberately outside every
manifest so that upgrading or removing remotex cannot register, reconfigure or
delete a service. It supervises the installed `remotex serve` with NSSM, whose
console Ctrl+C stop reaches the signal `serve` already waits on. See
[Windows service](../docs/install.md#windows-service-template).

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
| `windows/remotex-service.ps1` | operator template: register the installed gateway as an NSSM-supervised Windows service |
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

## x86-64 CPU compatibility

The Linux x86-64 binary targets the baseline x86-64 ISA and dispatches SIMD at
run time. Neither Cargo configuration nor packaging and CI set `target-cpu`, and
the prebuilt archives downloaded by the sys crates must use the same baseline
with their hand-written kernels selected by CPUID: libvpx's rtcd tables, opus's
`MAY_HAVE` dispatch, and FreeRDP's primitives autodetection. The sys crates fetch
each dependency repository's latest release, so that release's archives—not the
tag pinned in this repository's `Cargo.toml`—set the effective CPU floor.

This policy is measured, not merely conservative. On an i5-8500T,
`target-cpu=x86-64-v3` made PNG tile encoding 1.7 times slower through changes to
the autovectorized `png`/`fdeflate` loops. VP9 encoding was within noise of a
v3-scalar libvpx, while an opus archive using runtime dispatch consumed 0.73% of
a core against 0.65% with `PRESUME_AVX2`. A global v3 floor also caused `SIGILL`
at startup on Ivy Bridge. If a Rust hot path benefits from AVX2, guard a separate
function with `is_x86_feature_detected!` and `#[target_feature]`; never raise the
binary's global CPU floor.

## Prebuilt native dependencies

Release builds link `opus-prebuilt`, `libvpx-prebuilt`, and
`libfreerdp-prebuilt`. Their sys crates download static archives instead of
building vendored C, so this project needs no CMake, assembler, pkg-config,
libclang, vcpkg, or system copies of those libraries. Do not restore
`LIBOPUS_STATIC`, `LIBOPUS_NO_PKG`, `CMAKE_POLICY_VERSION_MINIMUM`, or a source
libopus build in `build-tarball.sh`. The libvpx archives are VP9-only and built
with `--enable-realtime-only`; additional features need a separately built
archive selected with `LIBVPX_PREBUILT_DIR`, not a source-build fallback.

The optional `apple-hp-audio` feature follows the same archive model through
`fdk-aac-prebuilt`, but its non-OSI-approved license keeps it out of every release
artifact. See [Audio frames](../docs/architecture.md#audio-frames) for the media
design and [Installing remotex](../docs/install.md#apple-high-performance-audio-build-it-yourself)
for a manual feature build.

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
