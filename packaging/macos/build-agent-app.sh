#!/usr/bin/env bash
# Build, sign and optionally notarize remotex-agent.app, and wrap it in a .dmg.
#
# Builds dist/remotex-agent.app — a background-only, self-registering bundle
# wrapping the `remotex-agent` binary. Two reasons it is a bundle at all:
#
#   * The TCC grants (Screen Recording, Accessibility) attach to a *stable signed
#     identity*, so they survive rebuilds — see packaging/macos/Info.plist.
#   * A bundle is what LaunchServices, the menu bar and the Privacy panes all
#     name the agent by.
#
# No LaunchAgent plist is embedded any more. Starting at login is arranged by the
# agent writing ~/Library/LaunchAgents/dev.remotex.agent.plist with its own
# *absolute* path, from the menu's Start at Login or --install-launchagent — see
# crates/rxa-agent/src/loginitem.rs for why a bundle-relative one was a trap.
#
# It then produces dist/remotex-agent-<version>-macos-arm64[-unsigned].dmg, which
# is what a user is meant to get: a disk image dragged to /Applications is the
# standard way to install a Mac app, and it is the more robust one here. The
# loose .app is removed once it is inside the image, so dist/ holds one artifact
# and nobody installs the copy that is not the delivered one. --no-dmg keeps it.
#
# The concrete part of "more robust" is that an image is a filesystem, so the
# bundle inside is the one that was signed, with no archive round-trip in the
# middle. An archive can damage a bundle: `ditto -c -k` *without*
# `--sequesterRsrc`, unpacked with plain `unzip`, leaves AppleDouble `._*` files
# inside the bundle, which the signature does not seal and
# `codesign --verify --strict` then rejects. An image cannot do that, and it
# carries an /Applications symlink so the drag has somewhere to go.
#
# Usage:
#   packaging/macos/build-agent-app.sh [options]
#
#   A signing identity from the keychain is used by default. Ad-hoc signing is
#   the last resort, not the norm — see the warning below for what it costs.
#
#   --debug                  Build the debug profile instead of release.
#   --no-dmg                 Stop after the .app. For a local build being run
#                            straight out of dist/.
#   --notary-profile NAME    Notarize and staple after signing — the .app and the
#                            .dmg both. NAME is a notarytool keychain profile
#                            created once with:
#                              xcrun notarytool store-credentials NAME \
#                                --key AuthKey_XXXX.p8 --key-id <ID> \
#                                --issuer <UUID>
#                            Requires a "Developer ID Application" identity.
#
# Signing identity, in order of preference:
#   1. $CODESIGN_IDENTITY, if set
#   2. a "Developer ID Application" identity (the one notarization needs)
#   3. the first "Apple Development" identity (fine for this Mac only)
#   4. ad-hoc ("-"), only when the keychain has nothing else
#
# ## Why ad-hoc signing is the last resort
#
# An ad-hoc signature has no stable code identity: every build produces a
# different one, and macOS treats each as a *different app*. The TCC grants do
# not carry over, and worse, they do not simply re-prompt — System Settings
# keeps the old entry, matching by path, while the system refuses the app behind
# it. After installing each new ad-hoc build you have to go to System Settings >
# Privacy & Security and, under BOTH Screen Recording and Accessibility, remove
# remotex-agent with the "-" button and add it again with "+", then reopen the
# agent. Every build. A signed identity — even a free "Apple Development" one,
# which is enough for your own Mac — makes the grants stick.
#
# The release workflow (.github/workflows/release.yml) still ships ad-hoc, which
# is why its artifact is named `-unsigned.dmg`.
#
# ## Do not alternate between the two
#
# Pick one source of builds and stay on it. Installing the GitHub release's
# ad-hoc `-unsigned.dmg` over a locally signed build — or the reverse — changes
# the code identity just as surely as two ad-hoc builds do, so the grants break
# and need the same manual remove-and-re-add. The symptom is an agent that looks
# approved in System Settings and still cannot capture the screen.
#
# ## Signing non-interactively (CI, or an SSH session)
#
# `codesign` needs the signing key's *partition list* to permit it, which the
# GUI's "Allow all applications to access this item" does not set. From a
# session that cannot show UI — CI, or SSH/VS Code Remote into a Mac — signing
# otherwise fails with `errSecInternalComponent` no matter how the keychain is
# unlocked. Set these to import a .p12 into a throwaway keychain instead, the
# same pattern ../ezvpn-apple uses in CI. Also set CODESIGN_IDENTITY to the
# imported identity's name:
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
make_dmg=1
# Set when the build ends up ad-hoc signed, so the closing notes can repeat what
# that costs — the warning at signing time scrolls away behind the build output.
adhoc=0
while [ $# -gt 0 ]; do
  case "$1" in
    # `--profile dev` rather than no flag at all: bash 3.2 (what macOS ships) is
    # the version where expanding an *empty* array under `set -u` is an error, so
    # an empty cargo_flags would take the build down instead of building debug.
    --debug) profile=debug; cargo_flags=(--profile dev); shift ;;
    --no-dmg) make_dmg=0; shift ;;
    --notary-profile) notary_profile="${2:?--notary-profile needs a name}"; shift 2 ;;
    -h|--help) sed -n '2,/^set -euo pipefail$/p' "$0" | sed '$d; s/^# \{0,1\}//'; exit 0 ;;
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
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources"
cp "target/$profile/remotex-agent" "$app/Contents/MacOS/remotex-agent"
chmod +x "$app/Contents/MacOS/remotex-agent"

