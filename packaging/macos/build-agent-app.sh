#!/usr/bin/env bash
# Build, sign and optionally notarize remotex-agent.app.
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
# Usage:
#   packaging/macos/build-agent-app.sh [options]
#
#   --debug                  Build the debug profile instead of release.
#   --notary-profile NAME    Notarize and staple after signing. NAME is a
#                            notarytool keychain profile created once with:
#                              xcrun notarytool store-credentials NAME \
#                                --key AuthKey_XXXX.p8 --key-id <ID> \
#                                --issuer <UUID>
#                            Requires a "Developer ID Application" identity.
#
# Signing identity, in order of preference:
#   1. $CODESIGN_IDENTITY, if set
#   2. a "Developer ID Application" identity (the one notarization needs)
#   3. the first "Apple Development" identity (fine for this Mac only)
#   4. ad-hoc ("-"), which works but changes the code identity on every build,
#      so both TCC grants need re-approving each time
#
# ## Signing non-interactively (CI, or an SSH session)
#
# `codesign` needs the signing key's *partition list* to permit it, which the
# GUI's "Allow all applications to access this item" does not set. From a
# session that cannot show UI — CI, or SSH/VS Code Remote into a Mac — signing
# otherwise fails with `errSecInternalComponent` no matter how the keychain is
# unlocked. Set these to import a .p12 into a throwaway keychain instead, the
# same pattern ../ezvpn-apple uses in CI:
#
#   MACOS_CERT_P12         base64 of a .p12 exported *with its private key*
#   MACOS_CERT_PASSWORD    that .p12's export password
#   MACOS_KEYCHAIN_PASSWORD  any string; scopes the temporary keychain
#
# Without them the script uses the login keychain as normal, which is what you
# want when running at the Mac's own console.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

[ "$(uname -s)" = Darwin ] || { echo "the agent only builds on macOS" >&2; exit 1; }

profile=release
cargo_flags=(--release)
notary_profile=""
while [ $# -gt 0 ]; do
  case "$1" in
    --debug) profile=debug; cargo_flags=(); shift ;;
    --notary-profile) notary_profile="${2:?--notary-profile needs a name}"; shift 2 ;;
    -h|--help) sed -n '2,44p' "$0" | sed 's/^# \{0,1\}//'; exit 0 ;;
    *) echo "unexpected argument: $1" >&2; exit 1 ;;
  esac
done

# The agent inherits `version.workspace = true`, so the number lives in the
# workspace manifest — reading the agent's own would find the inheritance marker,
# not a version.
version="$(python3 -c '
import tomllib
with open("Cargo.toml", "rb") as f:
    print(tomllib.load(f)["workspace"]["package"]["version"])
')"

# ── Optional: a throwaway keychain holding an imported .p12 ─────────────────
temp_keychain=""
temp_dir=""
cleanup() {
  if [ -n "$temp_keychain" ]; then
    security delete-keychain "$temp_keychain" 2>/dev/null || true
  fi
  # Holds the decoded .p12, so it must go even when a step below fails.
  if [ -n "$temp_dir" ]; then
    rm -rf "$temp_dir"
  fi
}
trap cleanup EXIT

if [ -n "${MACOS_CERT_P12:-}" ]; then
  : "${MACOS_CERT_PASSWORD:?MACOS_CERT_P12 is set but MACOS_CERT_PASSWORD is not}"
  : "${MACOS_KEYCHAIN_PASSWORD:?MACOS_CERT_P12 is set but MACOS_KEYCHAIN_PASSWORD is not}"
  echo ">> importing the signing certificate into a temporary keychain"
  # A private key must never land on a path another process could predict (or
  # pre-create): mktemp -d owns both files, and the trap above removes it.
  temp_dir="$(mktemp -d)"
  temp_keychain="$temp_dir/remotex-agent-signing.keychain-db"
  cert_path="$temp_dir/cert.p12"
  printf '%s' "$MACOS_CERT_P12" | base64 --decode > "$cert_path"
  security create-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$temp_keychain"
  security set-keychain-settings -lut 21600 "$temp_keychain"
  security unlock-keychain -p "$MACOS_KEYCHAIN_PASSWORD" "$temp_keychain"
  security import "$cert_path" -P "$MACOS_CERT_PASSWORD" -f pkcs12 \
    -k "$temp_keychain" -T /usr/bin/codesign
  rm -f "$cert_path"
  # Prepend ours to the search list, preserving the existing keychains. Each
  # existing path stays one argument — `security` prints them indented and
  # quoted, and a home directory with a space in it would otherwise be split
  # into two nonexistent keychains, silently dropping the login keychain.
  set_keychains=(list-keychains -d user -s "$temp_keychain")
  while IFS= read -r keychain; do
    keychain="${keychain%\"}"   # trailing quote
    keychain="${keychain#*\"}"  # leading indent and opening quote
    if [ -n "$keychain" ]; then
      set_keychains+=("$keychain")
    fi
  done < <(security list-keychains -d user)
  security "${set_keychains[@]}"
  # The step that actually makes non-interactive signing work.
  security set-key-partition-list -S apple-tool:,apple:,codesign: \
    -s -k "$MACOS_KEYCHAIN_PASSWORD" "$temp_keychain" >/dev/null
  if ! security find-identity -v -p codesigning "$temp_keychain" | grep -q "1)"; then
    echo "error: no codesigning identity after import — MACOS_CERT_P12 must be a" >&2
    echo "       .p12 exported WITH its private key (export the *identity* from" >&2
    echo "       Keychain Access, not just the certificate)" >&2
    exit 1
  fi
