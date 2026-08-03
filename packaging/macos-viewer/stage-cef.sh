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
# It needs no CEF on the machine beforehand: an absent `CEF_PATH` is a place to
# download Chromium *to*, not a missing prerequisite, so the first build here and
# the first build on a clean CI runner are the same command.
#
# Three files land there, and the second one is the whole reason this is a script
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
#
#   cef-dir               where the framework itself is, for the bundle assembly
#                         to copy from. Resolving it costs a cargo invocation and
#                         this script has already made one.
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)"
cd "$repo_root"

profile="${1:-release}"
case "$profile" in
  release) cargo_flags=(--release) ;;
  debug) cargo_flags=() ;;
  *) echo "usage: $0 [debug|release]" >&2; exit 1 ;;
esac

# `CEF_PATH` is a **destination** as much as a source, and that is what makes a
# clean machine work: `cef-dll-sys` uses the directory when it already holds a
# distribution, and downloads one into `<CEF_PATH>/<version>/<os-arch>` when it does
# not. So nothing here has to be exported by hand first, and a CI runner needs only
# to cache this path to skip the download next time.
#
# Exported rather than passed, because the same value has to reach *every* cargo
# invocation below: `cef-dll-sys` declares `rerun-if-env-changed=CEF_PATH`, so two
# invocations that disagree about it rebuild the entire Chromium binding graph
# between them — the difference between a five-second loop and a five-minute one.
export CEF_PATH="${CEF_PATH:-$HOME/.local/share/cef}"

# Both crates in one invocation, for the same reason: one build script run, not two.
echo ">> building remotex-cef and remotex-cef-helper ($profile)"
cargo build -p remotex-cef -p remotex-cef-helper ${cargo_flags[@]+"${cargo_flags[@]}"}

# Where CEF actually landed. Asked for rather than assumed: it is `CEF_PATH` itself
# when that already held a distribution, and a versioned directory underneath it
# when the crate had to download one — see `src/bin/cef-dir.rs`. At the profile just
# built, so this is a lookup rather than a second compile of the whole graph.
cef_dir="$(cargo run -q -p remotex-cef --bin cef-dir ${cargo_flags[@]+"${cargo_flags[@]}"})"
framework="$cef_dir/Chromium Embedded Framework.framework"
[ -d "$framework" ] || {
  echo "no Chromium Embedded Framework at $framework" >&2
  exit 1
}

staging="target/cef-link"
mkdir -p "$staging"
cp "target/$profile/libremotex_cef.a" "$staging/libremotex_cef.a"
# What the framework is, for whoever assembles the bundle. Written here because
# resolving it costs a cargo invocation, and this script has already paid for one.
printf '%s\n' "$cef_dir" > "$staging/cef-dir"

sandbox="$framework/Libraries/libcef_sandbox.dylib"
[ -f "$sandbox" ] || {
  echo "libcef_sandbox.dylib missing from $framework" >&2
  exit 1
}
cp "$sandbox" "$staging/libcef_sandbox.dylib"
chmod u+w "$staging/libcef_sandbox.dylib"
install_name_tool -id "@rpath/libcef_sandbox.dylib" "$staging/libcef_sandbox.dylib"

echo ">> staged $staging"
