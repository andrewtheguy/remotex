#!/usr/bin/env bash
# Build the container-only gateway binary. Containers expose only the deployed
# `serve` shape; the process-local managed-instance surface is a native concern.
#
# REMOTEX_CONTAINER_FEATURES names the features the binary has, which unset is
# `native-vp9` alone, as release CI leaves it: an operator's own image names
# that and what it adds (`native-vp9 apple-hp-media-static`), and one for an
# amd64 machine without AVX2 sets it empty beside the RUSTFLAGS that
# packaging/README.md ("x86-64 CPU compatibility") gives.
# Whatever it names, the checks below still refuse a binary that carries the
# managed-instance surface, or lacks a feature it was asked for.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"
output="$(realpath -m "${1:-tmp/container-bin/remotex}")"
features="${REMOTEX_CONTAINER_FEATURES-native-vp9}"

echo ">> building container gateway without default features${features:+, but with ${features}}"
cargo build --release --no-default-features ${features:+--features "$features"}

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