# The icon, committed rather than rasterized here: this script runs on CI runners
# with no SVG rasterizer, and what gets signed should be a fixed input. Regenerate
# it from packaging/macos/icon.svg with make-icon.sh.
[ -f packaging/macos/AppIcon.icns ] || {
  echo "error: packaging/macos/AppIcon.icns is missing —" >&2
  echo "       run packaging/macos/make-icon.sh (needs brew install librsvg)" >&2
  exit 1
}
cp packaging/macos/AppIcon.icns "$app/Contents/Resources/AppIcon.icns"

# Stamp the version into a copy of the template. CFBundleIdentifier is
# deliberately left alone — changing it resets both TCC grants.
sed -e "s|<string>0\.0\.0</string>|<string>${version}</string>|g" \
  packaging/macos/Info.plist > "$app/Contents/Info.plist"

# ── Resolve a signing identity ──────────────────────────────────────────────
if [ -n "${CODESIGN_IDENTITY:-}" ]; then
  identity="$CODESIGN_IDENTITY"
  # An explicit "-" is still ad-hoc, and costs the same as stumbling into it.
  # Spelled out rather than `[ ... ] && adhoc=1`, which under `set -e` exits the
  # script whenever the test is false.
  if [ "$identity" = "-" ]; then
    adhoc=1
  fi
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
    identity="-"
    adhoc=1
    echo ">> WARNING: no signing identity in the keychain; falling back to ad-hoc" >&2
    echo "   Every ad-hoc build is a different app to macOS, so the Screen" >&2
    echo "   Recording and Accessibility grants will not carry over — and they" >&2
    echo "   will not re-prompt either. After installing this build, open System" >&2
    echo "   Settings > Privacy & Security and, under BOTH Screen Recording and" >&2
    echo "   Accessibility, remove remotex-agent with '-' and add it back with" >&2
    echo "   '+'. You will have to repeat that for every ad-hoc build." >&2
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

