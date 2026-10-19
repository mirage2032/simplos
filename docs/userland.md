# What's on the disk

## The first program

Once it's up, the kernel runs the first of `/sbin/init`, `/etc/init`,
`/bin/init` and `/bin/sh` that's on the disk, on the console, as Linux
does, with Linux's environment for it: `HOME=/` and `TERM=linux`. One that's
there but can't be run is said so, and passed over. Which program, and its
arguments, can be chosen as Linux's `init=` does, through QEMU's fw_cfg,
`none` meaning none:

```sh
cargo run -- -fw_cfg name=opt/simplos/init,string="/bin/probe colors"
```

When there's none, or once it ends, which on Linux would stop everything,
the kernel's own shell runs instead, and its `run /PATH ARGS` runs a program
from the disk. With the userland below on the disk, the first program is
its `/sbin/init`, a script that logs root in to zsh.

## Userland

`user/ports/userland.sh` builds the programs that make a disk a system to use:
uutils' coreutils 0.12.0, GNU's commands rewritten in Rust, one program,
`/usr/bin/coreutils`, for its 104 (`ls`, `cp`, `sort`, `du`, `dd`, `timeout`
and the rest); busybox 1.38.0 for what they aren't, with a link for each of
its applets where Linux keeps them (`/bin/sh`, `/bin/grep`, `/usr/bin/awk`,
`/bin/vi`, `/bin/ps` and so on), but those uutils has, whose links are
uutils'; zsh 5.9.2 with its shell functions; and vim 9.2.1202 with its
runtime files, its help and syntax highlighting among them, on ncurses 6.6
with the terminals it knows built in.
They're static programs for x86-64 Linux, linked with musl: the C by zig cc
0.17.0, uutils by the Rust toolchain `rust-toolchain.toml` pins, for
`x86_64-unknown-linux-musl`, as `user/probe` is; from the sources the script
pins and checks by their SHA-256, and for uutils the crates its `Cargo.lock`
pins, into `target/userland/root`. The first build takes a few minutes and
needs the network, crates.io's included; after that it's done, until the
script changes. It builds on x86-64 Linux only.

uutils' commands, being Rust's, ask more of the kernel than busybox's: hash
maps keyed by `getrandom`; threads, which `sort`, `du` and `dd` always start;
POSIX timers and `sigwait`, which `timeout` waits by; `splice`, which `cat`
copies with; a socket pair, which Rust's `Command` hears of a failed `exec`
by; and `AT_FDCWD` as rustix passes it. Its 13 MB are read from the disk
once, and each program running it after shares them, in the page cache.

Over them go the files in `user/rootfs/`, copied every time, so changing them
needs no new build:

- `/sbin/init`, which the kernel runs first: it logs root in to zsh, as
  `login` would, in `/root`;
- `/etc/passwd`, `/etc/group` and `/etc/shells`, in which zsh is root's
  shell;
- `/etc/zshrc`, for every interactive zsh: completion, with a menu;
  history, kept in `~/.zsh_history`; Emacs's keys and the console's Home,
  End and Delete; and a prompt saying who, where and in which directory.

What the completion system knows of its functions, zsh's 998 and Rust's
`_cargo`, it keeps in `~/.zcompdump`, which the build makes for root once
the disk's trees are all in (`user/ports/completions.sh`), by running the same
zsh. compinit uses it only if it counts as many functions as there are, and
otherwise makes it again as root's zsh starts, which takes seconds under
emulation, ten thousand lookups on the disk.

`cargo run`'s disk gets all of it as it's made: delete `tools/boot/disk.img`
for one that has it. Then zsh is what you type at, and once it ends, the
kernel's own shell. For now:

- busybox is built without `init`, which the kernel would run first, and
  which wants a console and an `inittab` there aren't; and without `tc`;
- uutils is built without `stdbuf`, which its musl build leaves out; and its
  `chroot`, `mknod` and `mkfifo` fail, as busybox's did, as there are no
  `chroot` and no FIFOs here;
- the disk's files belong to whoever made it, as `mke2fs -d` copies owners,
  which is why `/etc/zshrc` has `compinit -u`;
- vim is built without `timer_create`, whose `SIGEV_THREAD` takes a thread,
  so it times what it times out (a regexp, `'redrawtime'`) with `setitimer`;
  and the `flock` it puts on its temporary directory, against cleaners
  that there aren't, isn't there.

busybox is GPL-2.0, as simplos is; uutils is MIT; zsh and ncurses are
MIT-style; vim is under the Vim license, which is GPL-compatible. A disk made
with them holds their programs, built from the upstream releases the script
names (vim's and uutils', from their tags), unchanged, configured as it says.

## GCC

