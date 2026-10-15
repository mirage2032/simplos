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
mod processes;
mod signals;
mod system;

pub use system::{DEFAULT_LIMITS, LIMITS};

use crate::fs::Handle;
use crate::io::interrupts::gdt;
use crate::io::interrupts::pit::TIMER_HZ;
use crate::memory::USER_END;
use crate::process::{self, LOWEST, MMAP_END, MMAP_START, Process, Registers, pages};
use crate::signal::{self, ERESTARTNOHAND, Syscall};
use crate::{serial_println, thread};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::PageTableFlags;

/// What Linux's `strerror` says of the errnos that starting a program can
/// end in, for the kernel to tell whoever's at the console.
pub fn strerror(errno: i64) -> &'static str {
    match errno {
        2 => "No such file or directory",
        5 => "I/O error",
        7 => "Argument list too long",
        8 => "Exec format error",
        12 => "Cannot allocate memory",
        13 => "Permission denied",
        20 => "Not a directory",
        _ => "Unknown error",
    }
}

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
/// The ones the calls themselves keep are saved for `fork`, which hands all of
/// them to the copy.
#[repr(C)]
struct Frame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    rbp: u64,
    rbx: u64,
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

/// Where `syscall` lands. Sixteen pushes onto the 16-byte aligned stack top
/// leave it aligned again for the call.
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
        "push rbx",
        "push rbp",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call {dispatch}",
        // Past the ones `dispatch` kept, and the call's number.
        "add rsp, 56",
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

impl Frame {
    /// The program's registers as `sysret` would leave them, with `rax` the
    /// call's result.
    fn registers(&self, rax: u64) -> Registers {
        Registers {
            rax,
            rbx: self.rbx,
            rcx: self.rip,
            rdx: self.rdx,
            rsi: self.rsi,
            rdi: self.rdi,
            rbp: self.rbp,
            r8: self.r8,
            r9: self.r9,
            r10: self.r10,
            r11: self.rflags,
            r12: self.r12,
            r13: self.r13,
            r14: self.r14,
            r15: self.r15,
            rip: self.rip,
            rflags: self.rflags,
            rsp: self.rsp,
        }
    }
}

extern "sysv64" fn dispatch(frame: &Frame) -> i64 {
    // A `syscall` in the last bytes of the lower half would have `sysret`
    // return to a non-canonical address, which faults in ring 0 on the
    // program's stack: Intel CPUs' sysret bug. Nothing is ever mapped there,
    // but this doesn't rely on it.
    if frame.rip >= USER_END {
        process::kill_current(signal::SIGSEGV);
    }
    interrupts::enable();
    let result = call(frame);
    // Signals to take before going back, or a call one ended to start
    // again: then back by `iretq`, which `sysret` can't do with every
    // register a handler's frame puts back.
    if let Some(process) = thread::current_process()
        && (process.signals.has_signal()
            || result == -signal::ERESTARTSYS
            || result == -ERESTARTNOHAND)
    {
        let mut registers = frame.registers(result as u64);
        signal::deliver(process, &mut registers, Some(Syscall { number: frame.nr, result }));
        process::enter_user(registers);
    }
    interrupts::disable();
    result
}

