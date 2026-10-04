#!/usr/bin/env bash
# Prints the next release version: the higher of the kernel's Cargo.toml version
# and the latest release tag, with one part bumped. Fails (with a GitHub Actions
# error annotation) on anything that could release a version twice or go
# backwards.
#
# Usage: .github/scripts/next-version.sh patch|minor|major (from the repo root)
set -euo pipefail

fail() {
    echo "::error::$1" >&2
    exit 1
}

semver='^(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)\.(0|[1-9][0-9]*)$'
# True if $1 < $2, comparing X.Y.Z numerically (so 0.9.0 < 0.10.0).
lower() { [[ $1 != "$2" && $(printf '%s\n%s\n' "$1" "$2" | sort -V | head -1) == "$1" ]]; }

bump=${1:-}
manifest=kernel/Cargo.toml
[[ -f $manifest ]] || fail "Run this from the repository root ($manifest not found)."
[[ $bump =~ ^(patch|minor|major)$ ]] || fail "Bump must be patch, minor or major, not '$bump'."

# `cargo pkgid` ends in "#<version>" or "#<name>@<version>".
pkgid=$(cargo pkgid --manifest-path "$manifest") || fail "Couldn't read the version from $manifest."
current=${pkgid##*[#@]}
[[ $current =~ $semver ]] || fail "$manifest's version '$current' isn't a plain X.Y.Z."

latest=$(
    git tag --list 'v*' | sed -E 's/^v//' |
        { grep -E "$semver" || true; } | sort -V | tail -1
)

base=$current
if [[ -n $latest ]] && lower "$current" "$latest"; then
    base=$latest
fi

IFS=. read -r major minor patch <<<"$base"
case $bump in
patch) patch=$((patch + 1)) ;;
minor) minor=$((minor + 1)) patch=0 ;;
major) major=$((major + 1)) minor=0 patch=0 ;;
esac
next="$major.$minor.$patch"

# Belt and braces: strictly above both, and not already tagged.
lower "$current" "$next" || fail "v$next isn't higher than $manifest's $current."
[[ -z $latest ]] || lower "$latest" "$next" || fail "v$next isn't higher than the latest release v$latest."
if git rev-parse -q --verify "refs/tags/v$next" >/dev/null; then
    fail "Tag v$next already exists."
fi

echo "$next"
