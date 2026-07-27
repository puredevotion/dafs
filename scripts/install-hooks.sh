#!/bin/sh
# Points this clone's git hooks at the repo-tracked .githooks/ directory.
#
# Local override of core.hooksPath, not global: a contributor (or this
# machine's own ~/.codex/git-hooks setup) may already have hooks configured
# for other repos, and this only needs to apply here.
#
# Idempotent — safe to run from the Nix devShell hook on every `nix develop`.

set -eu

repo_root=$(git rev-parse --show-toplevel)
cd "$repo_root"

chmod +x .githooks/*
git config core.hooksPath .githooks

echo "hooks installed: commit-msg, pre-push (core.hooksPath -> .githooks)"