`user/ports/toolchain.sh` builds a compiler for the disk, so programs can be
compiled on simplos and run there: gcc and g++ 14.3.0, binutils 2.44, and
musl 1.2.6, with `cc` for gcc, all under `/usr`; and musl's loader,
`/lib/ld-musl-x86_64.so.1`, and `ldd`.

```sh
gcc -O2 -o hello hello.c && ./hello
g++ -O2 -o hello hello.cc && ./hello
gcc -O2 -fPIC -shared -o libgreet.so greet.c && gcc -o greeted greeted.c -L. -lgreet
LD_LIBRARY_PATH=. ./greeted && LD_LIBRARY_PATH=. ldd ./greeted
```

They're static programs for x86-64 Linux with musl, as the userland's are,
made by musl-cross-make from the sources the script pins and checks by their
SHA-256: first a cross compiler, with the host's gcc, then with that one the
compiler itself. What they compile is linked as on Linux: dynamically, to
musl's `libc.so` and, for C++, `libstdc++.so.6` and `libgcc_s.so.1`, which
the loader the program names loads as it starts, as `dlopen` does as it
runs; or with `-static`, not. It's position-independent unless asked
otherwise, as Alpine's is, so that the loader, run as `ldd`, can put it
anywhere. Before it's done, the script compiles C and C++ with it on the
host, which can run what it makes, both ways. The first build takes 13
minutes on 32 cores, needs the network and a few GB of room while it runs,
and leaves 163 MB in `target/toolchain/root`; after that it's done, until
the script changes. It builds on x86-64 Linux only.

`cargo run`'s disk gets it over the userland, which makes it 1 GiB, on an
AHCI controller, and `cargo run` gives the guest 1 GiB of memory unless `-m`
is given; a hello world, even in C++ with `<iostream>`, compiles in QEMU's
default 128 MiB. Under emulation, `gcc -O2` compiles a C hello world there
in 5 s the first time, and in under 1 s after that, the compiler being in
the page cache by then; `g++ -O2` an `<iostream>` one in 9 s, then 4 s, or
14 s with `-static`, which copies what it needs of `libstdc++.a` into it.
For now:

- a program is read whole the first time it starts, and that's most of
  that time: `cc1`, 36 MB, takes 6 s to read from an AHCI disk, by DMA,
  and 25 s from an IDE one, a word at a time by port I/O;
- a program linked dynamically starts a little later than one linked
  statically, as the loader finds and maps its libraries, which are in the
  page cache by then: that C++ hello world in 0.03 s rather than 0.01 s;
- there's no LTO and no sanitizers.

gcc and binutils are GPL-3.0, with GCC's runtime library exception for what's
linked into programs; musl is MIT; gmp, mpfr and mpc, which gcc is linked
with, LGPL-3.0. Like the userland, they're built from the upstream releases,
unchanged, configured as the script says.

## Rust

`user/ports/rust.sh` puts Rust on the disk too, so Rust programs can be built
on simplos and run there: rustc, rustdoc and cargo 1.99.1, with the standard
library for `x86_64-unknown-linux-musl`, all under `/usr`.

```sh
rustc -O hello.rs && ./hello
cargo new hello && cd hello && cargo run
```

They're the Rust project's own release for x86-64 Linux with musl, which
the script downloads, checks by its SHA-256 and unpacks, keeping rustc,
cargo and the standard library, stripped, and leaving out what simplos has
no use for: debuggers' scripts, rust-analyzer's server, rust-lld, the
sanitizers' runtimes and the documentation. rustc and cargo are
dynamically linked, so they run in musl's loader from GCC's tree, with its
`libgcc_s.so.1`, and rustc runs most of itself from its 230 MB
`librustc_driver`. What rustc makes it links with `cc`, GCC's, statically,
as Rust's musl target does. Before it's done, the script builds a program
with rustc and a package with cargo on the host, in that loader, and runs
them. It takes 20 s and leaves 420 MB in `target/rust/root`, the download,
300 MB, kept in `target/rust/downloads`; it unpacks on x86-64 Linux only,
after `user/ports/toolchain.sh` has built GCC.

`cargo run`'s disk gets it over the userland and GCC, which makes it 2
GiB, and rustc needs `cargo run`'s 1 GiB of memory. cargo locks what it
builds with `flock`, and keeps its records in SQLite, which locks ranges
of its files with `fcntl`, both of which simplos has as Linux does. Under
emulation, `rustc` compiles a hello world in 47 s and `cargo run` builds
and runs a new package in 66 s. For now:

- each start of rustc reads all of its library into memory, simplos
  having no way to read a file only as it's used: 18 s each time, and
  cargo starts rustc three times for a build;
- there are no proc-macro crates, nor anything downloaded: simplos has no
  network.

Rust is MIT or Apache-2.0, at the user's choice.
