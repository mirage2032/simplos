#!/usr/bin/env bash
# Takes this repository's own crates out of cargo target directories, before
# CI caches them (.github/actions/cargo/save). A job builds them again every
# time anyway: a checkout dates every file as it writes it, newer than any
# build a cache holds, and cargo judges path dependencies by their files'
# dates. Cached, they'd only be carried along, and with every new version,
# under a new hash, carried along again beside the old ones. What's left is
# what a checkout doesn't make stale: crates.io's crates, and the standard
# library the builds make for themselves with -Zbuild-std.
#
# Usage: .github/scripts/prune-cargo-cache.sh TARGET_DIR... (from the repo root)
set -euo pipefail

# Every package this repository has a manifest for, vendor/ included: the
# name in each one's [package] section.
mapfile -t crates < <(git ls-files -z '*Cargo.toml' | xargs -0 awk '
    FNR == 1 { package = 0 }
    /^\[/ { package = ($0 ~ /^\[package\][[:space:]]*$/) }
    package && /^name[[:space:]]*=/ && match($0, /"[^"]+"/) {
        print substr($0, RSTART + 1, RLENGTH - 2)
        package = 0
    }')
((${#crates[@]})) || {
    echo "prune-cargo-cache: found none of this repository's crates" >&2
    exit 1
}

for target in "$@"; do
    [[ -d $target ]] || continue
    # Cargo builds each package in PROFILE/build/PACKAGE/HASH: the profile in
    # the target directory, in a target's directory in it, or in a nested
    # target directory, as the bootloader's builds of its stages make.
    while IFS= read -r -d '' build; do
        for crate in "${crates[@]}"; do
            rm -rf "${build:?}/$crate"
        done
    done < <(find "$target" -maxdepth 4 -type d -name build -print0)
    find "$target" -maxdepth 4 -type d -name incremental -prune -exec rm -rf {} +
done
