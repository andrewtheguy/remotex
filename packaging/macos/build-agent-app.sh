#!/usr/bin/env bash
# Build and sign remotex-agent.app.
#
# Produces dist/remotex-agent.app — a background-only, self-registering bundle
# wrapping the `remotex-agent` binary. Two reasons it is a bundle at all:
#
#   * The TCC grants (Screen Recording, Accessibility) attach to a *stable signed
#     identity*, so they survive rebuilds — see packaging/macos/Info.plist.
#   * It carries its own LaunchAgent plist, which SMAppService hands to launchd
#     when the agent registers itself. There is no install script: drag the
#     bundle to /Applications and open it once.
#
# Signing identity, in order of preference:
#   1. $CODESIGN_IDENTITY, if set
#   2. the first "Apple Development" identity in the keychain
#   3. ad-hoc ("-"), which still works but re-prompts for permissions more often
#
# Usage:
#   packaging/macos/build-agent-app.sh [--debug]
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

[ "$(uname -s)" = Darwin ] || { echo "the agent only builds on macOS" >&2; exit 1; }

profile=release
cargo_flags=(--release)
if [ "${1:-}" = --debug ]; then
  profile=debug
  cargo_flags=()
fi

version="$(python3 -c '
import tomllib
with open("crates/rxa-agent/Cargo.toml", "rb") as f:
    print(tomllib.load(f)["package"]["version"])
')"

echo ">> building remotex-agent ($profile)"
cargo build -p rxa-agent "${cargo_flags[@]}"

app="dist/remotex-agent.app"
echo ">> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Library/LaunchAgents"
cp "target/$profile/remotex-agent" "$app/Contents/MacOS/remotex-agent"
chmod +x "$app/Contents/MacOS/remotex-agent"

# Stamp the version into a copy of the template. CFBundleIdentifier is
# deliberately left alone — changing it resets both TCC grants.
sed -e "s|<string>0\.0\.0</string>|<string>${version}</string>|g" \
  packaging/macos/Info.plist > "$app/Contents/Info.plist"

# The plist SMAppService registers. Its filename must match the Label inside it
# and `loginitem::LABEL`, or registration finds nothing.
cp packaging/macos/embedded-launchagent.plist \
  "$app/Contents/Library/LaunchAgents/dev.remotex.agent.plist"

# Resolve a signing identity.
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
  identity="$CODESIGN_IDENTITY"
else
  identity="$(security find-identity -v -p codesigning 2>/dev/null \
    | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)"
  if [ -z "$identity" ]; then
    echo ">> no Apple Development identity found; falling back to ad-hoc signing"
    echo "   (permissions may need re-granting after a rebuild)"
    identity="-"
  fi
fi
echo ">> signing as: $identity"

# --force so a rebuild replaces the previous signature in place, and
# --options runtime (hardened runtime) because TCC treats a hardened,
# properly-signed process as a stable identity.
codesign --force --sign "$identity" --options runtime --timestamp=none \
  "$app/Contents/MacOS/remotex-agent"
codesign --force --sign "$identity" --options runtime --timestamp=none "$app"

echo ">> verifying"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv "$app" 2>&1 | sed -n 's/^\(Identifier\|TeamIdentifier\|Authority\)=/  \1=/p'

cat <<NOTES

>> wrote $app

To install:
    cp -R $app /Applications/
    open /Applications/remotex-agent.app

That first open writes the config with a fresh pre-shared key and registers the
agent in System Settings > General > Login Items. Then:

    /Applications/remotex-agent.app/Contents/MacOS/remotex-agent --show-psk

for the key to paste into the gateway's rxa target, and grant "remotex-agent"
BOTH Screen Recording and Accessibility in System Settings > Privacy & Security.
Check everything with --status.
NOTES
