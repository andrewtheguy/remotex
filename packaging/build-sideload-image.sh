#!/usr/bin/env bash
# Build an operator's own container image of a release as a docker-archive tar, for
# the k3s side-load ../andrewkubernetes describes ("`remotex` pins to kube2 for its
# image"):
#
#   scp <tar> root@kube2:/var/lib/rancher/k3s/agent/images/
#   ssh root@kube2 'k3s ctr images import /var/lib/rancher/k3s/agent/images/<tar>'
#
# This is the image release CI never publishes — by default the one with
# `apple-hp-audio` — so its tag carries the features after the release's
# (v0.0.242-apple-hp-audio) and exists nowhere but the node it is imported on.
#
# It builds a release tag and nothing else, from the source archive GitHub serves
# for that tag: neither an unreleased commit nor anything in this checkout reaches
# the image, and the build leaves this repository's refs and worktrees alone.
# linux/amd64 only, and it does not cross-build.
#
#   packaging/build-sideload-image.sh TAG [--features LIST] [--out DIR]
#
#   TAG         the release to build, e.g. v0.0.242
#   --features  comma-separated cargo features (default apple-hp-audio; '' for none)
#   --out       where the tar is copied (default /mnt/dasdata/tmp/remotex)
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

tag=
features=apple-hp-audio
out=/mnt/dasdata/tmp/remotex
image=ghcr.io/andrewtheguy/remotex
github=https://github.com/andrewtheguy/remotex

while [ $# -gt 0 ]; do
  case "$1" in
    --features) features="$2"; shift 2 ;;
    --out)      out="$2"; shift 2 ;;
    -*) echo "unknown option: $1" >&2; exit 2 ;;
    *)
      [ -z "$tag" ] || { echo "one tag, not both $tag and $1" >&2; exit 2; }
      tag="$1"; shift ;;
  esac
done
[ -n "$tag" ] || { echo "usage: $0 TAG [--features LIST] [--out DIR]" >&2; exit 2; }

# The features are spelled into the image tag, and cargo takes more than a tag
# does — `dep/feature`, or a list separated by spaces — so say so before the build
# rather than after it.
image_tag="${tag}${features:+-${features//,/-}}"
[[ "$image_tag" =~ ^[A-Za-z0-9_][A-Za-z0-9_.-]{0,127}$ ]] \
  || { echo "features '${features}' make '${image_tag}', which is not an image tag" >&2; exit 1; }

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) echo "this builds linux/amd64 on a linux/amd64 host, not $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

work="$repo_root/tmp/sideload-image"
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
flock -n 9 || { echo "another side-load build is running in $work" >&2; exit 1; }

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
# script took features would ignore them and yield an image its tag misdescribes.
export CARGO_TARGET_DIR="$repo_root/target/sideload-image"
REMOTEX_SOURCE_DIR="$source" REMOTEX_CONTAINER_FEATURES="$features" \
  bash packaging/build-container-binary.sh "$context/bin/remotex"

mkdir -p "$context/share/doc/remotex"
cp "$source/remotex.example.toml" "$context/share/doc/remotex/remotex.example.toml"

version="$("$context/bin/remotex" --version | awk '{print $2}')"
[ "v${version}" = "$tag" ] \
  || { echo "${tag} builds remotex ${version}; a release tag is v<its version>" >&2; exit 1; }

name="remotex-${image_tag}-amd64.tar"

echo ">> building ${image}:${image_tag}"
podman build \
  --platform linux/amd64 \
  -f "$source/packaging/Dockerfile" \
  --build-arg "VERSION=${version}" \
  --label "org.opencontainers.image.revision=${commit}" \
  -t "${image}:${image_tag}" \
  "$context"

rm -f "$work/$name"
podman save --format docker-archive -o "$work/$name" "${image}:${image_tag}"

mkdir -p "$out"
cp "$work/$name" "$out/$name"
echo ">> wrote $out/$name (${image}:${image_tag}, ${commit})"
