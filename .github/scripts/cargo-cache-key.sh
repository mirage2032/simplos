#!/usr/bin/env bash
# Prints the key of a job's cargo cache (.github/actions/cargo): its name, and
# what decides the builds such a cache keeps, which are crates.io's crates and
# the standard library as -Zbuild-std builds it (prune-cargo-cache.sh takes
# the rest out): the toolchain, then the crates each lock pins from outside
# this repository, the profiles they're built with and cargo's configuration.
# Not this repository's own crates, whose versions change with every release.
# Two parts, so that a new key can start from the latest cache of the same
# toolchain: the key less its last part is that one's prefix.
#
# Usage: .github/scripts/cargo-cache-key.sh NAME (from the repo root)
set -euo pipefail

name=${1:?usage: cargo-cache-key.sh NAME}
toolchain=$(sha256sum rust-toolchain.toml | cut -c1-16)
crates=$(
    {
        # The packages of each lock that come from a registry or git, which
        # say so: this repository's, from a path, don't.
        git ls-files -z '*Cargo.lock' | xargs -0 awk '
            FNR == 1 || /^\[\[package\]\]$/ {
                if (sourced) printf "%s", package
                package = ""
                sourced = 0
            }
            /^source = / { sourced = 1 }
            { package = package $0 "\n" }
            END { if (sourced) printf "%s", package }'
        git ls-files -z '*Cargo.toml' | xargs -0 awk '
            FNR == 1 { profile = 0 }
            /^\[/ { profile = /^\[profile[].]/ }
            profile'
        git ls-files -z '*.cargo/config.toml' | xargs -0 cat
    } | sha256sum | cut -c1-16
)
echo "cargo-$name-$toolchain-$crates"