fn call(frame: &Frame) -> i64 {
    let process = thread::current_process().expect("system calls come from programs");
    let [a0, a1, a2, a3, a4] = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8];
    match frame.nr {
        0 => files::read(&process, a0, a1, a2),
        1 => files::write(&process, a0, a1, a2),
        19 => files::readv(&process, a0, a1, a2),
        20 => files::writev(&process, a0, a1, a2),
        2 => files::open(&process, files::AT_FDCWD, a0, a1, a2),
        3 => files::close(&process, a0),
        // stat and lstat: the same, as no symbolic link is followed yet.
        4 | 6 => files::newfstatat(&process, files::AT_FDCWD, a0, a1, 0),
        5 => files::fstat(&process, a0, a1),
        7 => poll(&process, a0, a1, (a2 as i32 >= 0).then_some(a2 as i32 as u64)),
        8 => files::lseek(&process, a0, a1, a2),
        9 => mmap(&process, a0, a1, a2, a3),
        10 => mprotect(&process, a0, a1, a2),
        11 => munmap(&process, a0, a1),
        12 => brk(&process, a0),
        13 => signals::rt_sigaction(&process, a0, a1, a2, a3),
        14 => signals::rt_sigprocmask(&process, a0, a1, a2, a3),
        15 => match signal::sigreturn(&process, frame.rsp) {
            Some(mut registers) => {
                signal::deliver(process, &mut registers, None);
                process::enter_user(registers)
            }
            None => {
                drop(process);
                process::kill_current(signal::SIGSEGV)
            }
        },
        16 => files::ioctl(&process, a0, a1, a2),
        17 => files::pread(&process, a0, a1, a2, a3),
        18 => files::pwrite(&process, a0, a1, a2, a3),
        22 => files::pipe2(&process, a0, 0),
        32 => files::dup(&process, a0),
        33 => files::dup3(&process, a0, a1, 0, true),
        34 => signals::pause(&process),
        35 => clock_nanosleep(&process, CLOCK_MONOTONIC, 0, a0, a1),
        39 => process.id as i64,
        // The copy starts as the call returns, with 0 its result.
        56 => processes::clone(&process, frame.registers(0), a0, a1),
        // fork, vfork.
        57 | 58 => processes::clone(&process, frame.registers(0), processes::SIGCHLD, 0),
        59 => match processes::execve(&process, a0, a1, a2) {
            Ok(mut registers) => {
                // Nothing on this stack is ever dropped: `deliver` lets go.
                signal::deliver(process, &mut registers, None);
                process::enter_user(registers)
            }
            Err(errno) => -errno,
        },
        // exit, exit_group: one thread per program, so the same.
        60 | 231 => {
            drop(process);
            process::exit_current(a0 as i32)
        }
        61 => processes::wait4(&process, a0, a1, a2, a3),
        62 => signals::kill(&process, a0, a1),
        63 => system::uname(&process, a0),
        72 => files::fcntl(&process, a0, a1, a2),
        79 => files::getcwd(&process, a0, a1),
        80 => files::chdir(&process, a0),
        82 => files::renameat2(&process, files::AT_FDCWD, a0, files::AT_FDCWD, a1, 0),
        83 => files::mkdirat(&process, files::AT_FDCWD, a0, a1),
        84 => files::unlinkat(&process, files::AT_FDCWD, a0, files::AT_REMOVEDIR),
        87 => files::unlinkat(&process, files::AT_FDCWD, a0, 0),
        89 => files::readlinkat(&process, files::AT_FDCWD, a0, a2),
        95 => files::umask(&process, a0),
        97 => system::prlimit(&process, 0, a0, 0, a1),
        // getuid, getgid, geteuid and getegid.
        102 | 104 | 107 | 108 => 0,
        105 | 106 => system::set_id(a0),
        109 => signals::setpgid(&process, a0, a1),
        110 => process.parent_id() as i64,
        111 => process.pgid() as i64,
        112 => signals::setsid(&process),
        121 => signals::getpgid(&process, a0),
        124 => signals::getsid(&process, a0),
        127 => signals::rt_sigpending(&process, a0, a1),
        130 => signals::rt_sigsuspend(&process, a0, a1),
        // fsync, fdatasync.
        74 | 75 => files::fsync(&process, a0),
        131 => signals::sigaltstack(&process, a0, a1, frame.rsp),
        158 => arch_prctl(&process, a0, a1),
        160 => system::prlimit(&process, 0, a0, a1, 0),
        // gettid: one thread per program, so its id is the program's.
        186 => process.id as i64,
        // set_tid_address: nothing to clear at exit with one thread per program.
        200 => signals::tkill(&process, None, a0, a1),
        204 => system::sched_getaffinity(&process, a0, a1, a2),
        217 => files::getdents64(&process, a0, a1, a2),
        218 => process.id as i64,
        230 => clock_nanosleep(&process, a0, a1, a2, a3),
        234 => signals::tkill(&process, Some(a0), a1, a2),
        257 => files::open(&process, a0, a1, a2, a3),
        258 => files::mkdirat(&process, a0, a1, a2),
        262 => files::newfstatat(&process, a0, a1, a2, a3),
        263 => files::unlinkat(&process, a0, a1, a2),
        264 => files::renameat2(&process, a0, a1, a2, a3, 0),
        267 => files::readlinkat(&process, a0, a1, a3),
        271 => ppoll(&process, a0, a1, a2),
        292 => files::dup3(&process, a0, a1, a2, false),
        293 => files::pipe2(&process, a0, a1),
        302 => system::prlimit(&process, a0, a1, a2, a3),
        316 => files::renameat2(&process, a0, a1, a2, a3, a4),
        _ => {
            serial_println!("simplos: process {}: no system call {}", process.id, frame.nr);
            -ENOSYS
        }
    }
}

