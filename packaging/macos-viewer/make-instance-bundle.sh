#!/usr/bin/env bash
# Stamp out a second remotex.app: its own name, its own icon, its own instance.
#
# Usage:
#   packaging/macos-viewer/make-instance-bundle.sh <name> [icon.png|icon.icns]
#
#   name    the bundle's name, used verbatim. It is also the instance directory under
#           ~/Library/Application Support, so `remotex-work` keeps that folder tidy
#           beside `remotex` and `remotex-agent`.
#   icon    optional. A PNG (square, 512px or larger) is converted; an .icns is used
#           as it is. Without one the variant wears the same icon as remotex, which
#           makes two apps nobody can tell apart in the Dock.
#
# Options:
#   --source <app>   the bundle to copy (default /Applications/remotex.app)
#   --out <dir>      where to write it (default ~/Applications)
#
# **Why a whole bundle rather than a launcher.** LaunchServices hands a double-clicked
# app no arguments, and `open` without `-n` reactivates the running copy and discards
# `--args` — so a shell or AppleScript launcher is the only way to pass `--instance-dir`,
# and then the thing in the Dock is remotex rather than the instance. A variant bundle
# has no such problem: it is a separate application to macOS, with its own Dock tile,
# its own ⌘-Tab entry and its own menu bar, and the app reads its instance directory
# from `CFBundleName` (see `InstanceDirectory.defaultURL`) so there is nothing to pass.
#
# The cost is that this is a *copy*: ~13 MB, and it goes stale. Re-run this after
# updating remotex.app — it refreshes an existing variant in place, keeping its name
# and icon, so the instance's own data is never touched.
set -euo pipefail

source_app="/Applications/remotex.app"
out_dir="$HOME/Applications"
name=""
icon=""

while [ $# -gt 0 ]; do
  case "$1" in
    --source) source_app="$2"; shift 2 ;;
    --out) out_dir="$2"; shift 2 ;;
    -h | --help)
      sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d; s/^# \{0,1\}//'
      exit 0
      ;;
    *)
      if [ -z "$name" ]; then
        name="$1"
      elif [ -z "$icon" ]; then
        icon="$1"
      else
        echo "unexpected argument: $1" >&2
        exit 1
      fi
      shift
      ;;
  esac
done

[ -n "$name" ] || {
  echo "usage: $0 <name> [icon.png|icon.icns] [--source <app>] [--out <dir>]" >&2
  exit 1
}
# A name that is not one path component would put the instance somewhere other than
# where this script says it will be. Refused here rather than sanitized, so the
# directory the app uses is always the name that was asked for.
case "$name" in
  *[/:]* | .*)
    echo "the name must be one path component and must not start with a dot: $name" >&2
    exit 1
    ;;
esac
[ -d "$source_app" ] || {
  echo "no bundle at $source_app — install remotex.app, or pass --source" >&2
  exit 1
}

viewer="$source_app/Contents/MacOS/remotex-viewer"
[ -x "$viewer" ] || {
  echo "$source_app does not look like remotex.app (no Contents/MacOS/remotex-viewer)" >&2
  exit 1
}
version="$("$viewer" --version | awk '{print $NF}')"

app="$out_dir/$name.app"
plist="$app/Contents/Info.plist"
instance="$HOME/Library/Application Support/$name"

# ---------------------------------------------------------------- the copy

mkdir -p "$out_dir"
# Replaced rather than updated in place: an incremental copy over a running app's
# bundle is how you get a half-signed one, and there is nothing in here worth
# keeping — the icon is reapplied below and every byte of state lives in the
# instance directory, which this script never touches.
echo ">> copying $source_app ($version)"
rm -rf "$app"
cp -R "$source_app" "$app"

# ---------------------------------------------------------------- the identity

# A bundle identifier of its own is what makes macOS treat this as a separate
# application rather than another window of remotex: its own Dock tile, its own
# ⌘-Tab entry, its own saved window state.
source_id="$(/usr/libexec/PlistBuddy -c "Print :CFBundleIdentifier" "$source_app/Contents/Info.plist")"
slug="$(printf '%s' "$name" | tr '[:upper:]' '[:lower:]' | sed 's/[^a-z0-9]\{1,\}/-/g; s/^-//; s/-$//')"
echo ">> identity: $source_id.$slug / CFBundleName $name"
/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $source_id.$slug" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleName $name" "$plist"
/usr/libexec/PlistBuddy -c "Set :CFBundleDisplayName $name" "$plist"

# ---------------------------------------------------------------- the icon

if [ -n "$icon" ]; then
  [ -f "$icon" ] || {
    echo "no such icon: $icon" >&2
    exit 1
  }
  case "$icon" in
    *.icns)
      cp "$icon" "$app/Contents/Resources/AppIcon.icns"
      ;;
    *)
      echo ">> converting $icon"
      work="$(mktemp -d)"
      trap 'rm -rf "$work"' EXIT
      mkdir "$work/icon.iconset"
      for size in 16 32 128 256 512; do
        sips -z "$size" "$size" "$icon" \
          --out "$work/icon.iconset/icon_${size}x${size}.png" >/dev/null
        sips -z "$((size * 2))" "$((size * 2))" "$icon" \
          --out "$work/icon.iconset/icon_${size}x${size}@2x.png" >/dev/null
      done
      iconutil -c icns "$work/icon.iconset" -o "$app/Contents/Resources/AppIcon.icns"
      ;;
  esac
  echo ">> icon: $icon"
else
  echo ">> icon: unchanged — this variant looks exactly like remotex in the Dock"
fi

# ---------------------------------------------------------------- signing

# Inner code first, then the bundle, exactly as `build-viewer-app.sh` does it: a
# nested executable signed after its container invalidates the container's seal.
#
# Ad-hoc, and nothing is lost by it: the shipped bundle is ad-hoc signed too
# (`codesign -dv` says `Signature=adhoc`), and the viewer holds no TCC grants for a
# change of code identity to break — it captures keys with a *local* NSEvent monitor,
# which needs no Accessibility permission. Pass an identity in CODESIGN_IDENTITY to
# match a signed source bundle.
identity="${CODESIGN_IDENTITY:--}"
echo ">> signing as: $identity"
for binary in remotex-gateway remotex-viewer; do
  codesign --force --sign "$identity" --options runtime --timestamp=none \
    "$app/Contents/MacOS/$binary" >/dev/null 2>&1
done
codesign --force --sign "$identity" --options runtime --timestamp=none "$app" >/dev/null 2>&1
codesign --verify --deep --strict "$app"
# The icon is cached by path, and a replaced bundle at a path Finder has already seen
# keeps the old one until something touches it.
touch "$app"

echo ">> wrote $app"
echo "   instance: $instance"
echo "   double-click it; there is no flag to pass and no launcher to keep."
