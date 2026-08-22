#!/usr/bin/env bash
# Build the native installer(s) for the current platform from the release
# tarball produced by build-tarball.sh.
#
# Linux produces both package formats from the same payload:
#   dist/remotex-linux-amd64.deb
#   dist/remotex-linux-amd64.rpm
#
# macOS produces:
#   dist/remotex-macos-arm64.pkg
#
# Native packages use package-manager-owned paths directly. There is no
# versioned tree, active-version symlink, rollback copy, or package wrapper:
#
#   Linux: /usr/bin/remotex, /usr/share/remotex/web
#   macOS: /usr/local/bin/remotex, /usr/local/share/remotex/web
#
# The live config is not package-owned. It contains credentials, so the operator
# creates it from the packaged example with the ownership of the account that
# will run the gateway; package upgrades and removals consequently cannot
# replace or delete it.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

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
  x86_64|amd64)
    tar_arch=x86_64
    asset_arch=amd64
    deb_arch=amd64
    ;;
  arm64|aarch64)
    tar_arch=arm64
    asset_arch=arm64
    deb_arch=arm64
    ;;
  *) echo "unsupported architecture: $(uname -m)" >&2; exit 1 ;;
esac

tarball="dist/remotex-${version}-${os}-${tar_arch}.tar.gz"
[ -f "$tarball" ] || {
  echo "missing $tarball; run packaging/build-tarball.sh first" >&2
  exit 1
}

mkdir -p tmp dist
stage="$(mktemp -d "$repo_root/tmp/native-packages.XXXXXX")"
trap 'rm -rf "$stage"' EXIT

mkdir -p "$stage/release"
tar -xzf "$tarball" -C "$stage/release" --strip-components=1
release="$stage/release"

[ -x "$release/bin/remotex" ] || { echo "release tarball has no executable gateway" >&2; exit 1; }
[ -f "$release/share/remotex/web/index.html" ] || { echo "release tarball has no frontend index" >&2; exit 1; }
[ "$(cat "$release/VERSION")" = "$version" ] || { echo "release tarball VERSION does not match Cargo.toml" >&2; exit 1; }

reported="$("$release/bin/remotex" --version)"
[ "$reported" = "remotex $version" ] || {
  echo "release binary reports '$reported', expected 'remotex $version'" >&2
  exit 1
}

if [ "$os" = macos ]; then
  command -v pkgbuild >/dev/null 2>&1 || { echo "pkgbuild is required" >&2; exit 1; }
  payload="$stage/payload"
  mkdir -p "$payload/usr/local/bin" "$payload/usr/local/share/doc/remotex" "$payload/usr/local/share/remotex"
  cp "$release/bin/remotex" "$payload/usr/local/bin/remotex"
  cp "$release/share/doc/remotex/remotex.example.toml" "$payload/usr/local/share/doc/remotex/remotex.example.toml"
  cp -R "$release/share/remotex/web" "$payload/usr/local/share/remotex/web"
  output="dist/remotex-macos-${asset_arch}.pkg"
  pkgbuild \
    --root "$payload" \
    --identifier com.andrewtheguy.remotex.gateway \
    --version "$version" \
    --install-location / \
    "$output"

  pkgutil --payload-files "$output" > "$stage/pkg-contents"
  grep -qx './usr/local/bin/remotex' "$stage/pkg-contents"
  grep -qx './usr/local/share/remotex/web/index.html' "$stage/pkg-contents"
  grep -qx './usr/local/share/doc/remotex/remotex.example.toml' "$stage/pkg-contents"
  echo ">> wrote $output"
  exit 0
fi

command -v dpkg-deb >/dev/null 2>&1 || { echo "dpkg-deb is required" >&2; exit 1; }
command -v rpmbuild >/dev/null 2>&1 || { echo "rpmbuild is required" >&2; exit 1; }

payload="$stage/payload"
mkdir -p "$payload/usr/bin" "$payload/usr/share/doc/remotex" "$payload/usr/share/remotex"
cp "$release/bin/remotex" "$payload/usr/bin/remotex"
cp "$release/share/doc/remotex/remotex.example.toml" "$payload/usr/share/doc/remotex/remotex.example.toml"
cp -R "$release/share/remotex/web" "$payload/usr/share/remotex/web"

