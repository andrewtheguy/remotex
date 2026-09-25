#!/usr/bin/env bash
# Build the container-only gateway binary. Containers expose only the deployed
# `serve` shape; the process-local managed-instance surface is a native concern.
#
# REMOTEX_SOURCE_DIR is the tree to build when it is not the one this script sits
# in: publish-full-image.sh builds a tag's source with this checkout's script,
# since a tag holds whatever script it was cut with.
#
# REMOTEX_CONTAINER_FEATURES names the non-default features an operator's own
# image adds beside airplay (`apple-hp-media`); release CI leaves it unset.
# Whatever it names, the checks below still refuse a binary that carries the
# managed-instance surface, or lacks a feature it was asked for.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
output="$(realpath -m "${1:-tmp/container-bin/remotex}")"
cd "${REMOTEX_SOURCE_DIR:-$repo_root}"
features="airplay${REMOTEX_CONTAINER_FEATURES:+ $REMOTEX_CONTAINER_FEATURES}"

echo ">> building container gateway without default features, but with ${features}"
cargo build --release --no-default-features --features "$features"

binary="${CARGO_TARGET_DIR:-target}/release/remotex"
case "$("$binary" --help)" in
  *"  tui "*)
    echo "container gateway unexpectedly exposes tui" >&2
    exit 1
    ;;
  *serve-embedded*)
    echo "container gateway unexpectedly exposes serve-embedded" >&2
    exit 1
    ;;
esac
case "$("$binary" check-config --help)" in
  *--embedded*)
    echo "container gateway unexpectedly exposes check-config --embedded" >&2
    exit 1
    ;;
esac
for feature in $features; do
  case "$("$binary" --help)" in
    *"Features: "*"$feature"*) ;;
    *)
      echo "container gateway is missing the $feature feature" >&2
      exit 1
      ;;
  esac
done

mkdir -p "$(dirname "$output")"
cp "$binary" "$output"
chmod +x "$output"
echo ">> wrote $output"
