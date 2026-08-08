#!/usr/bin/env bash
# Rasterize icon.svg and icon-off.svg into the PNGs the manifest names.
#
# The PNGs are committed and this script is what regenerates them, the same bargain
# apps/viewer/build/make-icon.sh strikes and for the same reason: the release job runs
# on a runner with no SVG rasterizer, and the icon is not worth a package install per
# job. So: edit the SVG, run this, commit both.
#
# rsvg-convert (`brew install librsvg`, `apt install librsvg2-bin`) rather than sips or
# qlmanage — neither of those reads SVG at all. Unlike the viewer's script this one
# needs nothing macOS-only, so it runs on Linux too.
set -euo pipefail

here="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

command -v rsvg-convert >/dev/null || {
  echo "error: rsvg-convert not found — brew install librsvg" >&2
  exit 1
}

for variant in on off; do
  case "$variant" in
    on) svg="$here/icon.svg" ;;
    off) svg="$here/icon-off.svg" ;;
  esac
  for size in 16 32 48 128; do
    rsvg-convert -w "$size" -h "$size" "$svg" -o "$here/$variant-$size.png"
  done
done

echo "wrote $here/{on,off}-{16,32,48,128}.png"
