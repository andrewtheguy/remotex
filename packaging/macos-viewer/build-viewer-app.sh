#!/usr/bin/env bash
# Build and sign remotex.app — the macOS 26 client and the gateway it runs —
# optionally wrapping it in a DMG.
#
# Usage:
#   packaging/macos-viewer/build-viewer-app.sh [--debug] [--no-dmg]
#
# The bundle carries two executables: the Swift app, and a copy of the `remotex`
# gateway binary as `remotex-gateway`. The app starts that gateway on an ephemeral
# loopback port at launch (see docs/macos-viewer.md), so a build with only one of
# the two is not a working app. No frontend is copied: the embedded gateway serves
# no web UI.
#
# Builds are ad-hoc signed by default. Set CODESIGN_IDENTITY explicitly to use
# another identity; Developer ID distribution also requires notarization.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

[ "$(uname -s)" = Darwin ] || {
  echo "the viewer only builds on macOS" >&2
  exit 1
}

configuration=release
make_dmg=1
while [ $# -gt 0 ]; do
  case "$1" in
    --debug) configuration=debug; shift ;;
    --no-dmg) make_dmg=0; shift ;;
    -h|--help)
      sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d; s/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unexpected argument: $1" >&2; exit 1 ;;
  esac
done

version="$(
  awk '
    $0 == "[workspace.package]" { in_package = 1; next }
    in_package && /^\[/ { exit }
    in_package && /^version = "/ {
      value = $0
      sub(/^version = "/, "", value)
      sub(/"$/, "", value)
      print value
      exit
    }
  ' Cargo.toml
)"
[ -n "$version" ] || {
  echo "could not read workspace.package.version from Cargo.toml" >&2
  exit 1
}

# Nothing to cross-check against the source any more: the version below is the
# only one the viewer has, substituted into `CFBundleShortVersionString`, and
# `ProductInfo.version` reads it back out of the bundle. An unbundled build has no
# Info.plist and reports `0.0.0-unbundled` rather than a second copy of this that
# a bump could leave behind.

echo ">> building remotex-viewer ($configuration)"
swift build --package-path apps/remotex-viewer -c "$configuration"
bin_dir="$(
  swift build --package-path apps/remotex-viewer -c "$configuration" --show-bin-path
)"
binary="$bin_dir/remotex-viewer"
[ -x "$binary" ] || {
  echo "viewer binary missing at $binary" >&2
  exit 1
}

# The gateway the app runs. Built at the same configuration as the app, so a debug
# build is debuggable all the way down rather than half-release.
cargo_profile=release
cargo_flags=(--release)
if [ "$configuration" = debug ]; then
  cargo_profile=debug
  cargo_flags=()
fi
echo ">> building the remotex gateway ($cargo_profile)"
# No libopus env coaxing: remote audio links `opus-prebuilt` (a prebuilt static
# archive, no vendored cmake build — see the root Cargo.toml), so the gateway that
# goes into this bundle cannot come out needing a libopus dylib that exists only on
# the build machine, and there is no cmake_minimum_required for CMake 4 to reject.
cargo build --bin remotex "${cargo_flags[@]}"
gateway="target/$cargo_profile/remotex"
[ -x "$gateway" ] || {
  echo "gateway binary missing at $gateway" >&2
  exit 1
}

app="dist/remotex.app"
echo ">> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/remotex-viewer"
chmod +x "$app/Contents/MacOS/remotex-viewer"
# `remotex-gateway`, not `remotex`: two files in one directory cannot share a name,
# and the app's own executable is already there. The suffix also makes it obvious
# which process is which in Activity Monitor and to `pgrep`.
cp "$gateway" "$app/Contents/MacOS/remotex-gateway"
chmod +x "$app/Contents/MacOS/remotex-gateway"
cp packaging/macos-viewer/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
sed -e "s|<string>0\\.0\\.0</string>|<string>${version}</string>|g" \
  packaging/macos-viewer/Info.plist > "$app/Contents/Info.plist"

identity="${CODESIGN_IDENTITY:--}"
echo ">> signing as: $identity"

timestamp_flag=(--timestamp)
if [ "$identity" = "-" ]; then
  timestamp_flag=(--timestamp=none)
fi
# Inner code first, then the bundle. A nested executable signed *after* its bundle
# invalidates the outer signature, and `--verify --deep` is what catches it.
codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" \
  "$app/Contents/MacOS/remotex-gateway"
codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" \
  "$app/Contents/MacOS/remotex-viewer"
codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" "$app"
codesign --verify --deep --strict --verbose=2 "$app"

if [ "$make_dmg" -eq 0 ]; then
  echo ">> wrote $app"
  exit 0
fi

suffix=""
if [ "$identity" = "-" ]; then
  suffix="-unsigned"
fi
if [ "$configuration" = debug ]; then
  suffix="${suffix}-debug"
fi
# The image keeps the `remotex-viewer` name while the app inside it is `remotex.app`:
# `remotex-<version>-macos-arm64.tar.gz` is already the CLI gateway's release asset,
# and two downloads called the same thing is worse than one whose name is a little
# behind the product's.
dmg="dist/remotex-viewer-${version}-macos-arm64${suffix}.dmg"
staging="dist/viewer-dmg-root"
echo ">> building $dmg"
rm -rf "$staging" "$dmg"
mkdir -p "$staging"
/usr/bin/ditto "$app" "$staging/remotex.app"
ln -s /Applications "$staging/Applications"
hdiutil create -volname "remotex viewer $version" -srcfolder "$staging" \
  -fs HFS+ -format UDZO -ov -quiet "$dmg"
rm -rf "$staging" "$app"

if [ "$identity" != "-" ]; then
  codesign --force --sign "$identity" "${timestamp_flag[@]}" "$dmg"
fi
echo ">> wrote $dmg"
