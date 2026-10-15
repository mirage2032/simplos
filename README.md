# simplos

A small x86_64 operating system written in Rust, from the boot sector up. It
brings up long mode, a GDT and IDT, paging and a heap, a PS/2 keyboard, the
HPET, and draws a terminal into a linear framebuffer, on which it runs the
first program on its disk, as Linux would, or else a small command line of its
own (type `help`), from which `run` starts unmodified Linux x86-64 programs in
user mode. It boots under both BIOS and UEFI firmware, on real hardware or an
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
[the latest release](../../releases/latest) and boot it. Its `disk.qcow2` is
the ext4 disk `cargo run` would make, userland and all, 1 GiB to the guest in a
file of a few megabytes; attach it after the image with
`-drive format=qcow2,file=disk.qcow2`.

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
`tty`, `fd` and `stdin`, `stdout` and `stderr`, and `/proc` what the kernel
knows of the machine and its processes, both over whatever the disk has
there, as on Linux. The shell's `ls` and `cat` show what's there.

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
the next boot replays the journal, as Linux would. Small writes to a file,
one after another, wait in memory to be written as one, as each write
costs the disk a handful of its own however little it writes: until
anything else is done on the disk, the file is closed, a program calls
`fsync`, `fdatasync`, `syncfs` or `sync`, or a second has passed, so the
power going loses that second's at most. The work is done by
`fs/ext4` (see below), behind a lock that sleeps whoever waits for it
(`thread::SleepLock`), since a call can wait on the disk for a long time.
The disk is trusted: lwext4 is C, parsing it in ring 0, and isn't fuzzed yet.

Programs use it through Linux's system calls, so Rust's `std::fs` (or C's
stdio) works as it does on Linux: `open`, `openat`, `close`, `read`, `write`,
`pread64`, `pwrite64`, `lseek`, `stat`, `lstat`, `fstat`, `newfstatat`,
`getdents64`, `fcntl`, `fsync`, `mkdir`, `mkdirat`, `rmdir`, `unlink`,
`unlinkat`, `rename`, `renameat`, `renameat2`, `chdir`, `getcwd` and `umask`;
and for what isn't a file, `pipe`, `pipe2`, `dup`, `dup2` and `dup3`, and
`/dev/null`, `/dev/zero` and `/dev/tty`. `/dev/fd/N`, and `/proc/PID/fd/N`
for any process's, is descriptor N, which opening it duplicates, as FreeBSD's
`/dev/fd` does it, and `/dev/stdin` and the others link to `fd/0` and the
rest. `statfs` and `fstatfs` say how big a file system is and how much of it
is free. In the shell, `run cat /hello.txt` has the user program read it, and
`run` lists its other modes.

`/proc` is made as it's read (`proc.rs`), with the files Linux has there
that busybox's `free`, `ps`, `top`, `df` and `mount` read: `meminfo`,
`uptime`, `loadavg`, `stat`, `mounts` and `version`; `diskstats`, each
disk's reads and writes since boot; `self`, a link to the
reader's own directory; and in each process's, its `stat`, `status`,
`cmdline` and `comm`, and `fd`.

Symbolic links (`symlink`, `symlinkat`, `readlink`, `readlinkat`) are
followed wherever a path goes through one, as Linux follows them
(`fs::lookup`): at the end too, but for the calls about the link itself, as
`lstat`, `unlink` and `rename`, or asked not to (`O_NOFOLLOW`,
`AT_SYMLINK_NOFOLLOW`); `..` after one is the parent of where it led; 40 in a
row is `ELOOP`. Hard links (`link`, `linkat`) give a file another name, to
anything but a directory.

A file's permissions, owner and times are changed with `chmod`, `fchmod`,
`fchmodat`, `chown`, `fchown`, `lchown`, `fchownat` and `utimensat`, and its
length with `truncate` and `ftruncate`; making a file sets all three of its
times, writing to it when it was modified, and any change when it was
changed. `access`, `faccessat` and `faccessat2` answer as for root, who may
read and write anything but where nothing can be written (`EROFS`), and run
what anyone may. `sendfile` copies from one descriptor to another. For now:

- a file removed or renamed over while it's open stays until it's closed, as
  on Linux, but in `/lost+found`, as `.open-` and its inode, where Linux
  keeps it on ext4's orphan list, which lwext4 doesn't know; the disk's next
  mount takes out any the power went before closing. And a directory that's
  open can't be removed (`EBUSY`);
- `/dev` holds just those devices, and can't be written; and opening
  `/dev/fd/N` shares N's place in the file, where Linux opens the file anew;
- `/proc` has none of `maps`, `environ` and the rest, nor load averages, and
  every process's terminal reads as `tty1`;
