#!/usr/bin/env bash
# Build and sign the macOS 26 remotex viewer, optionally wrapping it in a DMG.
#
# Usage:
#   packaging/macos-viewer/build-viewer-app.sh [--debug] [--no-dmg]
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

# An unbundled `swift run` has no Info.plist, so ProductInfo carries the same
# value for development. Refuse to package a bridge that would reject itself.
development_version="$(
  sed -n 's/.*developmentVersion = "\([^"]*\)".*/\1/p' \
    apps/remotex-viewer/Sources/ProductInfo.swift
)"
[ "$development_version" = "$version" ] || {
  echo "viewer development version $development_version does not match $version" >&2
  exit 1
}

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

app="dist/remotex-viewer.app"
echo ">> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "$binary" "$app/Contents/MacOS/remotex-viewer"
chmod +x "$app/Contents/MacOS/remotex-viewer"
cp packaging/macos/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"
sed -e "s|<string>0\\.0\\.0</string>|<string>${version}</string>|g" \
  packaging/macos-viewer/Info.plist > "$app/Contents/Info.plist"

identity="${CODESIGN_IDENTITY:--}"
echo ">> signing as: $identity"

timestamp_flag=(--timestamp)
if [ "$identity" = "-" ]; then
  timestamp_flag=(--timestamp=none)
fi
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
dmg="dist/remotex-viewer-${version}-macos-arm64${suffix}.dmg"
staging="dist/viewer-dmg-root"
echo ">> building $dmg"
rm -rf "$staging" "$dmg"
mkdir -p "$staging"
/usr/bin/ditto "$app" "$staging/remotex-viewer.app"
ln -s /Applications "$staging/Applications"
hdiutil create -volname "remotex viewer $version" -srcfolder "$staging" \
  -fs HFS+ -format UDZO -ov -quiet "$dmg"
rm -rf "$staging" "$app"

if [ "$identity" != "-" ]; then
  codesign --force --sign "$identity" "${timestamp_flag[@]}" "$dmg"
fi
echo ">> wrote $dmg"
