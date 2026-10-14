//! System calls, with Linux's numbers and calling convention, so programs
//! built for Linux run unchanged.
//!
//! The `syscall` instruction lands in [`entry`] in ring 0, still on the
//! program's stack and with interrupts off. It moves to the thread's kernel
//! stack, and [`dispatch`] runs the call with interrupts on, so a call can block
//! and be preempted like any kernel code. Every pointer a program passes is
//! checked against its page tables (`AddressSpace::read` and `write`): a bad
//! one is EFAULT, never a fault in the kernel.

mod files;

use crate::io::interrupts::gdt;
use crate::memory::USER_END;
use crate::process::{self, LOWEST, MMAP_END, MMAP_START, Process, pages};
use crate::{serial_println, thread};
use alloc::vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::PageTableFlags;

const EPERM: i64 = 1;
const EBADF: i64 = 9;
const ENOMEM: i64 = 12;
const EFAULT: i64 = 14;
const EINVAL: i64 = 22;
const ENOTTY: i64 = 25;
const ENOSYS: i64 = 38;

/// The running program's kernel stack top, which `entry` switches to. One
/// CPU, so one of these; the scheduler sets it on every switch to a program.
static KERNEL_STACK: AtomicU64 = AtomicU64::new(0);
/// Where `entry` keeps the program's stack pointer for the few instructions
/// before it is on the kernel stack. Interrupts are off meanwhile.
static USER_RSP: AtomicU64 = AtomicU64::new(0);

/// Turn on `syscall`, aimed at `entry`.
pub fn init() {
    let selectors = gdt::selectors();
    // Safety: `entry` is ready for whatever ring 3 sends it.
    unsafe { Efer::update(|flags| flags.insert(EferFlags::SYSTEM_CALL_EXTENSIONS)) };
    Star::write(
        selectors.user_code_selector,
        selectors.user_data_selector,
        selectors.code_selector,
        selectors.data_selector,
    )
    .expect("the GDT keeps the order `syscall` and `sysret` need");
    LStar::write(VirtAddr::new(entry as *const () as u64));
    // Cleared on entry: interrupts (until `entry` is on the kernel stack),
    // single-stepping, and the two flags a program could set to trip the kernel.
    SFMask::write(
        RFlags::INTERRUPT_FLAG
            | RFlags::TRAP_FLAG
            | RFlags::DIRECTION_FLAG
            | RFlags::ALIGNMENT_CHECK,
    );
}

pub fn set_kernel_stack(top: VirtAddr) {
    KERNEL_STACK.store(top.as_u64(), Ordering::Relaxed);
}

/// What `entry` saves, from its last push up: `syscall` and Linux promise a
/// program that every register but `rax`, `rcx` and `r11` survives a call.
#[repr(C)]
struct Frame {
    nr: u64,
    r9: u64,
    r8: u64,
    r10: u64,
    rdx: u64,
    rsi: u64,
    rdi: u64,
    /// Where `sysret` returns to: the instruction after the `syscall`.
    rip: u64,
    rflags: u64,
    rsp: u64,
}

/// Where `syscall` lands. Ten pushes onto the 16-byte aligned stack top leave
/// it aligned again for the call.
#[unsafe(naked)]
unsafe extern "C" fn entry() {
    core::arch::naked_asm!(
        "mov [rip + {user_rsp}], rsp",
        "mov rsp, [rip + {kernel_stack}]",
        "push qword ptr [rip + {user_rsp}]",
        "push r11",
        "push rcx",
        "push rdi",
        "push rsi",
        "push rdx",
        "push r10",
        "push r8",
        "push r9",
        "push rax",
        "mov rdi, rsp",
        "call {dispatch}",
        "add rsp, 8",
        "pop r9",
        "pop r8",
        "pop r10",
        "pop rdx",
        "pop rsi",
        "pop rdi",
        "pop rcx",
        "pop r11",
        "pop rsp",
        "sysretq",
        user_rsp = sym USER_RSP,
        kernel_stack = sym KERNEL_STACK,
        dispatch = sym dispatch,
    );
}

extern "sysv64" fn dispatch(frame: &Frame) -> i64 {
    // A `syscall` in the last bytes of the lower half would have `sysret`
    // return to a non-canonical address, which faults in ring 0 on the
    // program's stack: Intel CPUs' sysret bug. Nothing is ever mapped there,
    // but this doesn't rely on it.
    if frame.rip >= USER_END {
        process::exit_current(process::KILLED_SIGSEGV);
    }
    interrupts::enable();
    let args = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];
    let result = call(frame.nr, args);
    interrupts::disable();
    result
}

