//! A user program for simplos: an ordinary Rust program, `std` and all, built
//! for Linux (static musl) and run by the kernel from the ramdisk. Its first
//! argument picks what it does, so one binary covers every test.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, MetadataExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

fn main() {
    let arg = |n: usize| std::env::args().nth(n).unwrap_or_default();
    match std::env::args().nth(1).as_deref() {
        None | Some("hello") => println!("hello from user space"),
        Some("sleep") => {
            let ms = std::env::args().nth(2).and_then(|ms| ms.parse().ok()).unwrap_or(100);
            println!("sleeping for {ms} ms");
            std::thread::sleep(Duration::from_millis(ms));
            println!("awake");
        }
        // The kernel should end this program, and nothing else.
        Some("fault") => unsafe { std::ptr::null_mut::<u8>().write_volatile(1) },
        // Long enough to be preempted many times; a second copy running
        // alongside must not disturb its floating-point registers.
        Some("float") => {
            let sum: f64 = (0..2_000_000u32).map(|i| f64::from(i).sqrt()).sum();
            println!("{sum}");
        }
        // The kernel must refuse these, not fault or hand over its memory: a
        // pointer into its half, one into nothing, a call it hasn't got, a
        // sleep for an impossible time and one whose time is in the kernel,
        // and a copy whose `fs` would be at an address that isn't one.
        Some("bad-calls") => {
            let kernel = syscall(1, 1, 0xFFFF_8000_0000_0000, 8);
            let unmapped = syscall(1, 1, 0x1000, 8);
            let unknown = syscall(999, 0, 0, 0);
            let timespec: [i64; 2] = [0, 2_000_000_000];
            let bad_time = syscall(35, timespec.as_ptr() as usize, 0, 0);
            let kernel_time = syscall(230, 1, 0, 0xFFFF_8000_0000_0000);
            const CLONE_SETTLS: usize = 0x8_0000;
            let tls = 0x8000_0000_0000_0000;
            let bad_tls = syscall5(56, [CLONE_SETTLS | SIGCHLD, 0, 0, 0, tls]);
            println!("{kernel} {unmapped} {unknown} {bad_time} {kernel_time} {bad_tls}");
        }
        // The same for files: a path and a buffer in the kernel's half, a
        // directory read too small for one entry and one read as a file, a
        // descriptor that isn't open, and a mapping past the last offset.
        Some("bad-files") => {
            let kernel = 0xFFFF_8000_0000_0000;
            let bad_path = syscall(2, kernel, 0, 0);
            let file = or_fail("/hello.txt", fs::File::open("/hello.txt"));
            let bad_buf = syscall(0, file.as_raw_fd() as usize, kernel, 8);
            let dir = or_fail("/dir", fs::File::open("/dir"));
            let mut buf = [0u8; 8];
            let small = syscall(217, dir.as_raw_fd() as usize, buf.as_mut_ptr() as usize, 8);
            let read_dir = syscall(0, dir.as_raw_fd() as usize, buf.as_mut_ptr() as usize, 8);
            let closed = syscall(3, 1000, 0, 0);
            // From an offset whose second page wraps to the file's first.
            let fd = file.as_raw_fd() as usize;
            let wraps = syscall6(9, [0, 8192, 1, 2, fd], 0xFFFF_FFFF_FFFF_F000);
            println!("{bad_path} {bad_buf} {small} {read_dir} {closed} {wraps}");
        }
        // Memory at the edges: the last page of the lower half, which
        // nothing may map, unmap or protect, and the kernel's half; then a
        // gigabyte, more than there is, from mmap and from brk, which give
        // back what they took in failing; and a megabyte, which there is.
        Some("memory-edges") => {
            let top = 0x7FFF_FFFF_F000;
            let edges = [
                syscall(11, top, 4096, 0),
                syscall(10, top, 4096, 1),
                syscall5(9, [top, 4096, 3, 0x32, 0]),
                syscall5(9, [0xFFFF_8000_0000_0000, 4096, 3, 0x32, 0]),
            ];
            let before = rss();
            let mapped = syscall5(9, [0, 1 << 30, 3, 0x22, 0]);
            let kept = rss() - before;
            let heap = syscall(12, 0, 0, 0);
            let refused = syscall(12, heap as usize + (1 << 30), 0, 0) == heap;
            let kept_by_brk = rss() - before;
            let megabyte = syscall5(9, [0, 1 << 20, 3, 0x22, 0]) > 0;
            // A page MAP_FIXED maps just where mmap would map next, which it
            // then doesn't.
            let below = syscall5(9, [0, 4096, 3, 0x22, 0]) as usize;
            let fixed = syscall5(9, [below + 4096, 4096, 3, 0x32, 0]);
            let claimed = syscall5(9, [0, 4096, 3, 0x22, 0]) != fixed;
            println!("{edges:?} {mapped} {kept} {refused} {kept_by_brk} {megabyte} {claimed}");
        }
        // mremap, in a window of its own: a mapping that can't grow where
        // it is, as something's right after it, unless it may move, and
        // then moved, what was in it taken along, its new pages zeros and
        // none left behind; shrunk where it is; moved where asked, longer
        // and then shorter; then what it refuses, a part not mapped among
        // it; all of it given back; one grown where it is, as
        // nothing's after it, which only simplos promises; and realloc,
        // which asks it to grow big blocks.
        Some("mremap") => {
            const PAGE: usize = 4096;
            const MAYMOVE: usize = 1;
            const FIXED: usize = 2;
            let mmap = |at: usize, len: usize, prot: usize, flags: usize| {
                syscall5(9, [at, len, prot, flags, 0]) as usize
            };
            let mremap =
                |old, old_len, new_len, flags, to| syscall5(25, [old, old_len, new_len, flags, to]);
            // Whether the byte at `at` can be read: as write(2) to a pipe,
            // which copies it, says.
            let (_reader, writer) = or_fail("pipe", io::pipe());
            let readable = |at: usize| syscall(1, writer.as_raw_fd() as usize, at, 1) == 1;
            // Safety: only ever of pages just mapped.
            let holds = |at: usize, len: usize, byte: u8| {
                let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
                bytes.iter().all(|&b| b == byte)
            };
            let before = rss();
            let window = mmap(0, 64 * PAGE, 0, 0x22);
            let first = mmap(window, 2 * PAGE, 3, 0x32);
            // Safety: the two pages just mapped.
            unsafe { std::ptr::write_bytes(first as *mut u8, 7, 2 * PAGE) };
            let after = mmap(window + 4 * PAGE, PAGE, 3, 0x32);
            let stuck = mremap(first, 2 * PAGE, 4 * PAGE, 0, 0);
            let moved = mremap(first, 2 * PAGE, 8 * PAGE, MAYMOVE, 0) as usize;
            let taken = moved != first
                && holds(moved, 2 * PAGE, 7)
                && holds(moved + 2 * PAGE, 6 * PAGE, 0)
                && !readable(first)
                && readable(after);
            let shrunk = mremap(moved, 8 * PAGE, PAGE, 0, 0) as usize == moved
                && readable(moved)
                && !readable(moved + PAGE);
            let there = window + 16 * PAGE;
            let fixed = mremap(moved, PAGE, 2 * PAGE, MAYMOVE | FIXED, there) as usize == there
                && holds(there, PAGE, 7)
                && holds(there + PAGE, PAGE, 0)
                && !readable(moved);
            // And there, made shorter, which leaves nothing where it was.
            let elsewhere = window + 24 * PAGE;
            let fixed = fixed
                && mremap(there, 2 * PAGE, PAGE, MAYMOVE | FIXED, elsewhere) as usize == elsewhere
                && holds(elsewhere, PAGE, 7)
                && !readable(there)
                && !readable(there + PAGE);
            let one = mmap(0, PAGE, 3, 0x22);
            let refused = [
                mremap(one + 1, PAGE, 2 * PAGE, MAYMOVE, 0),
                mremap(one, PAGE, PAGE, FIXED, window + 32 * PAGE),
                mremap(one, PAGE, PAGE, 8, 0),
                mremap(one, 0, PAGE, MAYMOVE, 0),
                mremap(one, PAGE, 0, MAYMOVE, 0),
                mremap(moved, PAGE, 2 * PAGE, MAYMOVE, 0),
                mremap(one, 2 * PAGE, 3 * PAGE, MAYMOVE, 0),
                mremap(one, PAGE, 2 * PAGE, MAYMOVE | FIXED, one),
                mremap(one, PAGE, PAGE, MAYMOVE | FIXED, 0x7FFF_FFFF_F000),
            ];
            syscall(11, window, 64 * PAGE, 0);
            syscall(11, one, PAGE, 0);
            let back = rss() - before;
            let last = mmap(0, PAGE, 3, 0x22);
            let in_place = mremap(last, PAGE, 2 * PAGE, 0, 0) as usize == last;
            syscall(11, last, if in_place { 2 * PAGE } else { PAGE }, 0);
            let mut big = vec![5u8; 2 << 20];
            big.resize(16 << 20, 6);
            let reallocated =
                big[..2 << 20].iter().all(|&b| b == 5) && big[2 << 20..].iter().all(|&b| b == 6);
            println!(
                "{stuck} {taken} {shrunk} {fixed} {refused:?} {back} {in_place} {reallocated}"
            );
        }
        // The stack, past what's mapped as a program starts: 4 MiB of calls
        // deep; written to 4 MiB below where it is, and read into 6 MiB
        // below, by read(2), past where the calls went; and a copy that
        // writes 100 MiB below, past where it can grow, which ends it.
        Some("deep-stack") => {
            #[inline(never)]
            fn dive(depth: u64) -> u64 {
                let mut frame = [0u8; 16 << 10];
                std::hint::black_box(&mut frame);
                frame[0] = depth as u8;
                if depth == 0 { 0 } else { dive(depth - 1) + u64::from(frame[0]) }
            }
            let dived = dive(256);
            let here = &raw const dived as usize;
            let written = (here - (4 << 20)) & !7;
            // Safety: where the stack grows to, which it will have to.
            unsafe { (written as *mut u64).write_volatile(0x5157) };
            let kept = unsafe { (written as *const u64).read_volatile() } == 0x5157;
            let (reader, mut writer) = or_fail("pipe", io::pipe());
            or_fail("pipe", writer.write_all(b"deep"));
            let into = (here - (6 << 20)) & !7;
            let read = syscall(0, reader.as_raw_fd() as usize, into, 4);
            // Safety: what read(2) just wrote.
            let landed = unsafe { std::slice::from_raw_parts(into as *const u8, 4) } == b"deep";
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                // Safety: past where the stack can grow, which ends it.
                unsafe { ((here - (100 << 20)) as *mut u8).write_volatile(1) };
                std::process::exit(0);
            }
            println!("{dived} {kept} {read} {landed} {}", wait(child).1);
        }
        // madvise: MADV_DONTNEED zeroing what it's given; advice taken and
        // nothing done; and what it refuses: an address not a page's,
        // memory not mapped, and advice there isn't; and nothing, taken.
        Some("madvise") => {
            let mmap = |len: usize| syscall5(9, [0, len, 3, 0x22, 0]) as usize;
            let at = mmap(2 * 4096);
            // Safety: the two pages just mapped.
            unsafe { std::ptr::write_bytes(at as *mut u8, 7, 2 * 4096) };
            let dropped = syscall(28, at, 2 * 4096, 4);
            // Safety: as above.
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, 2 * 4096) };
            let zeroed = bytes.iter().all(|&b| b == 0);
            let gone = mmap(4096);
            syscall(11, gone, 4096, 0);
            let refused = [
                syscall(28, at + 1, 4096, 4),
                syscall(28, gone, 4096, 4),
                syscall(28, at, 4096, 999),
            ];
            println!(
                "{dropped} {zeroed} {} {refused:?} {}",
                syscall(28, at, 4096, 0),
                syscall(28, at, 0, 4)
            );
        }
        // mmap of the file at PATH, which it makes, three pages and 100
        // bytes: from its second page, all of what's left, and zeros past
        // its end; MAP_SHARED, as the program can't write it; a private
        // copy written, which leaves the file as it was; and what it
        // refuses: an offset not a page's, a descriptor not open, a pipe,
        // a file open only to be written, and MAP_SHARED to write one open
        // only to be read.
        Some("file-mmap") => {
            let path = arg(2);
            let contents: Vec<u8> = (0..3 * 4096 + 100).map(|i: u32| (i % 251) as u8).collect();
            or_fail(&path, fs::write(&path, &contents));
            let file = or_fail(&path, fs::File::open(&path));
            let fd = file.as_raw_fd() as usize;
            let mmap =
                |len, prot, flags, fd, offset| syscall6(9, [0, len, prot, flags, fd], offset);
            // Safety: `len` bytes mapped at `at`.
            let bytes = |at: isize, len: usize| unsafe {
                std::slice::from_raw_parts(at as *const u8, len).to_vec()
            };
            let rest = mmap(3 * 4096, 1, 2, fd, 4096);
            let mapped = rest > 0
                && bytes(rest, 2 * 4096 + 100) == contents[4096..]
                && bytes(rest + 2 * 4096 + 100, 4096 - 100).iter().all(|&b| b == 0);
            let shared = mmap(4096, 1, 1, fd, 0);
            let shared = shared > 0 && bytes(shared, 4096) == contents[..4096];
            let copy = mmap(4096, 3, 2, fd, 0);
            // Safety: the page just mapped, to be written.
            unsafe { (copy as *mut u8).write_volatile(0xff) };
            let private = bytes(copy, 1) == [0xff] && or_fail(&path, fs::read(&path)) == contents;
            let (reader, _writer) = or_fail("pipe", io::pipe());
            let writing = or_fail(&path, OpenOptions::new().write(true).open(&path));
            let both = or_fail(&path, OpenOptions::new().read(true).write(true).open(&path));
            let shared_read = mmap(4096, 1, 1, both.as_raw_fd() as usize, 0) as usize;
            let refused = [
                mmap(4096, 1, 2, fd, 100),
                mmap(4096, 1, 2, 1000, 0),
                mmap(4096, 1, 2, reader.as_raw_fd() as usize, 0),
                mmap(4096, 1, 2, writing.as_raw_fd() as usize, 0),
                mmap(4096, 3, 1, fd, 0),
                // mprotect, PROT_READ | PROT_WRITE; again after PROT_READ.
                syscall(10, shared_read, 4096, 3),
                syscall(10, shared_read, 4096, 1).min(syscall(10, shared_read, 4096, 3)),
            ];
            println!("{mapped} {shared} {private} {refused:?}");
        }
        // A file's pages mapped read-only, privately and shared: whether what's
        // written to the file then shows in each, as Linux has it, and still
        // after MADV_DONTNEED; whether one mapped writable and written stays
        // the program's own; and whether the private one, made writable with
        // mprotect and written, does too, leaving the file and the shared
        // one as they were.
        Some("mapped-writes") => {
            let (prot_read, prot_write, map_shared, map_private) = (1, 2, 1, 2);
            let path = arg(2);
            or_fail(&path, fs::write(&path, [b'a'; 2 * 4096]));
            let file = or_fail(&path, OpenOptions::new().read(true).write(true).open(&path));
            let fd = file.as_raw_fd() as usize;
            let map = |prot, flags| syscall6(9, [0, 2 * 4096, prot, flags, fd], 0) as usize;
            let private = map(prot_read, map_private);
            let shared = map(prot_read, map_shared);
            let own = map(prot_read | prot_write, map_private);
            // Safety: the second of the two pages just mapped, to be written.
            unsafe { ((own + 4101) as *mut u8).write_volatile(b'o') };
            or_fail(&path, file.write_at(b"b", 4101));
            // Safety: in the second of the two pages each maps.
            let at = |mapped: usize| unsafe { ((mapped + 4101) as *const u8).read_volatile() };
            let seen = (at(private) == b'b', at(shared) == b'b');
            // MADV_DONTNEED.
            syscall(28, private, 2 * 4096, 4);
            let (after, kept) = (at(private) == b'b', at(own) == b'o');
            syscall(10, private, 2 * 4096, prot_read | prot_write);
            // Safety: the page just made writable.
            unsafe { ((private + 4101) as *mut u8).write_volatile(b'p') };
            let mut byte = [0];
            or_fail(&path, file.read_at(&mut byte, 4101));
            let apart = at(private) == b'p' && at(shared) == b'b' && byte == *b"b";
            println!("{} {} {after} {kept} {apart}", seen.0, seen.1);
        }
        // How much memory a copy made with fork takes, by sysinfo, while it's
        // there: under 4 MiB, as it is when the program's code is shared
        // rather than copied, however big that is?
        Some("fork-taken") => {
            let free = || {
                let mut info = [0u64; 14];
                syscall(99, info.as_mut_ptr() as usize, 0, 0);
                info[5] * (info[13] & 0xFFFF_FFFF)
            };
            let (ready, mut told) = or_fail("pipe", io::pipe());
            let before = free();
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                drop(ready);
                or_fail("pipe", told.write_all(b"x"));
                std::thread::sleep(Duration::from_millis(200));
                std::process::exit(0);
            }
            drop(told);
            or_fail("pipe", (&ready).read_exact(&mut [0]));
            let taken = before.saturating_sub(free());
            wait(child);
            println!("{}", taken < 4 << 20);
        }
        // mprotect: PROT_NONE, which keeps what's in the pages, out of reach,
        // which write(2) from them says, until they're given back; memory
        // mmap reserved with PROT_NONE given to the program, zeroed, but for
        // the guard page below it, as musl does with a thread's stack; and a
        // page made read-only, which read(2) can't fill but write(2) can
        // still copy from.
        Some("mprotect") => {
            const PAGE: usize = 4096;
            let mmap = |len: usize, prot: usize| syscall5(9, [0, len, prot, 0x22, 0]) as usize;
            let mprotect = |at: usize, len: usize, prot: usize| syscall(10, at, len, prot);
            let (reader, writer) = or_fail("pipe", io::pipe());
            let readable = |at: usize| syscall(1, writer.as_raw_fd() as usize, at, 1) == 1;
            // Safety: only ever of pages mapped for the program.
            let holds = |at: usize, len: usize, byte: u8| {
                let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
                bytes.iter().all(|&b| b == byte)
            };
            let at = mmap(2 * PAGE, 3);
            // Safety: the two pages just mapped.
            unsafe { std::ptr::write_bytes(at as *mut u8, 7, 2 * PAGE) };
            let hidden = [mprotect(at, 2 * PAGE, 0), readable(at) as isize];
            let back = mprotect(at, 2 * PAGE, 3) == 0 && holds(at, 2 * PAGE, 7);
            let stack = mmap(4 * PAGE, 0);
            let given =
                mprotect(stack + PAGE, 3 * PAGE, 3) == 0 && holds(stack + PAGE, 3 * PAGE, 0);
            // Safety: the three pages just given.
            unsafe { std::ptr::write_bytes((stack + PAGE) as *mut u8, 9, 3 * PAGE) };
            let used = holds(stack + PAGE, 3 * PAGE, 9) && !readable(stack);
            let read_only = mprotect(at, PAGE, 1);
            // A byte to read, so the read doesn't wait.
            syscall(1, writer.as_raw_fd() as usize, b"x".as_ptr() as usize, 1);
            let into = syscall(0, reader.as_raw_fd() as usize, at, 1);
            println!("{hidden:?} {back} {given} {used} {read_only} {into} {}", readable(at));
        }
        // getrandom, as Rust's std asks it for its hash maps' keys: as many
        // bytes as are asked for, whatever the flags but those there aren't,
        // and GRND_RANDOM with GRND_INSECURE; EFAULT into the kernel's half.
        // Then /dev/urandom and /dev/random: read, written to, and what they
        // are.
        Some("random") => {
            use std::os::unix::fs::FileTypeExt;
            let mut bytes = [[0u8; 32]; 2];
            let got = [0, 1, 2, 4, 5]
                .map(|flags| syscall(318, bytes[0].as_mut_ptr() as usize, 32, flags));
            syscall(318, bytes[1].as_mut_ptr() as usize, 32, 0);
            let differ = bytes[0] != bytes[1];
            let refused = [
                syscall(318, bytes[0].as_mut_ptr() as usize, 32, 8),
                syscall(318, bytes[0].as_mut_ptr() as usize, 32, 6),
                syscall(318, 0xFFFF_8000_0000_0000, 8, 0),
            ];
            let devices = ["/dev/urandom", "/dev/random"].map(|path| {
                let mut file = or_fail(path, OpenOptions::new().read(true).write(true).open(path));
                let mut read = [0u8; 64];
                or_fail(path, file.read_exact(&mut read));
                let written = or_fail(path, file.write(b"abc"));
                let metadata = or_fail(path, fs::metadata(path));
                let mode = metadata.permissions().mode() & 0o777;
                let device = metadata.file_type().is_char_device();
                format!("{} {written} {device} {mode:o}", read.iter().any(|&b| b != 0))
            });
            let mut map = std::collections::HashMap::new();
            map.insert("key", 42);
            println!("{got:?} {differ} {refused:?} {devices:?} {}", map["key"]);
        }
        // The *at calls given AT_FDCWD zero-extended, as rustix passes it,
        // which Linux reads as the int it is: PATH written, then opened and
        // read with openat, a link to it made with symlinkat and read with
        // readlinkat, and both removed with unlinkat.
        Some("at-fdcwd") => {
            const CWD: usize = 0xFFFF_FF9C;
            let path = arg(2);
            or_fail(&path, fs::write(&path, "through the working directory"));
            let name = std::ffi::CString::new(path.clone()).unwrap();
            let link = std::ffi::CString::new(format!("{path}.link")).unwrap();
            let (name, link) = (name.as_ptr() as usize, link.as_ptr() as usize);
            let fd = syscall(257, CWD, name, 0);
            let mut buf = [0u8; 64];
            let read = syscall(0, fd as usize, buf.as_mut_ptr() as usize, 64).max(0) as usize;
            let linked = syscall(266, name, CWD, link);
            let mut target = [0u8; 64];
            let len = syscall5(267, [CWD, link, target.as_mut_ptr() as usize, 64, 0]).max(0);
            let same = str_of(&target[..len as usize]) == path;
            let removed = [syscall(263, CWD, link, 0), syscall(263, CWD, name, 0)];
            println!("{} {:?} {linked} {same} {removed:?}", fd > 2, str_of(&buf[..read]));
        }
        // The extended attribute calls on PATH, by path, by path without
        // following a link, and by descriptor: get, list, set and remove;
        // then get of a path that isn't there and of a descriptor not open.
        Some("xattrs") => {
            let file = or_fail(&arg(2), fs::File::open(arg(2)));
            let fd = file.as_raw_fd() as usize;
            let path = std::ffi::CString::new(arg(2)).unwrap();
            let (path, name) = (path.as_ptr() as usize, c"user.probe".as_ptr() as usize);
            let mut buf = [0u8; 64];
            let buf = buf.as_mut_ptr() as usize;
            let calls = [
                syscall5(191, [path, name, buf, 64, 0]),
                syscall5(192, [path, name, buf, 64, 0]),
                syscall5(193, [fd, name, buf, 64, 0]),
                syscall(194, path, buf, 64),
                syscall(195, path, buf, 64),
                syscall(196, fd, buf, 64),
                syscall5(188, [path, name, buf, 1, 0]),
                syscall(197, path, name, 0),
                syscall5(191, [c"/missing".as_ptr() as usize, name, buf, 64, 0]),
                syscall5(193, [1000, name, buf, 64, 0]),
            ];
            println!("{calls:?}");
        }
        // fadvise64 on PATH: advice taken; and what it refuses, advice there
        // isn't, a pipe and a descriptor not open.
        Some("fadvise") => {
            let file = or_fail(&arg(2), fs::File::open(arg(2)));
            let (reader, _writer) = or_fail("pipe", io::pipe());
            let fadvise = |fd: usize, advice: usize| syscall5(221, [fd, 0, 0, advice, 0]);
            let fd = file.as_raw_fd() as usize;
            let results = [
                fadvise(fd, 4),
                fadvise(fd, 99),
                fadvise(reader.as_raw_fd() as usize, 0),
                fadvise(1000, 0),
            ];
            println!("{results:?}");
        }
        // socketpair, as Rust's Command makes one: each end reads what the
        // other writes, with write(2) and read(2) and with send and recv;
        // close-on-exec, as asked; a socket, to fstat; ready to be written
        // and not read, to poll. Once one end is closed the other reads
        // nothing, poll says so, and a send is EPIPE. Then what it refuses:
        // a family there isn't, a protocol, two types, and send and recv on
        // what isn't a socket.
        Some("socketpair") => {
            let socketpair = |domain: usize, kind: usize, protocol: usize, fds: &mut [i32; 2]| {
                syscall5(53, [domain, kind, protocol, fds.as_mut_ptr() as usize, 0])
            };
            let mut fds = [0i32; 2];
            let made = socketpair(1, 1 | 0o2000000, 0, &mut fds);
            let [a, b] = fds.map(|fd| fd as usize);
            let mut buf = [0u8; 16];
            let wrote = syscall(1, a, b"ping".as_ptr() as usize, 4);
            let read = syscall(0, b, buf.as_mut_ptr() as usize, 16);
            let sent = syscall6(44, [b, b"pong".as_ptr() as usize, 4, 0x4000, 0], 0);
            let got = syscall6(45, [a, buf[4..].as_mut_ptr() as usize, 12, 0, 0], 0);
            let cloexec = syscall(72, a, 1, 0);
            let mut stat = [0u8; 144];
            syscall(5, a, stat.as_mut_ptr() as usize, 0);
            let mode = u32::from_le_bytes(stat[24..28].try_into().unwrap());
            let socket = mode & 0o170000 == 0o140000;
            let poll = |fd: usize| {
                let mut pollfd = [0u8; 8];
                pollfd[..4].copy_from_slice(&(fd as i32).to_le_bytes());
                pollfd[4..6].copy_from_slice(&(1i16 | 4).to_le_bytes());
                syscall(7, pollfd.as_mut_ptr() as usize, 1, 0);
                i16::from_le_bytes(pollfd[6..8].try_into().unwrap())
            };
            let open = poll(a);
            syscall(3, b, 0, 0);
            let closed = poll(a);
            let eof = syscall(0, a, buf.as_mut_ptr() as usize, 16);
            let broken = syscall6(44, [a, b"ping".as_ptr() as usize, 4, 0x4000, 0], 0);
            let (reader, _writer) = or_fail("pipe", io::pipe());
            let pipe = reader.as_raw_fd() as usize;
            let refused = [
                socketpair(999, 1, 0, &mut fds),
                socketpair(1, 1, 5, &mut fds),
                socketpair(1, 4, 0, &mut fds),
                socketpair(1, 12, 0, &mut fds),
                syscall6(44, [pipe, b"ping".as_ptr() as usize, 4, 0, 0], 0),
                syscall6(45, [pipe, buf.as_mut_ptr() as usize, 4, 0, 0], 0),
            ];
            println!(
                "{made} {wrote} {read} {sent} {got} {:?} {cloexec} {socket} {open} {closed} \
                 {eof} {broken} {refused:?}",
                str_of(&buf[..8])
            );
        }
        // Rust's Command where posix_spawn won't do, as with a pre_exec
        // closure: fork, then exec, whose failure it hears of through a
        // socket pair. PROGRAM run with `hello`, then a program that isn't
        // there.
        Some("pre-exec") => {
            let run = |program: &str| {
                let mut command = Command::new(program);
                // Safety: the closure does nothing, which is safe after fork.
                unsafe { command.arg("hello").pre_exec(|| Ok(())) };
                command.output()
            };
            let ran = or_fail(&arg(2), run(&arg(2)));
            print!("{}", String::from_utf8_lossy(&ran.stdout));
            println!("{:?}", run("/missing").map(|_| ()).map_err(|error| error.raw_os_error()));
        }
        // That the LEN bytes at ADDR are i % 251 for each i: what tools/boot-test
        // puts in /bin/big, a segment bigger than the kernel's heap.
        Some("peek") => {
            let number = |n: usize| usize::from_str_radix(arg(n).trim_start_matches("0x"), 16);
            let (at, len) = (number(2).unwrap_or(0), number(3).unwrap_or(0));
            // Safety: what the program's own segment maps, as the caller says.
            let bytes = unsafe { std::slice::from_raw_parts(at as *const u8, len) };
            let wrong = bytes.iter().enumerate().position(|(i, &b)| b != (i % 251) as u8);
            println!("{}", wrong.map_or(String::from("ok"), |at| format!("wrong at {at}")));
        }
        // How the kernel says it loaded the program (the auxiliary vector):
        // whether AT_BASE is where this code is, as when this is another
        // program's interpreter (PT_INTERP), it being 0 otherwise; whether
        // the program AT_PHDR's headers are of is another copy, loaded
        // first; whether AT_ENTRY is that program's entry point; and whether
        // it says who the program runs as and that it isn't setuid, as
        // Linux always does, and musl's loader needs.
        Some("auxv") => {
            const PT_DYNAMIC: u32 = 2;
            const PT_LOAD: u32 = 1;
            const PT_PHDR: u32 = 6;
            let vector = auxiliary_vector();
            let auxv = |kind: usize| vector.iter().find(|(k, _)| *k == kind).map(|(_, v)| *v);
            let (phdr, phnum) = (auxv(AT_PHDR).unwrap(), auxv(AT_PHNUM).unwrap());
            // Safety: the program headers the kernel says are there.
            let headers = unsafe { std::slice::from_raw_parts(phdr as *const [u8; 56], phnum) };
            let field = |header: &[u8; 56], at: usize| {
                usize::from_le_bytes(header[at..at + 8].try_into().unwrap())
            };
            let find = |kind: u32| headers.iter().find(|h| h[..4] == kind.to_le_bytes());
            // This code's copy, by its own _DYNAMIC, as musl finds itself.
            let here = &raw const _DYNAMIC as usize - field(find(PT_DYNAMIC).unwrap(), 16);
            // Safety: the ELF header, at the start of the first segment.
            let elf = |at: usize| unsafe { ((here + at) as *const usize).read() };
            let (entry, phoff) = (elf(24), elf(32));
            // The first copy's, by where its program headers are: as PT_PHDR
            // says; or, as GNU ld leaves that out, e_phoff into the segment
            // the file starts with, as the kernel and musl find them. The two
            // copies are the same program, with the same e_phoff.
            let at_start = || {
                let load = |h: &&[u8; 56]| h[..4] == PT_LOAD.to_le_bytes() && field(h, 8) == 0;
                headers.iter().find(load).map(|h| field(h, 16) + phoff)
            };
            let first = phdr - find(PT_PHDR).map(|h| field(h, 16)).or_else(at_start).unwrap();
            let ids = [AT_UID, AT_EUID, AT_GID, AT_EGID, AT_SECURE].map(auxv);
            println!(
                "{} {} {} {}",
                auxv(AT_BASE) == Some(here),
                first != here,
                auxv(AT_ENTRY) == Some(first + entry),
                ids.iter().all(Option::is_some),
            );
        }
        Some("exit") => std::process::exit(arg(2).parse().unwrap_or(0)),
        Some("mkdir") => or_fail(&arg(2), fs::create_dir(arg(2))),
        Some("mkdir-p") => or_fail(&arg(2), fs::create_dir_all(arg(2))),
        Some("rm") => or_fail(&arg(2), fs::remove_file(arg(2))),
        Some("rmdir") => or_fail(&arg(2), fs::remove_dir(arg(2))),
        Some("rm-r") => or_fail(&arg(2), fs::remove_dir_all(arg(2))),
        Some("mv") => or_fail(&arg(2), fs::rename(arg(2), arg(3))),
        // mv, but never over what's there: renameat2's RENAME_NOREPLACE.
        Some("mv-n") => {
            let (from, to) =
                (std::ffi::CString::new(arg(2)).unwrap(), std::ffi::CString::new(arg(3)).unwrap());
            let at_fdcwd = -100isize as usize;
            let result = syscall5(
                316,
                [at_fdcwd, from.as_ptr() as usize, at_fdcwd, to.as_ptr() as usize, 1],
            );
            println!("{result}");
        }
        // Go to DIR, say where that is, then print PATH from there.
        Some("cd") => {
            or_fail(&arg(2), std::env::set_current_dir(arg(2)));
            println!("{}", or_fail(&arg(2), std::env::current_dir()).display());
            let contents = or_fail(&arg(3), fs::read(arg(3)));
            or_fail(&arg(3), io::stdout().write_all(&contents));
        }
        // fchdir: to DIR, open, which getcwd then says; and not to FILE,
        // nor to a descriptor that isn't open.
        Some("fchdir") => {
            let dir = or_fail(&arg(2), fs::File::open(arg(2)));
            let moved = syscall(81, dir.as_raw_fd() as usize, 0, 0);
            let here = or_fail(&arg(2), std::env::current_dir());
            let file = or_fail(&arg(3), fs::File::open(arg(3)));
            let refused = (syscall(81, file.as_raw_fd() as usize, 0, 0), syscall(81, 1000, 0, 0));
            println!("{moved} {} {refused:?}", here.display());
        }
        // select and pselect6: an empty pipe, not ready, at once and after a
        // while; one with a byte in it, ready to be read, its writing end
        // to be written, and nothing exceptional; a descriptor that isn't
        // open, and a time that isn't one; then pselect6 as busybox's
        // shells use ppoll, every signal blocked but while it waits, which
        // SIGALRM ends, the mask from before back.
        Some("select") => {
            let (reader, mut writer) = or_fail("pipe", io::pipe());
            let (read, write) = (reader.as_raw_fd() as usize, writer.as_raw_fd() as usize);
            // fd_sets of the first 64 descriptors, all one word needs.
            let select = |sets: &mut [u64; 3], timeout: Option<[i64; 2]>| {
                let at = |set: &mut u64| set as *mut u64 as usize;
                let time = timeout.as_ref().map_or(0, |time| time.as_ptr() as usize);
                let [r, w, e] = sets;
                syscall5(23, [64, at(r), at(w), at(e), time])
            };
            let mut sets = [1 << read, 0, 1 << read];
            let now = (select(&mut sets, Some([0, 0])), sets);
            let start = Instant::now();
            let mut sets = [1 << read, 0, 0];
            let later = select(&mut sets, Some([0, 50_000]));
            let waited = start.elapsed() >= Duration::from_millis(50);
            or_fail("pipe", writer.write_all(b"x"));
            let mut sets = [1 << read, 1 << write, 1 << read | 1 << write];
            let ready = (select(&mut sets, None), sets == [1 << read, 1 << write, 0]);
            let refused = (
                select(&mut [1 << 60, 0, 0], Some([0, 0])),
                select(&mut [1 << read, 0, 0], Some([0, -1])),
            );
            or_fail("pipe", (&reader).read_exact(&mut [0]));
            sigaction(SIGALRM, record, SA_SIGINFO);
            sigprocmask(SIG_BLOCK, !0);
            let all = sigprocmask(SIG_BLOCK, 0);
            let none = 0u64;
            let mask = [&raw const none as usize, 8];
            syscall(38, 0, [0u64, 0, 0, 50_000].as_ptr() as usize, 0);
            let mut set = 1u64 << read;
            let args = [64, &raw mut set as usize, 0, 0, 0];
            let pselected = syscall6(270, args, mask.as_ptr() as usize);
            let restored = sigprocmask(SIG_BLOCK, 0) == all && MASK.load(Ordering::SeqCst) == all;
            println!(
                "{now:?} {later} {waited} {ready:?} {refused:?} {pselected} {} {restored}",
                CAUGHT.load(Ordering::SeqCst)
            );
        }
        // A pipe within the program: through it and out, then into one no
        // one reads any more.
        Some("pipe") => {
            let (mut reader, mut writer) = or_fail("pipe", io::pipe());
            or_fail("pipe", writer.write_all(b"through a pipe\n"));
            drop(writer);
            print!("{}", or_fail("pipe", io::read_to_string(&mut reader)));
            let (reader, mut writer) = or_fail("pipe", io::pipe());
            drop(reader);
            match writer.write_all(b"to no one") {
                Ok(()) => println!("written to no one"),
                Err(error) => println!("{error}"),
            }
        }
        // Standard output made a copy of PATH's descriptor, with dup2, then
        // TEXT printed: into PATH.
        Some("dup-out") => {
            let file = or_fail(&arg(2), fs::File::create(arg(2)));
            let copied = syscall(33, file.as_raw_fd() as usize, 1, 0);
            println!("{} {copied}", arg(3));
        }
        // dup of a descriptor that isn't open, dup2 to one past the most
        // there can be, pipe2 into the kernel's half, and pipe2 with a flag
        // it doesn't take.
        Some("bad-pipes") => {
            let closed = syscall(32, 1000, 0, 0);
            let far = syscall(33, 1, 5000, 0);
            let kernel = syscall(293, 0xFFFF_8000_0000_0000, 0, 0);
            let mut fds = [0i32; 2];
            let unknown = syscall(293, fds.as_mut_ptr() as usize, 0o1, 0);
            println!("{closed} {far} {kernel} {unknown}");
        }
        // COUNT copies writing a megabyte each into a pipe that's full and
        // that no one reads, waiting, until they're killed: how many were.
        Some("pipe-writers") => {
            let count: usize = arg(2).parse().unwrap_or(16);
            let (_reader, writer) = or_fail("pipe", io::pipe());
            or_fail("pipe", (&writer).write_all(&[0; 64 << 10]));
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let children: Vec<isize> = (0..count)
                .map(|_| {
                    let child = syscall(57, 0, 0, 0);
                    if child == 0 {
                        let megabyte = vec![1u8; 1 << 20];
                        or_fail("pipe", (&ready_writer).write_all(b"r"));
                        let fd = writer.as_raw_fd() as usize;
                        syscall(1, fd, megabyte.as_ptr() as usize, megabyte.len());
                        std::process::exit(0);
                    }
                    child
                })
                .collect();
            or_fail("pipe", ready.read_exact(&mut vec![0; count]));
            std::thread::sleep(Duration::from_millis(200));
            children.iter().for_each(|&child| _ = kill(child, SIGKILL));
            let killed = children.iter().filter(|&&child| wait(child).1 == "signal: 9").count();
            println!("{killed}");
        }
        // One write of more than a pipe holds, all of it, as a thread reads
        // it out: how much was written, and read.
        Some("pipe-whole") => {
            let (mut reader, writer) = or_fail("pipe", io::pipe());
            let reading = std::thread::spawn(move || {
                let mut read = Vec::new();
                or_fail("pipe", reader.read_to_end(&mut read));
                read.len()
            });
            let data = vec![7u8; 200 << 10];
            let written =
                syscall(1, writer.as_raw_fd() as usize, data.as_ptr() as usize, data.len());
            drop(writer);
            println!("{written} {}", reading.join().unwrap());
        }
        // O_NONBLOCK: an empty pipe read, a pipe written until it's full,
        // F_GETFL of its writing end; then standard input made not to wait,
        // with FIONBIO, and read.
        Some("nonblocking") => {
            let mut fds = [0i32; 2];
            syscall(293, fds.as_mut_ptr() as usize, 0o4000, 0);
            let (read, write) = (fds[0] as usize, fds[1] as usize);
            let mut buf = [0u8; 4096];
            let empty = syscall(0, read, buf.as_mut_ptr() as usize, buf.len());
            let mut filled = 0;
            let full = loop {
                match syscall(1, write, buf.as_ptr() as usize, buf.len()) {
                    written if written > 0 => filled += written,
                    error => break error,
                }
            };
            let flags = syscall(72, write, 3, 0);
            let on = 1i32;
            syscall(16, 0, 0x5421, &raw const on as usize);
            let typed = syscall(0, 0, buf.as_mut_ptr() as usize, buf.len());
            println!("{empty} {filled} {full} {flags} {typed}");
        }
        // A copy of the program, with fork, which changes its memory and
        // ends; then the original's memory, as it was, and how the copy
        // ended, as wait4 says.
        Some("fork") => {
            let mut on_stack = 1;
            let mut on_heap = Box::new(1);
            let parent = syscall(39, 0, 0, 0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                on_stack += 1;
                *on_heap += 1;
                let (me, my_parent) = (syscall(39, 0, 0, 0), syscall(110, 0, 0, 0));
                println!("child: {on_stack} {} {} {}", on_heap, me != parent, my_parent == parent);
                std::process::exit(7);
            }
            let (waited, status) = wait(child);
            println!("parent: {on_stack} {} {} {status}", on_heap, waited == child);
        }
        // Every register a system call keeps set to a value of its own, then
        // fork: whether each side has them back as they were.
        Some("fork-registers") => {
            let mut seen = [0u64; 11];
            let child: isize;
            // Safety: rbx and rbp, which can't be named, are put back;
            // `seen` is what's written.
            unsafe {
                std::arch::asm!(
                    "push rbx",
                    "push rbp",
                    "mov rbx, 1",
                    "mov rbp, 2",
                    "mov rsi, 3",
                    "mov rdx, 4",
                    "mov r8, 5",
                    "mov r9, 6",
                    "mov r10, 7",
                    "mov r12, 8",
                    "mov r13, 9",
                    "mov r14, 10",
                    "mov r15, 11",
                    "syscall",
                    "mov [rdi], rbx",
                    "mov [rdi + 8], rbp",
                    "mov [rdi + 16], rsi",
                    "mov [rdi + 24], rdx",
                    "mov [rdi + 32], r8",
                    "mov [rdi + 40], r9",
                    "mov [rdi + 48], r10",
                    "mov [rdi + 56], r12",
                    "mov [rdi + 64], r13",
                    "mov [rdi + 72], r14",
                    "mov [rdi + 80], r15",
                    "pop rbp",
                    "pop rbx",
                    in("rdi") seen.as_mut_ptr(),
                    inlateout("rax") 57isize => child,
                    out("rcx") _,
                    out("rdx") _,
                    out("rsi") _,
                    out("r8") _,
                    out("r9") _,
                    out("r10") _,
                    out("r11") _,
                    out("r12") _,
                    out("r13") _,
                    out("r14") _,
                    out("r15") _,
                )
            };
            let kept = seen == std::array::from_fn(|i| i as u64 + 1);
            if child == 0 {
                println!("child: {kept}");
                std::process::exit(0);
            }
            println!("parent: {kept} {}", wait(child).1);
        }
        // A copy that faults, which ends it alone.
        Some("fork-fault") => {
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                unsafe { std::ptr::null_mut::<u8>().write_volatile(1) };
            }
            println!("{}", wait(child).1);
        }
        // COUNT copies one after the other, each waited for: how many were
        // made. Each copies all of the program's memory, so the kernel had
        // better free them.
        Some("forks") => {
            let count: usize = arg(2).parse().unwrap_or(1);
            for made in 0..count {
                let child = syscall(57, 0, 0, 0);
                if child == 0 {
                    std::process::exit(0);
                }
                if child < 0 || wait(child).0 != child {
                    println!("{made}: {child}");
                    std::process::exit(1);
                }
            }
            println!("{count}");
        }
        // wait4: for a child still running, with WNOHANG, then without; and
        // once there are none.
        Some("waits") => {
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::thread::sleep(Duration::from_millis(100));
                std::process::exit(3);
            }
            let early = syscall(61, child as usize, 0, 1);
            let (waited, status) = wait(-1);
            let none = syscall(61, -1isize as usize, 0, 0);
            println!("{early} {} {status} {none}", waited == child);
        }
        // What the kernel must refuse: clone asked for a thread, and execve
        // of a path, then of arguments, in its half. The program goes on.
        Some("bad-processes") => {
            let kernel = 0xFFFF_8000_0000_0000;
            let thread = syscall(56, 0x10000 | 17, 0, 0);
            let path = syscall(59, kernel, 0, 0);
            let probe = c"/bin/probe";
            let args = syscall(59, probe.as_ptr() as usize, kernel, 0);
            let pointers = [kernel, 0];
            let arg = syscall(59, probe.as_ptr() as usize, pointers.as_ptr() as usize, 0);
            println!("{thread} {path} {args} {arg}");
        }
        // Become PROGRAM, with ARGS, and FROM=exec in its environment.
        Some("exec") => {
            let error =
                Command::new(arg(2)).args(std::env::args().skip(3)).env("FROM", "exec").exec();
            or_fail::<()>(&arg(2), Err(error));
        }
        // exec, from DIR.
        Some("exec-in") => {
            or_fail(&arg(2), std::env::set_current_dir(arg(2)));
            let error = Command::new(arg(3)).args(std::env::args().skip(4)).exec();
            or_fail::<()>(&arg(3), Err(error));
        }
        // Its arguments and environment.
        Some("env") => {
            println!("{:?}", std::env::args().skip(1).collect::<Vec<_>>());
            std::env::vars().for_each(|(name, value)| println!("{name}={value}"));
        }
        // Run PROGRAM with ARGS, and say how it ended.
        Some("spawn") => {
            let status = Command::new(arg(2)).args(std::env::args().skip(3)).status();
            println!("{}", or_fail(&arg(2), status));
        }
        // Run PROGRAM with ARGS, TEXT its standard input and its standard
        // output read back, both through pipes: then what came back, and
        // how it ended.
        Some("feed") => {
            let mut child = or_fail(
                &arg(3),
                Command::new(arg(3))
                    .args(std::env::args().skip(4))
                    .stdin(Stdio::piped())
                    .stdout(Stdio::piped())
                    .spawn(),
            );
            let mut stdin = child.stdin.take().unwrap();
            or_fail("stdin", stdin.write_all(arg(2).as_bytes()));
            drop(stdin);
            let output = or_fail("stdout", io::read_to_string(child.stdout.take().unwrap()));
            print!("{output}");
            println!("{}", or_fail(&arg(3), child.wait()));
        }
        // Whether standard input and output are terminals, and a pipe; the
        // terminal's size, as TIOCGWINSZ says; and its name, as musl's
        // `ttyname` reads it, from the link /proc/self/fd/0.
        Some("tty") => {
            use std::io::IsTerminal;
            let mut size = [0u16; 4];
            let got = syscall(16, 1, 0x5413, size.as_mut_ptr() as usize);
            let pipe = std::os::fd::OwnedFd::from(or_fail("pipe", io::pipe()).0);
            let (stdin, stdout) = (io::stdin().is_terminal(), io::stdout().is_terminal());
            let mut name = [0u8; 64];
            let link = c"/proc/self/fd/0".as_ptr() as usize;
            let len = syscall(89, link, name.as_mut_ptr() as usize, name.len()).max(0) as usize;
            let name = String::from_utf8_lossy(&name[..len]);
            let (pipe, rows, cols) = (pipe.is_terminal(), size[0], size[1]);
            println!("{stdin} {stdout} {pipe} {rows}x{cols} {got} {name}");
        }
        // Standard input in raw mode, as cfmakeraw makes it: COUNT bytes
        // read as they come, each read's shown; then as it was.
        Some("raw") => {
            let saved = raw_mode();
            println!("raw");
            let count: usize = arg(2).parse().unwrap_or(1);
            let mut got = 0;
            while got < count {
                let mut buf = [0u8; 16];
                let read = syscall(0, 0, buf.as_mut_ptr() as usize, buf.len()).max(0) as usize;
                println!("{:?}", &buf[..read]);
                got += read.max(1);
            }
            syscall(16, 0, TCSETS, saved.as_ptr() as usize);
            println!("cooked");
        }
        // Where the terminal says its cursor is, asked as programs do: with
        // ESC [ 6 n, the answer coming back as if typed, in raw mode.
        Some("cursor") => {
            let saved = raw_mode();
            or_fail("stdout", io::stdout().write_all(b"\x1b[6n"));
            or_fail("stdout", io::stdout().flush());
            let mut answer = Vec::new();
            while answer.last() != Some(&b'R') && answer.len() < 32 {
                let mut byte = [0u8];
                if syscall(0, 0, byte.as_mut_ptr() as usize, 1) <= 0 {
                    break;
                }
                answer.push(byte[0]);
            }
            syscall(16, 0, TCSETS, saved.as_ptr() as usize);
            println!("{:?}", String::from_utf8_lossy(&answer));
        }
        // Text in each colour and attribute a terminal has.
        Some("colors") => {
            for (first, base) in [(30, "dim"), (90, "bright")] {
                let names = ["black", "red", "green", "yellow", "blue", "magenta", "cyan", "white"];
                for (n, name) in names.iter().enumerate() {
                    print!("\x1b[{}m{name} \x1b[{}m{name}\x1b[0m ", first + n, first + n + 10);
                }
                println!("({base})");
            }
            println!("\x1b[1mbold\x1b[0m \x1b[4munderlined\x1b[0m \x1b[7mreversed\x1b[0m plain");
            let ramp: String = (0..24).map(|n| format!("\x1b[48;5;{}m ", 232 + n)).collect();
            println!("{ramp}\x1b[0m");
        }
        // poll on a pipe: with nothing in it, at once and then for 50 ms;
        // then until a copy writes; then once it's gone and that's read. What
        // each returned, and the last two's revents.
        Some("poll") => {
            let (reader, writer) = or_fail("pipe", io::pipe());
            let fd = reader.as_raw_fd();
            let poll = |timeout: i32| {
                // `struct pollfd`: the fd, then POLLIN asked for, then revents.
                let mut entry = [fd, 1];
                let ready = syscall(7, entry.as_mut_ptr() as usize, 1, timeout as usize);
                (ready, entry[1] >> 16)
            };
            let (now, later) = (poll(0).0, poll(50).0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::thread::sleep(Duration::from_millis(50));
                or_fail("pipe", (&writer).write_all(b"x"));
                std::process::exit(0);
            }
            let written = poll(-1);
            wait(child);
            drop(writer);
            or_fail("pipe", (&reader).read_exact(&mut [0]));
            let closed = poll(-1);
            println!("{now} {later} {written:?} {closed:?}");
        }
        // Run PROGRAM with ARGS, collecting its output: then what it wrote to
        // each, and how it ended.
        Some("output") => {
            let output = Command::new(arg(2)).args(std::env::args().skip(3)).output();
            let output = or_fail(&arg(2), output);
            print!("{}", String::from_utf8_lossy(&output.stdout));
            println!("{:?} {}", String::from_utf8_lossy(&output.stderr), output.status);
        }
        // Signals within the program: a handler, told who sent it; blocked,
        // then not; sent by tkill; run once; ignored; SIGKILL, which can be
        // neither caught nor blocked; and what kill refuses.
        Some("signals") => {
            let me = getpid();
            let set = sigaction(SIGUSR1, record, SA_SIGINFO);
            let sent = kill(me, SIGUSR1);
            let first = (CAUGHT.load(Ordering::SeqCst), LAST.load(Ordering::SeqCst));
            let (from, code) = (FROM.load(Ordering::SeqCst) == me, CODE.load(Ordering::SeqCst));
            sigprocmask(SIG_BLOCK, bit(SIGUSR1));
            kill(me, SIGUSR1);
            let mut pending = 0u64;
            syscall(127, &raw mut pending as usize, 8, 0);
            // Pending but blocked, it doesn't end a sleep.
            let nap = [0i64, 20_000_000];
            let slept = syscall(35, nap.as_ptr() as usize, 0, 0);
            let blocked = CAUGHT.load(Ordering::SeqCst);
            sigprocmask(SIG_UNBLOCK, bit(SIGUSR1));
            let unblocked = CAUGHT.load(Ordering::SeqCst);
            sigaction(SIGUSR2, record, SA_SIGINFO | SA_RESETHAND);
            syscall(200, me as usize, SIGUSR2, 0);
            let (tkill, reset) = (CODE.load(Ordering::SeqCst), handler_of(SIGUSR2));
            set_action(SIGUSR1, SIG_IGN, 0);
            kill(me, SIGUSR1);
            let ignored = CAUGHT.load(Ordering::SeqCst);
            let catch_kill = sigaction(SIGKILL, record, 0);
            sigprocmask(SIG_BLOCK, bit(SIGKILL));
            let kill_blocked = sigprocmask(SIG_BLOCK, 0) & bit(SIGKILL) != 0;
            let refused = (kill(me, 65), kill(999_999, SIGUSR1), kill(me, 0));
            println!(
                "{set} {sent} {first:?} {from} {code}; {} {slept} {blocked} {unblocked}; {tkill} \
                 {reset} {ignored}; {catch_kill} {kill_blocked} {refused:?}",
                pending == bit(SIGUSR1)
            );
        }
        // A signal for a copy spinning with no system calls, which the timer
        // interrupt brings it: every register it had, and the SSE ones, are
        // as they were once the handler, which changes them, is done.
        Some("signal-registers") => {
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                sigaction(SIGUSR1, clobber, SA_SIGINFO);
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                std::process::exit(if spin_until_flagged() { 0 } else { 1 });
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
            std::thread::sleep(Duration::from_millis(30));
            kill(child, SIGUSR1);
            println!("{}", wait(child).1);
        }
        // Calls a signal ends: a read, with EINTR, unless the handler asked
        // for SA_RESTART; a sleep, with what was left of it.
        Some("interrupted") => {
            let parent = getpid();
            let (reader, writer) = or_fail("pipe", io::pipe());
            // A copy that signals, then writes.
            let poke = |write: bool| {
                let child = syscall(57, 0, 0, 0);
                if child == 0 {
                    std::thread::sleep(Duration::from_millis(50));
                    kill(parent, SIGUSR1);
                    std::thread::sleep(Duration::from_millis(50));
                    if write {
                        or_fail("pipe", (&writer).write_all(b"x"));
                    }
                    std::process::exit(0);
                }
                child
            };
            let mut byte = [0u8];
            let read = |byte: &mut [u8; 1]| {
                syscall(0, reader.as_raw_fd() as usize, byte.as_mut_ptr() as usize, 1)
            };
            sigaction(SIGUSR1, record, SA_SIGINFO);
            let child = poke(true);
            let plain = read(&mut byte);
            wait(child);
            read(&mut byte);
            sigaction(SIGUSR1, record, SA_SIGINFO | SA_RESTART);
            let child = poke(true);
            let restarted = read(&mut byte);
            wait(child);
            let child = poke(false);
            let (request, mut left) = ([5i64, 0], [0i64; 2]);
            let slept = syscall(35, request.as_ptr() as usize, left.as_mut_ptr() as usize, 0);
            wait(child);
            println!("{plain} {restarted} {slept} {}", (4..=5).contains(&left[0]));
        }
        // How copies end, sent: SIGTERM, by default; SIGUSR1, ignored, so
        // not; SIGKILL, though every signal is blocked; and SIGPIPE, by
        // default, as a pipe no one reads is written.
        Some("endings") => {
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let mut child = |setup: &dyn Fn()| {
                let child = syscall(57, 0, 0, 0);
                if child == 0 {
                    setup();
                    or_fail("pipe", (&ready_writer).write_all(b"r"));
                    std::thread::sleep(Duration::from_millis(100));
                    std::process::exit(5);
                }
                or_fail("pipe", ready.read_exact(&mut [0]));
                child
            };
            let mut ended = Vec::new();
            for (setup, signal) in [
                (&(|| {}) as &dyn Fn(), SIGTERM),
                (&|| _ = set_action(SIGUSR1, SIG_IGN, 0), SIGUSR1),
                (&|| _ = sigprocmask(SIG_SETMASK, !0), SIGKILL),
            ] {
                let pid = child(setup);
                kill(pid, signal);
                ended.push(wait(pid).1);
            }
            let pid = syscall(57, 0, 0, 0);
            if pid == 0 {
                set_action(SIGPIPE, SIG_DFL, 0);
                let (reader, mut writer) = or_fail("pipe", io::pipe());
                drop(reader);
                let _ = writer.write_all(b"to no one");
                std::process::exit(0);
            }
            ended.push(wait(pid).1);
            println!("{}", ended.join(", "));
        }
        // A copy stopped, continued and killed, as wait4 tells it and its
        // SIGCHLD's si_code does.
        Some("stops") => {
            sigaction(SIGCHLD, record, SA_SIGINFO | SA_RESTART);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            let code = || CODE.load(Ordering::SeqCst);
            kill(child, SIGSTOP);
            let stopped = (wait_status(child, WUNTRACED), code());
            kill(child, SIGCONT);
            let continued = (wait_status(child, WCONTINUED), code());
            kill(child, SIGKILL);
            let killed = (wait_status(child, 0), code());
            println!(
                "{:#x} {} {:#x} {} {} {}",
                stopped.0, stopped.1, continued.0, continued.1, killed.0, killed.1
            );
        }
        // Stopped by its own SIGSTOP, with no one to continue it but whoever
        // waits for it.
        Some("stop-self") => {
            kill(getpid(), SIGSTOP);
            println!("continued");
        }
        // rt_sigsuspend: what's blocked let through while it waits, and
        // blocked again once the handler's run.
        Some("suspend") => {
            let parent = getpid();
            sigaction(SIGUSR1, record, SA_SIGINFO);
            sigprocmask(SIG_BLOCK, bit(SIGUSR1));
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::thread::sleep(Duration::from_millis(30));
                kill(parent, SIGUSR1);
                std::process::exit(0);
            }
            let nothing = 0u64;
            let suspended = syscall(130, &raw const nothing as usize, 8, 0);
            let blocked = sigprocmask(SIG_BLOCK, 0) & bit(SIGUSR1) != 0;
            // The mask the handler's return puts back: the one from before.
            let saved = MASK.load(Ordering::SeqCst) & bit(SIGUSR1) != 0;
            wait(child);
            println!("{suspended} {} {blocked} {saved}", CAUGHT.load(Ordering::SeqCst));
        }
        // Process groups: its own, leading its session; a copy in a group of
        // its own, signalled as a group; what setsid and setpgid refuse.
        Some("groups") => {
            let me = getpid();
            let (group, session) = (syscall(121, 0, 0, 0), syscall(124, 0, 0, 0));
            let new_session = syscall(112, 0, 0, 0);
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                syscall(109, 0, 0, 0);
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
            let own_group = syscall(121, child as usize, 0, 0) == child;
            let unknown_group = syscall(109, child as usize, 999_999, 0);
            let sent = kill(-child, SIGTERM);
            let ended = wait(child).1;
            let gone = kill(-child, SIGTERM);
            println!(
                "{} {} {new_session} {own_group} {unknown_group} {sent} {ended} {gone}",
                group == me,
                session == me
            );
        }
        // A handler that asks for the alternate stack runs on it, and
        // sigaltstack says so there.
        Some("altstack") => {
            let stack = vec![0u8; 64 * 1024];
            let range = stack.as_ptr() as usize..stack.as_ptr() as usize + stack.len();
            let given = [stack.as_ptr() as u64, 0, stack.len() as u64];
            let set = syscall(131, given.as_ptr() as usize, 0, 0);
            sigaction(SIGUSR1, on_altstack, SA_SIGINFO | SA_ONSTACK);
            kill(getpid(), SIGUSR1);
            let mut after = [0u64; 3];
            syscall(131, 0, after.as_mut_ptr() as usize, 0);
            let on_it = range.contains(&WHERE.load(Ordering::SeqCst));
            println!("{set} {on_it} {} {}", ALT_FLAGS.load(Ordering::SeqCst), after[1]);
        }
        // An alternate stack at the very end of memory, where no frame fits:
        // a copy sent a signal for a handler there ends with SIGSEGV.
        Some("far-altstack") => {
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let given = [u64::MAX - 0xFFF, 0, 0x10000];
                let set = syscall(131, given.as_ptr() as usize, 0, 0);
                sigaction(SIGUSR1, record, SA_SIGINFO | SA_ONSTACK);
                kill(getpid(), SIGUSR1);
                std::process::exit(if set == 0 { 0 } else { 1 });
            }
            println!("{}", wait(child).1);
        }
        // ppoll as busybox's shells wait in `read`: with every signal
        // blocked but while it waits, so one that comes then ends it, with
        // EINTR, and the mask from before back once its handler's done. One
        // that finds input ready puts it back too; and a mask of the wrong
        // size is refused.
        Some("ppoll") => {
            sigaction(SIGALRM, record, SA_SIGINFO);
            let (reader, mut writer) = or_fail("pipe", io::pipe());
            sigprocmask(SIG_BLOCK, !0);
            let all = sigprocmask(SIG_BLOCK, 0);
            let none = 0u64;
            let mut entry = [reader.as_raw_fd(), 1];
            let ppoll = |entry: &mut [i32; 2], size: usize| {
                let mask = &raw const none as usize;
                syscall5(271, [entry.as_mut_ptr() as usize, 1, 0, mask, size])
            };
            syscall(38, 0, [0u64, 0, 0, 50_000].as_ptr() as usize, 0);
            let waited = ppoll(&mut entry, 8);
            let caught = CAUGHT.load(Ordering::SeqCst);
            let restored = sigprocmask(SIG_BLOCK, 0) == all;
            let saved = MASK.load(Ordering::SeqCst) == all;
            or_fail("pipe", writer.write_all(b"x"));
            let ready = ppoll(&mut entry, 8);
            let kept = sigprocmask(SIG_BLOCK, 0) == all;
            let wrong_size = ppoll(&mut entry, 4);
            println!("{waited} {caught} {restored} {saved} {ready} {kept} {wrong_size}");
        }
        // Wait for SIGINT or SIGQUIT, from a terminal: which came, and
        // whether from the kernel.
        Some("catch") => {
            sigaction(SIGINT, record, SA_SIGINFO);
            sigaction(SIGQUIT, record, SA_SIGINFO);
            println!("ready");
            while CAUGHT.load(Ordering::SeqCst) == 0 {
                syscall(34, 0, 0, 0);
            }
            let kernel = CODE.load(Ordering::SeqCst) == 0x80;
            println!("caught {}, from the kernel: {kernel}", LAST.load(Ordering::SeqCst));
        }
        // A copy in a group of its own put in the terminal's foreground, as
        // a shell puts a job: whatever the terminal sends then is its. How
        // it ended, once the program's back in the foreground.
        Some("foreground") => {
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                syscall(109, 0, 0, 0);
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                loop {
                    syscall(34, 0, 0, 0);
                }
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
            let group = child as i32;
            let set = syscall(16, 0, 0x5410, &raw const group as usize);
            let mut front = 0i32;
            syscall(16, 0, 0x540F, &raw mut front as usize);
            println!("{set} {}", front == group);
            let ended = wait(child).1;
            let me = getpid() as i32;
            syscall(16, 0, 0x5410, &raw const me as usize);
            println!("{ended}");
        }
        // Leave a copy behind, which says so when SIGHUP comes: as the
        // session's leader ends, its terminal hangs up on its foreground.
        Some("hangup") => {
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                sigaction(SIGHUP, hung_up, SA_SIGINFO);
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                loop {
                    syscall(34, 0, 0, 0);
                }
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
        }
        // End with PATH open, for the kernel to close.
        Some("hold") => std::mem::forget(or_fail(&arg(2), fs::File::open(arg(2)))),
        // Files, through std::fs, as any program would use them.
        // With no paths, or `-`, standard input, as it comes.
        Some("cat") => {
            let mut paths: Vec<String> = std::env::args().skip(2).collect();
            if paths.is_empty() {
                paths.push(String::from("-"));
            }
            for path in paths {
                if path == "-" {
                    let mut chunk = vec![0; 64 << 10];
                    loop {
                        let read = or_fail(&path, io::stdin().read(&mut chunk));
                        if read == 0 {
                            break;
                        }
                        or_fail(&path, io::stdout().write_all(&chunk[..read]));
                    }
                    continue;
                }
                let contents = or_fail(&path, fs::read(&path));
                or_fail(&path, io::stdout().write_all(&contents));
            }
        }
        // The directory at PATH opened again and again, none closed, until
        // it can't be, or a thousand times: how many, and the errno.
        Some("open-dirs") => {
            let path = std::ffi::CString::new(arg(2)).unwrap();
            const O_DIRECTORY: usize = 0o200000;
            let mut opened = 0;
            let mut error = 0;
            while opened < 1000 && error == 0 {
                match syscall(2, path.as_ptr() as usize, O_DIRECTORY, 0) {
                    fd if fd >= 0 => opened += 1,
                    errno => error = errno,
                }
            }
            println!("{opened} {error}");
        }
        Some("ls") => {
            let path = arg(2);
            let mut names: Vec<String> = or_fail(&path, fs::read_dir(&path))
                .map(|entry| {
                    let entry = or_fail(&path, entry);
                    let slash = if or_fail(&path, entry.file_type()).is_dir() { "/" } else { "" };
                    format!("{}{slash}", entry.file_name().to_string_lossy())
                })
                .collect();
            names.sort();
            names.iter().for_each(|name| println!("{name}"));
        }
        Some("write") => or_fail(&arg(2), fs::write(arg(2), arg(3))),
        Some("append") => {
            let file = OpenOptions::new().append(true).create(true).open(arg(2));
            or_fail(&arg(2), or_fail(&arg(2), file).write_all(arg(3).as_bytes()));
        }
        // A small write, which the kernel holds in memory to make with what
        // follows, made by fsync, or sync: the disks' writes, by
        // /proc/diskstats, go up. Thrice, in case the kernel's own writing,
        // once a second, comes in between.
        Some(call @ ("fsync" | "sync")) => {
            let writes = || {
                let stats = or_fail("/proc/diskstats", fs::read_to_string("/proc/diskstats"));
                let lines = stats.lines();
                lines.filter_map(|line| line.split_whitespace().nth(7)?.parse::<u64>().ok()).sum()
            };
            let mut file = or_fail(&arg(2), fs::File::create(arg(2)));
            let synced = (0..3).any(|_| {
                or_fail(&arg(2), file.write_all(b"x"));
                let before: u64 = writes();
                match call {
                    "fsync" => or_fail(&arg(2), file.sync_all()),
                    _ => drop(syscall(162, 0, 0, 0)),
                }
                writes() > before
            });
            println!("{}", if synced { "synced" } else { "still held" });
        }
        // What stat says, then what fstat says of it open.
        Some("stat") => {
            let open = or_fail(&arg(2), fs::File::open(arg(2)));
            for metadata in [fs::metadata(arg(2)), open.metadata()] {
                let metadata = or_fail(&arg(2), metadata);
                let kind = if metadata.is_dir() { "dir" } else { "file" };
                let mode = metadata.permissions().mode() & 0o7777;
                println!("{} {kind} {mode:o}", metadata.len());
            }
        }
        // NAME in the directory DIR, opened with openat from DIR open.
        Some("cat-at") => {
            let dir = or_fail(&arg(2), fs::File::open(arg(2)));
            let name = std::ffi::CString::new(arg(3)).unwrap();
            let fd = syscall(257, dir.as_raw_fd() as usize, name.as_ptr() as usize, 0);
            if fd < 0 {
                or_fail::<()>(&arg(3), Err(io::Error::from_raw_os_error(-fd as i32)));
            }
            // Safety: just opened, and owned by nothing else.
            let file = unsafe { fs::File::from_raw_fd(fd as i32) };
            let contents = or_fail(&arg(3), io::read_to_string(file));
            print!("{contents}");
        }
        // LEN bytes from OFFSET on, with pread.
        Some("read-at") => {
            let file = or_fail(&arg(2), fs::File::open(arg(2)));
            let mut buf = vec![0; arg(4).parse().unwrap_or(0)];
            let read = or_fail(&arg(2), file.read_at(&mut buf, arg(3).parse().unwrap_or(0)));
            or_fail(&arg(2), io::stdout().write_all(&buf[..read]));
        }
        // Several buffers in one call: writev's go out as one write, and
        // readv's come in as one read, filling the first before the next;
        // then too many buffers, EINVAL, and one that isn't there, EFAULT.
        Some("vectors") => {
            use std::io::{IoSlice, IoSliceMut};
            let (mut reader, mut writer) = or_fail("pipe", io::pipe());
            let parts = [IoSlice::new(b"one "), IoSlice::new(b""), IoSlice::new(b"two three")];
            let written = or_fail("writev", writer.write_vectored(&parts));
            drop(writer);
            let (mut first, mut second) = ([0u8; 4], [0u8; 32]);
            let mut into = [IoSliceMut::new(&mut first), IoSliceMut::new(&mut second)];
            let read = or_fail("readv", reader.read_vectored(&mut into));
            let iov = [b"x".as_ptr() as usize, 1];
            let too_many = syscall(20, 1, iov.as_ptr() as usize, 1025);
            let nowhere = syscall(20, 1, 0x1000, 1);
            let second = String::from_utf8_lossy(&second[..read.saturating_sub(4)]).into_owned();
            println!("{written} {read} {:?} {second:?} {too_many} {nowhere}", str_of(&first));
        }
        // preadv and pwritev at an offset, the file's place left where it
        // was; then pwritev2 at the place, -1, which it moves, and with
        // RWF_APPEND at the end, from the place or from 0, which it doesn't
        // move; and a flag there isn't, EOPNOTSUPP. All through a file
        // opened O_DSYNC, as F_GETFL says it is.
        Some("vectors-at") => {
            use std::os::unix::fs::OpenOptionsExt;
            const O_DSYNC: i32 = 0o10000;
            let mut options = OpenOptions::new();
            options.read(true).write(true).create(true).truncate(true).custom_flags(O_DSYNC);
            let file = or_fail(&arg(2), options.open(arg(2)));
            or_fail(&arg(2), (&file).write_all(b"abcdefgh"));
            let fd = file.as_raw_fd() as usize;
            syscall(8, fd, 0, 0);
            let iov = |parts: &[&[u8]]| -> Vec<usize> {
                parts.iter().flat_map(|part| [part.as_ptr() as usize, part.len()]).collect()
            };
            let parts = iov(&[b"XY", b"Z"]);
            let wrote = syscall5(296, [fd, parts.as_ptr() as usize, 2, 2, 0]);
            let (mut first, mut second) = ([0u8; 3], [0u8; 2]);
            let into = [first.as_mut_ptr() as usize, 3, second.as_mut_ptr() as usize, 2];
            let read = syscall5(295, [fd, into.as_ptr() as usize, 2, 1, 0]);
            let place = syscall(8, fd, 0, 1);
            let at_place = iov(&[b"12"]);
            let moved = syscall6(328, [fd, at_place.as_ptr() as usize, 1, -1isize as usize, 0], 0);
            let appended = iov(&[b"!"]);
            syscall6(328, [fd, appended.as_ptr() as usize, 1, -1isize as usize, 0], 0x10);
            let from_0 = iov(&[b"?"]);
            syscall6(328, [fd, from_0.as_ptr() as usize, 1, 0, 0], 0x10);
            let unknown = syscall6(327, [fd, into.as_ptr() as usize, 2, 0, 0], 0x1000);
            let now = or_fail(&arg(2), fs::read_to_string(arg(2)));
            println!(
                "{wrote} {read} {:?} {:?} {place} {moved} {} {now} {unknown} {}",
                str_of(&first),
                str_of(&second),
                syscall(8, fd, 0, 1),
                syscall(72, fd, 3, 0) & O_DSYNC as isize
            );
        }
        // Locks on the file PATH: flock's, an open file's, shared with its
        // dup and its copy's; one waited for; and fcntl's ranges, each a
        // process's, as F_GETLK tells, gone as it closes any descriptor of
        // the file; and F_OFD_SETLK's, an open file's.
        Some("locks") => {
            const LOCK_SH: usize = 1;
            const LOCK_EX: usize = 2;
            const LOCK_NB: usize = 4;
            const LOCK_UN: usize = 8;
            let path = arg(2);
            let open = || {
                let mut options = OpenOptions::new();
                let file = options.read(true).write(true).create(true).truncate(false).open(&path);
                or_fail(&path, file)
            };
            let (first, second) = (open(), open());
            let (a, b) = (first.as_raw_fd() as usize, second.as_raw_fd() as usize);
            let flock = |fd: usize, how: usize| syscall(73, fd, how, 0);
            let mut whole = vec![flock(a, LOCK_EX), flock(b, LOCK_EX | LOCK_NB)];
            whole.push(flock(b, LOCK_SH | LOCK_NB));
            let copy = syscall(32, a, 0, 0) as usize;
            whole.push(flock(copy, LOCK_EX | LOCK_NB));
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::process::exit(if flock(a, LOCK_EX | LOCK_NB) == 0 { 0 } else { 1 });
            }
            let in_copy = wait(child).1;
            whole.push(flock(a, LOCK_UN));
            whole.push(flock(b, LOCK_SH | LOCK_NB));
            whole.push(flock(b, LOCK_UN));
            // Held by a copy, on a file of its own, for 50 ms; waited for.
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let mine = open();
                flock(mine.as_raw_fd() as usize, LOCK_EX);
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                std::thread::sleep(Duration::from_millis(50));
                std::process::exit(0);
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
            let started = Instant::now();
            let waited = (flock(b, LOCK_EX), started.elapsed() >= Duration::from_millis(30));
            flock(b, LOCK_UN);
            wait(child);
            // fcntl: a write lock on bytes 0 to 9, then what a copy is told.
            let parent = getpid();
            let range = |kind: i16, start: i64, len: i64| {
                let mut flock = [0u8; 32];
                flock[..2].copy_from_slice(&kind.to_le_bytes());
                flock[8..16].copy_from_slice(&start.to_le_bytes());
                flock[16..24].copy_from_slice(&len.to_le_bytes());
                flock
            };
            let fcntl = |fd: usize, command: usize, flock: &mut [u8; 32]| {
                syscall(72, fd, command, flock.as_mut_ptr() as usize)
            };
            let ranges = fcntl(a, 6, &mut range(1, 0, 10));
            let (mut told, told_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let mut asked = range(1, 5, 1);
                fcntl(a, 5, &mut asked);
                let kind = i16::from_le_bytes([asked[0], asked[1]]);
                let at = i64::from_le_bytes(asked[8..16].try_into().unwrap());
                let len = i64::from_le_bytes(asked[16..24].try_into().unwrap());
                let pid = i32::from_le_bytes(asked[24..28].try_into().unwrap());
                let refused = fcntl(a, 6, &mut range(1, 5, 1));
                let apart = fcntl(a, 6, &mut range(1, 20, 10));
                let said =
                    format!("{kind} {at} {len} {} {refused} {apart}", pid as isize == parent);
                or_fail("pipe", (&told_writer).write_all(said.as_bytes()));
                std::process::exit(0);
            }
            drop(told_writer);
            let mut said = String::new();
            or_fail("pipe", told.read_to_string(&mut said));
            wait(child);
            // Closing another descriptor of the file lets go of the range.
            drop(second);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::process::exit(fcntl(a, 6, &mut range(1, 5, 1)) as i32 & 0xff);
            }
            let after_close = wait(child).1;
            // An open file's range: in the way of a copy's process range on
            // the same file, as both are fcntl's.
            let ofd = fcntl(a, 37, &mut range(1, 0, 0));
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let mine = open();
                let refused = fcntl(mine.as_raw_fd() as usize, 6, &mut range(0, 100, 1));
                std::process::exit(-refused as i32);
            }
            let ofd_in_the_way = wait(child).1;
            println!(
                "{whole:?} {in_copy} {waited:?}; {ranges} {said}; {after_close} {ofd} {ofd_in_the_way}"
            );
        }
        // prctl's names: this thread's, which /proc/self/comm tells, cut to
        // 15 bytes; a new thread's, its maker's until it names itself; and
        // an option there isn't, EINVAL.
        Some("names") => {
            let get = || {
                let mut name = [0u8; 16];
                syscall(157, 16, name.as_mut_ptr() as usize, 0);
                str_of(&name).to_string()
            };
            let set = |name: &std::ffi::CStr| syscall(157, 15, name.as_ptr() as usize, 0);
            let before = get();
            set(c"renamed");
            let comm = or_fail("comm", fs::read_to_string("/proc/self/comm"));
            let thread = std::thread::spawn(move || {
                let inherited = get();
                set(c"a name longer than fifteen bytes");
                (inherited, get())
            });
            let (inherited, own) = thread.join().unwrap();
            let unknown = syscall(157, 0x7fff_ffff, 0, 0);
            println!("{before} {} {inherited} {own:?} {} {unknown}", comm.trim_end(), get());
        }
        // Where this program is, as /proc/self/exe links to it.
        Some("exe") => {
            println!("{}", or_fail("exe", fs::read_link("/proc/self/exe")).display());
        }
        // One readv from standard input into buffers of 5 and 10 bytes.
        Some("readv") => {
            use std::io::IoSliceMut;
            let (mut first, mut second) = ([0u8; 5], [0u8; 10]);
            // Safety: standard input stays open; this doesn't close it.
            let stdin = std::mem::ManuallyDrop::new(unsafe { fs::File::from_raw_fd(0) });
            let mut into = [IoSliceMut::new(&mut first), IoSliceMut::new(&mut second)];
            let read = or_fail("readv", (&*stdin).read_vectored(&mut into));
            println!("{read} {:?} {:?}", str_of(&first), str_of(&second));
        }
        // What a program asks about the machine and itself: what uname says,
        // its user and group, the CPUs it has, and its limits, one of which a
        // copy inherits; and readlink of what isn't a link.
        Some("system") => {
            let mut uts = [0u8; 6 * 65];
            syscall(63, uts.as_mut_ptr() as usize, 0, 0);
            let uname: Vec<&str> = uts.chunks(65).map(str_of).collect();
            let ids = [102, 107, 104, 108].map(|nr| syscall(nr, 0, 0, 0));
            let set = [syscall(105, 0, 0, 0), syscall(105, 1000, 0, 0)];
            let mut mask = [0u8; 128];
            let cpus = syscall(204, 0, mask.len(), mask.as_mut_ptr() as usize);
            let short = syscall(204, 0, 4, mask.as_mut_ptr() as usize);
            let parallel = std::thread::available_parallelism().map_or(0, |n| n.get());
            let mut files = [0u64; 2];
            syscall(97, 7, files.as_mut_ptr() as usize, 0);
            let lowered = syscall(160, 7, [100u64, 1024].as_ptr() as usize, 0);
            let too_many = syscall(160, 7, [2000u64, 2000].as_ptr() as usize, 0);
            let backwards = syscall(160, 7, [20u64, 10].as_ptr() as usize, 0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let mut limit = [0u64; 2];
                syscall5(302, [0, 7, 0, limit.as_mut_ptr() as usize, 0]);
                std::process::exit(limit[0] as i32);
            }
            let inherited = wait(child).1;
            let mut core = [0u64; 2];
            syscall5(302, [0, 4, 0, core.as_mut_ptr() as usize, 0]);
            let mut buf = [0u8; 64];
            let mut link = |path: &std::ffi::CStr| {
                syscall(89, path.as_ptr() as usize, buf.as_mut_ptr() as usize, buf.len())
            };
            let (file, missing) = (link(c"/hello.txt"), link(c"/missing"));
            println!(
                "{uname:?} {ids:?} {set:?} {cpus} {} {short} {parallel} {files:?} {lowered} \
                 {too_many} {backwards} {inherited} {core:?} {file} {missing}",
                mask[0]
            );
        }
        // The time of day, which the disk's files were stamped with just
        // before boot, by each call that tells it; a sleep, never short; one
        // until a time, and until one past; how fine the clock is; and a
        // clock there isn't.
        Some("clocks") => {
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs();
            let stamped =
                or_fail("/hello.txt", fs::metadata("/hello.txt").and_then(|m| m.modified()));
            let stamped = stamped.duration_since(UNIX_EPOCH).unwrap().as_secs();
            let mut timeval = [0u64; 2];
            syscall(96, timeval.as_mut_ptr() as usize, 0, 0);
            let seconds = syscall(201, 0, 0, 0) as u64;
            let agree = timeval[0].abs_diff(now) <= 1
                && timeval[1] < 1_000_000
                && seconds.abs_diff(now) <= 1;
            let start = Instant::now();
            std::thread::sleep(Duration::from_millis(100));
            let slept = (100..2000).contains(&start.elapsed().as_millis());
            // Until a time of day, which taken for a while would be decades.
            let mut realtime = [0u64; 2];
            syscall(228, 0, realtime.as_mut_ptr() as usize, 0);
            let at = realtime[0] * 1_000_000_000 + realtime[1] + 100_000_000;
            let until = [at / 1_000_000_000, at % 1_000_000_000];
            let start = Instant::now();
            let absolute = syscall5(230, [0, 1, until.as_ptr() as usize, 0, 0]);
            let waited = (90..2000).contains(&start.elapsed().as_millis());
            let start = Instant::now();
            let past = syscall5(230, [0, 1, [1u64, 0].as_ptr() as usize, 0, 0]);
            let past = (past, start.elapsed().as_millis() < 500);
            let mut resolution = [0u64; 2];
            syscall(229, 1, resolution.as_mut_ptr() as usize, 0);
            let none = syscall(228, 99, realtime.as_mut_ptr() as usize, 0);
            println!(
                "{} {agree} {slept} {absolute} {waited} {past:?} {resolution:?} {none}",
                now.abs_diff(stamped) < 600
            );
        }
        // CPU time: computing, a program's own; making system calls, the
        // kernel's for it; a copy's, as wait4 tells it, its parent's
        // children's once waited for.
        Some("cpu-time") => {
            let usage = |who: usize| {
                let mut usage = [0u64; 18];
                syscall(98, who, usage.as_mut_ptr() as usize, 0);
                [usage[0] * 1000 + usage[1] / 1000, usage[2] * 1000 + usage[3] / 1000]
            };
            let computing = |ms| {
                let start = Instant::now();
                let mut x = 0u64;
                while start.elapsed() < Duration::from_millis(ms) {
                    for i in 0..100_000 {
                        x = std::hint::black_box(x.wrapping_mul(31).wrapping_add(i));
                    }
                }
            };
            computing(300);
            let [user, system] = usage(0);
            let start = Instant::now();
            while start.elapsed() < Duration::from_millis(300) {
                (0..100).for_each(|_| _ = syscall(110, 0, 0, 0));
            }
            let in_kernel = usage(0)[1] - system;
            let mut clock = [0u64; 2];
            syscall(228, 2, clock.as_mut_ptr() as usize, 0);
            let mut tms = [0u64; 4];
            let ticks = syscall(100, tms.as_mut_ptr() as usize, 0, 0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                computing(300);
                std::process::exit(0);
            }
            let before = usage(usize::MAX)[0];
            let mut told = [0u64; 18];
            syscall5(61, [child as usize, 0, 0, told.as_mut_ptr() as usize, 0]);
            let after = usage(usize::MAX)[0];
            println!(
                "{} {} {} {} {} {} {} {}",
                user >= 200,
                in_kernel >= 30,
                clock[0] * 1000 + clock[1] / 1_000_000 >= 200,
                tms[0] >= 20,
                ticks > 0,
                before == 0,
                told[0] * 1000 + told[1] / 1000 >= 200,
                after >= 200
            );
        }
        // Alarms: SIGALRM in 100 ms, which ends a wait for it; one every
        // 50 ms, three times; what's left of one, as getitimer and alarm say;
        // a copy starts without one; not handled, it ends the program; and
        // it ends a read, with EINTR.
        Some("alarms") => {
            sigaction(SIGALRM, record, SA_SIGINFO);
            let start = Instant::now();
            syscall(38, 0, [0u64, 0, 0, 100_000].as_ptr() as usize, 0);
            let paused = syscall(34, 0, 0, 0);
            let after =
                start.elapsed().as_millis() >= 100 && LAST.load(Ordering::SeqCst) == SIGALRM;
            CAUGHT.store(0, Ordering::SeqCst);
            let start = Instant::now();
            syscall(38, 0, [0u64, 50_000, 0, 50_000].as_ptr() as usize, 0);
            while CAUGHT.load(Ordering::SeqCst) < 3 {
                syscall(34, 0, 0, 0);
            }
            syscall(38, 0, [0u64; 4].as_ptr() as usize, 0);
            let repeated = start.elapsed().as_millis() >= 150;
            let none = syscall(37, 10, 0, 0);
            let mut left = [0u64; 4];
            syscall(36, 0, left.as_mut_ptr() as usize, 0);
            let off = syscall(37, 0, 0, 0);
            syscall(37, 5, 0, 0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let mut left = [0u64; 4];
                syscall(36, 0, left.as_mut_ptr() as usize, 0);
                std::process::exit(i32::from(left != [0; 4]));
            }
            let copy = wait(child).1;
            syscall(37, 0, 0, 0);
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                set_action(SIGALRM, SIG_DFL, 0);
                syscall(38, 0, [0u64, 0, 0, 50_000].as_ptr() as usize, 0);
                loop {
                    syscall(34, 0, 0, 0);
                }
            }
            let ended = wait(child).1;
            let (mut reader, _writer) = or_fail("pipe", io::pipe());
            syscall(38, 0, [0u64, 0, 0, 50_000].as_ptr() as usize, 0);
            let read = reader.read(&mut [0]).map_err(|e| e.raw_os_error());
            println!(
                "{paused} {after} {repeated} {none} {} {off} {copy} {ended} {read:?}",
                (9..=10).contains(&left[2])
            );
        }
        // Alarms too far off to count to: one that never comes, and is
        // still far off a while later; and one that comes once, then
        // repeats as far apart.
        Some("far-alarms") => {
            sigaction(SIGALRM, record, SA_SIGINFO);
            let never = [0u64, 0, i64::MAX as u64, 0];
            let set = syscall(38, 0, never.as_ptr() as usize, 0);
            std::thread::sleep(Duration::from_millis(50));
            let none = CAUGHT.load(Ordering::SeqCst);
            let mut left = [0u64; 4];
            syscall(36, 0, left.as_mut_ptr() as usize, 0);
            let once = [i64::MAX as u64, 0, 0, 10_000];
            syscall(38, 0, once.as_ptr() as usize, 0);
            std::thread::sleep(Duration::from_millis(100));
            let one = CAUGHT.load(Ordering::SeqCst);
            syscall(38, 0, [0u64; 4].as_ptr() as usize, 0);
            println!("{set} {none} {} {one}", left[2] > 1 << 32);
        }
        // rt_sigtimedwait, as sigwait comes to it: a signal of the set, which
        // the program blocks, taken rather than handled, with who sent it;
        // none within 50 ms, EAGAIN, and not before; at once for a time of
        // 0; a signal not in the set ending the wait, EINTR, its handler run;
        // a child's SIGCHLD, with how the child ended; the set still blocked
        // after, and no handler run for what was taken; and EINVAL for a
        // set's size that isn't 8, and a time that isn't one.
        Some("sigtimedwait") => {
            const USR1: usize = 10;
            const USR2: usize = 12;
            const ALRM: usize = 14;
            const CHLD: usize = 17;
            let set = bit(USR1) | bit(USR2) | bit(CHLD);
            sigprocmask(0, set);
            sigaction(USR1, record, 0);
            sigaction(ALRM, record, 0);
            let mut info = [0u8; 128];
            let int = |info: &[u8; 128], at: usize| {
                i32::from_le_bytes(info[at..at + 4].try_into().unwrap())
            };
            let wait = |set: u64, timeout: Option<[i64; 2]>, info: &mut [u8; 128], size: usize| {
                let timeout = timeout.as_ref().map_or(0, |time| time.as_ptr() as usize);
                syscall5(
                    128,
                    [&raw const set as usize, info.as_mut_ptr() as usize, timeout, size, 0],
                )
            };
            kill(getpid(), USR1);
            let got = wait(bit(USR1) | bit(USR2), None, &mut info, 8);
            let sender = (int(&info, 8), int(&info, 16) as isize == getpid());
            let start = Instant::now();
            let none = wait(bit(USR2), Some([0, 50_000_000]), &mut info, 8);
            let waited = start.elapsed() >= Duration::from_millis(50);
            let start = Instant::now();
            let at_once = wait(bit(USR2), Some([0, 0]), &mut info, 8);
            let quick = start.elapsed() < Duration::from_millis(50);
            // SIGALRM, which isn't waited for, in 30 ms.
            let alarm = [0i64, 0, 0, 30_000];
            syscall(38, 0, alarm.as_ptr() as usize, 0);
            let interrupted = wait(bit(USR2), None, &mut info, 8);
            let handled = LAST.load(Ordering::SeqCst) == ALRM;
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                syscall(60, 3, 0, 0);
            }
            let ended = wait(bit(CHLD), None, &mut info, 8);
            let how = (int(&info, 8), int(&info, 16) as isize == child, int(&info, 24));
            syscall(61, child as usize, 0, 0);
            let blocked = sigprocmask(0, 0) & set == set;
            let caught = CAUGHT.load(Ordering::SeqCst);
            let refused = [
                wait(bit(USR2), Some([0, 0]), &mut info, 7),
                wait(bit(USR2), Some([0, 2_000_000_000]), &mut info, 8),
            ];
            println!(
                "{got} {sender:?} {none} {waited} {at_once} {quick} {interrupted} {handled} \
                 {ended} {how:?} {blocked} {caught} {refused:?}"
            );
        }
        // POSIX timers: one by CLOCK_MONOTONIC sending SIGUSR1, which comes no
        // sooner than set, and is taken with SI_TIMER, its id and the value
        // given; again every 20 ms, three times; then off. One by the time of
        // day, set for a time already past (TIMER_ABSTIME), at once. One that
        // sends nothing, whose time left timer_gettime tells. SIGALRM with its
        // id as the value, for none given. One for this thread alone. One
        // deleted, which then isn't there to delete. And EINVAL for a clock
        // there isn't, timers there aren't, and a signal there isn't.
        Some("timers") => {
            const USR1: usize = 10;
            const ALRM: usize = 14;
            sigprocmask(0, bit(USR1) | bit(ALRM));
            let mut info = [0u8; 128];
            let int = |info: &[u8; 128], at: usize| {
                i32::from_le_bytes(info[at..at + 4].try_into().unwrap())
            };
            let take = |signal: usize, info: &mut [u8; 128]| {
                let set = bit(signal);
                syscall5(128, [&raw const set as usize, info.as_mut_ptr() as usize, 0, 8, 0])
            };
            let event = |notify: i32, signal: usize, value: i64, tid: isize| {
                let mut event = [0u8; 64];
                event[..8].copy_from_slice(&value.to_le_bytes());
                event[8..12].copy_from_slice(&(signal as i32).to_le_bytes());
                event[12..16].copy_from_slice(&notify.to_le_bytes());
                event[16..20].copy_from_slice(&(tid as i32).to_le_bytes());
                event
            };
            let create = |clock: usize, event: Option<[u8; 64]>| {
                let mut id = -1i32;
                let event = event.as_ref().map_or(0, |event| event.as_ptr() as usize);
                (syscall(222, clock, event, &raw mut id as usize), id)
            };
            let set = |id: i32, flags: usize, every: i64, at: [i64; 2]| {
                let spec = [every / 1_000_000_000, every % 1_000_000_000, at[0], at[1]];
                syscall5(223, [id as usize, flags, spec.as_ptr() as usize, 0, 0])
            };
            let left = |id: i32| {
                let mut spec = [0i64; 4];
                syscall(224, id as usize, spec.as_mut_ptr() as usize, 0);
                spec
            };
            let (made, one) = create(1, Some(event(0, USR1, 42, 0)));
            let start = Instant::now();
            set(one, 0, 0, [0, 50_000_000]);
            let got = take(USR1, &mut info);
            let late = start.elapsed() >= Duration::from_millis(50);
            let sent = (int(&info, 8), int(&info, 16) == one, int(&info, 24));
            set(one, 0, 20_000_000, [0, 20_000_000]);
            let repeated: Vec<isize> = (0..3).map(|_| take(USR1, &mut info)).collect();
            set(one, 0, 0, [0, 0]);
            let off = left(one) == [0; 4];
            let (_, past) = create(0, Some(event(0, USR1, 7, 0)));
            let start = Instant::now();
            set(past, 1, 0, [1, 0]);
            let at_once = take(USR1, &mut info) == USR1 as isize
                && int(&info, 24) == 7
                && start.elapsed() < Duration::from_millis(500);
            let (_, quiet) = create(1, Some(event(1, 0, 0, 0)));
            set(quiet, 0, 0, [10, 0]);
            let left_ns = |spec: [i64; 4]| spec[2] * 1_000_000_000 + spec[3];
            let counting = (9_000_000_000..=10_000_000_000).contains(&left_ns(left(quiet)));
            let (_, plain) = create(1, None);
            set(plain, 0, 0, [0, 10_000_000]);
            let alarmed = take(ALRM, &mut info) == ALRM as isize && int(&info, 24) == plain;
            // Its value a pointer, as `sival_ptr` often is: all 8 bytes back.
            let pointer = 0x7FFF_1234_5678;
            let (made_mine, mine) = create(1, Some(event(4, USR1, pointer, gettid())));
            set(mine, 0, 0, [0, 10_000_000]);
            let to_me = take(USR1, &mut info) == USR1 as isize
                && i64::from_le_bytes(info[24..32].try_into().unwrap()) == pointer;
            let deleted = [syscall(226, past as usize, 0, 0), syscall(226, past as usize, 0, 0)];
            let refused = [
                create(99, None).0,
                syscall(224, 999, info.as_mut_ptr() as usize, 0),
                syscall(225, 999, 0, 0),
                create(1, Some(event(0, 99, 0, 0))).0,
            ];
            println!(
                "{made} {got} {late} {sent:?} {repeated:?} {off} {at_once} {counting} {alarmed} \
                 {made_mine} {to_me} {deleted:?} {refused:?}"
            );
        }
        // splice, as uutils' cat moves what it copies: from PATH, from the
        // place given, into a pipe, that place moved on and the file's own
        // left as it was; out of the pipe into another file; from the file's
        // own place; 0 at its end. Then what it refuses: neither end a pipe,
        // a place for a pipe, flags there aren't, a descriptor not open.
        Some("splice") => {
            let path = arg(2);
            let copy_path = format!("{path}.copy");
            or_fail(&path, fs::write(&path, "0123456789"));
            let file = or_fail(&path, fs::File::open(&path));
            let copy = or_fail(&copy_path, fs::File::create(&copy_path));
            let (reader, writer) = or_fail("pipe", io::pipe());
            let [f, c, r, w] =
                [file.as_raw_fd(), copy.as_raw_fd(), reader.as_raw_fd(), writer.as_raw_fd()]
                    .map(|fd| fd as usize);
            let splice = |input: usize, at: usize, output: usize, len: usize, flags: usize| {
                syscall6(275, [input, at, output, 0, len], flags)
            };
            let mut place = 2i64;
            let into = splice(f, &raw mut place as usize, w, 4, 0);
            let own = syscall(8, f, 0, 1);
            let out = splice(r, 0, c, 16, 0);
            let rest = splice(f, 0, w, 64, 0);
            let drained = splice(r, 0, c, 64, 0);
            let end = splice(f, 0, w, 64, 0);
            let copied = or_fail(&copy_path, fs::read_to_string(&copy_path));
            let refused = [
                splice(f, 0, c, 4, 0),
                splice(r, &raw mut place as usize, c, 4, 0),
                splice(f, 0, w, 4, 0x100),
                splice(1000, 0, w, 4, 0),
            ];
            or_fail(&path, fs::remove_file(&path));
            or_fail(&copy_path, fs::remove_file(&copy_path));
            println!("{into} {place} {own} {out} {rest} {drained} {end} {copied:?} {refused:?}");
        }
        // Symbolic links: made and read back, followed, and lstat'd as
        // themselves; through a directory, after which `..` is the
        // directory's parent, not the link's; one that leads nowhere, which
        // O_CREAT makes the file for; a loop; O_NOFOLLOW; a program run
        // through one. Hard links: two names for a file, which outlives
        // either. And what Linux refuses.
        Some("links") => {
            use std::os::unix::fs::{OpenOptionsExt, symlink};
            let code = |result: io::Result<()>| result.map_err(|e| e.raw_os_error().unwrap_or(0));
            or_fail("/links", fs::create_dir("/links"));
            or_fail("abs", symlink("/hello.txt", "/links/abs"));
            let target = or_fail("abs", fs::read_link("/links/abs"));
            let through = or_fail("abs", fs::read_to_string("/links/abs"));
            let itself =
                or_fail("abs", fs::symlink_metadata("/links/abs")).file_type().is_symlink();
            let followed = or_fail("abs", fs::metadata("/links/abs")).is_file();
            or_fail("up", symlink("../dir", "/links/up"));
            let deep = or_fail("up", fs::read_to_string("/links/up/nested/deep.txt"));
            let inode = |path| or_fail(path, fs::metadata(path)).ino();
            let parent = inode("/links/up/nested/..") == inode("/dir");
            or_fail("nowhere", symlink("made-by-open", "/links/nowhere"));
            or_fail("nowhere", fs::write("/links/nowhere", "made\n"));
            let made = or_fail("made", fs::read_to_string("/links/made-by-open"));
            or_fail("loop", symlink("loop2", "/links/loop1"));
            or_fail("loop", symlink("loop1", "/links/loop2"));
            let looped = code(fs::read("/links/loop1").map(drop));
            let nofollow = code(
                OpenOptions::new().read(true).custom_flags(0o400000).open("/links/abs").map(drop),
            );
            or_fail("probe", symlink("/bin/probe", "/links/probe"));
            let ran = or_fail("probe", Command::new("/links/probe").output());
            or_fail("one", fs::write("/links/one", "shared\n"));
            or_fail("two", fs::hard_link("/links/one", "/links/two"));
            let links = or_fail("one", fs::metadata("/links/one")).nlink();
            or_fail("one", fs::remove_file("/links/one"));
            let kept = or_fail("two", fs::read_to_string("/links/two"));
            let refused = [
                code(fs::hard_link("/dir", "/links/dir")),
                code(symlink("anything", "/hello.txt")),
                code(fs::read_link("/hello.txt").map(drop)),
                code(fs::hard_link("/links/missing", "/links/three")),
                code(fs::metadata("/hello.txt/../dir").map(drop)),
                code(symlink("anything", "/links/new/")),
                code(fs::hard_link("/links/two", "/links/new/")),
                code(fs::write("/links/new/", "")),
                code(fs::rename("/links/two", "/links/new/")),
                code(fs::symlink_metadata("/links/new").map(drop)),
            ];
            println!(
                "{target:?} {through:?} {itself} {followed} {deep:?} {parent} {made:?} \
                 {looped:?} {nofollow:?} {:?} {links} {kept:?} {refused:?}",
                String::from_utf8_lossy(&ran.stdout)
            );
        }
        // A file's permissions, owner and times, changed by path and
        // through a descriptor, and a link's as itself; its times as it's
        // made and written; its length cut and grown; what access says of
        // files; and sendfile, from where a file's at and from a place given.
        Some("attributes") => {
            use std::fs::{FileTimes, Permissions};
            use std::os::unix::fs::{chown, lchown, symlink};
            let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs() as i64;
            let path = "/attributes.txt";
            or_fail(path, fs::write(path, "0123456789"));
            let stat = |path| or_fail(path, fs::symlink_metadata(path));
            let made = stat(path);
            let fresh =
                [made.atime(), made.mtime(), made.ctime()].iter().all(|t| (t - now).abs() <= 5);
            or_fail(path, fs::set_permissions(path, Permissions::from_mode(0o600)));
            let by_path = stat(path).mode() & 0o7777;
            or_fail(path, chown(path, Some(1000), None));
            or_fail(path, chown(path, None, Some(100)));
            or_fail("link", symlink(path, "/attributes-link"));
            or_fail("link", lchown("/attributes-link", Some(7), Some(7)));
            let owners = [stat(path).uid(), stat(path).gid(), stat("/attributes-link").uid()];
            let file = or_fail(path, OpenOptions::new().read(true).write(true).open(path));
            or_fail(path, file.set_permissions(Permissions::from_mode(0o640)));
            let by_fd = stat(path).mode() & 0o7777;
            let times = FileTimes::new()
                .set_accessed(UNIX_EPOCH + Duration::from_secs(1000))
                .set_modified(UNIX_EPOCH + Duration::from_secs(2000));
            or_fail(path, file.set_times(times));
            let set = [stat(path).atime(), stat(path).mtime()];
            or_fail(
                path,
                file.set_times(
                    FileTimes::new().set_modified(UNIX_EPOCH + Duration::from_secs(3000)),
                ),
            );
            let omitted = [stat(path).atime(), stat(path).mtime()];
            or_fail(path, (&file).write_all(b"x"));
            let written = (stat(path).mtime() - now).abs() <= 5 && stat(path).atime() == 1000;
            let path_c = c"/attributes.txt";
            // As `touch` does it: no times, both now.
            syscall5(280, [-100isize as usize, path_c.as_ptr() as usize, 0, 0, 0]);
            let touched = (stat(path).atime() - now).abs() <= 5;
            or_fail(path, file.set_len(4));
            let cut = or_fail(path, fs::read(path));
            or_fail(path, file.set_len(6));
            let grown = or_fail(path, fs::read(path));
            let truncated = syscall(76, path_c.as_ptr() as usize, 2, 0);
            let short = stat(path).len();
            let read_only = or_fail(path, fs::File::open(path));
            let refused = syscall(77, read_only.as_raw_fd() as usize, 0, 0);
            let access = |path: &std::ffi::CStr, mode| syscall(21, path.as_ptr() as usize, mode, 0);
            let accesses = [
                access(path_c, 4 | 2),
                access(path_c, 1),
                access(c"/bin/probe", 1),
                access(c"/dev/null", 2),
                access(c"/dev", 2),
                access(c"/missing", 0),
                access(path_c, 8),
            ];
            let hello = or_fail("/hello.txt", fs::File::open("/hello.txt"));
            let (mut reader, writer) = or_fail("pipe", io::pipe());
            let all =
                syscall5(40, [writer.as_raw_fd() as usize, hello.as_raw_fd() as usize, 0, 100, 0]);
            let mut place = 6u64;
            let part = syscall5(
                40,
                [
                    writer.as_raw_fd() as usize,
                    hello.as_raw_fd() as usize,
                    &raw mut place as usize,
                    4,
                    0,
                ],
            );
            let rest =
                syscall5(40, [writer.as_raw_fd() as usize, hello.as_raw_fd() as usize, 0, 100, 0]);
            drop(writer);
            let mut sent = String::new();
            or_fail("pipe", reader.read_to_string(&mut sent));
            drop((file, read_only));
            or_fail(path, fs::remove_file(path));
            or_fail("link", fs::remove_file("/attributes-link"));
            println!(
                "{fresh} {by_path:o} {owners:?} {by_fd:o} {set:?} {omitted:?} {written} {touched} {:?} \
                 {grown:?} {truncated} {short} {refused} {accesses:?} {all} {part} {place} {rest} \
                 {sent:?}",
                str_of(&cut),
            );
        }
        // A file removed while it's open, as zsh's here-documents are, and
        // one renamed over: both still read as they were through the
        // descriptors open on them.
        Some("unlinked") => {
            let path = "/here-document";
            or_fail(path, fs::write(path, "here\n"));
            let mut here = or_fail(path, fs::File::open(path));
            or_fail(path, fs::remove_file(path));
            let gone = fs::metadata(path).is_err();
            let path = "/replaced";
            or_fail(path, fs::write(path, "old\n"));
            let mut old = or_fail(path, fs::File::open(path));
            or_fail("/new", fs::write("/new", "new\n"));
            or_fail(path, fs::rename("/new", path));
            let (mut text, mut was) = (String::new(), String::new());
            or_fail("here", here.read_to_string(&mut text));
            or_fail(path, old.read_to_string(&mut was));
            let now = or_fail(path, fs::read_to_string(path));
            or_fail(path, fs::remove_file(path));
            println!("{gone} {text:?} {was:?} {now:?}");
        }
        // Scripts run by what their first line names: with its argument, the
        // script's path as given, and the script's own arguments; through
        // a script naming another; and those that can't be run.
        Some("scripts") => {
            let scripts = [
                ("/script", "#!/bin/probe env\nnot run\n"),
                ("/spaced", "#! \t/bin/probe   env \t\n"),
                ("/outer", "#!/script\n"),
                ("/loop", "#!/loop\n"),
                ("/nothing", "#!  \n"),
                ("/missing", "#!/not/there\n"),
                ("/text", "echo hi\n"),
            ];
            for (path, text) in scripts {
                or_fail(path, fs::write(path, text));
                or_fail(path, fs::set_permissions(path, fs::Permissions::from_mode(0o755)));
            }
            let run = |path: &str| match Command::new(path).arg("a").env_clear().output() {
                Ok(output) => String::from_utf8_lossy(&output.stdout).trim_end().to_string(),
                Err(error) => error.raw_os_error().unwrap_or(0).to_string(),
            };
            let ran: Vec<String> = scripts.iter().map(|(path, _)| run(path)).collect();
            let relative = run("./script");
            for (path, _) in scripts {
                or_fail(path, fs::remove_file(path));
            }
            println!("{} | {relative}", ran.join(" | "));
        }
        // What sysinfo says of the machine; the groups root is in besides
        // its own, none; how nice it is, set by process and by group, and
        // inherited; and a turn given up.
        Some("machine") => {
            let mut info = [0u64; 14];
            let told = syscall(99, info.as_mut_ptr() as usize, 0, 0);
            let mut boot = [0u64; 2];
            syscall(228, 7, boot.as_mut_ptr() as usize, 0);
            let (uptime, total, free, processes) = (info[0], info[4], info[5], info[10] & 0xFFFF);
            let unit = info[13] & 0xFFFF_FFFF;
            let machine = [
                boot[0] - uptime <= 1,
                total >= 64 << 20 && free < total,
                processes >= 1,
                unit == 1,
            ];
            let groups = [
                syscall(115, 0, 0, 0),
                syscall(116, 0, 0, 0),
                syscall(115, -1isize as usize, 0, 0),
            ];
            let priority = || syscall(140, 0, 0, 0);
            let mut nice = vec![priority(), syscall(141, 0, 0, 5), priority()];
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::process::exit(priority() as i32);
            }
            let inherited = wait(child).1;
            nice.extend([syscall(141, 1, 0, -3isize as usize), priority()]);
            nice.extend([
                syscall(141, 0, 0, 100),
                priority(),
                syscall(140, 3, 0, 0),
                syscall(140, 0, 99999, 0),
            ]);
            println!("{told} {machine:?} {groups:?} {nice:?} {inherited} {}", syscall(24, 0, 0, 0));
        }
        // What /proc says of it: its memory, as it maps a megabyte and
        // unmaps it again, and its name; and statfs and fstatfs of the
        // disk, which agree.
        Some("proc") => {
            let before = rss();
            let addr = syscall5(9, [0, 1 << 20, 3, 0x22, 0]);
            let mapped = rss() - before;
            syscall(11, addr as usize, 1 << 20, 0);
            let unmapped = rss() - before;
            let (mut by_path, mut by_fd) = ([0u64; 15], [0u64; 15]);
            syscall(137, c"/".as_ptr() as usize, by_path.as_mut_ptr() as usize, 0);
            let file = or_fail("/hello.txt", fs::File::open("/hello.txt"));
            syscall(138, file.as_raw_fd() as usize, by_fd.as_mut_ptr() as usize, 0);
            let name = or_fail("/proc/self/comm", fs::read_to_string("/proc/self/comm"));
            println!(
                "{mapped} {unmapped} {:#x} {} {} {}",
                by_path[0],
                by_path == by_fd,
                by_path[1],
                name.trim_end()
            );
        }
        // std's threads: four adding into a Mutex and an atomic, each with an
        // id and thread-locals of its own and the program's pid; a Condvar
        // passed back and forth; one waited on until it timed out; and park.
        Some("threads") => {
            use std::sync::{Arc, Condvar, Mutex};
            thread_local!(static MINE: std::cell::Cell<usize> = const { std::cell::Cell::new(0) });
            let pid = getpid();
            let (total, count) = (Arc::new(Mutex::new(0u64)), Arc::new(AtomicU64::new(0)));
            let workers: Vec<_> = (1..=4)
                .map(|n| {
                    let (total, count) = (total.clone(), count.clone());
                    std::thread::spawn(move || {
                        MINE.set(n);
                        for i in 0..10_000 {
                            *total.lock().unwrap() += i;
                            count.fetch_add(1, Ordering::SeqCst);
                        }
                        (getpid() == pid, gettid(), MINE.get() == n)
                    })
                })
                .collect();
            let seen: Vec<_> = workers.into_iter().map(|worker| worker.join().unwrap()).collect();
            let mut ids: Vec<isize> = seen.iter().map(|seen| seen.1).collect();
            ids.push(gettid());
            ids.sort();
            ids.dedup();
            let pids = seen.iter().all(|seen| seen.0);
            let locals = seen.iter().all(|seen| seen.2) && MINE.get() == 0;
            // Turns taken, even by this thread, odd by the other.
            let turn = Arc::new((Mutex::new(0u32), Condvar::new()));
            let take_turns = |turn: &(Mutex<u32>, Condvar), mine: u32| {
                let (lock, changed) = turn;
                for round in 0..500 {
                    let mut now = lock.lock().unwrap();
                    while *now != round * 2 + mine {
                        now = changed.wait(now).unwrap();
                    }
                    *now += 1;
                    changed.notify_one();
                }
            };
            let other = {
                let turn = turn.clone();
                std::thread::spawn(move || take_turns(&turn, 1))
            };
            take_turns(&turn, 0);
            other.join().unwrap();
            let turns = *turn.0.lock().unwrap();
            let (lock, never) = (Mutex::new(()), Condvar::new());
            let started = Instant::now();
            let waited = never.wait_timeout(lock.lock().unwrap(), Duration::from_millis(50));
            let timed_out = waited.unwrap().1.timed_out();
            let long_enough = started.elapsed() >= Duration::from_millis(50);
            let unparked = Arc::new(AtomicU32::new(0));
            let unparker = {
                let (main, unparked) = (std::thread::current(), unparked.clone());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(20));
                    unparked.store(1, Ordering::SeqCst);
                    main.unpark();
                })
            };
            while unparked.load(Ordering::SeqCst) == 0 {
                std::thread::park();
            }
            unparker.join().unwrap();
            println!(
                "{} {} {} {pids} {locals} {turns} {timed_out} {long_enough}",
                *total.lock().unwrap(),
                count.load(Ordering::SeqCst),
                ids.len()
            );
        }
        // futex, called directly: what it refuses, and wakes and requeues
        // counted.
        Some("futex") => {
            const WAIT: usize = 0;
            const WAKE: usize = 1;
            const CMP_REQUEUE: usize = 4;
            const WAIT_BITSET: usize = 9;
            const PRIVATE: usize = 128;
            let word: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(1)));
            let other: &'static AtomicU32 = Box::leak(Box::new(AtomicU32::new(0)));
            let (at, to) = (word.as_ptr() as usize, other.as_ptr() as usize);
            let futex = |at: usize, op: usize, value: usize, timeout: usize, to: usize, value3| {
                syscall6(202, [at, op, value, timeout, to], value3)
            };
            let (soon, past) = ([0i64, 10_000_000], [0i64, 0]);
            let refused = [
                futex(at, WAIT | PRIVATE, 0, 0, 0, 0),
                futex(at, WAIT | PRIVATE, 1, soon.as_ptr() as usize, 0, 0),
                futex(at, WAIT_BITSET | PRIVATE, 1, past.as_ptr() as usize, 0, !0),
                futex(at, WAIT_BITSET | PRIVATE, 1, 0, 0, 0),
                futex(at + 1, WAKE | PRIVATE, 1, 0, 0, 0),
                futex(at, 99, 1, 0, 0, 0),
                futex(at, WAKE | PRIVATE, 1, 0, 0, 0),
                futex(at, CMP_REQUEUE | PRIVATE, 0, 1, to, 5),
            ];
            // Woken until one is, as it may not be waiting yet.
            let wake_until = |at: usize, count: isize| {
                let mut woken = 0;
                while woken < count {
                    woken += futex(at, WAKE | PRIVATE, count as usize, 0, 0, 0);
                    std::thread::sleep(Duration::from_millis(1));
                }
                woken
            };
            let waiter = move || futex(at, WAIT | PRIVATE, 1, 0, 0, 0);
            let one = std::thread::spawn(waiter);
            let woken = wake_until(at, 1);
            let waited = one.join().unwrap();
            // Two moved to the other word, then woken there.
            let two = [std::thread::spawn(waiter), std::thread::spawn(waiter)];
            let mut moved = 0;
            while moved < 2 {
                moved += futex(at, CMP_REQUEUE | PRIVATE, 0, 2, to, 1);
                std::thread::sleep(Duration::from_millis(1));
            }
            let woken_there = wake_until(to, 2);
            let waited_there: Vec<isize> = two.map(|waiter| waiter.join().unwrap()).into();
            println!("{refused:?} {woken} {waited} {moved} {woken_there} {waited_there:?}");
        }
        // A thread's exit(5) ends its program, though the first waits for it
        // to end; and in a copy, the first thread's own exit leaves the
        // other to go on, whose exit_group(3) then ends it.
        Some("thread-exit") => {
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                let worker = std::thread::spawn(|| {
                    std::thread::sleep(Duration::from_millis(20));
                    std::process::exit(5)
                });
                let _ = worker.join();
                std::process::exit(1);
            }
            let first = wait(child).1;
            let (mut reader, writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                drop(reader);
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(30));
                    or_fail("pipe", (&writer).write_all(b"after the first"));
                    syscall(231, 3, 0, 0)
                });
                syscall(60, 0, 0, 0);
                unreachable!("exit returned");
            }
            drop(writer);
            let mut said = String::new();
            or_fail("pipe", reader.read_to_string(&mut said));
            println!("{first}; {said}, {}", wait(child).1);
        }
        // Signals and threads: one sent to the program goes to the thread
        // that doesn't block it; tgkill, to the thread named, or ESRCH for a
        // group not its own or a thread there isn't; and
        // a thread's alternate stack is its own.
        Some("thread-signals") => {
            static TAKER: AtomicIsize = AtomicIsize::new(0);
            extern "C" fn note(_signal: i32, _info: *const u8, _context: *const u8) {
                TAKER.store(gettid(), Ordering::SeqCst);
            }
            let (pid, main) = (getpid(), gettid());
            sigaction(SIGUSR1, note, SA_SIGINFO);
            sigaction(SIGUSR2, note, SA_SIGINFO);
            sigprocmask(SIG_BLOCK, bit(SIGUSR1));
            let stack = vec![0u8; 64 * 1024];
            let given = [stack.as_ptr() as u64, 0, stack.len() as u64];
            syscall(131, given.as_ptr() as usize, 0, 0);
            let (told, heard) = std::sync::mpsc::channel();
            let worker = std::thread::spawn(move || {
                sigprocmask(SIG_UNBLOCK, bit(SIGUSR1));
                told.send(gettid()).unwrap();
                while TAKER.load(Ordering::SeqCst) == 0 {
                    std::thread::sleep(Duration::from_millis(1));
                }
            });
            let worker_id = heard.recv().unwrap();
            kill(pid, SIGUSR1);
            worker.join().unwrap();
            let to_worker = TAKER.swap(0, Ordering::SeqCst) == worker_id;
            let sent =
                std::thread::spawn(move || syscall(234, pid as usize, main as usize, SIGUSR2));
            let sent = sent.join().unwrap();
            getpid();
            let to_main = TAKER.load(Ordering::SeqCst) == main;
            let wrong_group = syscall(234, pid as usize + 1, main as usize, SIGUSR2);
            let none = syscall(234, pid as usize, 999_999, SIGUSR2);
            let mut mine = [0u64; 3];
            syscall(131, 0, mine.as_mut_ptr() as usize, 0);
            let kept = mine[0] == given[0] && mine[1] == 0;
            println!("{to_worker} {sent} {to_main} {wrong_group} {none} {kept}");
        }
        // A copy with a thread that spins, stopped, continued and killed: all
        // of it stops, as its CPU time standing still says.
        Some("thread-stops") => {
            let (mut ready, ready_writer) = or_fail("pipe", io::pipe());
            let child = syscall(57, 0, 0, 0);
            if child == 0 {
                std::thread::spawn(|| {
                    loop {
                        std::hint::spin_loop();
                    }
                });
                or_fail("pipe", (&ready_writer).write_all(b"r"));
                loop {
                    std::thread::sleep(Duration::from_millis(10));
                }
            }
            or_fail("pipe", ready.read_exact(&mut [0]));
            // Its time running and in the kernel, in ticks.
            let time = || {
                let stat = fs::read_to_string(format!("/proc/{child}/stat")).unwrap_or_default();
                let fields: Vec<u64> = stat
                    .rsplit_once(") ")
                    .map(|(_, rest)| rest.split(' ').map(|f| f.parse().unwrap_or(0)).collect())
                    .unwrap_or_default();
                fields.get(11).zip(fields.get(12)).map(|(user, system)| user + system)
            };
            kill(child, SIGSTOP);
            let stopped = wait_status(child, WUNTRACED);
            std::thread::sleep(Duration::from_millis(30));
            let before = time();
            std::thread::sleep(Duration::from_millis(100));
            let still = before.is_some() && time() == before;
            kill(child, SIGCONT);
            let continued = wait_status(child, WCONTINUED);
            kill(child, SIGKILL);
            println!("{stopped:#x} {still} {continued:#x} {}", wait(child).1);
        }
        // A thread that execs PROGRAM, a probe: the others end, and the
        // program goes on as its `ids`, told the pid it had.
        Some("thread-exec") => {
            let (pid, program) = (getpid(), arg(2));
            std::thread::spawn(|| {
                loop {
                    std::thread::sleep(Duration::from_secs(1));
                }
            });
            let execs = std::thread::spawn(move || {
                let error = Command::new(&program).args(["ids", &pid.to_string()]).exec();
                or_fail::<()>(&program, Err(error));
            });
            let _ = execs.join();
            std::process::exit(1);
        }
        // Whether it's still PID, its thread numbered as it is; and how many
        // threads /proc says it has.
        Some("ids") => {
            let status = or_fail("status", fs::read_to_string("/proc/self/status"));
            let threads = status.lines().find(|line| line.starts_with("Threads:"));
            let was = getpid().to_string() == arg(2);
            println!("{was} {} {}", gettid() == getpid(), threads.unwrap_or_default());
        }
        // A thread that forks: the copy has the one thread, numbered as the
        // copy is; and the thread that made it is the one to wait for it.
        Some("thread-fork") => {
            let made = std::thread::spawn(|| {
                let child = syscall(57, 0, 0, 0);
                if child == 0 {
                    let status = or_fail("status", fs::read_to_string("/proc/self/status"));
                    let threads = status.lines().find(|line| line.starts_with("Threads:"));
                    println!("child: {} {}", gettid() == getpid(), threads.unwrap_or_default());
                    std::process::exit(4);
                }
                wait(child).1
            });
            println!("parent: {}", made.join().unwrap());
        }
        Some(other) => {
            eprintln!("probe: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

const TCGETS: usize = 0x5401;
const TCSETS: usize = 0x5402;

/// Put standard input in raw mode, as cfmakeraw would: what its settings
/// were, to put back.
fn raw_mode() -> [u8; 60] {
    let mut termios = [0u8; 60];
    syscall(16, 0, TCGETS, termios.as_mut_ptr() as usize);
    let saved = termios;
    let mut clear = |at: usize, bits: u32| {
        let flags = u32::from_le_bytes(termios[at..at + 4].try_into().unwrap()) & !bits;
        termios[at..at + 4].copy_from_slice(&flags.to_le_bytes());
    };
    // ICRNL and IXON; OPOST; ICANON, ECHO, ISIG and IEXTEN.
    clear(0, 0o400 | 0o2000);
    clear(4, 0o1);
    clear(12, 0o2 | 0o10 | 0o1 | 0o100000);
    // VMIN and VTIME: a byte at least, waiting as long as it takes.
    (termios[17 + 6], termios[17 + 5]) = (1, 0);
    syscall(16, 0, TCSETS, termios.as_ptr() as usize);
    saved
}

/// The pages it has, as /proc/self/stat says.
fn rss() -> i64 {
    let stat = or_fail("/proc/self/stat", fs::read_to_string("/proc/self/stat"));
    // The 24th field, the 22nd after the name.
    let after = stat.rsplit(')').next().unwrap_or_default();
    after.split_whitespace().nth(21).and_then(|rss| rss.parse().ok()).unwrap_or(-1)
}

/// What `result` holds, or else say what went wrong with `path`, and stop.
fn or_fail<T>(path: &str, result: io::Result<T>) -> T {
    result.unwrap_or_else(|error| {
        eprintln!("probe: {path}: {error}");
        std::process::exit(1)
    })
}

// Signals, through system calls of their own: std has none of these.

const SIGHUP: usize = 1;
const SIGINT: usize = 2;
const SIGQUIT: usize = 3;
const SIGKILL: usize = 9;
const SIGUSR1: usize = 10;
const SIGUSR2: usize = 12;
const SIGPIPE: usize = 13;
const SIGALRM: usize = 14;
const SIGTERM: usize = 15;
const SIGCHLD: usize = 17;
const SIGCONT: usize = 18;
const SIGSTOP: usize = 19;
const SIG_DFL: usize = 0;
const SIG_IGN: usize = 1;
const SA_SIGINFO: u64 = 4;
const SA_RESTORER: u64 = 0x0400_0000;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;
const SIG_BLOCK: usize = 0;
const SIG_UNBLOCK: usize = 1;
const SIG_SETMASK: usize = 2;
const WUNTRACED: usize = 2;
const WCONTINUED: usize = 8;

// What a handler returns to: rt_sigreturn, as libc's restorer does.
std::arch::global_asm!(".globl probe_restore_rt", "probe_restore_rt:", "mov eax, 15", "syscall");

unsafe extern "C" {
    fn probe_restore_rt();
    /// The program's dynamic section, where the linker put it.
    static _DYNAMIC: u8;
    /// The environment, as the kernel put it on the stack: what's changed
    /// it since, nothing has, as `auxv` reads it first.
    static environ: *const *const u8;
}

const AT_PHDR: usize = 3;
const AT_PHNUM: usize = 5;
const AT_BASE: usize = 7;
const AT_ENTRY: usize = 9;
const AT_UID: usize = 11;
const AT_EUID: usize = 12;
const AT_GID: usize = 13;
const AT_EGID: usize = 14;
const AT_SECURE: usize = 23;

/// The auxiliary vector, as the kernel put it on the stack, past the
/// environment's pointers and the null one that ends them: each entry's
/// kind and value.
fn auxiliary_vector() -> Vec<(usize, usize)> {
    let mut entries = Vec::new();
    // Safety: the stack as the kernel laid it out, which the program
    // hasn't touched, environ still pointing into it.
    unsafe {
        let mut at = environ;
        while !(*at).is_null() {
            at = at.add(1);
        }
        let mut at = at.add(1).cast::<usize>();
        while *at != 0 {
            entries.push((*at, *at.add(1)));
            at = at.add(2);
        }
    }
    entries
}

fn bit(signal: usize) -> u64 {
    1 << (signal - 1)
}

/// A signal handler, given the signal, its siginfo_t and its ucontext.
type Handler = extern "C" fn(i32, *const u8, *const u8);

/// Set `signal` to run `handler`.
fn sigaction(signal: usize, handler: Handler, flags: u64) -> isize {
    set_action(signal, handler as *const () as usize, flags)
}

/// Set what `signal` does: run the handler at `handler`, or SIG_DFL or
/// SIG_IGN.
fn set_action(signal: usize, handler: usize, flags: u64) -> isize {
    let restorer = probe_restore_rt as *const () as u64;
    let action = [handler as u64, flags | SA_RESTORER, restorer, 0];
    syscall5(13, [signal, action.as_ptr() as usize, 0, 8, 0])
}

/// What `signal` does now: its handler, or SIG_DFL or SIG_IGN.
fn handler_of(signal: usize) -> u64 {
    let mut action = [0u64; 4];
    syscall5(13, [signal, 0, action.as_mut_ptr() as usize, 8, 0]);
    action[0]
}

/// Change which signals are blocked: what were.
fn sigprocmask(how: usize, set: u64) -> u64 {
    let mut old = 0u64;
    syscall5(14, [how, &raw const set as usize, &raw mut old as usize, 8, 0]);
    old
}

fn kill(pid: isize, signal: usize) -> isize {
    syscall(62, pid as usize, signal, 0)
}

fn getpid() -> isize {
    syscall(39, 0, 0, 0)
}

fn gettid() -> isize {
    syscall(186, 0, 0, 0)
}

/// What `record` saw: how many signals, and of the last its number, its
/// sender, its si_code, and the mask its return puts back.
static CAUGHT: AtomicUsize = AtomicUsize::new(0);
static LAST: AtomicUsize = AtomicUsize::new(0);
static FROM: AtomicIsize = AtomicIsize::new(0);
static CODE: AtomicIsize = AtomicIsize::new(0);
static MASK: AtomicU64 = AtomicU64::new(0);

extern "C" fn record(signal: i32, info: *const u8, context: *const u8) {
    // Safety: the kernel passes a siginfo_t, whose si_code and si_pid are
    // ints at 8 and 16, and a ucontext_t, whose uc_sigmask is at 296.
    let int = |at: usize| unsafe { info.add(at).cast::<i32>().read_unaligned() };
    let mask = unsafe { context.add(296).cast::<u64>().read_unaligned() };
    LAST.store(signal as usize, Ordering::SeqCst);
    FROM.store(int(16) as isize, Ordering::SeqCst);
    CODE.store(int(8) as isize, Ordering::SeqCst);
    MASK.store(mask, Ordering::SeqCst);
    CAUGHT.fetch_add(1, Ordering::SeqCst);
}

static FLAG: AtomicU32 = AtomicU32::new(0);

/// A handler that changes every register a function may, and the SSE ones,
/// then sets FLAG.
extern "C" fn clobber(_signal: i32, _info: *const u8, _context: *const u8) {
    // Safety: the registers named are the ones changed.
    unsafe {
        std::arch::asm!(
            "xor eax, eax",
            "xor ecx, ecx",
            "xor edx, edx",
            "xor esi, esi",
            "xor edi, edi",
            "xor r8d, r8d",
            "xor r9d, r9d",
            "xor r10d, r10d",
            "xor r11d, r11d",
            "xorps xmm0, xmm0",
            "xorps xmm1, xmm1",
            "xorps xmm7, xmm7",
            "xorps xmm15, xmm15",
            out("rax") _, out("rcx") _, out("rdx") _, out("rsi") _, out("rdi") _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("xmm0") _, out("xmm1") _, out("xmm7") _, out("xmm15") _,
        )
    };
    FLAG.store(1, Ordering::SeqCst);
}

/// Spin, with a value of its own in every general register and some SSE
/// ones, until FLAG is set: whether they all hold them still.
fn spin_until_flagged() -> bool {
    let kept: u64;
    // Safety: rbx and rbp, which can't be named, are put back; the rest
    // are said to change.
    unsafe {
        std::arch::asm!(
            "push rbx",
            "push rbp",
            "mov rbx, 0x101",
            "mov rbp, 0x102",
            "mov rdx, 0x103",
            "mov rsi, 0x104",
            "mov rdi, 0x105",
            "mov r8, 0x106",
            "mov r9, 0x107",
            "mov r10, 0x108",
            "mov r11, 0x109",
            "mov r12, 0x10a",
            "mov r13, 0x10b",
            "mov r14, 0x10c",
            "mov r15, 0x10d",
            "movq xmm0, rbx",
            "movq xmm1, rdx",
            "movq xmm7, r8",
            "movq xmm15, r15",
            "2:",
            "cmp dword ptr [rcx], 0",
            "je 2b",
            "xor eax, eax",
            "cmp rbx, 0x101",
            "jne 3f",
            "cmp rbp, 0x102",
            "jne 3f",
            "cmp rdx, 0x103",
            "jne 3f",
            "cmp rsi, 0x104",
            "jne 3f",
            "cmp rdi, 0x105",
            "jne 3f",
            "cmp r8, 0x106",
            "jne 3f",
            "cmp r9, 0x107",
            "jne 3f",
            "cmp r10, 0x108",
            "jne 3f",
            "cmp r11, 0x109",
            "jne 3f",
            "cmp r12, 0x10a",
            "jne 3f",
            "cmp r13, 0x10b",
            "jne 3f",
            "cmp r14, 0x10c",
            "jne 3f",
            "cmp r15, 0x10d",
            "jne 3f",
            "movq rax, xmm0",
            "cmp rax, 0x101",
            "jne 3f",
            "movq rax, xmm1",
            "cmp rax, 0x103",
            "jne 3f",
            "movq rax, xmm7",
            "cmp rax, 0x106",
            "jne 3f",
            "movq rax, xmm15",
            "cmp rax, 0x10d",
            "jne 3f",
            "mov eax, 1",
            "3:",
            "pop rbp",
            "pop rbx",
            in("rcx") &raw const FLAG,
            out("rax") kept, out("rdx") _, out("rsi") _, out("rdi") _,
            out("r8") _, out("r9") _, out("r10") _, out("r11") _,
            out("r12") _, out("r13") _, out("r14") _, out("r15") _,
            out("xmm0") _, out("xmm1") _, out("xmm7") _, out("xmm15") _,
        )
    };
    kept == 1
}

/// Say so, then end.
extern "C" fn hung_up(_signal: i32, _info: *const u8, _context: *const u8) {
    let said = b"hung up\n";
    syscall(1, 1, said.as_ptr() as usize, said.len());
    syscall(60, 0, 0, 0);
}

/// Where `on_altstack` ran, and what sigaltstack said there.
static WHERE: AtomicUsize = AtomicUsize::new(0);
static ALT_FLAGS: AtomicU32 = AtomicU32::new(0);

extern "C" fn on_altstack(_signal: i32, _info: *const u8, _context: *const u8) {
    let here = 0u8;
    WHERE.store(&raw const here as usize, Ordering::SeqCst);
    let mut stack = [0u64; 3];
    syscall(131, 0, stack.as_mut_ptr() as usize, 0);
    ALT_FLAGS.store(stack[1] as u32, Ordering::SeqCst);
}

/// wait4 for child `pid` with `options`: the status it said.
fn wait_status(pid: isize, options: usize) -> i32 {
    let mut status = 0i32;
    syscall5(61, [pid as usize, &raw mut status as usize, options, 0, 0]);
    status
}

/// Wait for child `pid`, or any with -1, with wait4: which ended, and how,
/// as `ExitStatus` would say it.
fn wait(pid: isize) -> (isize, String) {
    let mut status = 0i32;
    let waited = syscall(61, pid as usize, &raw mut status as usize, 0);
    let how = match status & 0x7F {
        0 => format!("exit status: {}", status >> 8 & 0xFF),
        signal => format!("signal: {signal}"),
    };
    (waited, how)
}

/// A system call made directly, bypassing libc's checks.
/// The text in `bytes` before its first NUL.
fn str_of(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes.split(|&b| b == 0).next().unwrap_or_default()).unwrap_or("?")
}

fn syscall(nr: isize, a0: usize, a1: usize, a2: usize) -> isize {
    syscall5(nr, [a0, a1, a2, 0, 0])
}

/// The same, with up to five arguments, and a sixth of 0: mmap's offset,
/// which Linux refuses if it isn't a page's.
fn syscall5(nr: isize, args: [usize; 5]) -> isize {
    syscall6(nr, args, 0)
}

/// The same, with a sixth argument.
fn syscall6(nr: isize, [a0, a1, a2, a3, a4]: [usize; 5], a5: usize) -> isize {
    let result;
    // Safety: whatever a call is pointed at outlives it, and the compiler
    // takes the asm to read and write memory, as without `nomem` it does.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") nr => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            in("r10") a3,
            in("r8") a4,
            in("r9") a5,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        )
    };
    result
}