# ── Wrap it in a disk image ─────────────────────────────────────────────────
# The delivered artifact. See the header for why an image rather than an archive.
dmg=""
if [ "$make_dmg" = 1 ]; then
  suffix=""
  # The filename tells the truth about what is inside it. An ad-hoc bundle runs
  # on this Mac and nowhere else without a quarantine dance, and a user who
  # downloads one should be able to see that before they mount it.
  [ "$identity" = "-" ] && suffix="-unsigned"
  [ "$profile" = debug ] && suffix="${suffix}-debug"
  dmg="dist/remotex-agent-${version}-macos-arm64${suffix}.dmg"

  echo ">> building $dmg"
  staging="dist/dmg-root"
  rm -rf "$staging" "$dmg"
  mkdir -p "$staging"
  # ditto, not cp: it is the copy that reproduces a bundle exactly — extended
  # attributes, symlinks, permissions — and this one is under the signature.
  /usr/bin/ditto "$app" "$staging/remotex-agent.app"
  # Where the drag goes, so installing is one window and one gesture.
  ln -s /Applications "$staging/Applications"
  # UDZO: compressed and read-only, the ordinary format for a distributed image.
  # It does not stop anyone launching the agent straight off the mounted image;
  # what makes that a bad idea is the login item, which records the bundle it was
  # registered from — and a mount point does not survive an eject (the stale-path
  # failure is written up in packaging/macos/README.md). Hence "drag it first".
  #
  # No background picture and no arranged icon positions: setting those means
  # driving Finder over AppleScript, which needs a GUI session and would take
  # this script out of CI. A plain image with the app and the symlink in it is
  # the install every Mac user already knows.
  hdiutil create -volname "remotex-agent $version" -srcfolder "$staging" \
    -fs HFS+ -format UDZO -ov -quiet "$dmg"
  rm -rf "$staging"

  # Signed for the same reason the bundle is: the image is what gets downloaded,
  # and Gatekeeper checks it first. Ad-hoc adds nothing here, so it is skipped.
  if [ "$identity" != "-" ]; then
    echo ">> signing the disk image"
    codesign --force --sign "$identity" "${timestamp_flag[@]}" "$dmg"
  fi

  # Notarized separately from the bundle, and both are worth doing: the stapled
  # ticket on the image is what Gatekeeper reads when the download is opened,
  # and the one inside the bundle is what validates the copy in /Applications
  # afterwards, offline.
  if [ -n "$notary_profile" ]; then
    echo ">> notarizing the disk image"
    xcrun notarytool submit "$dmg" --keychain-profile "$notary_profile" --wait
    xcrun stapler staple "$dmg"
    xcrun stapler validate "$dmg"
  fi

  # The image now carries the bundle, so the loose one in dist/ is a second copy
  # of the same thing — and the wrong one to install from, since it is the image
  # that was notarized and stapled last. Drop it and leave dist/ unambiguous.
  echo ">> removing $app (it is inside the image now)"
  rm -rf "$app"
fi

if [ -n "$dmg" ]; then
  wrote=">> wrote $dmg"
  install_note="To install, open the image and drag remotex-agent.app onto Applications:
    open $dmg

Then open it once from /Applications, and eject the image. Opening it straight off
the image would register a login item naming a mount point, which is gone the
moment you eject."
else
  wrote=">> wrote $app"
  install_note="Built without an image (--no-dmg). To install this one by hand:
    cp -R $app /Applications/
    open /Applications/remotex-agent.app"
fi

if [ "$adhoc" -eq 1 ]; then
  adhoc_note="
!! This build is AD-HOC SIGNED, so macOS sees it as a different app from every
   other build. The Screen Recording and Accessibility grants will not carry
   over, and will not re-prompt: System Settings keeps the stale entry while the
   system refuses the app behind it. After installing, go to System Settings >
   Privacy & Security and, under BOTH Screen Recording and Accessibility, remove
   remotex-agent with '-' and add it back with '+', then reopen the agent.
   Repeat after every ad-hoc build. Signing with an identity avoids all of this.
"
else
  adhoc_note=""
fi

cat <<NOTES

${wrote}
${install_note}
${adhoc_note}

That first open writes the config with a fresh keypair and registers the agent
in System Settings > General > Login Items. It starts unpaired: it listens and
refuses every connection until it is given a gateway's public key.

Everything after that is in the menu bar item, which is the agent's whole
interface — the only flags that do anything but launch it are --public-key, which
prints this Mac's public key, and --import-private-key, which reads a private key
from stdin to give this Mac an identity it already had. Open it for:

    Settings...          listen address, displays, and the two public keys —
                         Copy puts this Mac's on the clipboard for the gateway's
                         agent_public_key, and the gateway's own (printed by
                         "remotex rxa-pubkey") is pasted in beside it

It also needs Screen Recording and Accessibility, and asks for whichever is
missing: the icon warns and the menu offers the right Privacy pane. Read the
grants there and not from a shell — macOS credits a permission to whatever
launched the process, so a shell asking on the agent's behalf answers for the
shell. The agent's own log has its answer too:

    grep permissions: ~/Library/Logs/remotex-agent.log | tail -2
NOTES
