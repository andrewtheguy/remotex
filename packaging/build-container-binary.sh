#!/usr/bin/env bash
# Build the container-only gateway binary. Containers expose only the deployed
# `serve` shape; the process-local managed-instance surface is a native concern.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

output="${1:-tmp/container-bin/remotex}"

echo ">> building container gateway without default features"
cargo build --release --no-default-features

binary="target/release/remotex"
case "$("$binary" --help)" in
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

mkdir -p "$(dirname "$output")"
cp "$binary" "$output"
chmod +x "$output"
echo ">> wrote $output"
