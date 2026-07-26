#!/bin/sh
# Guards against the regression that broke release-please's cargo-workspace
# plugin twice in a row: it needs a literal semver in every member's
# [package.version] to read and bump it. `version.workspace = true` parses as
# a table, not a string, and release-please fails with "invalid
# [package.version]" — see fix/release-please-cargo-workspace-versions.
#
# Also catches drift: a member whose literal version no longer matches
# [workspace.package].version (release-please should always move them
# together, but a hand edit could desync them).
#
# POSIX sh, no bashisms, to match scripts/install.sh's portability discipline.

set -eu

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

workspace_version=$(awk -F'"' '
  /^\[workspace\.package\]/ { in_section=1; next }
  /^\[/ { in_section=0 }
  in_section && /^version[[:space:]]*=/ { print $2; exit }
' Cargo.toml)

if [ -z "$workspace_version" ]; then
  echo "::error::could not read [workspace.package] version from Cargo.toml" >&2
  exit 1
fi

bad=0
for manifest in crates/*/Cargo.toml; do
  if grep -qE '^version\.workspace[[:space:]]*=[[:space:]]*true' "$manifest"; then
    echo "::error::$manifest uses version.workspace = true — release-please's cargo-workspace plugin needs a literal version (\"$workspace_version\")" >&2
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
  elif [ "$version" != "$workspace_version" ]; then
    echo "::error::$manifest version \"$version\" does not match workspace version \"$workspace_version\"" >&2
    bad=1
  fi
done

[ "$bad" -eq 0 ] || exit 1
echo "all crate manifests pin literal version \"$workspace_version\""
