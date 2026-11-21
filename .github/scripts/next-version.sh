#!/bin/sh
# next-version.sh PREFIX INITIAL [PATH...]
#
# Prints the semver the commit at HEAD releases as, for tags named PREFIX<x.y.z>.
# Canonical copy: homelab ci/release/next-version.sh. App repos vendor it
# unchanged as .github/scripts/next-version.sh.
#
#   HEAD already carries a PREFIX tag   -> that version (a re-run is idempotent)
#   no PREFIX tag exists yet            -> INITIAL
#   no commit since the last tag
#   touches PATH...                     -> nothing printed (nothing to release)
#   otherwise                           -> last tag bumped by the commits since:
#       <type>!: or BREAKING CHANGE:    -> major (minor while major is 0)
#       feat:                           -> minor
#       anything else                   -> patch
#
# Needs the full history and tags (actions/checkout fetch-depth: 0).
set -eu

usage() {
	echo "usage: $0 PREFIX INITIAL [PATH...]" >&2
	exit 2
}

[ $# -ge 2 ] || usage
prefix=$1
initial=$2
shift 2

is_semver() {
	printf '%s\n' "$1" | grep -qE '^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
}

is_semver "$initial" || {
	echo "next-version: INITIAL '$initial' is not x.y.z" >&2
	exit 2
}

# Every PREFIX tag, prefix stripped, keeping only plain x.y.z.
versions() {
	git tag --list "${prefix}*" "$@" | while IFS= read -r t; do
		v=${t#"$prefix"}
		if is_semver "$v"; then printf '%s\n' "$v"; fi
	done | sort -t. -k1,1n -k2,2n -k3,3n
}

here=$(versions --points-at HEAD | tail -n 1)
if [ -n "$here" ]; then
	printf '%s\n' "$here"
	exit 0
fi

last=$(versions | tail -n 1)
if [ -z "$last" ]; then
	printf '%s\n' "$initial"
	exit 0
fi

log=$(git log --format='%s%n%b' "${prefix}${last}..HEAD" -- "$@")
[ -n "$log" ] || exit 0

major=${last%%.*}
rest=${last#*.}
minor=${rest%%.*}
patch=${rest#*.}

if printf '%s\n' "$log" | grep -qE '^[a-z]+(\([^)]*\))?!:|^BREAKING[ -]CHANGE:'; then
	if [ "$major" -eq 0 ]; then
		minor=$((minor + 1)) patch=0
	else
		major=$((major + 1)) minor=0 patch=0
	fi
elif printf '%s\n' "$log" | grep -qE '^feat(\([^)]*\))?:'; then
	minor=$((minor + 1)) patch=0
else
	patch=$((patch + 1))
fi

printf '%s.%s.%s\n' "$major" "$minor" "$patch"
