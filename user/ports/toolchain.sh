#!/usr/bin/env bash
# Builds the compiler simplos runs from its disk, as a tree for one, in
# target/toolchain/root: gcc and g++ 14.3.0, binutils 2.44 and musl 1.2.6,
# installed under /usr. They're static programs for x86-64 Linux with musl,
# made by musl-cross-make from the sources pinned below, which have to match
# their checksums: first a cross compiler, with the host's, then with that
# one the compiler itself, built to run where it compiles. What it makes is
# linked as on Linux: dynamically, to musl's libc.so and, for C++,
# libstdc++.so and libgcc_s.so, which musl's loader,
# /lib/ld-musl-x86_64.so.1, loads; or with `-static`, not.
#
# Usage: user/ports/toolchain.sh
#
# What it built is kept until this script changes, as it takes a while:
# a quarter of an hour on 32 cores, most of an hour on CI's 4. The downloads
# are kept in target/toolchain/downloads.
#
# It runs on x86-64 Linux, which can run what it makes, as it checks the
# compiler by compiling with it there; with the host's gcc, g++, make,
# curl and sha256sum.
#
# gcc and binutils are GPL-3.0, with GCC's runtime library exception for
# what's linked into programs; musl is MIT; gmp, mpfr and mpc LGPL-3.0.
set -euo pipefail

# musl-cross-make's master as of 2026-06-16, whose hashes/ name the sources
# below by their SHA-1, which they match.
mcm_commit=227df8b99103f9c59f6570babf892978e293082f
mcm_sha256=bb3fc7851088e1e5e1274ee56a0ab6ae176043d160fdf0b71027934b091f208a
gcc_version=14.3.0
gcc_sha256=e0dc77297625631ac8e50fa92fffefe899a4eb702592da5c32ef04e2293aca3a
binutils_version=2.44
binutils_sha256=0cdd76777a0dfd3dd3a63f215f030208ddb91c2361d2bcc02acec0f1c16b6a2e
musl_version=1.2.6
musl_sha256=d585fd3b613c66151fc3249e8ed44f77020cb5e6c1e635a616d3f9f82460512a
gmp_version=6.3.0
gmp_sha256=a3c2b80201b89e68616f4ad30bc66aee4927c3ce50e33929ca819d5c43538898
mpc_version=1.3.1
mpc_sha256=ab642492f5cf882b74aa0cb730cd410a81edcdbec895183ce930e706c1c759b8
mpfr_version=4.2.2
mpfr_sha256=b67ba0383ef7e8a8563734e2e889ef5ec3c3b898a01d00fa0a6869ad81c6ce01
linux_headers_version=4.19.88-2
linux_headers_sha256=dc7abf734487553644258a3822cfd429d74656749e309f2b25f09f4282e05588

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/../.." && pwd)
work=$root/target/toolchain
out=$work/root
downloads=$work/downloads
build=$work/build

if [[ $(uname -sm) != "Linux x86_64" ]]; then
    echo "toolchain: this builds on x86-64 Linux, not $(uname -sm)" >&2
    exit 1
fi

mkdir -p "$downloads"
# One build at a time: the test runner can start several at once.
exec 9>"$work/lock"
flock 9

stamp=$(sha256sum "${BASH_SOURCE[0]}" | cut -d' ' -f1)
if [[ -d $out && $(cat "$work/built" 2>/dev/null) == "$stamp" ]]; then
    exit 0
fi
echo "toolchain: building gcc $gcc_version into $out, which takes a while" >&2

# Download $1 as $2 unless it's there, and check it's what was pinned, $3.
fetch() {
    local file=$downloads/$2
    if [[ ! -f $file ]]; then
        echo "toolchain: downloading $1" >&2
        curl -fsSL --retry 3 -o "$file.part" "$1"
        mv "$file.part" "$file"
    fi
    if ! echo "$3  $file" | sha256sum -c --quiet - >/dev/null 2>&1; then
        rm -f "$file"
        echo "toolchain: $2 isn't what was pinned, so it's deleted: try again" >&2
        exit 1
    fi
}

gnu=https://ftp.gnu.org/gnu
fetch "https://github.com/richfelker/musl-cross-make/archive/$mcm_commit.tar.gz" \
    "musl-cross-make-$mcm_commit.tar.gz" "$mcm_sha256"
fetch "$gnu/gcc/gcc-$gcc_version/gcc-$gcc_version.tar.xz" "gcc-$gcc_version.tar.xz" "$gcc_sha256"
fetch "$gnu/binutils/binutils-$binutils_version.tar.gz" "binutils-$binutils_version.tar.gz" \
    "$binutils_sha256"
fetch "https://musl.libc.org/releases/musl-$musl_version.tar.gz" "musl-$musl_version.tar.gz" \
    "$musl_sha256"
fetch "$gnu/gmp/gmp-$gmp_version.tar.xz" "gmp-$gmp_version.tar.xz" "$gmp_sha256"
fetch "$gnu/mpc/mpc-$mpc_version.tar.gz" "mpc-$mpc_version.tar.gz" "$mpc_sha256"
fetch "$gnu/mpfr/mpfr-$mpfr_version.tar.xz" "mpfr-$mpfr_version.tar.xz" "$mpfr_sha256"
fetch "https://ftp.barfooze.de/pub/sabotage/tarballs/linux-headers-$linux_headers_version.tar.xz" \
    "linux-headers-$linux_headers_version.tar.xz" "$linux_headers_sha256"

