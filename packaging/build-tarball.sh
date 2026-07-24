#!/usr/bin/env bash
# Build a distro-agnostic release tarball for the current OS/arch.
#
# Produces dist/rdpweb-<version>-<os>-<arch>.tar.gz containing a relocatable
# tree that install.sh lays down under <prefix>/versions/<version>:
#
#   rdpweb-<version>/
#   ├── VERSION
#   ├── bin/rdpweb                # release binary
#   ├── etc/rdpweb.env.sample     # config template (install.sh seeds etc/rdpweb.env)
#   ├── share/rdpweb/web/         # built frontend (index.html + assets)
#   ├── install.sh
#   └── uninstall.sh
#
# Run on each target platform you want to ship (macOS builds the mac tarball,
# Linux builds the linux tarball) — this does not cross-compile.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

version="$(grep -m1 '^version = ' Cargo.toml | sed -E 's/.*"(.*)".*/\1/')"

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

pkg="rdpweb-${version}"
stage="$(mktemp -d)"
root="${stage}/${pkg}"
trap 'rm -rf "$stage"' EXIT

echo ">> building frontend"
( cd frontend && bun run build )

echo ">> building release binary"
cargo build --release

echo ">> assembling ${pkg}"
mkdir -p "$root/bin" "$root/etc" "$root/share/rdpweb"
cp target/release/rdpweb "$root/bin/rdpweb"
cp packaging/etc/rdpweb.env.sample "$root/etc/rdpweb.env.sample"
cp -R frontend/dist "$root/share/rdpweb/web"
cp packaging/install.sh packaging/uninstall.sh "$root/"
chmod +x "$root/install.sh" "$root/uninstall.sh" "$root/bin/rdpweb"
printf '%s\n' "$version" > "$root/VERSION"

mkdir -p dist
tarball="dist/${pkg}-${os}-${arch}.tar.gz"
tar -czf "$tarball" -C "$stage" "$pkg"
echo ">> wrote $tarball"
