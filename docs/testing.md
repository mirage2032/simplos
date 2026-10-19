# Tests

```sh
cd kernel
cargo test
```

Every test is a kernel. They are built for the bare-metal target with no `main`
and no libtest, so `tools/boot-test` stands in as cargo's test runner: it wraps
each test binary in a bootable image, starts QEMU, and turns how the guest shut
itself down back into a pass or a fail. The guest's side of that is QEMU's
`isa-debug-exit` device — writing to its port makes QEMU exit with a status
derived from the value written. Every test also gets two blank scratch disks,
1 MiB on IDE and 2 MiB on AHCI, which `tests/disk.rs` writes to, and a 16 MiB
ext4 disk made by e2fsprogs' `mke2fs`, which `tests/ext4.rs` writes to, and
`tests/files.rs` has the user program read and write through system calls. It holds
the user program as `/bin/probe`, which `tests/processes.rs` has it start
copies of and run again; `tests/tty.rs` types into a terminal for it to read;
`tests/signals.rs` has it send signals to itself and its copies;
`tests/time.rs` has it read the clocks, count CPU time and set alarms.
`tests/shells.rs` runs busybox's sh and zsh, as commands and typed at on a
terminal: its disk is 128 MiB and holds the userland too, which the runner
builds with `user/ports/userland.sh` the first time. `tests/toolchain.rs`
compiles C and C++ with gcc and g++, linked dynamically and statically, a
shared library and a program that loads one with `dlopen`, and runs what
they made: its disk is
1 GiB, with GCC from `user/ports/toolchain.sh` as well, and it gets as long as
compiling takes. `tests/rust.rs` builds a program with rustc and a package
with cargo, and runs them: its disk is 2 GiB, with Rust from
`user/ports/rust.sh` too, and it gets 1 GiB of memory, and half an hour. After a run that passes, e2fsck must find nothing wrong with it, and it must have been
unmounted, so the tests need e2fsprogs installed.

Each test boots twice, under BIOS and UEFI at once, and passes only if it
passes both times; what each run prints is shown in turn. The runner tells the
guest which firmware it started, and `tests/basic_boot.rs` checks that the
kernel saw the same one. `SIMPLOS_FIRMWARE=bios cargo test` (or `uefi`) boots
just the one.

A test still running after 30 s is taken to hang: it fails there and then,
named, rather than leaving its kernel to the runner's limit of 180 s. A test
that knows it takes longer, as compiling does, says so with
`simplos::take_longer`, and the toolchain kernel's limit is 900 s. Test
kernels are built a little optimised and without debug information
(`[profile.test]` in `kernel/Cargo.toml`): with it each was 15 MB, which the
BIOS bootloader takes 18 s to read through emulated disk I/O, and unoptimised
copying made the slowest test take 18 s. To step through a test kernel in a
debugger, take `debug` and `strip` out of it.

`scripts/mutants.sh` checks that the tests notice what they should. Give it
patches, as `git diff` prints them, each a change to the kernel that some test
ought to fail for, and it reports any change that no test fails for:

```sh
scripts/mutants.sh no-sigpipe.patch rcx-lost.patch
```

A patch may start with a line naming the test kernels to boot for it, such as
`Test: signals tty`, so that the others don't. Each change is built in a
worktree of its own under `target/mutants`, kept between runs so each build
takes seconds, and booted under BIOS only, many at a time: a change that hangs
a test leaves its QEMU idle until the 30 s are up.

`scripts/smoke-test.sh` is the coarser check, and the one that catches the most:
it boots the real images both ways and asserts the kernel's boot banner names
the right firmware and the right framebuffer mode.
