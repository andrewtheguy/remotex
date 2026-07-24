#!/usr/bin/env bash
# rdpweb network installer for Linux and macOS.
#
#   curl -fsSL https://andrewtheguy.github.io/rdpweb/install.sh | bash
#   curl -fsSL https://raw.githubusercontent.com/andrewtheguy/rdpweb/main/install.sh | bash
#
# Downloads the release tarball for your platform, verifies its SHA-256 against
# the GitHub-published digest, and lays it out under <prefix> with a launcher on
# PATH (see packaging/install.sh for the on-disk layout).
#
# Usage: install.sh [RELEASE_TAG]
# Env:   PREFIX (default /usr/local/opt/rdpweb), BINDIR (default /usr/local/bin)
set -euo pipefail

REPO_OWNER="andrewtheguy"
REPO_NAME="rdpweb"
PREFIX="${PREFIX:-/usr/local/opt/rdpweb}"
BINDIR="${BINDIR:-/usr/local/bin}"
tmp_dir=""

RED='\033[0;31m'; GREEN='\033[0;32m'; YELLOW='\033[1;33m'; NC='\033[0m'
info()  { echo -e "${GREEN}[INFO]${NC} $1"; }
warn()  { echo -e "${YELLOW}[WARN]${NC} $1"; }
error() { echo -e "${RED}[ERROR]${NC} $1" >&2; }

cleanup() {
  if [ -n "$tmp_dir" ]; then
    rm -rf "$tmp_dir"
  fi
}

trap cleanup EXIT

fetch() {
  # fetch <url> -> stdout
  if command -v curl >/dev/null 2>&1; then curl -fsSL "$1"
  elif command -v wget >/dev/null 2>&1; then wget -qO- "$1"
  else error "need curl or wget"; exit 1; fi
}

download() {
  # download <url> <out>
  if command -v curl >/dev/null 2>&1; then curl -fL -o "$2" "$1"
  elif command -v wget >/dev/null 2>&1; then wget -O "$2" "$1"
  else error "need curl or wget"; exit 1; fi
}

sha256() {
  if command -v sha256sum >/dev/null 2>&1; then sha256sum "$1" | cut -d' ' -f1
  elif command -v shasum >/dev/null 2>&1; then shasum -a 256 "$1" | cut -d' ' -f1
  else error "need sha256sum or shasum"; exit 1; fi
}

detect_platform() {
  case "$(uname -s)" in
    Linux)  OS=linux ;;
    Darwin) OS=macos ;;
    *) error "unsupported OS: $(uname -s) (Linux/macOS only)"; exit 1 ;;
  esac
  case "$(uname -m)" in
    x86_64|amd64)  ARCH=x86_64 ;;
    arm64|aarch64) ARCH=arm64 ;;
    *) error "unsupported arch: $(uname -m)"; exit 1 ;;
  esac
}

latest_tag() {
  fetch "https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/latest" \
    | grep -m1 '"tag_name"' | sed -E 's/.*"tag_name": *"([^"]+)".*/\1/'
}

main() {
  detect_platform
  local tag="${1:-}"
  [ -n "$tag" ] || { info "resolving latest release…"; tag="$(latest_tag)"; }
  [ -n "$tag" ] || { error "could not determine a release tag"; exit 1; }

  local version="${tag#v}"
  local asset="${REPO_NAME}-${version}-${OS}-${ARCH}.tar.gz"
  info "installing ${REPO_NAME} ${tag} (${OS}-${ARCH})"

  # Look up the asset + its digest from the release metadata (integrity check).
  local release_json
  release_json="$(fetch "https://api.github.com/repos/${REPO_OWNER}/${REPO_NAME}/releases/tags/${tag}")"
  if echo "$release_json" | grep -q '"message": *"Not Found"'; then
    error "release ${tag} not found"; exit 1
  fi
  local expected
  if command -v jq >/dev/null 2>&1; then
    # Select the asset by exact name and strip the "sha256:" prefix.
    expected="$(echo "$release_json" \
      | jq -r --arg n "$asset" '.assets[] | select(.name == $n) | .digest // empty' \
      | head -1 | sed 's/^sha256://')"
  else
    # Fallback: scan the JSON textually near the matching asset name.
    expected="$(echo "$release_json" | grep -A40 "\"name\": *\"${asset}\"" \
      | grep '"digest"' | head -1 | grep -o 'sha256:[a-f0-9]*' | cut -d: -f2)"
  fi
  if [ -z "$expected" ]; then
    error "no ${asset} in release ${tag} (unsupported platform for this release?)"; exit 1
  fi

  tmp_dir="$(mktemp -d)"
  local url="https://github.com/${REPO_OWNER}/${REPO_NAME}/releases/download/${tag}/${asset}"
  info "downloading ${asset}"
  download "$url" "$tmp_dir/$asset"

  info "verifying checksum"
  local actual; actual="$(sha256 "$tmp_dir/$asset")"
  if [ "$actual" != "$expected" ]; then
    error "checksum mismatch — expected ${expected}, got ${actual}"; exit 1
  fi
  info "checksum ok: ${actual:0:16}…"

  tar -xzf "$tmp_dir/$asset" -C "$tmp_dir"
  local extracted="$tmp_dir/${REPO_NAME}-${version}"
  [ -x "$extracted/install.sh" ] || { error "bundled install.sh missing"; exit 1; }

  # System prefixes usually need root; escalate only if we can't write.
  local sudo=""
  if [ ! -w "$(dirname "$PREFIX")" ] || [ ! -w "$BINDIR" ]; then
    if [ "$(id -u)" -ne 0 ] && command -v sudo >/dev/null 2>&1; then
      warn "elevating with sudo to write ${PREFIX} and ${BINDIR}"
      sudo=sudo
    fi
  fi

  $sudo env PREFIX="$PREFIX" BINDIR="$BINDIR" "$extracted/install.sh"

  echo
  info "done — run: rdpweb serve"
  info "edit config: ${PREFIX}/current/etc/rdpweb.env"
}

main "$@"
