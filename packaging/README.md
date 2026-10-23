# Packaging

Native packages are the release install contract. Linux ships both `.deb` and
`.rpm`; macOS ships `.pkg`. The distro-agnostic tarball is the layout input
for native package and container builds, and no release asset. Containers replace its native binary
with a build that excludes the `embedded-gateway` default feature.

Every artifact carries one gateway binary with the web client compiled into it
(`src/assets.rs` embeds the bundle from Cargo's private output directory at build
time). No package installs a web directory, and there is nothing to point the
gateway at.

## Native layouts

Linux package managers own the conventional FHS paths directly:

```text
/usr/bin/zdesk
/usr/share/doc/zdesk/zdesk.example.toml
/usr/share/doc/zdesk/LICENSE
```

The macOS package owns the corresponding local prefix:

```text
/usr/local/bin/zdesk
/usr/local/share/doc/zdesk/zdesk.example.toml
/usr/local/share/doc/zdesk/LICENSE
```

By default the Windows package (`.msi`) owns the same tree under the 64-bit Program
Files directory and puts its `bin` on the machine `PATH`; its install wizard can
select another directory:

```text
C:\Program Files\zdesk\bin\zdesk.exe
C:\Program Files\zdesk\VERSION
C:\Program Files\zdesk\share\doc\zdesk\zdesk.example.toml
C:\Program Files\zdesk\share\doc\zdesk\LICENSE
```

Every artifact, container images included, carries zdesk's MIT `LICENSE`.

There is no package wrapper, version directory, active-version symlink, or
package-managed rollback. The package manager replaces and removes its files.

The live config is deliberately outside the manifests:
`/etc/zdesk/zdesk.toml` on Linux,
`/usr/local/etc/zdesk/zdesk.toml` on macOS and
`%ProgramData%\zdesk\zdesk.toml` on Windows. The operator creates it from the
example with mode `0600` and ownership of the account that runs the gateway.
That keeps both upgrades and removals away from stored credentials.

What the gateway keeps between runs — today only the `[meter]` database — is
outside the manifests for the same reason, in a state directory the gateway
creates when it first needs it: `/var/lib/zdesk` on Linux,
`/usr/local/var/zdesk` on macOS and `%ProgramData%\zdesk` on Windows. The
account that runs the gateway must be able to create or write it. The container
uses `/opt/zdesk/var`, which wants a volume for the records to outlive it.

## Scripts

| Path | Purpose |
|---|---|
| `build-tarball.sh` | build the gateway and assemble the common release payload |
| `build-native-packages.sh` | consume that payload and build `.deb` + `.rpm` or `.pkg` |
| `build-windows-msi.ps1` | build the gateway on Windows and the `.msi` from `windows/zdesk.wxs` (WiX 5) |
| `verify-windows-msi.ps1` | install that `.msi`, run the installed gateway, remove it, check nothing is left |
| `build-container-binary.sh` | build and verify a gateway with default features disabled, plus any `ZDESK_CONTAINER_FEATURES` |
| `uninstall-macos-pkg.sh` | remove the installed `.pkg` by its receipt and forget it |
| `Dockerfile` | build an image from an extracted release tarball |

## Local build

```sh
cd frontend && bun install --frozen-lockfile && cd ..
bash packaging/build-tarball.sh
bash packaging/build-native-packages.sh
```