- times are kept to the second, as lwext4 sets none of ext4's nanoseconds;
  reading doesn't set when a file was read, as if mounted `noatime`; and a
  stream of writes through one descriptor sets when it was modified once a
  second, not each time.

## Processes

A program can start others as on Linux: `fork`, `vfork` and `clone` (as C
libraries use it for them) copy it, `execve` replaces it with an ELF program
from the disk, closing the descriptors marked close-on-exec, and `wait4`
waits for its copies and tells how each ended; `getpid` and `getppid` say
who's who. `execve` runs a script through what its first line names
(`#!/bin/sh -e` and the like), as Linux does, with the line's one argument
and the script's path; that can be a script too, five deep. So Rust's `std::process::Command` works: its `spawn`, `status`,
`output` and `exec`. `cargo run`'s
disk has the user program as `/bin/probe` (a `disk.img` made before this
doesn't: delete it), so in the shell, `run spawn /bin/probe hello` has it
run itself again.

A program can ask what it runs on: `uname` says Linux, the interface it's
talking to, with simplos as the host and in the version; `sched_getaffinity`
gives it the one CPU; `sysinfo` how long it's been up, how much memory there
is and is free, and how many processes; it runs as root (`getuid` and the
like say 0), in no other groups (`getgroups`); `getpriority` and
`setpriority` keep how nice it is; and `getrlimit`, `setrlimit` and
`prlimit64` keep its limits, which its copies inherit. `readv` and `writev` move several buffers in one call, as musl's
`printf` and the like do. For now:

- every id is 0, and can't be set to another, as there are no other users;
  what `setgroups` sets is forgotten;
- limits and niceness are kept and reported, not enforced: a program has the
  1024 descriptors and 256 KiB of stack there are, whatever it sets, and the
  same turns at the CPU as any other;

- `fork` copies all of a program's memory at once, rather than sharing it
  until either side writes to it;
- `clone` makes no threads: asked to share memory (`CLONE_VM`), it copies it
  too, which is all `posix_spawn` needs, and `CLONE_VFORK` doesn't hold the
  parent back;
- a program that ended keeps its memory until it's waited for, and those
  never waited for go with their parent rather than to an init;
- `execve` takes ELF programs of up to 4 MiB, which it reads whole.

## Terminal

The screen is a terminal, and the keyboard types into it, as on Linux's
console: a program's standard input, output and error are the terminal,
`/dev/tty` too, and what it reads is what's typed. Between them is Linux's
line discipline (`tty.rs`): in canonical mode, the default, a line is typed
and edited first, with Backspace, ^U, ^W, and ^D for the end of file, and
handed over on Enter; in raw mode each key is ready as it's pressed. A
program reads and changes the settings, laid out as Linux's `termios`, with
`ioctl` (`TCGETS`, `TCSETS` and the rest), and asks the size with
`TIOCGWINSZ`.

The screen (`terminal.rs`) keeps a grid of characters, each with its colours
and attributes, and a cursor; it draws only the rows that changed, and
scrolls the whole screen by moving it. It understands the escape sequences
the Linux console does, as its `linux` terminfo entry lists them, and
programs are given `TERM=linux`: moving the cursor, erasing, inserting and
deleting characters and lines, scrolling a region, the 16 colours and 256 and
any, bold, underlined and reversed, saving the cursor, hiding it, and saying
where it is, which comes back as if typed; and the alternate screen, which
full-screen programs ask for whatever the entry says. It looks like the
Linux console too: light grey on black, and bold text bright, as the
console draws it. The keys send what the console's do. In the shell, `run
colors` shows the colours.

`poll` and `ppoll` wait for pipes and terminals, as long as they're asked
to, and `O_NONBLOCK` (from `open`, `pipe2`, `fcntl` or `FIONBIO`) makes what
would wait fail with `EAGAIN` instead. In the shell, `run cat` copies what's
typed until ^D. For now:

- of the escape sequences, no line-drawing characters, tab stops but every
  eighth column, insert mode or application cursor keys, none of which the
  `linux` entry asks for; what isn't known is read and ignored;
- in raw mode, `VTIME` is ignored: a read waits for `VMIN` bytes however
  long they take;
- the font is ASCII: anything else shows as `?`.

## Signals

Signals are Linux's (`signal.rs`): a program sends them with `kill`, `tkill`
and `tgkill`, to a process or a process group; its terminal sends ^C's
SIGINT, ^\'s SIGQUIT and ^Z's SIGTSTP to the group in its foreground; and
the kernel sends SIGCHLD as a child ends, stops or continues, and SIGPIPE
as a pipe no one reads is written. Each does what the program asked with
`rt_sigaction`, or else its default: ending the program, stopping it until
SIGCONT, or nothing. One that's blocked (`rt_sigprocmask`) waits, pending
(`rt_sigpending`), until it isn't; `rt_sigsuspend` and `pause` wait for one.

A handler runs on the program's stack, or the one `sigaltstack` gave it,
with the frame Linux puts there: what the program was doing, its registers
and its SSE state, all put back by `rt_sigreturn`. A signal is acted on as
the program goes back to ring 3, as a system call returns or as the timer
interrupts it, so one that never calls into the kernel still gets it; and a
call that was waiting (a read, `wait4`, a sleep, `poll`) ends early, with
`EINTR`, or starts again if the handler asked for `SA_RESTART`. `wait4`
tells of children that stopped or continued if asked (`WUNTRACED`,
`WCONTINUED`), and a child's SIGCHLD can be ignored, which frees it as it
ends.

Programs are in process groups, in sessions (`setpgid`, `setsid`,
`getpgid`, `getsid`): a program the kernel starts leads one, with the
terminal it's on and its own group in the terminal's foreground, which
`TIOCSPGRP` changes, as a shell does for its jobs. As a session's leader
ends, its foreground is sent SIGHUP. In the shell, `run sleep 100000` is
ended by ^C. For now:

- a fault ends its program with its signal even if it has a handler for it;
- a signal whose default dumps core just ends the program, as there's
  nowhere to dump it;
- real-time signals aren't queued: one of each is kept at a time;
- a sleep that a stop, or a signal with no handler, interrupts starts its
  whole time again, where Linux's sleeps for what's left;
- a session's leader giving its terminal up (`TIOCNOTTY`) doesn't hang up
  the terminal's foreground;
- ^Z, SIGTTIN and SIGTTOU stop no one in a group no one outside it could
  continue, as POSIX says, which a program the kernel starts is in; a job a
  shell starts can be stopped;
- a program in the background isn't stopped for reading or writing its
  terminal, and one whose parent ended isn't given to an init.

## Time

The time of day comes from the CMOS real-time clock, read once at boot, and
the HPET carries it on from there (`time.rs`). Programs read it, and the
time since boot, with `clock_gettime`, `gettimeofday` and `time`, to the
nanosecond; `clock_nanosleep` sleeps for a while or until a time. Each timer
tick is counted to the program it interrupts, as its own time or the
kernel's for it, which `getrusage`, `times` and `wait4` report, its waited-for
children's too. `setitimer` and `alarm` set a program's alarm, which the
alarm thread sends as SIGALRM when it's due, and every so often after if
asked. For now:

- the clock is taken to be UTC, as QEMU keeps it, and can't be set;
- CPU time is counted to the tick, 10 ms;
- an alarm can come up to a tick late, though never early, and only the
  real-time one is there: none that counts CPU time (`ITIMER_VIRTUAL`,
  `ITIMER_PROF`).

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

`scripts/userland.sh` builds the programs that make a disk a system to use:
busybox 1.38.0, with a link for each of its applets where Linux keeps them
(`/bin/sh`, `/bin/ls`, `/usr/bin/awk` and the rest), and zsh 5.9.2 with its
shell functions, on ncurses 6.6 with the terminals it knows built in.
They're static programs for x86-64 Linux, linked with musl by zig cc 0.17.0,
from the sources the script pins and checks by their SHA-256, into
`target/userland/root`. The first build takes a few minutes and needs the
network; after that it's done, until the script changes. It builds on x86-64
Linux only.

Over them go the files in `userland/`, copied every time, so changing them
needs no new build:

- `/sbin/init`, which the kernel runs first: it logs root in to zsh, as
  `login` would, in `/root`;
- `/etc/passwd`, `/etc/group` and `/etc/shells`, in which zsh is root's
  shell;
- `/etc/zshrc`, for every interactive zsh: completion, with a menu;
  history, kept in `~/.zsh_history`; Emacs's keys and the console's Home,
  End and Delete; and a prompt saying who, where and in which directory.

What the completion system knows of its 998 functions it keeps in
`~/.zcompdump`, which the build makes for root, by running the same zsh:
made on simplos, it takes half a minute, most of it reading the functions
a byte at a time, each byte from the disk.

`cargo run`'s disk gets all of it as it's made: delete `fullkernel/disk.img`
for one that has it. Then zsh is what you type at, and once it ends, the
kernel's own shell. For now:

- busybox is built without `init`, which the kernel would run first, and
  which wants a console and an `inittab` there aren't; and without `tc`;
- the disk's files belong to whoever made it, as `mke2fs -d` copies owners,
  which is why `/etc/zshrc` has `compinit -u`;
- what's read a little at a time is read from the disk each time, as
  lwext4 keeps no file's data in memory, so zsh's `read`, which reads a
  byte at a time, is slow.

busybox is GPL-2.0, as simplos is; zsh and ncurses are MIT-style. A disk
made with them holds their programs, built from the upstream releases the
script names, unchanged, configured as it says.

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
copies of and run again; `tests/tty.rs` types into a terminal for it to read;
`tests/signals.rs` has it send signals to itself and its copies;
`tests/time.rs` has it read the clocks, count CPU time and set alarms.
`tests/shells.rs` runs busybox's sh and zsh, as commands and typed at on a
terminal: its disk is 64 MiB and holds the userland too, which the runner
builds with `scripts/userland.sh` the first time. After a run that passes, e2fsck must find nothing wrong with it, and it must have been
unmounted, so the tests need e2fsprogs installed.

Each test boots twice, under BIOS and UEFI at once, and passes only if it
passes both times; what each run prints is shown in turn. The runner tells the
guest which firmware it started, and `tests/basic_boot.rs` checks that the
kernel saw the same one. `SIMPLOS_FIRMWARE=bios cargo test` (or `uefi`) boots
just the one.

A test still running after 30 s is taken to hang: it fails there and then,
named, rather than leaving its kernel to the runner's limit of 180 s. Test
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
| `scripts/` | `build-images.sh`, `smoke-test.sh`, `boot-test.sh`, `mutants.sh`, and `userland.sh`, which builds busybox and zsh for the disk. |

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
"GPL-2.0 or later"), like simplos. It has seven local fixes, each marked
`LOCAL PATCH` in the source, all found by `fs/ext4`'s tests:

- **`metadata_csum_seed`, which e2fsprogs 1.47.4's `mke2fs` turns on for
  every ext4** — it refused such a file system, whose metadata checksums all
  start from a seed its superblock keeps, so that its uuid can change
  without them. Upstream started each from crc32c of the uuid, which is the
  seed only until the uuid changes: `ext4_sb_csum_seed` (`src/ext4_super.c`)
  now gives all eight the right one, as Linux's `s_csum_seed` does.
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
- **`src/ext4.c`, a symbolic link too long for its inode** — its target went
  into a block of its own, after whatever a file freed before had left
  there, where Linux zeroes the rest: e2fsck took such links to be broken.

And two additions, also marked: **`src/ext4.c`, renaming onto a file that's
there** replaces it in the rename's own transaction, as `rename(2)` promises,
so a crash leaves the old file or the new one there, never neither. Upstream
refuses (`EEXIST`). And **`ext4_inode_attrs_set`** changes a file's
permissions, owner, group and times by inode, in one transaction: upstream
sets each by path, in a transaction of its own, and the path of a file that's
open may have changed.

`fs/ext4` compiles it with the host's C compiler, the same way for the
kernel as for its tests on the host: freestanding, with no C library but the
few headers in `fs/ext4/src/include` and the few functions in
`fs/ext4/src/libc.c`, and only ever the general registers
(`-mgeneral-regs-only`). That last matters: the kernel saves just the general
registers of a thread that's in it, so it must never touch SSE ones, which
hold user programs' state. Compiled without it, an optimised kernel has a
couple of hundred SSE instructions; CI checks it has none. Its `build.rs` also
turns off debug `printf`, picks lwext4's own errno values, gives it `malloc`
and friends from Rust's allocator, and has it keep up to 512 blocks of each
file system in memory, where upstream's 8 had every lookup in a big directory
read it from the disk again: zsh's `compinit` checking its 1185 functions, in
one directory, took 24 s, and now takes 1.6 s. Nothing outside `fs/ext4` sees
it, so it can be replaced by Rust code without touching the rest. Its habits that differ from
Linux's are handled there rather than patched: it would mount a file system
whose journal needs replaying as if it were clean, make the missing
directories on the way to a new file, say nothing to making a directory that's
there or unlinking a directory, move a directory into itself, write to a file
opened for reading, quietly mount read-only what it can't write, and crash
unmounting after a failed mount.

What it still can't do: rename a directory onto an empty one in one
transaction (it's two), remove a file over 16 MiB in one transaction, keep a
file that's open once its last name is gone (it would free it under whoever
has it open: `fs/ext4` gives it another name first), or leave a hole in a
file (writing past the end fills the gap with zeros). A new file's permissions and times are
set just after it's made, in a transaction of their own. Every call is a
transaction of its own, in the journal before the call returns, where
there's one: lwext4's cache then writes the blocks to their places in its
own time (its write-back mode), or as the file system is unmounted. Without
a journal, it writes them through. So `fs/ext4` holds a stream of small
writes to a file, and makes them in one call.

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
Release with both disk images and the ext4 disk (`disk.qcow2`, made by
`scripts/build-images.sh`) attached.

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
