#!/usr/bin/env bash
# Build the image release CI never publishes — remotex with `apple-hp-audio`, the
# optional feature a container can carry and a release artifact may not — and push
# it to the operator's private registry, ghcr.io/andrewtheguy/remotex-full, under
# the release's own tag (v0.0.243).
#
# The package must stay private: the feature is kept out of release artifacts
# because of its decoder's licence, and a public package is a release artifact.
# The first push creates it `internal` — readable by the organization — and only
# the package's settings page on GitHub can make it private; there is no API for
# it. Every push, this script stops if an anonymous client can read the image.
#
# It builds a release tag and nothing else, from the source archive GitHub serves
# for that tag: neither an unreleased commit nor anything in this checkout reaches
# the image, and the build leaves this repository's refs and worktrees alone.
# linux/amd64 only, and it does not cross-build.
#
# Log in first, with a token that has `write:packages`:
#
#   podman login ghcr.io
#
#   packaging/publish-full-image.sh TAG
#
#   TAG  the release to build, e.g. v0.0.243
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

features=apple-hp-audio
registry=ghcr.io
package=andrewtheguy/remotex-full
image="${registry}/${package}"
github=https://github.com/andrewtheguy/remotex

[ $# -eq 1 ] && [ "${1#-}" = "$1" ] || { echo "usage: $0 TAG" >&2; exit 2; }
tag="$1"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) echo "this builds linux/amd64 on a linux/amd64 host, not $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

# Before the build rather than after it.
podman login --get-login "$registry" >/dev/null 2>&1 \
  || { echo "not logged in to ${registry}: podman login ${registry}" >&2; exit 1; }

work="$repo_root/tmp/full-image"
archive="$work/source.tar.gz"
source="$work/source"
context="$work/context"

cleanup() {
  rm -rf "$archive" "$source" "$context"
}

# One build at a time: they share the source directory, and a second run's cleanup
# would take it out from under the first.
mkdir -p "$work"
exec 9>"$work/lock"
flock -n 9 || { echo "another full-image build is running in $work" >&2; exit 1; }

trap cleanup EXIT
cleanup

echo ">> fetching the source of ${tag}"
curl -fsSL -o "$archive" "${github}/archive/refs/tags/${tag}.tar.gz" \
  || { echo "${github} serves no source archive for a tag ${tag}" >&2; exit 1; }
# git archive records the commit in the tar's header, which is all of git the
# image's revision label needs.
# Not a pipe: git stops reading after the header, and pipefail would make gzip's
# SIGPIPE this script's failure.
commit="$(git get-tar-commit-id < <(gzip -dc "$archive"))"
mkdir -p "$source"
tar -xzf "$archive" -C "$source" --strip-components=1

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
  || { echo "${tag} builds remotex ${version}; a release tag is v<its version>" >&2; exit 1; }

echo ">> building ${image}:${tag}"
podman build \
  --platform linux/amd64 \
  -f "$source/packaging/Dockerfile" \
  --build-arg "VERSION=${version}" \
  --label "org.opencontainers.image.revision=${commit}" \
  -t "${image}:${tag}" \
  "$context"

echo ">> pushing ${image}:${tag}"
podman push "${image}:${tag}"

# What a client with no credentials is told: a token that can pull means the
# package is public.
anonymous="$(curl -sS "https://${registry}/token?scope=repository:${package}:pull" \
  | sed -n 's/.*"token":"\([^"]*\)".*/\1/p')"
status="$(curl -s -o /dev/null -w '%{http_code}' \
  -H "Authorization: Bearer ${anonymous}" \
  -H 'Accept: application/vnd.oci.image.manifest.v1+json' \
  -H 'Accept: application/vnd.docker.distribution.manifest.v2+json' \
  "https://${registry}/v2/${package}/manifests/${tag}")"
[ "$status" != 200 ] \
  || { echo "${image} is public: anyone can pull ${tag}. Make the package private" >&2; exit 1; }

echo ">> pushed ${image}:${tag} (${commit}); anonymous pull: HTTP ${status}"
