#!/usr/bin/env bash
# Build a distro-agnostic release tarball for the current OS/arch.
#
# Produces dist/remotex-<version>-<os>-<arch>.tar.gz containing a relocatable
# tree that install.sh lays down under <prefix>/versions/<version>:
#
#   remotex-<version>/
#   ├── VERSION
#   ├── bin/remotex                # release binary
#   ├── share/doc/remotex/remotex.example.toml # config template
#   ├── install.sh
#   └── uninstall.sh
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

echo ">> building release binary"
# build.rs creates the frontend in Cargo's OUT_DIR. Release CI sets
# REMOTEX_PREBUILT_FRONTEND=frontend/dist so each target stages the one
# platform-independent bundle built by the frontend job instead of running Bun.
# No env coaxing here for either prebuilt C library. Remote audio links
# `opus-prebuilt` and VP9 links `libvpx-prebuilt` — each pulls a prebuilt static
# archive rather than compiling vendored C, so neither needs cmake, pkg-config, a
# system library or a `*_STATIC` variable set. The RDP client's TLS is rustls over
# `ring`, so there is no libssl to find either. The binary runs on
# debian:trixie-slim, and there is no cmake_minimum_required for CMake 4 to reject.
cargo build --release

echo ">> assembling ${pkg}"
mkdir -p "$root/bin" "$root/share/doc/remotex"
cp target/release/remotex "$root/bin/remotex"
cp remotex.example.toml "$root/share/doc/remotex/remotex.example.toml"
cp packaging/install.sh packaging/uninstall.sh "$root/"
chmod +x "$root/install.sh" "$root/uninstall.sh" "$root/bin/remotex"
printf '%s\n' "$version" > "$root/VERSION"

mkdir -p dist
tarball="dist/${pkg}-${os}-${arch}.tar.gz"
tar -czf "$tarball" -C "$stage" "$pkg"
echo ">> wrote $tarball"
