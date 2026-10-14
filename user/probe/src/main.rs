//! A user program for simplos: an ordinary Rust program, `std` and all, built
//! for Linux (static musl) and run by the kernel from the ramdisk. Its first
//! argument picks what it does, so one binary covers every test.

use std::time::Duration;

fn main() {
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
        Some("exit") => {}
        Some(other) => {
            eprintln!("probe: unknown mode {other:?}");
            std::process::exit(2);
        }
    }
}

/// A system call made directly, bypassing libc's checks.
fn syscall(nr: isize, a0: usize, a1: usize, a2: usize) -> isize {
    let result;
    // Safety: the calls made here touch no memory of the program's.
    unsafe {
        std::arch::asm!(
            "syscall",
            inlateout("rax") nr => result,
            in("rdi") a0,
            in("rsi") a1,
            in("rdx") a2,
            out("rcx") _,
            out("r11") _,
            options(nostack),
        )
    };
    result
}
