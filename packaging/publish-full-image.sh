#!/usr/bin/env bash
# Build the image release CI never publishes — the public image with the
# software HEVC decoder's release archive, which no release artifact holds — and
# push it to the operator's private registry, ghcr.io/andrewtheguy/remotex-full,
# under the tag's own name (v0.0.294).
#
# The tags it builds are release tags, and it builds nothing of remotex: the
# release workflow has published the public image of the tag, and this is one
# layer over that image's linux/amd64 half, placing the archive the tag's
# src/hevc_wasm.rs pins, downloaded from the
# private andrewtheguy/hevc-wasm-archives through `gh` and checked against that
# pin, in /opt/remotex/versions/<version>/share/remotex, the release tree's data
# directory, where the gateway looks for it (src/config.rs, data_dir) and,
# finding it, serves it: the mounted config says nothing of it.
#
# A tag whose gateway still decodes the Mac's picture with FFmpeg or its sound
# with fdk-aac, pins no software decoder, or looks for it elsewhere, is refused.
#
# The package must stay private: the public image leaves the decoder out, and a
# public package is a release artifact. The first push creates it `internal` —
# readable by the organization — and only the package's settings page on GitHub
# can make it private; there is no API for it. This script asks ghcr whether an
# anonymous client can pull the package, and does not push unless the answer is
# no or there is no package yet, and fails unless it is no after the push.
#
# Log in first, to ghcr with a token that has `write:packages`, and to GitHub
# with access to andrewtheguy/hevc-wasm-archives:
#
#   podman login ghcr.io
#   gh auth login
#
#   packaging/publish-full-image.sh TAG
#
#   TAG  the released tag to build on, e.g. v0.0.286
set -euo pipefail

repo_root="$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)"
cd "$repo_root"

registry=ghcr.io
package=andrewtheguy/remotex-full
image="${registry}/${package}"

[ $# -eq 1 ] && [ "${1#-}" = "$1" ] || { echo "usage: $0 TAG" >&2; exit 2; }
tag="$1"
public="${registry}/andrewtheguy/remotex:${tag}"

case "$(uname -s)-$(uname -m)" in
  Linux-x86_64) ;;
  *) echo "this builds linux/amd64 on a linux/amd64 host, not $(uname -s)-$(uname -m)" >&2; exit 1 ;;
esac

commit="$(git rev-parse --verify --quiet "refs/tags/${tag}^{commit}")" \
  || { echo "this checkout has no tag ${tag}: git fetch --tags" >&2; exit 1; }
if git cat-file -e "${commit}:src/libav.rs" 2>/dev/null; then
  echo "${tag} decodes the Mac's picture with FFmpeg, which this image no longer carries: build it from that tag's own script" >&2
  exit 1
fi
if git grep -q 'libfdk-aac' "${commit}" -- src/aac_eld.rs; then
  echo "${tag} decodes the Mac's sound with fdk-aac, which this image no longer carries: build it from that tag's own script" >&2
  exit 1
fi

# The software decoder the tag pins, read from its source: the gateway refuses any
# other archive.
pin="$(git show "${commit}:src/hevc_wasm.rs" 2>/dev/null)" \
  || { echo "${tag} pins no software HEVC decoder (no src/hevc_wasm.rs)" >&2; exit 1; }
wasm_version="$(sed -n 's/^pub const VERSION: &str = "\(.*\)";$/\1/p' <<<"$pin")"
wasm_sha256="$(sed -n 's/^const SHA256: &str = "\([0-9a-f]\{64\}\)";$/\1/p' <<<"$pin")"
[ -n "$wasm_version" ] && [ -n "$wasm_sha256" ] \
  || { echo "could not read the pinned version and SHA-256 from ${tag}'s src/hevc_wasm.rs" >&2; exit 1; }
wasm_archive="hevc-wasm-v${wasm_version}.tar.gz"
git grep -q 'fn data_dir_for_exe' "$commit" -- src/config.rs \
  || { echo "${tag}'s gateway does not look for the decoder in its release tree's share/remotex" >&2; exit 1; }

# Before the build rather than after it.
podman login --get-login "$registry" >/dev/null 2>&1 \
  || { echo "not logged in to ${registry}: podman login ${registry}" >&2; exit 1; }

echo ">> pulling ${public}"
podman pull --platform linux/amd64 "$public"
reported="$(podman run --rm --platform linux/amd64 "$public" --version)"
[ "$reported" = "remotex ${tag#v}" ] \
  || { echo "${public} reports '${reported}', not remotex ${tag#v}" >&2; exit 1; }

layer="$(mktemp -d)"
trap 'rm -rf "$layer"' EXIT

echo ">> downloading ${wasm_archive}"
gh release download "v${wasm_version}" --repo andrewtheguy/hevc-wasm-archives \
  --pattern "$wasm_archive" --dir "$layer"
echo "${wasm_sha256}  ${layer}/${wasm_archive}" | sha256sum --check --quiet \
  || { echo "${wasm_archive} is not the release ${tag} pins (SHA-256 ${wasm_sha256})" >&2; exit 1; }

cat >"$layer/Containerfile" <<'CONTAINERFILE'
ARG BASE
FROM ${BASE}
ARG VERSION
ARG WASM_ARCHIVE
COPY ${WASM_ARCHIVE} /opt/remotex/versions/${VERSION}/share/remotex/${WASM_ARCHIVE}
CONTAINERFILE

echo ">> building ${image}:${tag}"
podman build \
  --platform linux/amd64 \
  -f "$layer/Containerfile" \
  --build-arg "BASE=${public}" \
  --build-arg "VERSION=${tag#v}" \
  --build-arg "WASM_ARCHIVE=${wasm_archive}" \
  --label "org.opencontainers.image.revision=${commit}" \
  -t "${image}:${tag}" \
  "$layer"

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
echo ">> its software HEVC decoder: /opt/remotex/versions/${tag#v}/share/remotex/${wasm_archive}"
