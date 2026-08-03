#!/usr/bin/env bash
# Build and sign remotex.app — the macOS 15+ client and the gateway it runs — and
# wrap it in the DMG that is the release asset.
#
# Usage:
#   packaging/macos-viewer/build-viewer-app.sh [--debug] [--no-dmg]
#
# **Run it bare.** With no arguments this is character for character what
# `release.yml` runs — no flags, no environment — so the thing built here and the
# thing released are not two commands apart. That is the way to build the viewer,
# and what the docs tell you to run.
#
# `--no-dmg` stops after the app bundle, for a fast edit-build-launch loop. It does
# not change the bundle: signing happens before the image and what the image holds
# is the same bytes. It is a shortcut, not a second kind of build — and much less
# of one now that the image step leaves `dist/remotex.app` in place rather than
# deleting it.
#
# `--debug` changes the compiler configuration, not the shape of the output: the
# same bundle, named so it cannot be mistaken for a release.
#
# The bundle carries two executables: the Swift app, and a copy of the `remotex`
# gateway binary as `remotex-gateway`. The app starts that gateway on an ephemeral
# loopback port at launch (see docs/macos-viewer.md), so a build with only one of
# the two is not a working app.
#
# It also carries `Contents/Resources/web`, which **is** the SPA — the same build a
# browser gets. The bundled gateway serves it on loopback and the app shows that
# page in a WKWebView, so the client has one implementation and the app is the
# native shell around it.
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

# The client itself. The very same `bun run build` a deployment ships, because it
# is the very same client: the app's window is a web view onto this, served by the
# gateway beside it on loopback.
echo ">> building the SPA"
command -v bun >/dev/null || {
  echo "bun is required to build the SPA the app shows" >&2
  exit 1
}
(cd frontend && bun install --frozen-lockfile && bun run build)
web="frontend/dist"
[ -f "$web/index.html" ] || {
  echo "SPA missing at $web/index.html" >&2
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
# `ditto` rather than `cp -R`: the destination must be exactly this directory's
# contents, and `cp -R` onto an existing directory nests instead of replacing.
# `--web-root` names this path at launch; nothing about the layout is the
# gateway's to guess.
/usr/bin/ditto "$web" "$app/Contents/Resources/web"
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
# The staging copy goes; the app itself stays. It is what QA launches
# (`open -n dist/remotex.app`), and it is the same bundle the image holds — the
# image was made from it. Deleting it here is what made "build the image" and
# "have something to run" two different commands. `release.yml` uploads
# `dist/*.dmg`, so the bundle left beside it reaches nothing.
rm -rf "$staging"

if [ "$identity" != "-" ]; then
  codesign --force --sign "$identity" "${timestamp_flag[@]}" "$dmg"
fi
echo ">> wrote $dmg"
echo ">> wrote $app"
