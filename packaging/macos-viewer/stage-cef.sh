#!/usr/bin/env bash
# Build the Chromium host crate and stage what the Swift app links against.
#
# Usage:
#   packaging/macos-viewer/stage-cef.sh [debug|release]
#
# `swift build` and `swift test` both need this to have run: `Package.swift`
# links `-L<repo>/target/cef-link`, and this is what puts anything there.
# `build-viewer-app.sh` runs it for you; run it by hand before a bare
# `swift test`.
#
# Two files land there, and the second one is the whole reason this is a script
# rather than a `cp`:
#
#   libremotex_cef.a      the crate, built at the requested profile. A directory
#                         of its own rather than target/<profile>, because the
#                         manifest cannot know which profile was asked for and
#                         naming both would silently link the stale one.
#
#   libcef_sandbox.dylib  CEF's seatbelt. The app never calls it — entering the
#                         sandbox is `remotex-cef-helper`'s job — but it has to
#                         resolve the symbols anyway, because a Rust *staticlib*
#                         keeps every public symbol of its dependency graph and
#                         `cef::sandbox` is one of them. CEF ships it with the
#                         install name `./libcef_sandbox.dylib`, a relative path
#                         dyld would resolve against the working directory, so
#                         the staged copy is restamped `@rpath/…` and the app
#                         carries an `-rpath` into its own Frameworks directory.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

profile="${1:-release}"
case "$profile" in
  release) cargo_flags=(--release) ;;
  debug) cargo_flags=() ;;
  *) echo "usage: $0 [debug|release]" >&2; exit 1 ;;
esac

cef_path="${CEF_PATH:-$HOME/.local/share/cef}"
[ -d "$cef_path/Chromium Embedded Framework.framework" ] || {
  echo "no CEF at $cef_path — set CEF_PATH, or export one with:" >&2
  echo "  cargo run -p export-cef-dir -- --force \"\$HOME/.local/share/cef\"" >&2
  exit 1
}
export CEF_PATH="$cef_path"

# Both crates in **one** cargo invocation, and this is not tidiness. `cef-dll-sys`
# declares `rerun-if-env-changed=CEF_PATH`, so a second `cargo build` that does not
# have this variable exported rebuilds it — and `cef`, and both crates on top —
# every single time. Two invocations that disagree about one variable is the whole
# difference between a one-minute edit-build-launch loop and a five-second one.
echo ">> building remotex-cef and remotex-cef-helper ($profile)"
cargo build -p remotex-cef -p remotex-cef-helper ${cargo_flags[@]+"${cargo_flags[@]}"}

staging="target/cef-link"
mkdir -p "$staging"
cp "target/$profile/libremotex_cef.a" "$staging/libremotex_cef.a"

sandbox="$cef_path/Chromium Embedded Framework.framework/Libraries/libcef_sandbox.dylib"
[ -f "$sandbox" ] || {
  echo "libcef_sandbox.dylib missing from $cef_path" >&2
  exit 1
}
cp "$sandbox" "$staging/libcef_sandbox.dylib"
chmod u+w "$staging/libcef_sandbox.dylib"
install_name_tool -id "@rpath/libcef_sandbox.dylib" "$staging/libcef_sandbox.dylib"

echo ">> staged $staging"
