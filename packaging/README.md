# Packaging

Native packages are the release install contract. Linux ships both `.deb` and
`.rpm`; macOS ships `.pkg`. The distro-agnostic tarball is the layout input
for native package and container builds, and no release asset. Containers replace its native binary
with a build that excludes the `embedded-gateway` default feature and keeps
`native-vp9`.

Every artifact carries one gateway binary with the web client compiled into it
(`src/assets.rs` embeds the bundle from Cargo's private output directory at build
time). No package installs a web directory, and there is nothing to point the
gateway at.

## Native layouts

Linux package managers own the conventional FHS paths directly:

```text
/usr/bin/remotex
/usr/share/doc/remotex/remotex.example.toml
/usr/share/doc/remotex/LICENSE
```

The macOS package owns the corresponding local prefix:

```text
/usr/local/bin/remotex
/usr/local/share/doc/remotex/remotex.example.toml
/usr/local/share/doc/remotex/LICENSE
```

By default the Windows package (`.msi`) owns the same tree under the 64-bit Program
Files directory and puts its `bin` on the machine `PATH`; its install wizard can
select another directory:

```text
C:\Program Files\remotex\bin\remotex.exe
C:\Program Files\remotex\VERSION
C:\Program Files\remotex\share\doc\remotex\remotex.example.toml
C:\Program Files\remotex\share\doc\remotex\LICENSE
```

Every artifact, container images included, carries remotex's MIT `LICENSE`.

There is no package wrapper, version directory, active-version symlink, or
package-managed rollback. The package manager replaces and removes its files.

The live config is deliberately outside the manifests:
`/etc/remotex/remotex.toml` on Linux,
`/usr/local/etc/remotex/remotex.toml` on macOS and
`%ProgramData%\remotex\remotex.toml` on Windows. The operator creates it from the
example with mode `0600` and ownership of the account that runs the gateway.
That keeps both upgrades and removals away from stored credentials.

What the gateway keeps between runs — today only the `[meter]` database — is
outside the manifests for the same reason, in a state directory the gateway
creates when it first needs it: `/var/lib/remotex` on Linux,
`/usr/local/var/remotex` on macOS and `%ProgramData%\remotex` on Windows. The
account that runs the gateway must be able to create or write it. The container
uses `/opt/remotex/var`, which wants a volume for the records to outlive it.

## Scripts

