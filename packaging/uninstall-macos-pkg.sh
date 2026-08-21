#!/usr/bin/env bash
# Remove the macOS gateway package installed with
# `installer -pkg remotex-macos-arm64.pkg -target /`.
#
# The removal is driven by the installed receipt, not by a hardcoded file list:
# `pkgutil --files` names exactly what this package put on disk, so an older
# receipt removes that version's files. Directories are removed only when they
# are the package's own (a `remotex` component in the path) and end up empty,
# which leaves the shared prefixes pkgbuild also records — /usr/local/bin,
# /usr/local/share, /usr/local/share/doc — alone.
#
# The live config is not package-owned and is never touched. Delete
# /usr/local/etc/remotex separately to remove the stored credentials.
#
#   --dry-run   print what would be removed, change nothing
#
# Run with sudo; the payload lives under a root-owned prefix.
set -euo pipefail

pkgid=com.andrewtheguy.remotex.gateway
config=/usr/local/etc/remotex/remotex.toml
dry_run=false

case "${1:-}" in
  --dry-run) dry_run=true ;;
  "") : ;;
  *) echo "usage: $(basename "$0") [--dry-run]" >&2; exit 2 ;;
esac

[ "$(uname -s)" = Darwin ] || { echo "error: this uninstaller is macOS-only" >&2; exit 1; }

if ! info="$(pkgutil --pkg-info "$pkgid" 2>/dev/null)"; then
  echo "error: no receipt for $pkgid — the package is not installed" >&2
  echo "       a tarball install is removed by packaging/uninstall.sh instead" >&2
  exit 1
fi

if [ "$dry_run" = false ] && [ "$(id -u)" -ne 0 ]; then
  echo "error: run with sudo" >&2
  exit 1
fi

# Where the receipt says the payload was written. Read both fields rather than
# assume: `installer -target /` records a volume of / and leaves the location
# empty, while other receipts spell that same root as `/`. Both forms collapse
# to an empty root here, which is what makes the payload paths absolute.
volume="$(printf '%s\n' "$info" | awk -F': ' '/^volume: /{print $2}')"
location="$(printf '%s\n' "$info" | awk -F': ' '/^location: /{print $2}')"
root="${volume%/}/${location#/}"
root="${root%/}"

run() {
  if [ "$dry_run" = true ]; then
    echo "would: $*"
  else
    "$@"
  fi
}

# A receipt is only as trustworthy as whatever wrote it, and this script
# deletes: nothing outside the prefix the package is built for is touched.
under_prefix() {
  case "$1" in
    usr/local/?*) ;;
    *) return 1 ;;
  esac
  case "$1" in
    */../*|*/..|../*) return 1 ;;
  esac
  return 0
}

removed=0
while IFS= read -r relative; do
  [ -n "$relative" ] || continue
  under_prefix "$relative" || {
    echo "error: receipt names a file outside /usr/local: $relative" >&2
    exit 1
  }
  path="$root/$relative"
  if [ -e "$path" ] || [ -L "$path" ]; then
    if [ "$dry_run" = true ]; then
      echo "would: rm $path"
    else
      rm -f "$path"
      echo ">> removed $path"
    fi
    removed=$((removed + 1))
  fi
done < <(pkgutil --only-files --files "$pkgid")

# Deepest first, so a directory's children are gone before it is considered.
# `rmdir` on a non-empty directory fails, which is the point: anything the
# operator added under a payload directory keeps that directory.
while IFS= read -r relative; do
  [ -n "$relative" ] || continue
  under_prefix "$relative" || continue
  case "/$relative/" in */remotex/*) ;; *) continue ;; esac
  path="$root/$relative"
  [ -d "$path" ] || continue
  if [ "$dry_run" = true ]; then
    echo "would: rmdir $path (once its payload files are gone)"
  elif rmdir "$path" 2>/dev/null; then
    echo ">> removed $path"
  fi
done < <(pkgutil --only-dirs --files "$pkgid" | sort -r)

run pkgutil --forget "$pkgid"

if [ "$dry_run" = true ]; then
  echo ">> $removed payload file(s) would be removed"
else
  echo ">> $removed payload file(s) removed"
fi
if [ -e "$config" ]; then
  echo ">> config kept at $config — 'sudo rm -rf ${config%/*}' removes the credentials"
fi