/// Wait until one of the descriptors in the `struct pollfd`s at `fds` is
/// ready as asked, or for `timeout` milliseconds if it isn't `None`: how
/// many are, each told how in its `revents`.
fn poll(process: &Process, fds: u64, count: u64, timeout: Option<u64>) -> i64 {
    const POLLIN: i16 = 0x1;
    const POLLOUT: i16 = 0x4;
    const POLLERR: i16 = 0x8;
    const POLLHUP: i16 = 0x10;
    const POLLNVAL: i16 = 0x20;
    if count > 1024 {
        return -EINVAL;
    }
    // `struct pollfd`: an int fd, then two shorts, events and revents.
    let mut table = vec![0u8; count as usize * 8];
    if process.memory.lock().space.read(fds, &mut table).is_none() {
        return -EFAULT;
    }
    let entry = |entry: &[u8; 8]| {
        let fd = i32::from_le_bytes(entry[..4].try_into().unwrap());
        let events = i16::from_le_bytes([entry[4], entry[5]]);
        // A negative fd is skipped, as poll's caller intends.
        (events, (fd >= 0).then(|| files::open_handle(process, fd)))
    };
    let entries: Vec<_> = table.as_chunks::<8>().0.iter().map(entry).collect();
    let mut revents = vec![0i16; entries.len()];
    // What each is ready for, of what was asked; errors and hang-ups are
    // told whether asked or not.
    let check = |revents: &mut [i16]| {
        for ((events, handle), revents) in entries.iter().zip(revents.iter_mut()) {
            let ready = match handle {
                None => 0,
                Some(None) => POLLNVAL,
                Some(Some(handle)) => match &**handle {
                    Handle::Tty { tty, .. } => POLLOUT | if tty.readable() { POLLIN } else { 0 },
                    Handle::Pipe { end, .. } => {
                        let pipe = end.poll();
                        match (end.write, pipe.closed) {
                            (false, closed) => {
                                (if pipe.now { POLLIN } else { 0 })
                                    | if closed { POLLHUP } else { 0 }
                            }
                            (true, true) => POLLERR,
                            (true, false) => {
                                if pipe.now {
                                    POLLOUT
                                } else {
                                    0
                                }
                            }
                        }
                    }
                    // Files never make a program wait.
                    _ => POLLIN | POLLOUT,
                },
            };
            *revents = ready & (events | POLLERR | POLLHUP | POLLNVAL);
        }
        revents.iter().any(|&revents| revents != 0)
    };
    // A timeout of 0 only looks.
    let deadline = timeout.map(|ms| if ms == 0 { 0 } else { thread::ticks_after(ms) });
    if thread::poll_until(deadline, || check(&mut revents)).is_err() {
        return -ERESTARTNOHAND;
    }
    let mut ready = 0;
    for (entry, revents) in table.as_chunks_mut::<8>().0.iter_mut().zip(&revents) {
        entry[6..].copy_from_slice(&revents.to_le_bytes());
        ready += i64::from(*revents != 0);
    }
    if process.memory.lock().space.write(fds, &table).is_none() {
        return -EFAULT;
    }
    ready
}

/// `poll`, with its timeout a `struct timespec` at `timeout`, or none.
///
/// shortcut: the signal mask is taken and ignored, as there are no signals.
fn ppoll(process: &Process, fds: u64, count: u64, timeout: u64) -> i64 {
    if timeout == 0 {
        return poll(process, fds, count, None);
    }
    match timespec_ms(process, timeout) {
        Ok(ms) => poll(process, fds, count, Some(ms)),
        Err(errno) => -errno,
    }
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

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;

/// Sleep for the `struct timespec` at `request`: what `std::thread::sleep`
/// does, through `clock_nanosleep`, and `nanosleep` too. A signal ends it
/// early, with what was left of it at `remain`.
///
/// shortcut: one that stops the program and continues it starts the whole
/// sleep again, where Linux sleeps for what's left.
fn clock_nanosleep(process: &Process, clock: u64, flags: u64, request: u64, remain: u64) -> i64 {
    // shortcut: relative sleeps only; TIMER_ABSTIME needs a clock a program
    // can read, which comes with clock_gettime when one asks for it.
    if flags != 0 || (clock != CLOCK_REALTIME && clock != CLOCK_MONOTONIC) {
        return -EINVAL;
    }
    let ms = match timespec_ms(process, request) {
        Ok(ms) => ms,
        Err(errno) => return -errno,
    };
    let deadline = thread::ticks_after(ms);
    if thread::poll_until(Some(deadline), || false).is_ok() {
        return 0;
    }
    if remain != 0 {
        let left = deadline.saturating_sub(thread::ticks()) * 1000 / TIMER_HZ as u64;
        let mut timespec = [0u8; 16];
        timespec[..8].copy_from_slice(&(left / 1000).to_le_bytes());
        timespec[8..].copy_from_slice(&(left % 1000 * 1_000_000).to_le_bytes());
        if process.memory.lock().space.write(remain, &timespec).is_none() {
            return -EFAULT;
        }
    }
    -ERESTARTNOHAND
}

/// The `struct timespec` at `addr`, in milliseconds, rounded up.
fn timespec_ms(process: &Process, addr: u64) -> Result<u64, i64> {
    let mut timespec = [0u8; 16];
    process.memory.lock().space.read(addr, &mut timespec).ok_or(EFAULT)?;
    let seconds = i64::from_le_bytes(timespec[..8].try_into().unwrap());
    let nanos = i64::from_le_bytes(timespec[8..].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(EINVAL);
    }
    Ok((seconds as u64).saturating_mul(1000).saturating_add((nanos as u64).div_ceil(1_000_000)))
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
