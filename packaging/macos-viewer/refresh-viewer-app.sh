#!/usr/bin/env bash
# Put a fresh shell and a fresh Chromium host into the bundle that is already
# there, for the edit-build-launch loop.
#
# Usage:
#   packaging/macos-viewer/refresh-viewer-app.sh [--debug] [--web]
#
# **This is not a build.** `build-viewer-app.sh` is, and with no arguments it is
# character for character what `release.yml` runs — trust a result from that one.
# This script only replaces the parts of `dist/remotex.app` that a change to
# `crates/remotex-cef`, `crates/remotex-cef-helper` or `apps/remotex-viewer` can
# affect, and leaves the expensive parts of the bundle where they are:
#
#   Chromium Embedded Framework   317 MB, copied file by file, and identical
#                                 until the CEF version changes.
#   remotex-gateway               a second Rust binary and its link.
#   Contents/Resources/web        `bun install && bun run build`, which is a
#                                 frontend change and not this one. `--web`
#                                 re-copies `frontend/dist` as it stands, without
#                                 building it.
#
# A Chromium *switch* needs none of this: `REMOTEX_CHROMIUM_SWITCHES` is read at
# startup, so trying one is a relaunch rather than a rebuild. See
# `crates/remotex-cef/src/app.rs`.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

configuration=release
copy_web=0
while [ $# -gt 0 ]; do
  case "$1" in
    --debug) configuration=debug; shift ;;
    --web) copy_web=1; shift ;;
    -h|--help)
      sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d; s/^# \{0,1\}//'
      exit 0
      ;;
    *) echo "unexpected argument: $1" >&2; exit 1 ;;
  esac
done

app="dist/remotex.app"
[ -d "$app/Contents/Frameworks/Chromium Embedded Framework.framework" ] || {
  echo "no bundle to refresh at $app — run packaging/macos-viewer/build-viewer-app.sh first" >&2
  exit 1
}

cargo_profile=release
cargo_flags=(--release)
if [ "$configuration" = debug ]; then
  cargo_profile=debug
  cargo_flags=()
fi

# The same `CEF_PATH` every cargo invocation below sees. `cef-dll-sys` rebuilds
# whenever this variable changes, so a script that sets it for one build and not
# the next pays for a full Chromium-bindings rebuild on every run.
export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

# Builds both Rust crates and stages what the Swift app links against.
packaging/macos-viewer/stage-cef.sh "$cargo_profile"
cef_helper="target/$cargo_profile/remotex-cef-helper"

echo ">> building remotex-viewer ($configuration)"
bin_dir="$(
  swift build --package-path apps/remotex-viewer -c "$configuration" --show-bin-path
)"
binary="$bin_dir/remotex-viewer"
# SwiftPM does not watch `libremotex_cef.a`. With no Swift source changed it
# reports "Build complete" in a tenth of a second and leaves the previous link in
# place — so an edit to `crates/remotex-cef` would be built, staged, signed, and
# not actually in the app. Removing the product is what forces the link; the
# compile is still cached.
rm -f "$binary"
swift build --package-path apps/remotex-viewer -c "$configuration"
[ -x "$binary" ] || {
  echo "viewer binary missing at $binary" >&2
  exit 1
}

cp "$binary" "$app/Contents/MacOS/remotex-viewer"
chmod +x "$app/Contents/MacOS/remotex-viewer"

# The same one binary in all five bundles, named after the role as CEF derives it.
for helper_app in "$app/Contents/Frameworks/"*Helper*.app; do
  role="$(basename "$helper_app" .app)"
  cp "$cef_helper" "$helper_app/Contents/MacOS/$role"
  chmod +x "$helper_app/Contents/MacOS/$role"
done

if [ "$copy_web" -eq 1 ]; then
  [ -f "frontend/dist/index.html" ] || {
    echo "no SPA at frontend/dist — run bun run build in frontend/" >&2
    exit 1
  }
  echo ">> copying frontend/dist as it stands (not built here)"
  /usr/bin/ditto frontend/dist "$app/Contents/Resources/web"
fi

identity="${CODESIGN_IDENTITY:--}"
timestamp_flag=(--timestamp)
if [ "$identity" = "-" ]; then
  timestamp_flag=(--timestamp=none)
fi
sign() {
  codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" "$@"
}

# Inner first, exactly as the full build signs it. Replacing the main executable
# invalidates the outer signature and nothing else, but a helper whose binary just
# changed has to be re-signed before the bundle around it — so both, in order.
echo ">> signing as: $identity"
for helper_app in "$app/Contents/Frameworks/"*Helper*.app; do
  sign --entitlements packaging/macos-viewer/helper.entitlements "$helper_app"
done
sign --entitlements packaging/macos-viewer/app.entitlements "$app"
codesign --verify --deep --strict "$app"

echo ">> refreshed $app"
