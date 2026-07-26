#!/bin/sh
# Installs (or self-updates) the dafs daemon from a GitHub release.
#
# POSIX sh, no bashisms — this has to run on whatever /bin/sh a user's system
# points at, the same hermetic-build discipline the rest of the project holds
# itself to. It never runs anything it hasn't checksum-verified first.
#
# Usage:
#   install.sh [--version vX.Y.Z]                 # install (default: latest)
#   install.sh --check-only [--current-version vX.Y.Z]
#   install.sh --self-update --target-path PATH [--current-version vX.Y.Z]
#
# `--self-update`/`--check-only`/`--current-version` exist so `dafs self-update`
# can embed this exact script (include_str!) and shell out to it, rather than
# reimplementing fetch/verify/replace in Rust — one source of truth, and zero
# new dependencies in the daemon binary for a rarely-used code path.
#
# Exit codes: 0 = installed / already up to date. 1 = error. 3 = (check-only
# only) an update is available but was not applied.

set -eu

REPO="puredevotion/dafs"
GITHUB="https://github.com/${REPO}"
API="https://api.github.com/repos/${REPO}"

VERSION=""
CURRENT_VERSION=""
CHECK_ONLY=0
SELF_UPDATE=0
TARGET_PATH=""

while [ $# -gt 0 ]; do
  case "$1" in
    --version)
      VERSION="$2"
      shift 2
      ;;
    --current-version)
      CURRENT_VERSION="$2"
      shift 2
      ;;
    --check-only)
      CHECK_ONLY=1
      shift
      ;;
    --self-update)
      SELF_UPDATE=1
      shift
      ;;
    --target-path)
      TARGET_PATH="$2"
      shift 2
      ;;
    *)
      echo "install.sh: unknown argument: $1" >&2
      exit 1
      ;;
  esac
done

log() { echo "install.sh: $*" >&2; }
die() {
  echo "install.sh: $*" >&2
  exit 1
}

command -v curl >/dev/null 2>&1 || die "curl is required"
command -v sha256sum >/dev/null 2>&1 || die "sha256sum is required"
command -v tar >/dev/null 2>&1 || die "tar is required"

# Only Linux ships a release asset today (README's Platforms table: Linux is
# the primary target, Windows/mobile are later). Fail loudly on anything else
# rather than guess an asset name that doesn't exist.
os="$(uname -s)"
arch="$(uname -m)"
case "$os" in
  Linux) : ;;
  *) die "no release asset for '$os' — see README's Platforms table" ;;
esac
case "$arch" in
  x86_64|amd64) asset="dafs-x86_64-linux" ;;
  aarch64|arm64) asset="dafs-aarch64-linux" ;;
  *) die "no release asset for architecture '$arch'" ;;
esac

if [ -z "$VERSION" ]; then
  VERSION="$(curl -fsSL "${API}/releases/latest" | grep -o '"tag_name": *"[^"]*"' | head -1 | cut -d'"' -f4)"
  [ -n "$VERSION" ] || die "could not determine the latest release (GitHub API returned nothing usable)"
fi

if [ "$CHECK_ONLY" -eq 1 ]; then
  echo "current: ${CURRENT_VERSION:-unknown}"
  echo "latest:  $VERSION"
  if [ -n "$CURRENT_VERSION" ] && [ "$CURRENT_VERSION" = "$VERSION" ]; then
    exit 0
  fi
  exit 3
fi

workdir="$(mktemp -d)"
trap 'rm -rf "$workdir"' EXIT

tarball="${asset}.tar.gz"
log "downloading ${GITHUB}/releases/download/${VERSION}/${tarball}"
curl -fsSL -o "${workdir}/${tarball}" "${GITHUB}/releases/download/${VERSION}/${tarball}"
curl -fsSL -o "${workdir}/${tarball}.sha256" "${GITHUB}/releases/download/${VERSION}/${tarball}.sha256"

# Never extract before the checksum is verified.
( cd "$workdir" && sha256sum -c "${tarball}.sha256" ) || die "checksum verification failed — refusing to install"

tar -xzf "${workdir}/${tarball}" -C "$workdir" dafs
chmod +x "${workdir}/dafs"

if [ "$SELF_UPDATE" -eq 1 ]; then
  [ -n "$TARGET_PATH" ] || die "--self-update requires --target-path"
  install_dir="$(dirname "$TARGET_PATH")"
else
  install_dir="${DAFS_INSTALL_DIR:-$HOME/.local/bin}"
  TARGET_PATH="${install_dir}/dafs"
fi

mkdir -p "$install_dir"

# Atomic replace: stage in the same directory as the target so `mv` is a
# rename on one filesystem, not a copy — safe even while the old binary is
# the process currently running it (self-update).
staged="${install_dir}/.dafs.new.$$"
cp "${workdir}/dafs" "$staged"
mv "$staged" "$TARGET_PATH"

log "installed $VERSION to $TARGET_PATH"
