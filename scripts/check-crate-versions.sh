#!/bin/sh
# Every crate keeps a literal [package.version] equal to crates/dafs-daemon's,
# which flake.nix reads. Originally release-please's cargo-workspace plugin
# needed it (it failed on `version.workspace = true`); releases are git tags now
# (release.yml), but one literal, uniform version is still what the flake and
# the binary's --version report.
#
# POSIX sh, no bashisms, to match scripts/install.sh's portability discipline.

set -eu

# Derived from this script's own path, NOT `git rev-parse --show-toplevel`. The
# Dagger mirror (ci/main.go's CargoVersions) copies the tree into /src as a
# Directory, so there is no .git and no git binary — rev-parse exited 127 there
# on every nightly while the GitHub job passed, since a runner checkout has both.
repo_root=$(CDPATH= cd -- "$(dirname -- "$0")/.." && pwd)
cd "$repo_root"

reference_manifest=crates/dafs-daemon/Cargo.toml
reference_version=$(awk -F'"' '
  /^\[package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version[[:space:]]*=/ { print $2; exit }
' "$reference_manifest")

if [ -z "$reference_version" ]; then
  echo "::error::could not read [package.version] from $reference_manifest" >&2
  exit 1
fi

bad=0
for manifest in crates/*/Cargo.toml; do
  if grep -qE '^version\.workspace[[:space:]]*=[[:space:]]*true' "$manifest"; then
    echo "::error::$manifest uses version.workspace = true — every member keeps a literal version (\"$reference_version\")" >&2
    bad=1
    continue
  fi

  version=$(awk -F'"' '
    /^\[package\]/ { in_section=1; next }
    /^\[/ { in_section=0 }
    in_section && /^version[[:space:]]*=/ { print $2; exit }
  ' "$manifest")

  if [ -z "$version" ]; then
    echo "::error::$manifest has no literal [package.version]" >&2
    bad=1
  elif [ "$version" != "$reference_version" ]; then
    echo "::error::$manifest version \"$version\" does not match crates/dafs-daemon's version \"$reference_version\"" >&2
    bad=1
  fi
done

[ "$bad" -eq 0 ] || exit 1
echo "all crate manifests pin literal version \"$reference_version\""
