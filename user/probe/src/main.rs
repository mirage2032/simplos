//! A user program for simplos: an ordinary Rust program, `std` and all, built
//! for Linux (static musl) and run by the kernel from the ramdisk. Its first
//! argument picks what it does, so one binary covers every test.

use std::fs::{self, OpenOptions};
use std::io::{self, Write};
use std::os::fd::{AsRawFd, FromRawFd};
use std::os::unix::fs::{FileExt, PermissionsExt};
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
        Some("exit") => {}
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
        // End with PATH open, for the kernel to close.
        Some("hold") => std::mem::forget(or_fail(&arg(2), fs::File::open(arg(2)))),
        // Files, through std::fs, as any program would use them.
        Some("cat") => {
            for path in std::env::args().skip(2) {
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

/// What `result` holds, or else say what went wrong with `path`, and stop.
fn or_fail<T>(path: &str, result: io::Result<T>) -> T {
    result.unwrap_or_else(|error| {
        eprintln!("probe: {path}: {error}");
        std::process::exit(1)
    })
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
