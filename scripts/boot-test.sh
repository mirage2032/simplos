#!/usr/bin/env bash
# `cargo test`'s runner for the kernel (see kernel/.cargo/config.toml).
#
# This exists only to get out of the kernel's directory before building a host
# tool. Cargo reads .cargo/config.toml from the *current* directory, not from
# the manifest's, so invoking `cargo run --manifest-path tools/boot-test/...`
# from kernel/ would hand the host build the kernel's `build-std` settings and
# bare-metal target. Entering the tool's own directory gives it its own config.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)

# The test binary's path is relative to where cargo invoked us, which is about
# to stop being the current directory.
args=()
for arg in "$@"; do
    if [[ -e $arg ]]; then
        args+=("$(realpath "$arg")")
    else
        args+=("$arg")
    fi
done

cd "$root/tools/boot-test"
exec cargo run --quiet --release -- "${args[@]}"