fn call(nr: u64, [a0, a1, a2, a3, _a4, _a5]: [u64; 6]) -> i64 {
    let process = thread::current_process().expect("system calls come from programs");
    match nr {
        0 => files::read(&process, a0, a1, a2),
        1 => files::write(&process, a0, a1, a2),
        2 => files::open(&process, files::AT_FDCWD, a0, a1),
        3 => files::close(&process, a0),
        // stat and lstat: the same, as no symbolic link is followed yet.
        4 | 6 => files::newfstatat(&process, files::AT_FDCWD, a0, a1, 0),
        5 => files::fstat(&process, a0, a1),
        7 => poll(&process, a0, a1),
        8 => files::lseek(&process, a0, a1, a2),
        9 => mmap(&process, a0, a1, a2, a3),
        10 => mprotect(&process, a0, a1, a2),
        11 => munmap(&process, a0, a1),
        12 => brk(&process, a0),
        13 => rt_sigaction(&process, a0, a2, a3),
        14 => rt_sigprocmask(&process, a2, a3),
        16 => -ENOTTY,
        17 => files::pread(&process, a0, a1, a2, a3),
        18 => files::pwrite(&process, a0, a1, a2, a3),
        35 => clock_nanosleep(&process, CLOCK_MONOTONIC, 0, a0),
        // exit, exit_group: one thread per program, so the same.
        60 | 231 => {
            files::close_all(&process);
            drop(process);
            process::exit_current((a0 & 0xFF) as i32)
        }
        72 => files::fcntl(&process, a0, a1, a2),
        // fsync, fdatasync.
        74 | 75 => files::fsync(&process, a0),
        131 => sigaltstack(&process, a1),
        158 => arch_prctl(&process, a0, a1),
        // gettid: one thread per program, so its id is the program's.
        186 => process.id as i64,
        // set_tid_address: nothing to clear at exit with one thread per program.
        217 => files::getdents64(&process, a0, a1, a2),
        218 => process.id as i64,
        230 => clock_nanosleep(&process, a0, a1, a2),
        257 => files::open(&process, a0, a1, a2),
        262 => files::newfstatat(&process, a0, a1, a2, a3),
        _ => {
            serial_println!("simplos: process {}: no system call {}", process.id, nr);
            -ENOSYS
        }
    }
}

/// Which descriptors are open, which is what Rust's start-up asks of 0-2.
///
/// shortcut: never waits, and never reports anything readable or writable;
/// nothing a program has open can make it wait yet.
fn poll(process: &Process, fds: u64, count: u64) -> i64 {
    const POLLNVAL: i16 = 0x20;
    if count > 1024 {
        return -EINVAL;
    }
    // `struct pollfd`: an int fd, then two shorts, events and revents.
    let mut table = vec![0u8; count as usize * 8];
    let mut memory = process.memory.lock();
    if memory.space.read(fds, &mut table).is_none() {
        return -EFAULT;
    }
    let mut ready = 0;
    for entry in table.as_chunks_mut::<8>().0 {
        let fd = i32::from_le_bytes(entry[..4].try_into().unwrap());
        // A negative fd is skipped, as poll's caller intends.
        let revents = if fd < 0 || files::is_open(process, fd) { 0 } else { POLLNVAL };
        entry[6..].copy_from_slice(&revents.to_le_bytes());
        ready += i64::from(revents != 0);
    }
    if memory.space.write(fds, &table).is_none() {
        return -EFAULT;
    }
    ready
}

const PROT_WRITE: u64 = 2;
const PROT_EXEC: u64 = 4;

/// The page flags a program asks for with `PROT_*`.
fn page_flags(prot: u64) -> PageTableFlags {
    let mut flags = PageTableFlags::empty();
    if prot & PROT_WRITE != 0 {
        flags |= PageTableFlags::WRITABLE;
    }
    if prot & PROT_EXEC == 0 {
        flags |= PageTableFlags::NO_EXECUTE;
    }
    flags
}

/// `start..start + len` as a page-aligned range a program may change: `None`
/// if it isn't aligned, or reaches outside its part of the address space.
fn user_range(start: u64, len: u64) -> Option<(u64, u64)> {
    let end = start.checked_add(len)?.checked_next_multiple_of(4096)?;
    (start.is_multiple_of(4096) && start >= LOWEST && end <= USER_END && len > 0)
        .then_some((start, end))
}

/// Anonymous memory only: no files yet.
fn mmap(process: &Process, addr: u64, len: u64, prot: u64, flags: u64) -> i64 {
    const MAP_PRIVATE: u64 = 0x02;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANONYMOUS: u64 = 0x20;
    if flags & MAP_ANONYMOUS == 0 || flags & MAP_PRIVATE == 0 {
        return -EINVAL;
    }
    let Some(len) = len.checked_next_multiple_of(4096) else { return -ENOMEM };
    let mut memory = process.memory.lock();
    let start = if flags & MAP_FIXED != 0 {
        addr
    } else {
        let start = memory.mmap_next;
        match start.checked_add(len).filter(|&end| end <= MMAP_END) {
            Some(end) => memory.mmap_next = end,
            None => return -ENOMEM,
        }
        start
    };
    let Some((start, end)) = user_range(start, len) else { return -EINVAL };
    // What was there goes, as MAP_FIXED says; a fresh range has nothing.
    memory.space.unmap(pages(start, end));
    // PROT_NONE: reserved, and left unmapped so any access faults.
    if prot != 0 && memory.space.map(pages(start, end), page_flags(prot)).is_none() {
        return -ENOMEM;
    }
    start as i64
}

