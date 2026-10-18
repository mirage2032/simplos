# Disks and ext4

## Disks

At boot the kernel finds every hard disk on the legacy IDE channels (ATA PIO)
and on any AHCI (SATA) controller on PCI, logs one `simplos: disk N: ...` line
for each over serial, and shows the count on screen. Kernel code reads and
writes raw 512-byte sectors with `drivers::disk::{list, read, write}`. Both drivers
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
there, as on Linux. The shell's `ls` and `cat` show what's there: `cat` up
to a megabyte, which it holds whole.

`cargo run` attaches `tools/boot/disk.img`, which it makes the first time
with e2fsprogs' `mke2fs` and keeps, so what's written to it is there next
time; delete it to start afresh. To use a disk of your own, make one from a
directory and attach it, and the kernel finds it first:

```sh
mkdir -p tree && echo hello > tree/hello.txt
/usr/sbin/mke2fs -t ext4 -d tree disk.img 64M
cargo run -- -drive format=raw,file=disk.img
```

Nothing unmounts it when QEMU is closed, which to the disk is a power cut:
the next boot replays what its journal holds, as Linux would; which is
nothing once a second has passed with nothing done on the disk, as what the
journal holds is then written where it goes. Small writes to a file,
one after another, wait in memory to be written as one, as each write
costs the disk a handful of its own however little it writes: until
anything else is done on the disk, the file is closed, a program calls
`fsync`, `fdatasync`, `syncfs` or `sync`, or a second has passed, so the
power going loses that second's at most; but for a file opened `O_SYNC` or
`O_DSYNC`, whose every write is on the disk as it returns; if the disk
won't take them, the next `fsync` says so, as Linux's does of what it
writes back. The work is done by `lib/ext4` (see below), behind a lock that
sleeps whoever waits for it (`thread::SleepLock`), since a call can wait on
the disk for a long time.
The disk is trusted: lwext4 is C, parsing it in ring 0, and isn't fuzzed yet.

What's read of a file is kept in memory, a page at a time, in the page cache
(`mm/page_cache.rs`): read again, it comes from there rather than the disk. Programs
map its pages of their code, and of files they map read-only, all of them
the same frames, so every program running uutils' coreutils, or linked to
`libc.so`, has one copy of it between them. What's written goes to the file
as it's written, and into whatever of it is kept: ext4 tells the cache of
every change to a file, whoever makes it (`Ext4::watch`). It keeps up to
256 MiB: past that, the pages used longest ago go, those programs map
staying theirs. It gives back the pages no program maps, those used
longest ago first, as free memory runs low, and all of them when it runs
out; and
`/proc/meminfo` counts them as `Cached`. Under emulation, ten runs of
uutils' 14 MB `coreutils true` took 10 s before it and take 1 s; a second
`gcc -O2` of a hello world, 10 s before and 0.7 s now.

A program's memory is mapped as it's touched (`mm/`). `execve`,
`mmap` and `brk` only note what each part of it holds, its code and data
what part of which file, the rest zeros; a page is mapped the first time
the program, or a system call for it, touches it, so a program reads in
only as much of itself as it runs. A page the cache doesn't have is read
in with the fifteen around it, in one read from the disk, and the cached
pages around one a program touches are mapped with it. `fork` shares a
program's pages with its copy until either writes one, which then gets a
copy of its own, unless the other has let go of it by then, as a shell's
copy does as it execs. For now:

- a page a program can write, of its data or of a file it maps writable,
  is a copy, made as it's first touched rather than as it's first
  written;
- what's cut off the end of a file stays in the pages programs map of it,
  and a page past the end reads as zeros, where Linux sends SIGBUS.

Programs use it through Linux's system calls, so Rust's `std::fs` (or C's
stdio) works as it does on Linux: `open`, `openat`, `close`, `read`, `write`,
`pread64`, `pwrite64`, `lseek`, `stat`, `lstat`, `fstat`, `newfstatat`,
`getdents64`, `fcntl`, `fsync`, `mkdir`, `mkdirat`, `rmdir`, `unlink`,
`unlinkat`, `rename`, `renameat`, `renameat2`, `chdir`, `fchdir`, `getcwd`
and `umask`; and for what isn't a file, `pipe`, `pipe2`, `dup`, `dup2` and `dup3`, and
`/dev/null`, `/dev/zero`, `/dev/tty`, `/dev/random` and `/dev/urandom`. `/dev/fd/N`, and `/proc/PID/fd/N`
for any process's, is descriptor N, which opening it duplicates, as FreeBSD's
`/dev/fd` does it, and `/dev/stdin` and the others link to `fd/0` and the
rest. `readlink` of one says what it's open on, as musl's `ttyname` asks:
`/dev/tty` for the terminal. `statfs` and `fstatfs` say how big a file system is and how much of it
is free. `socketpair` makes two connected Unix-domain sockets, each reading
what the other writes, with `read` and `write` or `send` and `recv`, as
Rust's `Command` makes to hear whether a program it forked failed to
`exec`. `fadvise64` takes advice. The `*at` calls read `AT_FDCWD` as the
`int` it is, as rustix passes it zero-extended. `preadv` and `pwritev`, and
`preadv2` and `pwritev2` with their flags, read and write several buffers
at an offset. Files lock as Linux's do (`fs/locks.rs`): whole, with `flock`,
each lock its open file's, as cargo locks its build; and in ranges of
bytes, with `fcntl`'s `F_SETLK`, `F_SETLKW` and `F_GETLK`, each its
process's and gone as it closes any descriptor of the file, as SQLite
locks, or with the `F_OFD_` ones, its open file's. In the shell, `run cat /hello.txt` has the user program read it, and
`run` lists its other modes.