| Path | Purpose |
|---|---|
| `build-tarball.sh` | build the gateway and assemble the common release payload |
| `build-native-packages.sh` | consume that payload and build `.deb` + `.rpm` or `.pkg` |
| `build-windows-msi.ps1` | build the gateway on Windows and the `.msi` from `windows/remotex.wxs` (WiX 5) |
| `verify-windows-msi.ps1` | install that `.msi`, run the installed gateway, remove it, check nothing is left |
| `build-container-binary.sh` | build and verify a gateway with default features disabled, but with `REMOTEX_CONTAINER_FEATURES`, which is `native-vp9` unless set |
| `publish-full-image.sh` | add Debian's libavcodec, and the pinned software HEVC decoder's archive, to a release's public linux/amd64 image and push the result to the private `ghcr.io/andrewtheguy/remotex-full` |
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
`crates/remotex-rdp-graphics`, the page's compositor for a passed RDP pipeline),
and the page's FLAC decoder for a session's lossless sound (`frontend/wasm/flac`),
which the stable toolchain builds. It also takes a third module it does not
build, the page's software VP9 decoder: the release of
[vp9-wasm](https://github.com/andrewtheguy/vp9-wasm) that
`frontend/wasm/vp9/pin.json` names by version and SHA-256, downloaded from that
repository's public releases and unpacked into `frontend/wasm/vp9/pkg`. A build
with no network sets `REMOTEX_VP9_WASM_ARCHIVE` to the whole path of a copy of
that archive; either way an archive that is not the pinned one is refused. The
pin is the one place a release is changed. So wherever the frontend is built — `bun run build`, or a
Cargo build without `REMOTEX_PREBUILT_FRONTEND` — the compositor's module is built by the nightly
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
dist/remotex-linux-amd64.deb
dist/remotex-linux-amd64.rpm
dist/remotex-macos-arm64.pkg
dist/remotex-windows-x86_64.msi
```

Arm Linux runners use `arm64` in the asset names. The tarballs keep versioned
filenames, which the container build selects by release version.

## x86-64 CPU compatibility

An x86-64 gateway is built for AVX2, and a machine without it is the fallback
build below. The default `native-vp9` feature is screen-vp9's experimental VP9
encoder in Rust, which a target's `experimental_native_vp9` asks for; its
kernels are AVX2 with no choosing at run time, and it refuses to build without
them. Cargo cannot set a flag by feature, so `.cargo/config.toml` sets
`-C target-feature=+avx2` for every x86-64 build, and every release artifact
for x86-64, the container's binary included, needs a processor with AVX2. Arm64
needs nothing set: NEON is in its baseline.

The fallback is that feature off and the flag taken back out, which the
environment does, since `RUSTFLAGS` replaces the configuration's:

```sh
RUSTFLAGS='-C target-feature=-avx2' RUSTDOCFLAGS='-C target-feature=-avx2' \
  cargo build --release --no-default-features --features embedded-gateway
```

For a container's binary it is the same environment with
`REMOTEX_CONTAINER_FEATURES=` set empty. Such a gateway has libvpx alone, says
so in `--help` (`Features:` without `native-vp9`), and refuses
`experimental_native_vp9` on every target but a `wlshare` one, whose encoder
is wlshare's own.

A Cargo feature may need a CPU flag of the machine this way, AVX2 or another:
its flag goes in `.cargo/config.toml` beside it, and the fallback is the
feature off and the flag taken back out. The fallback build goes without what
the feature does, and nothing limits what that may be: were libvpx dropped,
VP9 encoding itself could be a feature a machine without AVX2 has not. So that the fallback stays that
small, code no feature gates may not need one. Neither Cargo
configuration nor packaging and CI set `target-cpu`,
everything outside such a feature dispatches SIMD at run time, and
the prebuilt archives downloaded by the sys crates must use the baseline x86-64 ISA
with their hand-written kernels selected by CPUID: libvpx's rtcd tables,
opus's `MAY_HAVE` dispatch and libFLAC's own CPU detection. The sys crates fetch
each dependency repository's latest release, so that release's archives—not the
tag pinned in this repository's `Cargo.toml`—set the effective CPU floor.

If a Rust hot path that no feature gates benefits from AVX2, guard a separate
function with `is_x86_feature_detected!` and `#[target_feature]`, so that the
fallback build keeps it where the processor has it.

## Prebuilt native dependencies

Release builds link `opus-prebuilt`, `libvpx-prebuilt` and, under
[sound-flac](https://github.com/andrewtheguy/sound-flac), `libflac-prebuilt`.
Their sys crates download static archives instead of building vendored C and
C++, so this project needs no CMake, assembler, pkg-config, libclang, vcpkg, or
system copies of those libraries, and no artifact carries or depends on one.
`LIBVPX_PREBUILT_DIR`, `LIBOPUS_PREBUILT_DIR` and `LIBFLAC_PREBUILT_DIR` select
locally built archives.

`ard-high-performance` targets decode the Mac's picture with a decoder whose
licence keeps it out of every artifact: FFmpeg's libavcodec
(LGPL-2.1-or-later), for the HEVC. The Mac's AAC-ELD sound needs none: the
browser decodes it. Published release
artifacts neither compile nor link FFmpeg: the gateway loads the system's shared libraries
when a session needs them, or on Windows the ones in the folder `[hp_decoders]`
names when it starts (`src/libav.rs`), and a host without them
runs those targets only with the picture passed through, for browsers that decode
it. The `.deb` recommends the Linux one, the public container image does not
carry it and the private one `publish-full-image.sh` builds carries Debian's;
elsewhere the operator installs it, as
[High Performance decoder](../docs/high-performance-decoder.md) says for each
platform.

The non-default `apple-hp-media-static` feature links private static archives
instead, and is in no release artifact. No artifact holds the BETA software
HEVC decoder either, libavcodec in WebAssembly for the page, which FFmpeg's
licence keeps out as it keeps the native decoder out: an operator
downloads the release that `src/hevc_wasm.rs` pins by version and SHA-256 from
the private `andrewtheguy/hevc-wasm-archives` through `gh`, and every build
serves it when it finds it. Every release target looks for it by its release
name in `share/remotex`, beside the `share/doc/remotex` it installs, unless
`[hevc_wasm].archive` names another file:
`/usr/share/remotex` for the `.deb` and `.rpm`, `/usr/local/share/remotex` for
the `.pkg`, `share\remotex` under the `.msi`'s install directory, and
`/opt/remotex/versions/<version>/share/remotex` in the
container image. No package owns or makes that directory: the operator does. The
private image `publish-full-image.sh` builds carries it there.
`libavcodec-hevc-prebuilt` links FFmpeg's libavcodec and libavutil, configured
down to the HEVC decoder and parser, and on macOS its VideoToolbox hwaccel, which
links Apple's VideoToolbox, CoreMedia, CoreVideo and CoreFoundation frameworks;
its build script downloads the latest release of
`andrewtheguy/libavcodec-hevc-prebuilt-archives` through `gh`, or takes
`LIBAVCODEC_HEVC_PREBUILT_DIR`.
FFmpeg linked statically obliges a distributor of a binary to let its recipient
relink it against a modified FFmpeg (see that repository's README).
Do not restore
`LIBOPUS_STATIC`, `LIBOPUS_NO_PKG`, `CMAKE_POLICY_VERSION_MINIMUM`, or a source
libopus build in `build-tarball.sh`. The libvpx archives are VP9-only and built
with `--enable-realtime-only`; additional features need a separately built
archive selected with `LIBVPX_PREBUILT_DIR`, not a source-build fallback.

The one C library built from source is jemalloc, through `tikv-jemallocator`.
It is the global allocator on Unix and is linked with unprefixed `malloc` symbols,
so allocations in libvpx and libopus use it as well as Rust allocations. The build
environment must provide a C compiler and `make`. Windows continues to use its
system heap.

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
`bin/remotex` with the separately built container gateway. The build
script, release smoke test, and Dockerfile all reject a binary that exposes
`tui`, `serve-embedded`, or `check-config --embedded`. The tarballs are build
plumbing between the workflow's jobs and are not published: the native packages
and the image are what bring the gateway everything it needs.
