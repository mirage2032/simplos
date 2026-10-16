#!/usr/bin/env bash
# Puts Rust on simplos's disk, as a tree for one, in target/rust/root:
# rustc, rustdoc and cargo 1.99.1, as the Rust project builds them for
# x86-64 Linux with musl, and the standard library for the same target,
# under /usr. They're dynamically linked: they run in musl's loader,
# /lib/ld-musl-x86_64.so.1, with libgcc_s.so.1, both of which
# scripts/toolchain.sh's tree has. What rustc makes it links with cc, that
# tree's gcc, and is static, as Rust's musl target links by default.
#
# Usage: scripts/rust.sh
#
# What it unpacked is kept until this script changes. The download, 300 MB,
# is kept in target/rust/downloads.
#
# It runs on x86-64 Linux, which can run what it unpacks, as it checks
# rustc and cargo by building with them there, in musl's loader from
# scripts/toolchain.sh's tree, which it builds first if it isn't yet; with
# curl, tar, xz, sha256sum and strip.
#
# Rust is MIT or Apache-2.0.
set -euo pipefail

version=1.99.1
target=x86_64-unknown-linux-musl
sha256=da398394254c8ffd29d1e397dfc00941dfb6a631bc7254cf797481fec8df6f5a

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$root/target/rust
out=$work/root
downloads=$work/downloads
build=$work/build
toolchain=$root/target/toolchain/root

if [[ $(uname -sm) != "Linux x86_64" ]]; then
    echo "rust: this unpacks on x86-64 Linux, not $(uname -sm)" >&2
    exit 1
fi

mkdir -p "$downloads"
# One at a time: the test runner can start several at once.
exec 9>"$work/lock"
flock 9

stamp=$(sha256sum "${BASH_SOURCE[0]}" | cut -d' ' -f1)
if [[ -d $out && $(cat "$work/built" 2>/dev/null) == "$stamp" ]]; then
    exit 0
fi
# The loader, libgcc_s and the linker it's checked with.
"$root/scripts/toolchain.sh"
echo "rust: unpacking rust $version into $out" >&2

archive=rust-$version-$target.tar.xz
file=$downloads/$archive
if [[ ! -f $file ]]; then
    echo "rust: downloading $archive" >&2
    curl -fsSL --retry 3 -o "$file.part" "https://static.rust-lang.org/dist/$archive"
    mv "$file.part" "$file"
fi
if ! echo "$sha256  $file" | sha256sum -c --quiet - >/dev/null 2>&1; then
    rm -f "$file"
    echo "rust: $archive isn't what was pinned, so it's deleted: try again" >&2
    exit 1
fi

rm -rf "$build" "$out" "$work/built"
mkdir -p "$build" "$out"
tar -xJf "$file" -C "$build"
"$build/rust-$version-$target/install.sh" --prefix=/usr --destdir="$out" \
    --components="rustc,cargo,rust-std-$target" --disable-ldconfig >/dev/null

# What simplos has no use for: debuggers' scripts, rust-analyzer's server,
# linkers rustc runs only when asked (rust-lld and the wasm ones), the
# sanitizers' runtimes, which only nightly uses, and the documentation.
usr=$out/usr
rm -rf "$usr"/bin/rust-gdb* "$usr/bin/rust-lldb" "$usr/libexec" "$usr/share/man" \
    "$usr/share/doc" "$usr/etc" "$usr/lib/rustlib/$target/bin" \
    "$usr"/lib/rustlib/$target/lib/librustc-*_rt.*.a
# Without their debugging information, a third less to read each time
# they run: 270 MB for rustc's, rather than 350.
strip "$usr/bin/rustc" "$usr/bin/rustdoc" "$usr/bin/cargo" "$usr"/lib/librustc_driver-*.so

# They build and run, here, as they will from /usr/bin: rustc alone, and
# cargo, which runs rustc, each in the loader, with the libraries next to
# it and toolchain.sh's; what they make is linked by its gcc.
echo "rust: checking it" >&2
check=$build/check
mkdir -p "$check/project/src"
loader=("$toolchain/lib/ld-musl-x86_64.so.1" --library-path "$usr/lib:$toolchain/usr/lib")
printf '#!/bin/sh\nexec %s "$@"\n' "${loader[*]} $usr/bin/rustc --sysroot $usr" >"$check/rustc"
chmod +x "$check/rustc"
printf 'fn main() {\n    println!("hello from rustc");\n}\n' >"$check/hello.rs"
printf '[package]\nname = "hello"\nedition = "2024"\n' >"$check/project/Cargo.toml"
printf 'fn main() {\n    println!("hello from cargo");\n}\n' >"$check/project/src/main.rs"
export PATH=$toolchain/usr/bin:$PATH
"$check/rustc" -O -o "$check/hello" "$check/hello.rs"
CARGO_HOME=$check/home RUSTC=$check/rustc "${loader[@]}" "$usr/bin/cargo" build -q --release \
    --offline --manifest-path "$check/project/Cargo.toml"
[[ $("$check/hello") == "hello from rustc" &&
    $("$check/project/target/release/hello") == "hello from cargo" ]] || {
    echo "rust: what it built didn't run as it should" >&2
    exit 1
}
static=$(file "$check/hello" "$check/project/target/release/hello" | grep -cE "static(ally|-pie) linked")
[[ $static == 2 ]] || {
    echo "rust: what it built isn't static, as musl's target links it" >&2
    exit 1
}

rm -rf "$build"
echo "$stamp" >"$work/built"
echo "rust: done, in $out" >&2
