#!/usr/bin/env bash
# Build a distro-agnostic release tarball for the current OS/arch.
#
# Produces dist/remotex-<version>-<os>-<arch>.tar.gz containing a relocatable
# tree that install.sh lays down under <prefix>/versions/<version>:
#
#   remotex-<version>/
#   ├── VERSION
#   ├── bin/remotex                # release binary
#   ├── share/doc/remotex/remotex.toml.example # config template
#   ├── share/remotex/web/                  # built frontend (index.html + assets)
#   ├── install.sh
#   └── uninstall.sh
#
# The gateway only. The macOS screen agent is a separate program with a separate
# install story — a .dmg you drag into /Applications and open once, which then
# registers itself — so it is built and shipped separately, by
# packaging/macos/build-agent-app.sh and the mac-agent job in
# .github/workflows/release.yml. Nothing here needs a Swift toolchain.
#
# Run on each target platform you want to ship (macOS builds the mac tarball,
# Linux builds the linux tarball) — this does not cross-compile.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

# Real TOML parse + semver check, not grep/sed (same as .github/workflows/release.yml).
# Plain python3 (3.11+ for tomllib) so this also runs on CI runners without uv.
# `[workspace.package]`, not `[package]`: every member inherits that one version,
# the root package included, so it is the only literal one in the tree.
version="$(python3 -c '
import re, sys, tomllib
with open("Cargo.toml", "rb") as f:
    version = tomllib.load(f)["workspace"]["package"]["version"]
if not re.fullmatch(r"\d+\.\d+\.\d+(?:-[0-9A-Za-z.-]+)?(?:\+[0-9A-Za-z.-]+)?", version):
    sys.exit(f"invalid version in Cargo.toml: {version!r}")
print(version)
')"

case "$(uname -s)" in
  Linux)  os=linux ;;
  Darwin) os=macos ;;
  *) echo "unsupported OS: $(uname -s)" >&2; exit 1 ;;
esac
case "$(uname -m)" in
  x86_64|amd64)  arch=x86_64 ;;
  arm64|aarch64) arch=arm64 ;;
  *) arch="$(uname -m)" ;;
esac

pkg="remotex-${version}"
stage="$(mktemp -d)"
root="${stage}/${pkg}"
trap 'rm -rf "$stage"' EXIT

# The frontend bundle is platform-agnostic, so CI builds it once and reuses it
# across every target. Set SKIP_FRONTEND_BUILD=1 to use an existing frontend/dist
# instead of rebuilding (requires `bun run build` to have run first).
if [ "${SKIP_FRONTEND_BUILD:-0}" = 1 ]; then
  [ -d frontend/dist ] || { echo "SKIP_FRONTEND_BUILD=1 but frontend/dist is missing" >&2; exit 1; }
  echo ">> using prebuilt frontend/dist"
else
  echo ">> building frontend"
  ( cd frontend && bun run build )
fi

echo ">> building release binary"
# libopus is linked *statically*, and this is not a preference. audiopus_sys (via
# the `opus` crate, for remote audio) probes pkg-config first on Linux/GNU, so on a
# runner that happens to have libopus-dev the binary would come out needing
# libopus.so — and packaging/Dockerfile runs this binary on debian:trixie-slim,
# which does not ship it. That failure would appear only inside the image, only at
# start-up, after every build and test had passed. Forcing the vendored build here
# makes every runner produce the same thing.
#
# `CMAKE_POLICY_VERSION_MINIMUM` is the second half of the same story. That vendored
# opus declares `cmake_minimum_required` below 3.5, and CMake 4 — which the
# macos-latest runners now have — refuses to configure it at all:
#
#   CMake Error at CMakeLists.txt:1 (cmake_minimum_required):
#     Compatibility with CMake < 3.5 has been removed from CMake.
#
# 3.5 is the escape hatch CMake's own error message names. It is set here rather than
# only for macOS because the Linux runners are on CMake 3 today and will meet this the
# day they are not, and it is inert on any CMake that predates the variable.
LIBOPUS_STATIC=1 LIBOPUS_NO_PKG=1 CMAKE_POLICY_VERSION_MINIMUM=3.5 \
  cargo build --release

echo ">> assembling ${pkg}"
mkdir -p "$root/bin" "$root/share/doc/remotex" "$root/share/remotex"
cp target/release/remotex "$root/bin/remotex"
cp packaging/etc/remotex.toml.example "$root/share/doc/remotex/remotex.toml.example"
cp -R frontend/dist "$root/share/remotex/web"
cp packaging/install.sh packaging/uninstall.sh "$root/"
chmod +x "$root/install.sh" "$root/uninstall.sh" "$root/bin/remotex"
printf '%s\n' "$version" > "$root/VERSION"

mkdir -p dist
tarball="dist/${pkg}-${os}-${arch}.tar.gz"
tar -czf "$tarball" -C "$stage" "$pkg"
echo ">> wrote $tarball"
