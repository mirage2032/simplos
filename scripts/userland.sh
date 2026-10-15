#!/usr/bin/env bash
# Builds the programs simplos runs from its disk, as a tree for one, in
# target/userland/root: busybox, with a link for each of its applets where
# Linux keeps them (/bin/sh among them), zsh with its shell functions, and
# vim with its runtime files.
# All static, for x86-64 Linux with musl, compiled by zig cc from the sources
# pinned below, which have to match their checksums. Then over them, the
# files of simplos's own in userland/: /sbin/init, and what's in /etc.
#
# Usage: scripts/userland.sh
#
# What it built is kept: run again, it only copies userland/ again, unless
# this script has changed since. The downloads are kept in
# target/userland/downloads.
#
# It runs on x86-64 Linux, which can run what zig makes for the target as the
# builds need to (the C compiler checks, ncurses' and busybox's generators),
# with the host's make, cc, strip, curl and sha256sum, as CI has them.
#
# busybox is GPL-2.0, zsh and ncurses MIT-style, vim under the Vim license,
# which is GPL-compatible: their sources are these, unchanged, configured as
# below.
set -euo pipefail

zig_version=0.17.0
zig_sha256=1cbe9df9f27e6b78d14ccbca43b6703a404ef79ef1c463de901d7f088d4e2026
busybox_version=1.38.0
busybox_sha256=34f9ea6ff8636f2c9241153b9114eefa9e65674a45318ae1ef95bb5f31c53bb2
# Verified against Thomas E. Dickey's signature when pinned.
ncurses_version=6.6
ncurses_sha256=355b4cbbed880b0381a04c46617b7656e362585d52e9cf84a67e2009b749ff11
# Verified against the zsh release key's signature when pinned.
zsh_version=5.9.2
zsh_sha256=36fa734374b44783582cec09bcd67822e2f992c779ec1624ab5596df078d2f81
# The tag's source, as GitHub makes it an archive.
vim_version=9.2.1202
vim_sha256=9e97414b85bdcb2d28d5264c65dd730d86b9a75319b005c63da0bc130c715a2f

root=$(cd "$(dirname "${BASH_SOURCE[0]}")/.." && pwd)
work=$root/target/userland
out=$work/root
downloads=$work/downloads
build=$work/build

if [[ $(uname -sm) != "Linux x86_64" ]]; then
    echo "userland: this builds on x86-64 Linux, not $(uname -sm)" >&2
    exit 1
fi

mkdir -p "$downloads"
# One build at a time: the test runner can start several at once.
exec 9>"$work/lock"
flock 9

# userland/, over what's built: copied every time, as it's quick, so that
# changing it needs no new build. git keeps neither /tmp nor modes of
# directories.
overlay() {
    cp -a "$root/userland/." "$out/"
    mkdir -p "$out/tmp"
    chmod 700 "$out/root"
    chmod 1777 "$out/tmp"
}

stamp=$(sha256sum "${BASH_SOURCE[0]}" | cut -d' ' -f1)
if [[ -d $out && $(cat "$work/built" 2>/dev/null) == "$stamp" ]]; then
    overlay
    exit 0
fi
echo "userland: building busybox $busybox_version, zsh $zsh_version and vim $vim_version" \
    "into $out" >&2

# Download $1 as $2 unless it's there, and check it's what was pinned, $3.
fetch() {
    local file=$downloads/$2
    if [[ ! -f $file ]]; then
        echo "userland: downloading $1" >&2
        curl -fsSL --retry 3 -o "$file.part" "$1"
        mv "$file.part" "$file"
    fi
    if ! echo "$3  $file" | sha256sum -c --quiet - >/dev/null 2>&1; then
        rm -f "$file"
        echo "userland: $2 isn't what was pinned, so it's deleted: try again" >&2
        exit 1
    fi
}

fetch "https://ziglang.org/download/$zig_version/zig-x86_64-linux-$zig_version.tar.xz" \
    "zig-$zig_version.tar.xz" "$zig_sha256"
fetch "https://busybox.net/downloads/busybox-$busybox_version.tar.bz2" \
    "busybox-$busybox_version.tar.bz2" "$busybox_sha256"
fetch "https://invisible-island.net/archives/ncurses/ncurses-$ncurses_version.tar.gz" \
    "ncurses-$ncurses_version.tar.gz" "$ncurses_sha256"
fetch "https://www.zsh.org/pub/zsh-$zsh_version.tar.xz" "zsh-$zsh_version.tar.xz" "$zsh_sha256"
fetch "https://github.com/vim/vim/archive/refs/tags/v$vim_version.tar.gz" \
    "vim-$vim_version.tar.gz" "$vim_sha256"