fn mprotect(process: &Process, addr: u64, len: u64, prot: u64) -> i64 {
    let Some((start, end)) = user_range(addr, len) else { return -EINVAL };
    let mut memory = process.memory.lock();
    if prot == 0 {
        // shortcut: PROT_NONE unmaps, so its pages lose their contents and
        // come back unmapped; keep the frames when a program needs them back.
        memory.space.unmap(pages(start, end));
    } else {
        memory.space.protect(pages(start, end), page_flags(prot));
    }
    0
}

fn munmap(process: &Process, addr: u64, len: u64) -> i64 {
    let Some((start, end)) = user_range(addr, len) else { return -EINVAL };
    process.memory.lock().space.unmap(pages(start, end));
    0
}

/// The heap's end: moved to `addr` if it can be, and returned either way, as
/// Linux does. `brk(0)` just asks.
fn brk(process: &Process, addr: u64) -> i64 {
    let mut memory = process.memory.lock();
    if addr < memory.brk_start || addr > MMAP_START {
        return memory.brk as i64;
    }
    let (old_end, new_end) = (memory.brk.next_multiple_of(4096), addr.next_multiple_of(4096));
    if new_end > old_end {
        let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        if memory.space.map(pages(old_end, new_end), flags).is_none() {
            return memory.brk as i64;
        }
    } else {
        memory.space.unmap(pages(new_end, old_end));
    }
    memory.brk = addr;
    addr as i64
}

/// Accepted, and every signal reported as having its default action.
///
/// shortcut: signals are never delivered, so a handler is never called; a
/// fault ends the program. Real signals are Phase 4.
fn rt_sigaction(process: &Process, signal: u64, old: u64, set_size: u64) -> i64 {
    if set_size != 8 || !(1..=64).contains(&signal) {
        return -EINVAL;
    }
    // `struct sigaction`: handler, flags, restorer and mask, all zero for
    // SIG_DFL.
    if old != 0 && process.memory.lock().space.write(old, &[0; 32]).is_none() {
        return -EFAULT;
    }
    0
}

fn rt_sigprocmask(process: &Process, old: u64, set_size: u64) -> i64 {
    if set_size != 8 {
        return -EINVAL;
    }
    if old != 0 && process.memory.lock().space.write(old, &[0; 8]).is_none() {
        return -EFAULT;
    }
    0
}

/// Accepted; the old one reported as disabled.
fn sigaltstack(process: &Process, old: u64) -> i64 {
    const SS_DISABLE: u64 = 2;
    // `stack_t`: a pointer, an int of flags (padded to 8), a size.
    let mut stack = [0u8; 24];
    stack[8..16].copy_from_slice(&SS_DISABLE.to_le_bytes());
    if old != 0 && process.memory.lock().space.write(old, &stack).is_none() {
        return -EFAULT;
    }
    0
}

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// Sleep for the `struct timespec` at `request`: what `std::thread::sleep`
/// does, through `clock_nanosleep`, and `nanosleep` too. Nothing interrupts a
/// sleep (no signals yet), so the time left over is never written.
fn clock_nanosleep(process: &Process, clock: u64, flags: u64, request: u64) -> i64 {
    // shortcut: relative sleeps only; TIMER_ABSTIME needs a clock a program
    // can read, which comes with clock_gettime when one asks for it.
    if flags != 0 || (clock != CLOCK_REALTIME && clock != CLOCK_MONOTONIC) {
        return -EINVAL;
    }
    let mut timespec = [0u8; 16];
    if process.memory.lock().space.read(request, &mut timespec).is_none() {
        return -EFAULT;
    }
    let seconds = i64::from_le_bytes(timespec[..8].try_into().unwrap());
    let nanos = i64::from_le_bytes(timespec[8..].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return -EINVAL;
    }
    let ms =
        (seconds as u64).saturating_mul(1000).saturating_add((nanos as u64).div_ceil(1_000_000));
    thread::sleep(ms);
    0
}

/// `fs`, where musl keeps its thread-local storage.
fn arch_prctl(process: &Process, code: u64, addr: u64) -> i64 {
    const ARCH_SET_FS: u64 = 0x1002;
    const ARCH_GET_FS: u64 = 0x1003;
    match code {
        ARCH_SET_FS if addr < USER_END => {
            thread::with_current_user(|user| user.set_fs_base(addr));
            0
        }
        ARCH_SET_FS => -EPERM,
        ARCH_GET_FS => {
            let base = thread::with_current_user(|user| user.fs_base()).unwrap_or(0);
            match process.memory.lock().space.write(addr, &base.to_le_bytes()) {
                Some(()) => 0,
                None => -EFAULT,
            }
        }
        _ => -EINVAL,
    }
}
