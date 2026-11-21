#!/bin/sh
# release-commit.sh VERSION — sets every crate's [package] version (and their
# Cargo.lock entries) to VERSION and commits that to main through GitHub's
# createCommitOnBranch API, which signs the commit (main requires signatures).
# Prints the new commit SHA. Needs GH_TOKEN with contents: write and a ruleset
# bypass for GitHub Actions on main.
set -eu

ver=$1
repo=${GITHUB_REPOSITORY:?}
branch=main

set_versions() {
  names=""
  for m in crates/*/Cargo.toml; do
    awk -v v="$ver" '
      /^\[package\]/ { s=1; print; next }
      /^\[/ { s=0 }
      s && !done && /^version[[:space:]]*=/ { print "version = \"" v "\""; done=1; next }
      { print }
    ' "$m" > "$m.tmp" && mv "$m.tmp" "$m"
    names="$names $(awk -F'"' '/^\[package\]/{s=1;next} /^\[/{s=0} s&&/^name[[:space:]]*=/{print $2;exit}' "$m")"
  done
  # A workspace member's lock entry is name + version with no source line.
  awk -v v="$ver" -v names="$names" '
    BEGIN { n=split(names, a, " "); for (i=1;i<=n;i++) want[a[i]]=1 }
    /^name = "/ { split($0, q, "\""); hit = (q[2] in want) }
    hit && /^version = "/ { print "version = \"" v "\""; hit=0; next }
    { print }
  ' Cargo.lock > Cargo.lock.tmp && mv Cargo.lock.tmp Cargo.lock
}

b64() { base64 < "$1" | tr -d '\n'; }
d='$' # GraphQL variable sigil

tries=0
while :; do
  tries=$((tries + 1))
  git fetch -q origin "$branch"
  git checkout -q --detach "origin/$branch"
  head=$(git rev-parse HEAD)
  set_versions
  sh scripts/check-crate-versions.sh >/dev/null
  if git diff --quiet; then
    echo "$head"
    exit 0
  fi

  additions=""
  for f in $(git diff --name-only); do
    additions="$additions{\"path\":\"$f\",\"contents\":\"$(b64 "$f")\"},"
  done
  q="mutation(${d}i:CreateCommitOnBranchInput!){createCommitOnBranch(input:${d}i){commit{oid}}}"
  body=$(printf '{"query":"%s","variables":{"i":{"branch":{"repositoryNameWithOwner":"%s","branchName":"%s"},"expectedHeadOid":"%s","message":{"headline":"chore(release): v%s"},"fileChanges":{"additions":[%s]}}}}' \
    "$q" "$repo" "$branch" "$head" "$ver" "${additions%,}")

  if out=$(printf '%s' "$body" | gh api graphql --input - 2>&1); then
    printf '%s' "$out" | sed -n 's/.*"oid":"\([0-9a-f]*\)".*/\1/p'
    exit 0
  fi
  git checkout -q -- .
  # expectedHeadOid mismatch = main moved; anything else is a real failure.
  case "$out" in
    *"Expected branch to point to"*) [ "$tries" -lt 6 ] && continue ;;
  esac
  echo "::error::release commit failed: $out" >&2
  exit 1
done
