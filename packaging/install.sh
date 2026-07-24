#!/usr/bin/env bash
# Install remotex from an extracted release tarball.
#
# Layout (distro-agnostic, works on Linux and macOS):
#
#   <prefix>/etc/remotex.toml                        # stable user configuration
#   <prefix>/versions/<version>/{bin,share}         # this version's files
#   <prefix>/current -> versions/<version>          # active version (atomic swap)
#   <bindir>/remotex -> <prefix>/current/bin/remotex  # launcher on PATH (stable)
#
# Upgrade model:
#   1. The new version is staged fully into versions/<version> before anything
#      user-visible changes.
#   2. `current` is flipped to it with an atomic rename(2), so the launcher
#      never observes a half-installed version — a running server keeps serving
#      the version it started from.
#   3. Older versions are pruned, keeping only the new one and the immediately
#      previous one (for rollback: point `current` back at it).
#
# Config is seeded once at <prefix>/etc/remotex.toml and is never rolled with
# versions. A lock prevents two installs from racing on the same prefix.
#
# Env overrides:
#   PREFIX   install root   (default: /usr/local/opt/remotex)
#   BINDIR   launcher dir   (default: /usr/local/bin)
#
# Run from inside the extracted remotex-<version>/ directory. Use sudo if the
# target directories are not writable by your user.
set -euo pipefail

src="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
prefix="${PREFIX:-/usr/local/opt/remotex}"
bindir="${BINDIR:-/usr/local/bin}"

[ -f "$src/VERSION" ] || { echo "error: run this from an extracted remotex release" >&2; exit 1; }
version="$(cat "$src/VERSION")"

# Serialize installs against this prefix. `mkdir` is atomic on POSIX, so it
# doubles as a portable lock (flock isn't on stock macOS). The lock is released
# on any exit; a leftover lock means another install is in progress.
mkdir -p "$prefix"
lock="$prefix/.install.lock"
if ! mkdir "$lock" 2>/dev/null; then
  echo "error: another install is running (lock: $lock). Remove it if stale." >&2
  exit 1
fi
trap 'rm -rf "$lock" "${staging:-}"' EXIT

# Atomically replace the symlink `$2` with a new symlink to `$1`, without ever
# dereferencing an existing symlink-to-directory. GNU coreutils uses `-T`; BSD
# (macOS) uses `-h`. Both resolve to a single rename(2).
swap_symlink() {
  local target="$1" link="$2" tmp
  tmp="$(dirname "$link")/.$(basename "$link").new.$$"
  ln -s "$target" "$tmp"
  if mv --version >/dev/null 2>&1; then
    mv -Tf "$tmp" "$link"
  else
    mv -fh "$tmp" "$link"
  fi
}

# What is active right now, before we touch anything — this becomes "previous".
prev_link="$(readlink "$prefix/current" 2>/dev/null || true)"   # e.g. versions/0.0.1
prev_version="${prev_link#versions/}"

# Stage the new version into a temp dir, then publish it in one move so a partial
# copy is never named versions/<version>.
staging="$prefix/versions/.incoming.$version.$$"
final="$prefix/versions/$version"
mkdir -p "$prefix/versions"

echo ">> staging remotex $version"
rm -rf "$staging"
mkdir -p "$staging"
cp -R "$src/bin" "$src/share" "$staging/"
cp "$src/VERSION" "$staging/VERSION"

config_dir="$prefix/etc"
config="$config_dir/remotex.toml"
mkdir -p "$config_dir"
if [ -e "$config" ] && [ ! -f "$config" ]; then
  echo "error: config path is not a regular file: $config" >&2
  exit 1
fi
if [ ! -f "$config" ]; then
  echo ">> seeding config from sample — edit $config"
  cp "$src/share/doc/remotex/remotex.toml.example" "$config"
else
  echo ">> preserving config at $config"
fi
if [ -n "${SUDO_UID:-}" ] && [ -n "${SUDO_GID:-}" ]; then
  chown "$SUDO_UID:$SUDO_GID" "$config_dir" "$config"
fi
chmod 700 "$config_dir"
if ! chmod 600 "$config"; then
  echo "error: could not set 600 permissions on $config" >&2
  echo "       refusing to install — the credentials file would not be secured." >&2
  exit 1
fi

# Publish the version directory (replace any same-version dir from a prior run).
rm -rf "$final"
mv "$staging" "$final"

# Flip the active version atomically, then ensure the launcher exists (stable —
# it always follows `current`, so upgrades don't touch it).
echo ">> activating $version"
swap_symlink "versions/$version" "$prefix/current"
mkdir -p "$bindir"
ln -sfn "$prefix/current/bin/remotex" "$bindir/remotex"

# Keep only the active version and the immediately previous one; remove the rest.
for dir in "$prefix"/versions/*/; do
  [ -d "$dir" ] || continue
  name="$(basename "$dir")"
  case "$name" in
    "$version"|"$prev_version") : ;;
    *) echo ">> removing old version $name"; rm -rf "$dir" ;;
  esac
done

echo ">> installed. 'remotex' -> $bindir/remotex -> $prefix/current/bin/remotex"
if [ -n "$prev_version" ] && [ "$prev_version" != "$version" ]; then
  echo ">> previous version $prev_version kept for rollback:"
  echo "     $(basename "$0" .sh) rollback  ->  ln -sfn versions/$prev_version $prefix/current"
fi
echo ">> config: $config"
echo ">> run:    remotex serve"