fi

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

# ── Resolve a signing identity ──────────────────────────────────────────────
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
  identity="$CODESIGN_IDENTITY"
else
  available="$(security find-identity -v -p codesigning 2>/dev/null || true)"
  # Developer ID first: it is the only kind notarization accepts, and the only
  # kind that runs on someone else's Mac.
  identity="$(printf '%s' "$available" \
    | sed -n 's/.*"\(Developer ID Application:[^"]*\)".*/\1/p' | head -1)"
  if [ -z "$identity" ]; then
    identity="$(printf '%s' "$available" \
      | sed -n 's/.*"\(Apple Development:[^"]*\)".*/\1/p' | head -1)"
  fi
  if [ -z "$identity" ]; then
    echo ">> no signing identity found; falling back to ad-hoc"
    echo "   (the code identity changes every build, so macOS will ask for the"
    echo "    Screen Recording and Accessibility grants again each time)"
    identity="-"
  fi
fi
echo ">> signing as: $identity"

if [ -n "$notary_profile" ] && [ "${identity#Developer ID Application}" = "$identity" ]; then
  echo "error: notarization needs a 'Developer ID Application' identity," >&2
  echo "       but signing with: $identity" >&2
  exit 1
fi

# A secure timestamp is required for notarization and harmless otherwise, but it
# needs the network — skip it for ad-hoc, which cannot carry one anyway.
timestamp_flag=(--timestamp)
[ "$identity" = "-" ] && timestamp_flag=(--timestamp=none)

# --force so a rebuild replaces the previous signature in place, and
# --options runtime (hardened runtime) because notarization requires it and TCC
# treats a hardened, properly-signed process as a stable identity.
codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" \
  "$app/Contents/MacOS/remotex-agent"
codesign --force --sign "$identity" --options runtime "${timestamp_flag[@]}" "$app"

echo ">> verifying"
codesign --verify --deep --strict --verbose=2 "$app"
codesign -dv "$app" 2>&1 | sed -n 's/^\(Identifier\|TeamIdentifier\|Authority\)=/  \1=/p'

# ── Optional: notarize and staple ───────────────────────────────────────────
if [ -n "$notary_profile" ]; then
  echo ">> notarizing (this waits on Apple, usually a minute or two)"
  zip_path="${TMPDIR:-/tmp}/remotex-agent-notarize.zip"
  rm -f "$zip_path"
  /usr/bin/ditto -c -k --keepParent "$app" "$zip_path"
  xcrun notarytool submit "$zip_path" --keychain-profile "$notary_profile" --wait
  rm -f "$zip_path"
  # Staples the ticket into the bundle so it validates offline, which matters:
  # a downloaded .app is quarantined and Gatekeeper checks it before any network.
  echo ">> stapling"
  xcrun stapler staple "$app"
  xcrun stapler validate "$app"
  echo ">> gatekeeper assessment"
  spctl --assess --type execute --verbose=2 "$app" || true
fi

cat <<NOTES

>> wrote $app

To install:
    cp -R $app /Applications/
    open /Applications/remotex-agent.app

That first open writes the config with a fresh pre-shared key and registers the
agent in System Settings > General > Login Items.

Everything after that is in the menu bar item, which is the agent's whole
interface — there are no subcommands. Open it for:

    Copy Pre-Shared Key  to paste the key into the gateway's rxa target
    Settings...          listen address, display and key, in one dialog

It also needs Screen Recording and Accessibility, and asks for whichever is
missing: the icon warns and the menu offers the right Privacy pane. Read the
grants there and not from a shell — macOS credits a permission to whatever
launched the process, so a shell asking on the agent's behalf answers for the
shell. The agent's own log has its answer too:

    grep permissions: ~/Library/Logs/remotex-agent.log | tail -2
NOTES