# Every build afresh, from the sources as they came.
rm -rf "$build" "$out" "$work/built"
mkdir -p "$build" "$out"
tar -xzf "$downloads/musl-cross-make-$mcm_commit.tar.gz" -C "$build"
cd "$build/musl-cross-make-$mcm_commit"
jobs=$(nproc)
log=$work/build.log
: >"$log"
# Run a build step, its output in the log; the end of it shown if it fails.
step() {
    if ! "$@" >>"$log" 2>&1; then
        tail -40 "$log" >&2
        echo "toolchain: failed: $*; all of it is in $log" >&2
        exit 1
    fi
}

# The sources from downloads/, which musl-cross-make takes as they are and
# downloads nothing into. Stage 1 is compiled by the host's compiler, told
# -std=gnu17 and -std=gnu++17: its C23 gmp's configure can't take, nor its
# C++20 GCC's own libcody, which then adds -std=c++11 after them. Stage 2,
# NATIVE, compiles the compiler with the cross compiler stage 1 made,
# static, and looks at musl's headers, those stage 1 has, for what the C
# library has: not the host's, glibc's, which would have it emit what only
# glibc has. Its root is where it's installed, as it's moved there: /usr,
# from /usr/bin, so its headers are in the root's /include. What it compiles
# is position-independent unless asked otherwise, as Alpine's is, so that
# musl's loader, run as `ldd`, can map any program where it likes: simplos's
# mmap takes no address as a hint.
cat >config.mak <<EOF
TARGET = x86_64-linux-musl
SOURCES = $downloads
GCC_VER = $gcc_version
BINUTILS_VER = $binutils_version
MUSL_VER = $musl_version
GMP_VER = $gmp_version
MPC_VER = $mpc_version
MPFR_VER = $mpfr_version
LINUX_VER = headers-$linux_headers_version
COMMON_CONFIG += --disable-nls CFLAGS="-g0 -O2" CXXFLAGS="-g0 -O2" LDFLAGS="-s"
GCC_CONFIG += --disable-lto --disable-libsanitizer --enable-default-pie
ifeq (\$(NATIVE),)
COMMON_CONFIG += CC="gcc -std=gnu17" CXX="g++ -std=gnu++17"
else
COMMON_CONFIG += CC="x86_64-linux-musl-gcc -static --static" CXX="x86_64-linux-musl-g++ -static --static"
GCC_CONFIG += --with-build-sysroot=$build/cross/x86_64-linux-musl --with-native-system-header-dir=/include
endif
EOF

echo "toolchain: stage 1, a cross compiler" >&2
step make -j"$jobs" OUTPUT="$build/cross" install
# What it was built from, gigabytes, isn't needed again: room on CI's disk.
rm -rf build/local
echo "toolchain: stage 2, the compiler for simplos" >&2
PATH=$build/cross/bin:$PATH step make -j"$jobs" NATIVE=1 OUTPUT="$build/native" install

# Under /usr: gcc finds what it runs and includes from where it is.
mkdir -p "$out/usr"
cp -a "$build/native/." "$out/usr/"
rm -rf "$out/usr/share/man" "$out/usr/share/info"
ln -sf gcc "$out/usr/bin/cc"
# The loader, where programs name it (PT_INTERP): musl's libc.so, which is
# it. musl links it there to /lib/libc.so, where it went before the move.
mkdir -p "$out/lib"
rm -f "$out/usr/lib/ld-musl-x86_64.so.1"
ln -s ../usr/lib/libc.so "$out/lib/ld-musl-x86_64.so.1"
# Run as ldd, it lists what a program loads.
ln -s ../../lib/ld-musl-x86_64.so.1 "$out/usr/bin/ldd"

# It compiles, links and runs, here, which can run what it makes, from
# where it is in the tree, as it will from /usr/bin: static, and dynamic,
# run by the loader it built, which finds the libraries it built first.
echo "toolchain: checking it" >&2
check=$build/check
mkdir -p "$check"
printf '#include <stdio.h>\nint main(void) { puts("hello from gcc"); }\n' >"$check/hello.c"
printf '#include <iostream>\n#include <vector>\nint main() { std::vector<int> v{1, 2, 3};\n%s\n}\n' \
    'std::cout << "hello from g++ " << v.size() << "\n";' >"$check/hello.cc"
for how in static dynamic; do
    flags=$([[ $how == static ]] && echo -static || true)
    step "$out/usr/bin/gcc" $flags -O2 -o "$check/hello-$how" "$check/hello.c"
    step "$out/usr/bin/g++" $flags -O2 -o "$check/hello++-$how" "$check/hello.cc"
done
loaded() { LD_LIBRARY_PATH=$out/usr/lib "$out/lib/ld-musl-x86_64.so.1" "$@"; }
[[ $("$check/hello-static") == "hello from gcc" && $("$check/hello++-static") == "hello from g++ 3" &&
    $(loaded "$check/hello-dynamic") == "hello from gcc" &&
    $(loaded "$check/hello++-dynamic") == "hello from g++ 3" ]] || {
    echo "toolchain: what it compiled didn't run as it should" >&2
    exit 1
}
# Static PIEs, as -static makes them by default; the compiler, not PIE.
static=$(file "$check"/hello*-static "$out/usr/bin/gcc" | grep -vcE "static(ally|-pie) linked" || true)
dynamic=$(file "$check"/hello*-dynamic | grep -c "interpreter /lib/ld-musl-x86_64.so.1" || true)
[[ $static == 0 && $dynamic == 2 ]] || {
    echo "toolchain: what it compiled isn't linked as asked" >&2
    exit 1
}

# The sources and objects, gigabytes, aren't needed again.
rm -rf "$build"
echo "$stamp" >"$work/built"
echo "toolchain: done, in $out" >&2
