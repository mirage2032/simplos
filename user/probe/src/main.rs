//! A user program for simplos: an ordinary Rust program, `std` and all, built
//! for Linux (static musl) and run by the kernel from the ramdisk. Its first
//! argument picks what it does, so one binary covers every test.

use std::fs::{self, OpenOptions};
use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, PermissionsExt};
use std::os::unix::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicIsize, AtomicU32, AtomicU64, AtomicUsize, Ordering};
use std::time::Duration;

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
        // sleep for an impossible time and one whose time is in the kernel.
        Some("bad-calls") => {
            let kernel = syscall(1, 1, 0xFFFF_8000_0000_0000, 8);
            let unmapped = syscall(1, 1, 0x1000, 8);
            let unknown = syscall(999, 0, 0, 0);
            let timespec: [i64; 2] = [0, 2_000_000_000];
            let bad_time = syscall(35, timespec.as_ptr() as usize, 0, 0);
            let kernel_time = syscall(230, 1, 0, 0xFFFF_8000_0000_0000);
            println!("{kernel} {unmapped} {unknown} {bad_time} {kernel_time}");
        }
        // The same for files: a path and a buffer in the kernel's half, a
        // directory read too small for one entry and one read as a file, and
        // a descriptor that isn't open.
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
            println!("{bad_path} {bad_buf} {small} {read_dir} {closed}");
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
        // Whether standard input and output are terminals, and a pipe; and
        // the terminal's size, as TIOCGWINSZ says.
        Some("tty") => {
            use std::io::IsTerminal;
            let mut size = [0u16; 4];
            let got = syscall(16, 1, 0x5413, size.as_mut_ptr() as usize);
            let pipe = std::os::fd::OwnedFd::from(or_fail("pipe", io::pipe()).0);
            let (stdin, stdout) = (io::stdin().is_terminal(), io::stdout().is_terminal());
            println!("{stdin} {stdout} {} {}x{} {got}", pipe.is_terminal(), size[0], size[1]);
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
fn syscall(nr: isize, a0: usize, a1: usize, a2: usize) -> isize {
    syscall5(nr, [a0, a1, a2, 0, 0])
}

/// The same, with up to five arguments.
fn syscall5(nr: isize, [a0, a1, a2, a3, a4]: [usize; 5]) -> isize {
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
            out("rcx") _,
            out("r11") _,
            options(nostack),
        )
    };
    result
}