The frontend's
build compiles two WebAssembly modules: one from the gateway's graphics crate
(`frontend/wasm/egfx` around
`crates/zdesk-rdp-graphics`, the page's compositor for a passed RDP pipeline),
and the page's decoder for a Mac's Adaptive picture (`frontend/wasm/apple-adaptive`),
which the stable toolchain builds. So wherever the frontend is built — `bun run build`, or a
Cargo build without `ZDESK_PREBUILT_FRONTEND` — the compositor's module is built by the nightly
toolchain `frontend/wasm/egfx/rust-toolchain.toml` pins, which its threads need and
nothing else is built with. rustup installs it on the first build, unless
`RUSTUP_AUTO_INSTALL=0` turns that off; `rustup toolchain install` in that
directory installs it ahead of the build, as release CI does. wasm-pack comes with `bun install`. The native builder requires `dpkg-deb` and `rpmbuild` on Linux, or
`pkgbuild` on macOS. On Windows, in PowerShell 7 with WiX on `PATH`
(`dotnet tool install --global wix --version 5.0.2`):

```powershell
pwsh -File packaging\build-windows-msi.ps1
pwsh -File packaging\verify-windows-msi.ps1   # elevated: installs and removes it
```

Outputs are:

```text
dist/zdesk-linux-amd64.deb
dist/zdesk-linux-amd64.rpm
dist/zdesk-macos-arm64.pkg
dist/zdesk-windows-x86_64.msi
```

Arm Linux runners use `arm64` in the asset names. The tarballs keep versioned
filenames, which the container build selects by release version.

## x86-64 CPU compatibility

An x86-64 gateway is built for the baseline x86-64 ISA: nothing sets
`target-cpu`, and SIMD is chosen at run time. The prebuilt archives the sys
crates download keep that baseline, with their hand-written kernels selected by
CPUID: opus's `MAY_HAVE` dispatch and libFLAC's own CPU detection, as the tile
coder's JPEG encoder picks its AVX2 kernels at run time. The sys crates fetch each dependency repository's latest release, so
that release's archives—not the tag pinned in this repository's
`Cargo.toml`—set the effective CPU floor.

## Prebuilt native dependencies

Release builds link `opus-prebuilt` and, under
[sound-flac](https://github.com/andrewtheguy/sound-flac), `libflac-prebuilt`.
Their sys crates download static archives instead of building vendored C and
C++, so this project needs no CMake, assembler, pkg-config, libclang, vcpkg, or
system copies of those libraries, and no artifact carries or depends on one.
`LIBOPUS_PREBUILT_DIR` and `LIBFLAC_PREBUILT_DIR` select locally built archives.

No artifact holds the software HEVC decoder, the page's decoder of a High
Performance Mac's passed HEVC, whose licence keeps it out: an operator
downloads the release that `src/hevc_wasm.rs` pins by version and SHA-256 from
the private `andrewtheguy/hevc-wasm-archives` through `gh`, and every build
serves it when it finds it. Every release target looks for it by its release
name in `share/zdesk`, beside the `share/doc/zdesk` it installs, unless
`[hevc_wasm].archive` names another file:
`/usr/share/zdesk` for the `.deb` and `.rpm`, `/usr/local/share/zdesk` for
the `.pkg`, `share\zdesk` under the `.msi`'s install directory, and
`/opt/zdesk/versions/<version>/share/zdesk` in the
container image. No package owns or makes that directory: the operator does. A
container can be given it from outside the image, as the private image
`ghcr.io/andrewtheguy/hevc-wasm-archives` that hevc-wasm's `publish-image.sh`
pushes under hevc-wasm's version, holding that archive and every one before it,
mounted on that directory as an image volume. The gateway
decodes none of the Mac's streams: its HEVC and its H.264 are passed, and its
AAC-ELD sound too, so no artifact needs FFmpeg.
Do not restore
`LIBOPUS_STATIC`, `LIBOPUS_NO_PKG`, `CMAKE_POLICY_VERSION_MINIMUM`, or a source
libopus build in `build-tarball.sh`.

The C libraries built from source are SQLite, for `[meter]`, through `rusqlite`'s
`bundled` feature, and jemalloc, through `tikv-jemallocator`.
jemalloc is the global allocator on Linux and is linked with unprefixed `malloc` symbols,
so allocations in libopus and libFLAC use it as well as Rust allocations. The build
environment must provide a C compiler and `make`. macOS and Windows, which have no
glibc arenas, keep their system allocators.

The choice is based on a 2026-09-12 comparison of six consecutive motion-heavy
sessions against one RDP target. The observed process footprint reached 289 MB
with glibc malloc and stayed between 27 and 34 MB with jemalloc. The measurement
record in this repository does not preserve the exact process-memory metric or
command, so these values justify the allocator choice but are not a reproducible
cross-platform benchmark.
The suspected cause was glibc's per-thread arenas accumulating across each
session's engine and encode-worker threads.

jemalloc's build-time page-size setting also matters on Linux arm64. Release CI
sets `JEMALLOC_SYS_WITH_LG_PAGE=16` for that target, selecting a 2^16-byte
allocator page. The `tikv-jemalloc-sys` version locked here documents that this
configured size must be at least the system page size and may be larger, so 64 KiB
covers the 4, 16 and 64 KiB kernel page sizes in scope. The current workflow builds
the native artifact on an arm64 CI runner but does not execute it on each
non-4-KiB kernel. Release validation on 16- and 64-KiB systems remains the check
for compatibility of the complete binary.

## Releases

`.github/workflows/release.yml` creates a draft, builds the frontend once, then
builds native packages and tarballs for Linux x86-64, Linux arm64, and macOS
arm64, and the MSI for Windows x86-64. The release is published only after the packages and common artifacts
succeed.

Container images take their layout from the Linux tarballs, then replace
`bin/zdesk` with the separately built container gateway. The build
script, release smoke test, and Dockerfile all reject a binary that exposes
`tui`, `serve-embedded`, or `check-config --embedded`. The tarballs are build
plumbing between the workflow's jobs and are not published: the native packages
and the image are what bring the gateway everything it needs.
