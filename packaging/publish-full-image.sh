#!/usr/bin/env bash
# Build the image release CI never publishes — remotex with `apple-hp-media`, the
# feature an `ard-high-performance` target needs and a release artifact may not
# carry — and push it to the operator's private registry,
# ghcr.io/andrewtheguy/remotex-full, under the tag's own name (v0.0.262).
#
# The tags it builds are release tags: the release workflow builds every public
# artifact of one without the feature, and this script is the only build of it
# that has the feature.
#
# The package must stay private: the feature is kept out of release artifacts
# because of its decoders' licences, and a public package is a release artifact.
# The first push creates it `internal` — readable by the organization — and only
# the package's settings page on GitHub can make it private; there is no API for
# it. This script asks ghcr whether an anonymous client can pull the package, and
# does not push unless the answer is no or there is no package yet, and fails
# unless it is no after the push.
#
# It builds a tag and nothing else, from `git archive` of that tag in this
# checkout, and only one GitHub also has at the same commit: neither an untagged
# commit nor anything uncommitted reaches the image, and the build leaves this
# repository's refs and worktrees alone. linux/amd64 only, and it does not
# cross-build.
#
# Log in first, with a token that has `write:packages`:
#
#   podman login ghcr.io
#
#   packaging/publish-full-image.sh TAG
#
#   TAG  the tag to build, e.g. v0.0.262
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

features=apple-hp-media
registry=ghcr.io
package=andrewtheguy/remotex-full
image="${registry}/${package}"

[ $# -eq 1 ] && [ "${1#-}" = "$1" ] || { echo "usage: $0 TAG" >&2; exit 2; }
tag="$1"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) echo "this builds linux/amd64 on a linux/amd64 host, not $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

commit="$(git rev-parse --verify --quiet "refs/tags/${tag}^{commit}")" \
  || { echo "this checkout has no tag ${tag}" >&2; exit 1; }
remote="$(git ls-remote origin "refs/tags/${tag}^{}" "refs/tags/${tag}" | awk '{print $1}' | tail -n 1)"
[ -n "$remote" ] || { echo "origin has no tag ${tag}: push it first" >&2; exit 1; }
remote_commit="$(git rev-parse --verify --quiet "${remote}^{commit}" 2>/dev/null || echo "$remote")"
[ "$remote_commit" = "$commit" ] \
  || { echo "${tag} is ${commit} here and ${remote_commit} on origin" >&2; exit 1; }

# Before the build rather than after it.
podman login --get-login "$registry" >/dev/null 2>&1 \
  || { echo "not logged in to ${registry}: podman login ${registry}" >&2; exit 1; }

work="$repo_root/tmp/full-image"
source="$work/source"
context="$work/context"

cleanup() {
  rm -rf "$source" "$context"
}

# One build at a time: they share the source directory, and a second run's cleanup
# would take it out from under the first.
mkdir -p "$work"
exec 9>"$work/lock"
flock -n 9 || { echo "another full-image build is running in $work" >&2; exit 1; }

trap cleanup EXIT
cleanup

echo ">> extracting ${tag} (${commit})"
mkdir -p "$source"
git archive "$commit" | tar -x -C "$source"

# build.rs runs the frontend build but not its install.
(cd "$source/frontend" && bun install --frozen-lockfile)

# Outside the source directory, so dependencies stay compiled between runs, and
# apart from this checkout's own target/, whose release binary is the native build.
# This checkout's build-container-binary.sh, not the tag's: a tag cut before that
# script took features would ignore them and yield an image its name misdescribes.
export CARGO_TARGET_DIR="$repo_root/target/full-image"
REMOTEX_SOURCE_DIR="$source" REMOTEX_CONTAINER_FEATURES="$features" \
  bash packaging/build-container-binary.sh "$context/bin/remotex"

mkdir -p "$context/share/doc/remotex"
cp "$source/remotex.example.toml" "$context/share/doc/remotex/remotex.example.toml"

version="$("$context/bin/remotex" --version | awk '{print $2}')"
[ "v${version}" = "$tag" ] \
  || { echo "${tag} builds remotex ${version}; a tag is v<its version>" >&2; exit 1; }

echo ">> building ${image}:${tag}"
podman build \
  --platform linux/amd64 \
  -f "$source/packaging/Dockerfile" \
  --build-arg "VERSION=${version}" \
  --label "org.opencontainers.image.revision=${commit}" \
  -t "${image}:${tag}" \
  "$context"

# What ghcr tells a client with no credentials that asks to pull the package, as
# measured: a token for a public package, 401 UNAUTHORIZED for a private one, and
# 403 DENIED for one that does not exist. Anything else says nothing.
anonymous_access() {
  local response body code
  response="$(curl -sS -w '\n%{http_code}' "https://${registry}/token?scope=repository:${package}:pull")" \
    || { echo "unreachable"; return; }
  body="${response%$'\n'*}"
  code="${response##*$'\n'}"
  case "$code" in
    200) grep -q '"token":"[^"]' <<<"$body" && echo public || echo "HTTP 200 without a token" ;;
    401) grep -q '"code":"UNAUTHORIZED"' <<<"$body" && echo private || echo "HTTP 401: ${body}" ;;
    403) grep -q '"code":"DENIED"' <<<"$body" && echo missing || echo "HTTP 403: ${body}" ;;
    *) echo "HTTP ${code}: ${body}" ;;
  esac
}

# Before a layer goes up. A package that does not exist yet is the first push,
# which the check after it covers.
access="$(anonymous_access)"
case "$access" in
  private | missing) ;;
  public) echo "${image} is public: make the package private before pushing to it" >&2; exit 1 ;;
  *) echo "could not tell whether ${image} is private (${access}); not pushing" >&2; exit 1 ;;
esac

echo ">> pushing ${image}:${tag}"
podman push "${image}:${tag}"

access="$(anonymous_access)"
[ "$access" = private ] \
  || { echo "${image} is not confirmed private after the push (${access}): anyone may pull ${tag}. Make the package private" >&2; exit 1; }

echo ">> pushed ${image}:${tag} (${commit}); anonymous pull refused"
