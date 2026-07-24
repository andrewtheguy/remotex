#!/usr/bin/env bash
# Remove remotex installed by install.sh.
#
#   PREFIX   install root   (default: /usr/local/opt/remotex)
#   BINDIR   launcher dir   (default: /usr/local/bin)
#
# By default removes the whole install tree (all versions + config). Pass a
# version as $1 to remove only that version, deactivating current if it points
# there. Use sudo if the directories are not writable by your user.
set -euo pipefail

prefix="${PREFIX:-/usr/local/opt/remotex}"
bindir="${BINDIR:-/usr/local/bin}"
version="${1:-}"

# Only remove the launcher symlink if it points into our prefix.
launcher="$bindir/remotex"
if [ -L "$launcher" ] && [ "$(readlink "$launcher")" = "$prefix/current/bin/remotex" ]; then
  rm -f "$launcher"
  echo ">> removed $launcher"
fi

if [ -n "$version" ]; then
  if [ "$(readlink "$prefix/current" 2>/dev/null)" = "versions/$version" ]; then
    rm -f "$prefix/current"
    echo ">> deactivated current (was $version)"
  fi
  rm -rf "$prefix/versions/$version"
  echo ">> removed version $version"
else
  rm -rf "$prefix"
  echo ">> removed $prefix"
fi