# '-' separates the Debian revision, so a SemVer prerelease has to become '~',
# which sorts before everything: '0.0.1-rc.1-1' would otherwise sort *after* the
# '0.0.1-1' release. '+' is left alone — it is legal in a Debian version and
# already sorts after the plain release, which is what build metadata means.
deb_version="${version//-/~}"

deb_root="$stage/deb-root"
cp -R "$payload" "$deb_root"
mkdir -p "$deb_root/DEBIAN"
{
  echo "Package: remotex"
  echo "Version: ${deb_version}-1"
  echo "Architecture: $deb_arch"
  echo "Maintainer: andrewtheguy <andrewchen5678@gmail.com>"
  echo "Section: net"
  echo "Priority: optional"
  echo "Depends: ca-certificates, libc6 (>= 2.39)"
  echo "Homepage: https://github.com/andrewtheguy/remotex"
  echo "Description: Single-user browser remote desktop gateway"
  echo " Connects a browser to RDP, VNC, and macOS Screen Sharing targets."
} > "$deb_root/DEBIAN/control"

deb_output="dist/remotex-linux-${asset_arch}.deb"
dpkg-deb --build --root-owner-group "$deb_root" "$deb_output"
[ "$(dpkg-deb --field "$deb_output" Package)" = remotex ]
dpkg-deb --contents "$deb_output" > "$stage/deb-contents"
grep -q '\./usr/bin/remotex$' "$stage/deb-contents"
grep -q '\./usr/share/remotex/web/index.html$' "$stage/deb-contents"
grep -q '\./usr/share/doc/remotex/remotex.example.toml$' "$stage/deb-contents"
echo ">> wrote $deb_output"

# RPM does not accept SemVer's '-' in Version or '+' in either Version or
# Release. Preserve their ordering semantics with '~' for a prerelease and '.'
# for build metadata. Release filenames retain the exact Cargo version through
# the release tag; this only affects RPM's internal version field.
rpm_version="${version//-/~}"
rpm_version="${rpm_version//+/.}"

rpm_top="$stage/rpmbuild"
mkdir -p "$rpm_top/BUILD" "$rpm_top/BUILDROOT" "$rpm_top/RPMS" "$rpm_top/SOURCES" "$rpm_top/SPECS" "$rpm_top/SRPMS"
spec="$rpm_top/SPECS/remotex.spec"
{
  echo '%global debug_package %{nil}'
  echo '%global __os_install_post %{nil}'
  echo 'Name: remotex'
  echo "Version: $rpm_version"
  echo 'Release: 1'
  echo 'Summary: Single-user browser remote desktop gateway'
  echo 'License: LicenseRef-remotex'
  echo 'URL: https://github.com/andrewtheguy/remotex'
  echo 'Requires: ca-certificates'
  echo
  echo '%description'
  echo 'Connects a browser to RDP, VNC, and macOS Screen Sharing targets.'
  echo
  echo '%prep'
  echo
  echo '%build'
  echo
  echo '%install'
  echo 'mkdir -p %{buildroot}'
  echo 'cp -a "%{payload}/." "%{buildroot}/"'
  echo
  echo '%files'
  echo '/usr/bin/remotex'
  echo '/usr/share/remotex'
  echo '/usr/share/doc/remotex/remotex.example.toml'
} > "$spec"

rpmbuild -bb \
  --define "_topdir $rpm_top" \
  --define "payload $payload" \
  "$spec"

rpm_built="$(find "$rpm_top/RPMS" -type f -name '*.rpm' -print -quit)"
[ -n "$rpm_built" ] || { echo "rpmbuild produced no package" >&2; exit 1; }
rpm_output="dist/remotex-linux-${asset_arch}.rpm"
cp "$rpm_built" "$rpm_output"
rpm -qpl "$rpm_output" > "$stage/rpm-contents"
grep -qx '/usr/bin/remotex' "$stage/rpm-contents"
grep -qx '/usr/share/remotex/web/index.html' "$stage/rpm-contents"
grep -qx '/usr/share/doc/remotex/remotex.example.toml' "$stage/rpm-contents"
echo ">> wrote $rpm_output"