`/proc` is made as it's read (`fs/procfs.rs`), with the files Linux has there
that busybox's `free`, `ps`, `top`, `df` and `mount` read: `meminfo`,
`uptime`, `loadavg`, `stat`, `mounts` and `version`; `diskstats`, each
disk's reads and writes since boot; `self`, a link to the
reader's own directory; and in each process's, its `stat`, `status`,
`cmdline` and `comm`, `fd`, `mounts` and `mountinfo`, which uutils' `df`
reads, and `exe`, a link to its program, by which rustc finds its own
libraries. `prctl` names threads (`PR_SET_NAME`, `PR_GET_NAME`), the first
one's name being its program's, as `comm` tells.

Symbolic links (`symlink`, `symlinkat`, `readlink`, `readlinkat`) are
followed wherever a path goes through one, as Linux follows them
(`fs::lookup`): at the end too, but for the calls about the link itself, as
`lstat`, `unlink` and `rename`, or asked not to (`O_NOFOLLOW`,
`AT_SYMLINK_NOFOLLOW`); `..` after one is the parent of where it led; a name
with `/` or `/.` after it has to be a directory, or a link to one, else
`ENOTDIR`; 40 in a row is `ELOOP`. Hard links (`link`, `linkat`) give a file another name, to
anything but a directory.

A file's permissions, owner and times are changed with `chmod`, `fchmod`,
`fchmodat`, `chown`, `fchown`, `lchown`, `fchownat` and `utimensat`, and its
length with `truncate` and `ftruncate`; making a file sets all three of its
times, writing to it when it was modified, and any change when it was
changed. `access`, `faccessat` and `faccessat2` answer as for root, who may
read and write anything but where nothing can be written (`EROFS`), and run
what anyone may. `sendfile` copies from one descriptor to another, and
`splice` into a pipe or out of one, as uutils' `cat` copies. For now:

- pipes hold their data on the kernel's heap, so all of them together hold
  at most a quarter of it, but for the first 4 KiB of each: past that, a
  pipe counts as full sooner than its 64 KiB, where Linux takes the room
  from all of memory; so do open directories their entries, opening one
  past that `ENOMEM`; and there are at most 4096 locks, all processes'
  together, the next one `ENOLCK`;
- a file removed or renamed over while it's open stays until it's closed, as
  on Linux, but in `/lost+found`, as `.open-` and its inode, where Linux
  keeps it on ext4's orphan list, which lwext4 doesn't know; the disk's next
  mount takes out any the power went before closing. And a directory that's
  open can't be removed (`EBUSY`);
- there are no extended attributes, as on a file system without them on
  Linux: `listxattr` lists none and the rest are `EOPNOTSUPP`;
- locks aren't checked for deadlocks: two processes each waiting for a
  range the other holds wait until a signal, where Linux tells one
  `EDEADLK`; and only files on the disk are locked, `flock` of anything
  else succeeding;
- the only sockets are the pairs `socketpair` makes, datagram and
  sequenced-packet ones as streams, the messages in them not kept apart;
- `/dev` holds just those devices, and can't be written; opening
  `/dev/fd/N` shares N's place in the file, where Linux opens the file anew;
  and `readlink` of one open on a file on the disk is `EINVAL`, as its
  path isn't kept;
- `/proc` has none of `maps`, `environ` and the rest, nor load averages, and
  every process's terminal reads as `tty1`;
- times are kept to the second, as lwext4 sets none of ext4's nanoseconds;
  reading doesn't set when a file was read, as if mounted `noatime`; and a
  stream of writes through one descriptor sets when it was modified once a
  second, not each time.

## The vendored lwext4

