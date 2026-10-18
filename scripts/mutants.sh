#!/usr/bin/env bash
# Checks that the kernel's tests notice what they ought to: each patch given is
# applied to a copy of the code, the test kernels it names are built and
# booted, and a change none of them fails is reported as surviving.
#
# Usage: scripts/mutants.sh [-j JOBS] PATCH...
#
# A PATCH is what `git diff` prints for one change to the kernel, after a line
# naming the test kernels that should fail for it, as cargo names them (`lib`
# for the library's own tests); without that line, all of them boot:
#
#     Test: signals tty
#     diff --git a/kernel/src/signal.rs b/kernel/src/signal.rs
#     ...
#
# The changes are made to the code as it is on disk, committed or not, in a
# worktree of its own (target/mutants/tree) that's kept between runs, so that
# only the first build is a long one. The tests as they are boot first, as a
# change can't be caught by tests that fail anyway. Then each change is built
# in turn, and up to JOBS of them boot at once: under BIOS only, which halves
# the time; the full suite boots both. A test that hangs fails after the test
# runner's limit of 30 s. JOBS is twice the CPUs unless given.
set -euo pipefail

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
self=$root/scripts/mutants.sh
work=$root/target/mutants
tree=$work/tree
out=$work/out
boot_test=$root/tools/target/release/boot-test
export SIMPLOS_FIRMWARE=${SIMPLOS_FIRMWARE:-bios}

# Boot the test kernels in directory $2 until one fails, and say how that went:
# on stdout and in its `result`.
if [[ ${1-} == --boot ]]; then
    dir=$2
    name=$(basename "$dir")
    result=survived
    for kernel in "$dir"/kernels/*; do
        if ! "$boot_test" "$kernel" >>"$dir/log" 2>&1; then
            # The test that failed, the last named before what the kernel
            # said in between; or else what boot-test said went wrong.
            why=$(awk '/\.\.\.\t/ { test = $0; sub(/\.\.\..*/, "", test) }
                /\[failed\]/ { print test; exit }' "$dir/log")
            if grep -q 'Error: not done after' "$dir/log"; then
                why="$why (hung)"
            fi
            if [[ -z $why ]]; then
                why=$(grep '^boot-test: .*): ' "$dir/log" | tail -1 | sed 's/.*): //' || true)
            fi
            result="caught $why"
            break
        fi
    done
    echo "$result" >"$dir/result"
    if [[ $result == survived ]]; then
        echo "SURVIVED $name"
    else
        echo "caught   $name: ${result#caught }"
    fi
    exit 0
fi

# Twice the CPUs: a change that makes a test hang leaves its QEMU idle.
jobs=$((2 * $(getconf _NPROCESSORS_ONLN)))
while (($#)); do
    case $1 in
    -j)
        jobs=${2:?-j needs a number}
        shift 2
        ;;
    -*)
        echo "unknown option: $1" >&2
        exit 2
        ;;
    *) break ;;
    esac
done
if ((!$#)); then
    echo "usage: scripts/mutants.sh [-j JOBS] PATCH..." >&2
    exit 2
fi
((jobs >= 1)) || jobs=1
patches=()
for patch in "$@"; do
    if [[ ! -f $patch ]]; then
        echo "no such patch: $patch" >&2
        exit 2
    fi
    patches+=("$(cd "$(dirname "$patch")" && pwd)/$(basename "$patch")")
done

# The test kernels a patch names; nothing for all of them.
tests_of() {
    sed -n 's/^Test: *//p' "$1" | head -1
}

# Build the test kernels named in the worktree (all of them, for none), with
# cargo's output in $1, and copy them into $2.
build() {
    local log=$1 into=$2 args=() name
    shift 2
    mkdir -p "$into"
    for name in "$@"; do
        case $name in
        lib) args+=(--lib) ;;
        *) args+=(--test "$name") ;;
        esac
    done
    (cd "$tree/kernel" && cargo test --no-run ${args[@]+"${args[@]}"}) >"$log" 2>&1 || return 1
    sed -n 's/^ *Executable .*(\(.*\))$/\1/p' "$log" | while read -r kernel; do
        cp "$tree/kernel/$kernel" "$into/"
    done
}

# Put the worktree back as the code was before a patch.
restore() {
    git -C "$tree" checkout --quiet --force -- .
    git -C "$tree" clean -fdq
}

mkdir -p "$work"
rm -rf "$out"
mkdir -p "$out"

# The code as it is on disk, tracked or not but for what git ignores, as a
# commit on no branch for the worktree to check out.
index=$work/index
rm -f "$index"
GIT_INDEX_FILE=$index git -C "$root" add -A
code=$(git -C "$root" -c user.name=mutants -c user.email=mutants commit-tree -p HEAD \
    -m "as on disk" "$(GIT_INDEX_FILE=$index git -C "$root" write-tree)")
rm -f "$index"
if [[ -e $tree/.git ]]; then
    git -C "$tree" checkout --quiet --force --detach "$code"
    restore
else
    git -C "$root" worktree prune
    git -C "$root" worktree add --quiet --detach "$tree" "$code"
fi

(cd "$root/tools/boot-test" && cargo build --release --quiet)

# Every test kernel the patches name, or all of them if one names none.
names=()
for patch in "${patches[@]}"; do
    named=$(tests_of "$patch")
    if [[ -z $named ]]; then
        names=()
        break
    fi
    # shellcheck disable=SC2206 # the names are words
    names+=($named)
done
# Each once: cargo takes `--lib` only once.
# shellcheck disable=SC2207 # the names are words
names=($(printf '%s\n' ${names[@]+"${names[@]}"} | sort -u))
echo "the tests as they are..."
if ! build "$out/as-is/log" "$out/as-is/kernels" ${names[@]+"${names[@]}"}; then
    echo "they don't build: see $out/as-is/log" >&2
    exit 1
fi
"$self" --boot "$out/as-is" >/dev/null
if [[ $(cat "$out/as-is/result") != survived ]]; then
    echo "they fail: see $out/as-is/log" >&2
    exit 1
fi

echo "${#patches[@]} changes, $jobs at a time..."
for patch in "${patches[@]}"; do
    dir=$out/$(basename "$patch" .patch)
    mkdir -p "$dir"
    if ! git -C "$tree" apply "$patch" 2>"$dir/log"; then
        echo "invalid: doesn't apply" >"$dir/result"
        echo "NO APPLY $(basename "$dir")" >&2
        continue
    fi
    # shellcheck disable=SC2046 # the names are words
    if build "$dir/log" "$dir/kernels" $(tests_of "$patch"); then
        echo "$dir"
    else
        echo "invalid: doesn't build" >"$dir/result"
        echo "NO BUILD $(basename "$dir")" >&2
    fi
    restore
done | xargs -P "$jobs" -I {} "$self" --boot {}

caught=0 survived=0 invalid=0
for patch in "${patches[@]}"; do
    case $(cat "$out/$(basename "$patch" .patch)/result") in
    caught*) caught=$((caught + 1)) ;;
    survived) survived=$((survived + 1)) ;;
    *) invalid=$((invalid + 1)) ;;
    esac
done
echo "$caught caught, $survived survived, $invalid not run; logs in $out"
((survived == 0 && invalid == 0))
