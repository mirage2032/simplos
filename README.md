# simplos

A small x86_64 operating system written in Rust, from the boot sector up. It
brings up long mode, a GDT and IDT, paging and a heap, a PS/2 keyboard, the
HPET, and drops into a small command line (type `help`) drawn into a linear
framebuffer, from which `run` starts an unmodified Linux x86-64 program in user
mode. It boots under both BIOS and UEFI firmware, on real hardware or an
emulator — including [in a browser tab](https://lazyscript.com/projects/simplos), where QEMU itself
is compiled to WebAssembly.

Nothing here is `std`. The kernel is `#![no_std]`, built for
`x86_64-unknown-none` with a rebuilt `core`, and the toolchain is a pinned
nightly (`rust-toolchain.toml`) because `build-std`, artifact dependencies and
`abi_x86_interrupt` are all still unstable.

## Running it

```sh
cd fullkernel
cargo run              # UEFI
cargo run -- --bios    # BIOS
cargo run -- --debug   # wait for a debugger on :1234 before the first instruction
```

`fullkernel` is not part of the OS: it is a host program whose build script
turns the kernel into two bootable disk images and whose `main` starts QEMU on
one of them. It uses KVM when `/dev/kvm` is there and falls back to software
emulation when it isn't, so it works on a machine or a CI runner either way.
Any other argument is passed straight through to QEMU.

A release's images need none of this — download `bios.img` or `uefi.img` from
[the latest release](../../releases/latest) and boot it.

## Resolution

Both images come up at **1920x1080** and the kernel will narrow that on request:

```sh
cargo run -- -fw_cfg name=opt/simplos/fb,string=1024x768
```

Which is worth explaining, because the two firmware paths get there very
differently. The mode is chosen *before* the kernel exists — UEFI through the
GOP, BIOS through a VESA call in the bootloader's 16-bit real-mode second stage —
and neither can be told anything at runtime. So the built-in mode is a build-time
constant, `SIMPLOS_FB_WIDTH` / `SIMPLOS_FB_HEIGHT`, read by both `kernel/build.rs`
and the vendored bootloader's `bios/stage-2/build.rs` so the UEFI minimum and the
BIOS cap cannot drift apart.

The runtime request is a different mechanism: the kernel reads
`opt/simplos/fb` from QEMU's fw_cfg device (`kernel/src/io/fwcfg.rs`) and sets
the mode itself through the Bochs VBE registers (`kernel/src/io/video/vbe.rs`).

**It can only make the framebuffer smaller.** The bootloader maps exactly
`FrameBufferInfo::byte_len` bytes of video memory and nothing later maps more, so
a larger mode would write past the end of the mapping. A request that doesn't
fit is refused and the firmware's mode kept — which is also what happens on real
hardware, where those registers don't exist.

One 1920x1080 image therefore serves everyone, the browser build included.

## Disks

At boot the kernel finds every hard disk on the legacy IDE channels (ATA PIO)
and on any AHCI (SATA) controller on PCI, logs one `simplos: disk N: ...` line
for each over serial, and shows the count on screen. Kernel code reads and
writes raw 512-byte sectors with `io::disk::{list, read, write}`. Both drivers
poll, and neither knows which disk it booted from, so `write` will happily
overwrite the boot image if told to.

Every `-drive` on QEMU's default machine is an IDE disk. For an AHCI one:

```sh
cargo run -- -device ich9-ahci,id=ahci -drive id=d,file=disk.img,format=raw,if=none -device ide-hd,drive=d,bus=ahci.0
```

NVMe isn't supported, and on many real PCs that is where the disk is.

## ext4

The kernel mounts the first disk holding an ext4 (or ext3, or ext2) file
system to read and write, replaying its journal first if it wasn't unmounted
cleanly, as the root of every path; with no such disk, the root is an empty
directory that can't be written. Either way, `/dev` holds `null`, `zero` and
`tty`, mounted over whatever the disk has there, as on Linux. The shell's `ls`
and `cat` show what's there.

`cargo run` attaches `fullkernel/disk.img`, which it makes the first time
with e2fsprogs' `mke2fs` and keeps, so what's written to it is there next
time; delete it to start afresh. To use a disk of your own, make one from a
directory and attach it, and the kernel finds it first:

```sh
mkdir -p tree && echo hello > tree/hello.txt
/usr/sbin/mke2fs -t ext4 -d tree disk.img 64M
cargo run -- -drive format=raw,file=disk.img
```

Nothing unmounts it when QEMU is closed, which to the disk is a power cut:
the next boot replays the journal, as Linux would. The work is done by
`fs/ext4` (see below), behind a lock that sleeps whoever waits for it
(`thread::SleepLock`), since a call can wait on the disk for a long time.
The disk is trusted: lwext4 is C, parsing it in ring 0, and isn't fuzzed yet.

Programs use it through Linux's system calls, so Rust's `std::fs` (or C's
stdio) works as it does on Linux: `open`, `openat`, `close`, `read`, `write`,
`pread64`, `pwrite64`, `lseek`, `stat`, `lstat`, `fstat`, `newfstatat`,
`getdents64`, `fcntl`, `fsync`, `mkdir`, `mkdirat`, `rmdir`, `unlink`,
`unlinkat`, `rename`, `renameat`, `renameat2`, `chdir`, `getcwd` and `umask`;
and for what isn't a file, `pipe`, `pipe2`, `dup`, `dup2` and `dup3`, and
`/dev/null`, `/dev/zero` and `/dev/tty`. In the shell, `run cat /hello.txt`
has the user program read it, and `run` lists its other modes. For now:

- no symbolic link is followed, so `stat` and `lstat` are the same;
- a file that's open can't be removed or renamed over (`EBUSY`);
- pipes always wait (no `O_NONBLOCK`), and writing to one nobody reads is
  `EPIPE` without `SIGPIPE`, as there are no signals yet;
- `/dev` holds just those three, and can't be written;
- standard input is always at its end.

## Processes

A program can start others as on Linux: `fork`, `vfork` and `clone` (as C
libraries use it for them) copy it, `execve` replaces it with an ELF program
from the disk, closing the descriptors marked close-on-exec, and `wait4`
waits for its copies and tells how each ended; `getpid` and `getppid` say
who's who. So Rust's `std::process::Command` works: its `spawn`, `status`
and `exec`, but not yet `output`, which needs `poll` to wait. `cargo run`'s
disk has the user program as `/bin/probe` (a `disk.img` made before this
doesn't: delete it), so in the shell, `run spawn /bin/probe hello` has it
run itself again. For now:

- `fork` copies all of a program's memory at once, rather than sharing it
  until either side writes to it;
- `clone` makes no threads: asked to share memory (`CLONE_VM`), it copies it
  too, which is all `posix_spawn` needs, and `CLONE_VFORK` doesn't hold the
  parent back;
- a program that ended keeps its memory until it's waited for, and those
  never waited for go with their parent rather than to an init;
- there are no process groups, so `wait4` for a group waits for any child,
  and no signals, so nothing is ever stopped;
- `execve` takes ELF programs of up to 4 MiB, which it reads whole, and not
  scripts: shells run those themselves when told `ENOEXEC`.

## Tests

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
copies of and run again. After a run that passes, e2fsck must find nothing wrong with it, and it must have been
unmounted, so the tests need e2fsprogs installed.

Each test boots twice, under BIOS and then under UEFI, and passes only if it
passes both times. The runner tells the guest which firmware it started, and
`tests/basic_boot.rs` checks that the kernel saw the same one.

`scripts/smoke-test.sh` is the coarser check, and the one that catches the most:
it boots the real images both ways and asserts the kernel's boot banner names
the right firmware and the right framebuffer mode.

## Layout

| | |
|---|---|
| `kernel/` | the OS. A `no_std` library plus the binary that draws the screen. |
| `fullkernel/` | host-side: builds the disk images, runs QEMU. |
| `tools/boot-test/` | cargo's test runner for the kernel's bare-metal tests. |
| `user/probe/` | a user program: ordinary Rust with `std`, built for Linux (static musl). Both images and every test boot carry it as the bootloader's ramdisk, and the shell's `run` runs it. |
| `vendor/bootloader/` | [rust-osdev/bootloader](https://github.com/rust-osdev/bootloader) 0.11.17, vendored and patched. |
| `vendor/ps2/` | the `ps2` crate, with its `x86_64` dependency bumped. |
| `fs/ext4/` | ext4 (and ext3 and ext2) file systems, read and written with a journal: a Rust interface over the vendored lwext4, tested on the host against e2fsprogs. |
| `vendor/lwext4/` | [lwext4](https://github.com/gkostka/lwext4), the C library `fs/ext4` uses for now. |
| `scripts/` | `build-images.sh`, `smoke-test.sh`, `boot-test.sh`. |

There is no cargo workspace: `kernel` builds for `x86_64-unknown-none` with
`build-std`, the host tools and `fs/ext4`'s tests build for the host, `user/probe` builds for
`x86_64-unknown-linux-musl` (linked by the toolchain's `rust-lld`), and the settings for
those are mutually exclusive. Each crate has its own `Cargo.toml`, `Cargo.lock`,
`.cargo/config.toml` and target directory — so a `[patch]` added to one has to
be added to the others that need it.

### The vendored lwext4

`vendor/lwext4` is upstream [lwext4](https://github.com/gkostka/lwext4) at
commit `58bcf89` (2022-09-22): its `src/` and `include/` without the mkfs and
MBR code, which nothing uses. It is GPL-2.0 (its extents and xattr code are
"GPL-2.0 or later"), like simplos. It has five local fixes, each marked
`LOCAL PATCH` in the source, all found by `fs/ext4`'s tests:

- **`src/ext4.c`, renaming a directory** — the block holding its `..` entry
  was changed without its checksum, which e2fsck then finds wrong on any file
  system with metadata checksums, as Linux makes them by default.
- **`src/ext4_dir_idx.c`, making a directory** — part of its first block was
  set after the block's checksum was taken, from memory not yet set: the
  checksum was wrong whenever that memory wasn't zero.
- **`src/ext4.c`, removing a file** — the file was emptied in one transaction
  and unlinked in the next, so a crash in between left it there, empty. Now
  emptying it (up to 16 MiB, which is one step) is part of the unlinking's
  transaction.
- **`src/ext4_journal.c`, unmounting** — the journal is now marked empty
  before the file system is marked as not needing it, not after: a crash in
  between left a clean file system with a full journal. And the journal's
  sequence number carries on rather than going back to 0, which let
  transactions left from one mount pass for the next mount's, so that a crash
  replayed old blocks over new ones.
- **`src/ext4_fs.c`, mounting to write** — it marked the file system as
  having errors, where Linux clears its "clean" flag: after a crash, Linux
  would warn of errors that weren't there.

And one addition, also marked: **`src/ext4.c`, renaming onto a file that's
there** replaces it in the rename's own transaction, as `rename(2)` promises,
so a crash leaves the old file or the new one there, never neither. Upstream
refuses (`EEXIST`).

`fs/ext4` compiles it with the host's C compiler, the same way for the
kernel as for its tests on the host: freestanding, with no C library but the
few headers in `fs/ext4/src/include` and the few functions in
`fs/ext4/src/libc.c`, and only ever the general registers
(`-mgeneral-regs-only`). That last matters: the kernel saves just the general
registers of a thread that's in it, so it must never touch SSE ones, which
hold user programs' state. Compiled without it, an optimised kernel has a
couple of hundred SSE instructions; CI checks it has none. Its `build.rs` also
turns off debug `printf`, picks lwext4's own errno values, and gives it
`malloc` and friends from Rust's allocator. Nothing outside `fs/ext4` sees it, so it can be
replaced by Rust code without touching the rest. Its habits that differ from
Linux's are handled there rather than patched: it would mount a file system
whose journal needs replaying as if it were clean, make the missing
directories on the way to a new file, say nothing to making a directory that's
there or unlinking a directory, move a directory into itself, write to a file
opened for reading, quietly mount read-only what it can't write, and crash
unmounting after a failed mount.

What it still can't do: rename a directory onto an empty one in one
transaction (it's two), remove a file over 16 MiB in one transaction, remove
or replace a file that's open (refused, where Linux removes the name and
keeps the file until it's closed), set timestamps, or leave a hole in a file
(writing past the end fills the gap with zeros). A new file's permissions are
set just after it's made, in a transaction of their own. Every change is on the device before the call
returns, in the journal if there is one, which is simple and slow.

Building and running `fs/ext4`'s tests needs e2fsprogs (`mke2fs`, `e2fsck`,
`debugfs`) and `gzip`. Every test that writes ends with `e2fsck -fn` on the
result. `tests/journal.rs` replays `tests/linux-crash.img.gz`, a disk Linux's
own ext4 driver left with changes still in its journal (`tests/linux-crash.sh`,
run as root, makes it again), and cuts the power at every write a run of
changes makes, checking that both e2fsck and `fs/ext4` recover from each.

### The vendored bootloader

`vendor/bootloader` is upstream 0.11.17 with three local changes, each marked
`LOCAL PATCH` in the source:

- **`bios/stage-2/build.rs`** and **`bios/stage-2/src/main.rs`** — the VESA mode
  cap comes from `SIMPLOS_FB_WIDTH` / `SIMPLOS_FB_HEIGHT` instead of being
  hardcoded.
- **`build.rs`** — declares those two variables as build inputs. Without it the
  nested `cargo install` runs that build the BIOS stages are never re-invoked
  when they change, and a stale stage-2 is silently reused with the old cap
  compiled in.
- **`uefi/src/main.rs`** — prefers an exact GOP mode match before falling back
  to upstream's "first mode at least this large".

Its build is unusual and worth knowing about before debugging it: `build.rs`
shells out to five nested `cargo install --locked` runs to build four real-mode
BIOS stages and the UEFI loader, each for its own custom target with its own
rebuilt `core`. They go to `vendor/bootloader/target`, a fourth target directory.
Because they are `--locked`, editing a vendored manifest without updating
`vendor/bootloader/Cargo.lock` fails *inside a build script*, where the error is
easy to misread.

## Branches

`develop` is the default branch and where work lands, through pull requests that
CI checks. `master` is only ever moved by the **Promote to master** workflow,
which re-runs the checks at the commit it is about to release, bumps the version,
fast-forwards both branches to it, tags it `vX.Y.Z` and publishes a GitHub
Release with both disk images attached.

So: branch from `develop`, open a pull request into `develop`, and release by
running Promote to master from the Actions tab.

### What the branch rules do and don't enforce

Both branches have a ruleset that refuses **force-pushes and deletion** — the
two things that lose work irrecoverably. Repository admins can bypass them, so
you can't lock yourself out.

Neither enforces "must go through a pull request with a passing check", and that
is a deliberate trade, not an omission. Promote pushes the release commit
*straight* to both branches, and a `pull_request` or `required_status_checks`
rule rejects a direct push — including a clean fast-forward, and including one
from Actions itself:

    remote: - Changes must be made through a pull request.
    remote: - Required status check "check" is in progress.

The usual answer is to give the workflow's token a bypass, but a ruleset bypass
actor can only be an app on a repository owned by an *organisation*; on a
user-owned repository the built-in `GITHUB_TOKEN` cannot be granted one. So
enforcing it would mean giving up the automatic version bump, or giving the job
a personal access token or deploy key held as a secret and added as a bypass
actor. Until then the pull request flow is a convention — CI still runs on every
one, and promote refuses to release if master has commits develop doesn't.

## License

GPL-2.0-only, the same as Linux, so that Linux code can be ported in where it
saves real work (filesystem drivers, for one). See [LICENSE](LICENSE). The
crates vendored under `vendor/` keep their own licenses (MIT, or MIT/Apache-2.0),
which permit this.