`vendor/lwext4` is upstream [lwext4](https://github.com/gkostka/lwext4) at
commit `58bcf89` (2022-09-22): its `src/` and `include/` without the mkfs and
MBR code, which nothing uses. It is GPL-2.0 (its extents and xattr code are
"GPL-2.0 or later"), like simplos. It has thirteen local fixes, each marked
`LOCAL PATCH` in the source, all found by `lib/ext4`'s tests:

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
- **`src/ext4_journal.c`, a transaction written where it goes** — whether it
  was the oldest in the journal was asked before the transactions older than
  it were finished, which writing it can do: then it stayed, written, and
  the journal's start with it, so a crash replayed everything after.
- **`src/ext4_bcache.c` and `src/ext4.c`, a write the device refuses** — a
  block written through had its write's error dropped, the journal's own
  blocks among them, so a transaction that never reached the disk was taken
  as made; and nothing that commits one looks at what that says, so the call
  and `sync` said all was well. Now a commit that fails stops all writing,
  as Linux's `errors=remount-ro` does, and `Ext4::sync` says so from then on.
  Its journal is still stopped as it's unmounted, which upstream skipped for
  a read-only file system, leaving it to the next mount in the same slot.
- **`src/ext4.c`, unmounting when the superblock can't be written** — the
  mount's slot stayed taken, by its name and the device, which the caller
  then frees: the next mount by that name was taken as made, on the freed
  device.
- **`src/ext4.c`, write-back mode** — truncating, writing and making a long
  symbolic link turned it on and, failing, didn't turn it off: without a
  journal, every block changed after, out of disk space or not, stayed in
  memory, where `sync` didn't reach it.
- **`src/ext4.c`, a name of 255 bytes**, ext4's longest — wasn't found;
  `lib/ext4` says `NameTooLong` for longer ones, which lwext4 said weren't
  there.
- **`src/ext4.c`, making a directory on a full disk** — its name went into
  its parent before its index's block was had, and stayed when that couldn't
  be, naming the inode then freed: e2fsck found it, and the next file made
  got that inode under both names.

And two additions, also marked: **`src/ext4.c`, renaming onto a file that's
there** replaces it in the rename's own transaction, as `rename(2)` promises,
so a crash leaves the old file or the new one there, never neither. Upstream
refuses (`EEXIST`). And **`ext4_inode_attrs_set`** changes a file's
permissions, owner, group and times by inode, in one transaction: upstream
sets each by path, in a transaction of its own, and the path of a file that's
open may have changed.

And three for speed, marked too. Its checksums of metadata are checked
(`src/ext4_dir.c` and the others) only where a mismatch is logged, which with
debug `printf` off is nowhere: checking them was most of what a lookup took,
each directory block's 4 KiB on each lookup. Its crc32c (`src/ext4_crc32.c`)
takes eight bytes at a time rather than one, five times as fast. And
replaying a journal (`src/ext4_journal.c`) writes each block once, from its
cache, before the journal's marked empty, rather than once for each
transaction it's in.

`lib/ext4` compiles it with the host's C compiler, the same way for the
kernel as for its tests on the host: freestanding, with no C library but the
few headers in `lib/ext4/src/include` and the few functions in
`lib/ext4/src/libc.c`, and only ever the general registers
(`-mgeneral-regs-only`). That last matters: the kernel saves just the general
registers of a thread that's in it, so it must never touch SSE ones, which
hold user programs' state. Compiled without it, an optimised kernel has a
couple of hundred SSE instructions; CI checks it has none. Its `build.rs` also
turns off debug `printf`, picks lwext4's own errno values, gives it `malloc`
and friends from Rust's allocator, and has it keep up to 512 blocks of each
file system in memory, where upstream's 8 had every lookup in a big directory
read it from the disk again: zsh's `compinit` checking its 1185 functions, in
one directory, took 24 s, and now takes 1.6 s. Nothing outside `lib/ext4` sees
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
has it open: `lib/ext4` gives it another name first), or leave a hole in a
file (writing past the end fills the gap with zeros). A new file's permissions and times are
set just after it's made, in a transaction of their own. Every call is a
transaction of its own, in the journal before the call returns, where
there's one: lwext4's cache then writes the blocks to their places in its
own time (its write-back mode), or as the file system is unmounted, or as
nothing else is being done on it (`Ext4::flush`). Without a journal, it
writes them through. So `lib/ext4` holds a stream of small writes to a file,
and makes them in one call. And it keeps where paths led, to an inode or to
nothing, as lwext4 would read every directory on the way again: forgotten
whole as any name is made, moved or removed, and none longer than 1 KiB.

Building and running `lib/ext4`'s tests needs e2fsprogs (`mke2fs`, `e2fsck`,
`debugfs`) and `gzip`. Every test that writes ends with `e2fsck -fn` on the
result. `tests/journal.rs` replays `tests/linux-crash.img.gz`, a disk Linux's
own ext4 driver left with changes still in its journal (`tests/linux-crash.sh`,
run as root, makes it again), and cuts the power at every write a run of
changes makes, checking that both e2fsck and `lib/ext4` recover from each.