# Every build afresh, from the sources as they came.
rm -rf "$build" "$out" "$work/built"
mkdir -p "$build" "$out"
for archive in "$downloads"/*.tar.*; do
    tar -xf "$archive" -C "$build"
done
zig=$build/zig-x86_64-linux-$zig_version/zig
export ZIG_GLOBAL_CACHE_DIR=$build/zig-cache ZIG_LOCAL_CACHE_DIR=$build/zig-cache
cc="$zig cc -target x86_64-linux-musl"
jobs=$(nproc)
log=$work/build.log
: >"$log"
# Run a build step, its output in the log; the end of it shown if it fails.
step() {
    if ! "$@" >>"$log" 2>&1; then
        tail -30 "$log" >&2
        echo "userland: failed: $*; all of it is in $log" >&2
        exit 1
    fi
}

echo "userland: busybox" >&2
cd "$build/busybox-$busybox_version"
step make defconfig
# Static. Not tc, which needs kernel headers musl's don't have; not init,
# which the kernel would run first, before /bin/sh, and which wants a console
# and an inittab there aren't.
sed -i -e 's/^# CONFIG_STATIC is not set$/CONFIG_STATIC=y/' \
    -e 's/^CONFIG_TC=y$/# CONFIG_TC is not set/' \
    -e 's/^CONFIG_INIT=y$/# CONFIG_INIT is not set/' \
    -e 's/^CONFIG_LINUXRC=y$/# CONFIG_LINUXRC is not set/' .config
step make oldconfig </dev/null
grep -q '^CONFIG_STATIC=y$' .config && ! grep -qE '^CONFIG_(TC|INIT|LINUXRC)=y$' .config
step make -j"$jobs" CC="$cc" HOSTCC=cc busybox
step make CC="$cc" HOSTCC=cc CONFIG_PREFIX="$out" install

echo "userland: ncurses, for zsh and vim" >&2
ncurses=$build/ncurses
cd "$build/ncurses-$ncurses_version"
# No terminfo files to read: the terminals it knows are built in.
step ./configure --prefix="$ncurses" CC="$cc" --without-shared --with-normal \
    --without-debug --without-ada --without-cxx --without-cxx-binding --without-manpages \
    --without-progs --without-tests --disable-database \
    --with-fallbacks=linux,vt100,xterm,xterm-256color
step make -j"$jobs"
step make install

echo "userland: zsh" >&2
cd "$build/zsh-$zsh_version"
step ./configure --prefix=/usr --bindir=/bin CC="$cc" \
    CPPFLAGS="-I$ncurses/include -I$ncurses/include/ncursesw" \
    LDFLAGS="-static -s -L$ncurses/lib" \
    --disable-dynamic --enable-multibyte --with-term-lib=ncursesw \
    --disable-gdbm --disable-pcre --disable-cap
step make -j"$jobs"
install -m 755 Src/zsh "$out/bin/zsh"
step make DESTDIR="$out" install.fns
# compinit's record of what the completion functions complete, which it
# checks against the functions there are and uses if it's right: made here
# by the same zsh, as simplos writes it a line at a time, for most of a
# minute, the first time root's zsh starts.
mkdir -p "$out/root"
step "$out/bin/zsh" -f -c "fpath=($out/usr/share/zsh/site-functions \
    $out/usr/share/zsh/$zsh_version/functions)
    autoload -Uz compinit && compinit -u -d $out/root/.zcompdump"

echo "userland: vim" >&2
cd "$build/vim-$vim_version"
# Without timer_create, whose SIGEV_THREAD needs threads, which simplos
# hasn't got: vim times regexps and 'redrawtime' out with setitimer then.
# And without pkg-config, which would offer the host's libraries.
step ./configure --prefix=/usr CC="$cc" PKG_CONFIG=false \
    CPPFLAGS="-I$ncurses/include -I$ncurses/include/ncursesw" \
    LDFLAGS="-static -s -L$ncurses/lib" \
    --with-features=normal --with-tlib=ncursesw --enable-multibyte \
    --enable-gui=no --without-x --enable-gpm=no --disable-sysmouse --disable-nls \
    --disable-netbeans --disable-channel --disable-libsodium --disable-canberra \
    --disable-selinux --disable-smack --disable-acl --disable-xattr --without-wayland \
    --with-compiledby=simplos vim_cv_timer_create=no vim_cv_timer_create_with_lrt=no
step make -j"$jobs"
step make DESTDIR="$out" install

overlay
# The sources and objects, half a gigabyte, aren't needed again.
rm -rf "$build"
echo "$stamp" >"$work/built"
echo "userland: done, in $out" >&2
