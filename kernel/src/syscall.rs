//! System calls, with Linux's numbers and calling convention, so programs
//! built for Linux run unchanged.
//!
//! The `syscall` instruction lands in [`entry`] in ring 0, still on the
//! program's stack and with interrupts off. It moves to the thread's kernel
//! stack, and [`dispatch`] runs the call with interrupts on, so a call can block
//! and be preempted like any kernel code. Every pointer a program passes is
//! checked against its page tables (`AddressSpace::read` and `write`): a bad
//! one is EFAULT, never a fault in the kernel.

mod clocks;
mod files;
mod processes;
mod signals;
mod system;

pub use system::{DEFAULT_LIMITS, LIMITS, RELEASE};

use crate::fs::Handle;
use crate::io::interrupts::gdt;
use crate::io::interrupts::pit::TIMER_HZ;
use crate::memory::USER_END;
use crate::process::{self, LOWEST, MMAP_END, MMAP_START, Process, Registers, Task, pages};
use crate::signal::{self, ERESTARTNOHAND, Syscall};
use crate::{serial_println, thread};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::model_specific::{Efer, EferFlags, LStar, SFMask, Star};
use x86_64::registers::rflags::RFlags;
use x86_64::structures::paging::{Page, PageTableFlags};

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
        80 => "Accessing a corrupted shared library",
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
    if let Some((process, task)) = thread::current_user()
        && (signal::has_signal(&process, &task)
            || result == -signal::ERESTARTSYS
            || result == -ERESTARTNOHAND)
    {
        let mut registers = frame.registers(result as u64);
        let call = Some(Syscall { number: frame.nr, result });
        signal::deliver(process, task, &mut registers, call);
        process::enter_user(registers);
    }
    interrupts::disable();
    result
}

fn call(frame: &Frame) -> i64 {
    let (process, task) = thread::current_user().expect("system calls come from programs");
    let [a0, a1, a2, a3, a4, a5] = [frame.rdi, frame.rsi, frame.rdx, frame.r10, frame.r8, frame.r9];
    match frame.nr {
        0 => files::read(&process, a0, a1, a2),
        1 => files::write(&process, a0, a1, a2),
        19 => files::readv(&process, a0, a1, a2),
        20 => files::writev(&process, a0, a1, a2),
        2 => files::open(&process, files::AT_FDCWD, a0, a1, a2),
        3 => files::close(&process, a0),
        // stat, and lstat, which doesn't follow a link at the end.
        4 => files::newfstatat(&process, files::AT_FDCWD, a0, a1, 0),
        6 => files::newfstatat(&process, files::AT_FDCWD, a0, a1, files::AT_SYMLINK_NOFOLLOW),
        5 => files::fstat(&process, a0, a1),
        7 => poll(&process, a0, a1, (a2 as i32 >= 0).then_some(a2 as i32 as u64)),
        8 => files::lseek(&process, a0, a1, a2),
        9 => mmap(&process, a0, a1, a2, a3, a4, a5),
        10 => mprotect(&process, a0, a1, a2),
        11 => munmap(&process, a0, a1),
        12 => brk(&process, a0),
        13 => signals::rt_sigaction(&process, a0, a1, a2, a3),
        14 => signals::rt_sigprocmask(&process, &task, a0, a1, a2, a3),
        15 => match signal::sigreturn(&process, &task, frame.rsp) {
            Some(mut registers) => {
                signal::deliver(process, task, &mut registers, None);
                process::enter_user(registers)
            }
            None => {
                drop((process, task));
                process::kill_current(signal::SIGSEGV)
            }
        },
        16 => files::ioctl(&process, a0, a1, a2),
        17 => files::pread(&process, a0, a1, a2, a3),
        18 => files::pwrite(&process, a0, a1, a2, a3),
        21 => files::faccessat(&process, files::AT_FDCWD, a0, a1, 0),
        22 => files::pipe2(&process, a0, 0),
        23 => match a4 {
            0 => select(&process, a0, [a1, a2, a3], None),
            timeout => match timeval_ms(&process, timeout) {
                Ok(ms) => select(&process, a0, [a1, a2, a3], Some(ms)),
                Err(errno) => -errno,
            },
        },
        24 => {
            thread::yield_now();
            0
        }
        28 => madvise(&process, a0, a1, a2),
        25 => mremap(&process, a0, a1, a2, a3, a4),
        32 => files::dup(&process, a0),
        33 => files::dup3(&process, a0, a1, 0, true),
        34 => signals::pause(&task),
        35 => clock_nanosleep(&process, CLOCK_MONOTONIC, 0, a0, a1),
        36 => clocks::getitimer(&process, a0, a1),
        37 => clocks::alarm(&process, a0),
        38 => clocks::setitimer(&process, a0, a1, a2),
        39 => process.id as i64,
        40 => files::sendfile(&process, a0, a1, a2, a3),
        44 => files::sendto(&process, a0, a1, a2, a3, [a4, a5]),
        45 => files::recvfrom(&process, a0, a1, a2, a3, [a4, a5]),
        53 => files::socketpair(&process, a0, a1, a2, a3),
        188..=199 => files::xattr(&process, frame.nr, a0),
        221 => files::fadvise(&process, a0, a3),
        275 => files::splice(&process, [a0, a1], [a2, a3], a4, a5),
        // The copy or the new thread starts as the call returns, with 0
        // its result.
        56 => processes::clone(&process, frame.registers(0), a0, a1, [a2, a3], a4),
        // fork, vfork.
        57 | 58 => processes::clone(&process, frame.registers(0), processes::SIGCHLD, 0, [0; 2], 0),
        59 => match processes::execve(&process, a0, a1, a2) {
            Ok(mut registers) => {
                // Nothing on this stack is ever dropped: `deliver` lets go.
                signal::deliver(process, task, &mut registers, None);
                process::enter_user(registers)
            }
            Err(errno) => -errno,
        },
        60 => {
            drop((process, task));
            process::exit_thread(a0 as i32)
        }
        231 => {
            drop((process, task));
            process::exit_current(a0 as i32)
        }
        61 => processes::wait4(&process, a0, a1, a2, a3),
        62 => signals::kill(&process, a0, a1),
        63 => system::uname(&process, a0),
        72 => files::fcntl(&process, a0, a1, a2),
        79 => files::getcwd(&process, a0, a1),
        80 => files::chdir(&process, a0),
        81 => files::fchdir(&process, a0),
        82 => files::renameat2(&process, files::AT_FDCWD, a0, files::AT_FDCWD, a1, 0),
        83 => files::mkdirat(&process, files::AT_FDCWD, a0, a1),
        84 => files::unlinkat(&process, files::AT_FDCWD, a0, files::AT_REMOVEDIR),
        86 => files::linkat(&process, files::AT_FDCWD, a0, files::AT_FDCWD, a1, 0),
        87 => files::unlinkat(&process, files::AT_FDCWD, a0, 0),
        88 => files::symlinkat(&process, a0, files::AT_FDCWD, a1),
        89 => files::readlinkat(&process, files::AT_FDCWD, a0, a1, a2),
        90 => files::fchmodat(&process, files::AT_FDCWD, a0, a1),
        91 => files::fchmodat(&process, a0, 0, a1),
        92 => files::fchownat(&process, files::AT_FDCWD, a0, a1, a2, 0),
        93 => files::fchownat(&process, a0, 0, a1, a2, 0),
        94 => files::fchownat(&process, files::AT_FDCWD, a0, a1, a2, files::AT_SYMLINK_NOFOLLOW),
        95 => files::umask(&process, a0),
        96 => clocks::gettimeofday(&process, a0, a1),
        97 => system::prlimit(&process, 0, a0, 0, a1),
        98 => clocks::getrusage(&process, a0, a1),
        99 => system::sysinfo(&process, a0),
        100 => clocks::times(&process, a0),
        // getuid, getgid, geteuid and getegid.
        102 | 104 | 107 | 108 => 0,
        105 | 106 => system::set_id(a0),
        109 => signals::setpgid(&process, a0, a1),
        110 => process.parent_id() as i64,
        111 => process.pgid() as i64,
        112 => signals::setsid(&process),
        // getgroups, setgroups.
        115 | 116 => system::groups(a0),
        121 => signals::getpgid(&process, a0),
        124 => signals::getsid(&process, a0),
        127 => signals::rt_sigpending(&process, &task, a0, a1),
        128 => signals::rt_sigtimedwait(&process, &task, a0, a1, a2, a3),
        130 => signals::rt_sigsuspend(&process, &task, a0, a1),
        // fsync, fdatasync.
        74 | 75 => files::fsync(&process, a0),
        76 => files::truncate(&process, a0, a1),
        77 => files::ftruncate(&process, a0, a1),
        131 => signals::sigaltstack(&process, &task, a0, a1, frame.rsp),
        137 => files::statfs(&process, a0, a1),
        138 => files::fstatfs(&process, a0, a1),
        140 => system::getpriority(&process, a0, a1),
        141 => system::setpriority(&process, a0, a1, a2),
        158 => arch_prctl(&process, a0, a1),
        160 => system::prlimit(&process, 0, a0, a1, 0),
        162 => files::sync(),
        186 => task.id() as i64,
        200 => signals::tkill(&process, None, a0, a1),
        201 => clocks::time(&process, a0),
        202 => processes::futex(&process, a0, a1, a2, a3, a4, a5),
        204 => system::sched_getaffinity(&process, a0, a1, a2),
        217 => files::getdents64(&process, a0, a1, a2),
        // set_tid_address: where to write 0 and wake a futex as the thread
        // ends; its id.
        222 => clocks::timer_create(&process, a0, a1, a2),
        223 => clocks::timer_settime(&process, a0, a1, a2, a3),
        224 => clocks::timer_gettime(&process, a0, a1),
        225 => clocks::timer_getoverrun(&process, a0),
        226 => clocks::timer_delete(&process, a0),
        218 => {
            task.clear_tid.store(a0, Ordering::Relaxed);
            task.id() as i64
        }
        228 => clocks::clock_gettime(&process, a0, a1),
        229 => clocks::clock_getres(&process, a0, a1),
        230 => clock_nanosleep(&process, a0, a1, a2, a3),
        234 => signals::tkill(&process, Some(a0), a1, a2),
        257 => files::open(&process, a0, a1, a2, a3),
        258 => files::mkdirat(&process, a0, a1, a2),
        260 => files::fchownat(&process, a0, a1, a2, a3, a4),
        262 => files::newfstatat(&process, a0, a1, a2, a3),
        263 => files::unlinkat(&process, a0, a1, a2),
        264 => files::renameat2(&process, a0, a1, a2, a3, 0),
        265 => files::linkat(&process, a0, a1, a2, a3, a4),
        266 => files::symlinkat(&process, a0, a1, a2),
        267 => files::readlinkat(&process, a0, a1, a2, a3),
        268 => files::fchmodat(&process, a0, a1, a2),
        269 => files::faccessat(&process, a0, a1, a2, 0),
        270 => pselect6(&process, &task, a0, [a1, a2, a3], a4, a5),
        271 => ppoll(&process, &task, a0, a1, a2, a3, a4),
        280 => files::utimensat(&process, a0, a1, a2, a3),
        292 => files::dup3(&process, a0, a1, a2, false),
        293 => files::pipe2(&process, a0, a1),
        302 => system::prlimit(&process, a0, a1, a2, a3),
        // syncfs.
        306 => files::fsync(&process, a0),
        316 => files::renameat2(&process, a0, a1, a2, a3, a4),
        318 => system::getrandom(&process, a0, a1, a2),
        324 => processes::membarrier(a0, a1),
        439 => files::faccessat(&process, a0, a1, a2, a3),
        _ => {
            serial_println!("simplos: process {}: no system call {}", process.id, frame.nr);
            -ENOSYS
        }
    }
}

const POLLIN: i16 = 0x1;
const POLLPRI: i16 = 0x2;
const POLLOUT: i16 = 0x4;
const POLLERR: i16 = 0x8;
const POLLHUP: i16 = 0x10;
const POLLNVAL: i16 = 0x20;

/// What `handle` is ready for now, as `poll` tells it: to be read, written,
/// or that its other end has gone.
fn readiness(handle: &Handle) -> i16 {
    match handle {
        Handle::Tty { tty, .. } => POLLOUT | if tty.readable() { POLLIN } else { 0 },
        Handle::Pipe { end, .. } => {
            let pipe = end.poll();
            match (end.write, pipe.closed) {
                (false, closed) => {
                    (if pipe.now { POLLIN } else { 0 }) | if closed { POLLHUP } else { 0 }
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
        // Once the other end has closed, a read or a write won't wait: one
        // gets nothing, the other EPIPE.
        Handle::Socket { input, output, .. } => {
            let (incoming, outgoing) = (input.poll(), output.poll());
            match incoming.closed {
                true => POLLIN | POLLOUT | POLLHUP,
                false => {
                    (if incoming.now { POLLIN } else { 0 }) | if outgoing.now { POLLOUT } else { 0 }
                }
            }
        }
        // Files never make a program wait.
        _ => POLLIN | POLLOUT,
    }
}

/// The tick to wait until for `timeout` milliseconds, if there is one: 0,
/// for 0, which only looks.
fn deadline(timeout: Option<u64>) -> Option<u64> {
    timeout.map(|ms| if ms == 0 { 0 } else { thread::ticks_after(ms) })
}

/// Wait until one of the descriptors in the `struct pollfd`s at `fds` is
/// ready as asked, or for `timeout` milliseconds if it isn't `None`: how
/// many are, each told how in its `revents`.
fn poll(process: &Process, fds: u64, count: u64, timeout: Option<u64>) -> i64 {
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
                Some(Some(handle)) => readiness(handle),
            };
            *revents = ready & (events | POLLERR | POLLHUP | POLLNVAL);
        }
        revents.iter().any(|&revents| revents != 0)
    };
    if thread::poll_until(deadline(timeout), || check(&mut revents)).is_err() {
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

/// `poll`, with its timeout a `struct timespec` at `timeout`, or none; and
/// while it waits, the signals in the set at `mask` blocked instead, if it's
/// given, `size` bytes long. So a program can block a signal, check for it,
/// then wait for it or for input without missing it, as busybox's shells
/// do in `read`.
fn ppoll(
    process: &Process,
    task: &Task,
    fds: u64,
    count: u64,
    timeout: u64,
    mask: u64,
    size: u64,
) -> i64 {
    let ms = match timeout {
        0 => None,
        timeout => match timespec_ms(process, timeout) {
            Ok(ms) => Some(ms),
            Err(errno) => return -errno,
        },
    };
    masked(process, task, mask, size, || poll(process, fds, count, ms))
}

/// Run `wait`, a call that waits, with the signals in the set at `mask`
/// blocked instead for `task`, the current thread, if it's given, `size`
/// bytes long, as `ppoll` and `pselect6` wait.
fn masked(process: &Process, task: &Task, mask: u64, size: u64, wait: impl FnOnce() -> i64) -> i64 {
    if mask == 0 {
        return wait();
    }
    if size != 8 {
        return -EINVAL;
    }
    let mut set = [0u8; 8];
    if process.memory.lock().space.read(mask, &mut set).is_none() {
        return -EFAULT;
    }
    task.signals.suspend_with(u64::from_le_bytes(set));
    let ready = wait();
    // Ended by a signal, `deliver` puts the mask back: after the handler,
    // which its frame tells, or before starting the call again.
    if ready != -ERESTARTNOHAND {
        task.signals.end_suspend();
    }
    ready
}

/// Wait until any of the first `count` descriptors the `fd_set`s at `sets`
/// name, those that aren't 0, is ready: to be read, for the first; to be
/// written, for the second; or with something exceptional, for the third,
/// which none ever is here. Or for `timeout` milliseconds, if it isn't
/// `None`. How many are, each set left with just those.
fn select(process: &Process, count: u64, sets: [u64; 3], timeout: Option<u64>) -> i64 {
    // What each set takes to be ready, as Linux's does.
    const READY: [i16; 3] = [POLLIN | POLLHUP | POLLERR, POLLOUT | POLLERR, POLLPRI];
    if (count as i32) < 0 {
        return -EINVAL;
    }
    // None past the descriptors there can be, as Linux takes none past its.
    let count = (count as u32 as u64).min(files::MOST_FILES as u64);
    // A bit for each descriptor, in 64-bit words.
    let bytes = count.div_ceil(64) as usize * 8;
    let mut asked = [vec![0u8; bytes], vec![0u8; bytes], vec![0u8; bytes]];
    for (set, &addr) in asked.iter_mut().zip(&sets) {
        if addr != 0 && process.memory.lock().space.read(addr, set).is_none() {
            return -EFAULT;
        }
    }
    let has = |set: &[u8], fd: usize| set[fd / 8] & 1 << (fd % 8) != 0;
    // Each descriptor asked of, with which of the sets it's in.
    let mut entries = Vec::new();
    for fd in 0..count as usize {
        let of: Vec<usize> = (0..3).filter(|&set| has(&asked[set], fd)).collect();
        if !of.is_empty() {
            let Some(handle) = files::open_handle(process, fd as i32) else { return -EBADF };
            entries.push((fd, of, handle));
        }
    }
    let mut ready = asked.clone();
    let check = |ready: &mut [Vec<u8>; 3]| {
        ready.iter_mut().for_each(|set| set.fill(0));
        for (fd, of, handle) in &entries {
            let now = readiness(handle);
            for &set in of.iter().filter(|&&set| now & READY[set] != 0) {
                ready[set][fd / 8] |= 1 << (fd % 8);
            }
        }
        ready.iter().flatten().any(|&byte| byte != 0)
    };
    if thread::poll_until(deadline(timeout), || check(&mut ready)).is_err() {
        return -ERESTARTNOHAND;
    }
    for (set, &addr) in ready.iter().zip(&sets) {
        if addr != 0 && process.memory.lock().space.write(addr, set).is_none() {
            return -EFAULT;
        }
    }
    ready.iter().flatten().map(|byte| i64::from(byte.count_ones())).sum()
}

/// `select`, with its timeout a `struct timespec` at `timeout`, or none;
/// and while it waits, the signals blocked those of the set the pair at
/// `mask` names, its address and size, if it's given, as for `ppoll`.
fn pselect6(
    process: &Process,
    task: &Task,
    count: u64,
    sets: [u64; 3],
    timeout: u64,
    mask: u64,
) -> i64 {
    let ms = match timeout {
        0 => None,
        timeout => match timespec_ms(process, timeout) {
            Ok(ms) => Some(ms),
            Err(errno) => return -errno,
        },
    };
    let mut pair = [0u8; 16];
    if mask != 0 && process.memory.lock().space.read(mask, &mut pair).is_none() {
        return -EFAULT;
    }
    let word = |at: usize| u64::from_le_bytes(pair[at..at + 8].try_into().unwrap());
    masked(process, task, word(0), word(8), || select(process, count, sets, ms))
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
/// That stops a page short of the lower half's end, as Linux's does: the
/// last page is never mapped (see `dispatch`), and `USER_END` itself isn't
/// an address at all, which `pages` would panic making.
fn user_range(start: u64, len: u64) -> Option<(u64, u64)> {
    let end = start.checked_add(len)?.checked_next_multiple_of(4096)?;
    (start.is_multiple_of(4096) && start >= LOWEST && end < USER_END && len > 0)
        .then_some((start, end))
}

/// Memory for the program: zeros, anonymous; or from `offset` on in the file
/// `fd` is open on, as MAP_PRIVATE maps it, a copy of the program's own.
///
/// shortcut: a file is read into memory as it's mapped, rather than as it's
/// used, and nothing written to it after shows through. MAP_SHARED is taken
/// as MAP_PRIVATE when the mapping can't be written, as nothing could tell,
/// and refused (ENODEV) when it can be; and no MAP_SHARED anonymous memory.
fn mmap(
    process: &Process,
    addr: u64,
    len: u64,
    prot: u64,
    flags: u64,
    fd: u64,
    offset: u64,
) -> i64 {
    const MAP_SHARED: u64 = 0x01;
    const MAP_PRIVATE: u64 = 0x02;
    const MAP_FIXED: u64 = 0x10;
    const MAP_ANONYMOUS: u64 = 0x20;
    const EACCES: i64 = 13;
    const ENODEV: i64 = 19;
    let (shared, private) = (flags & MAP_SHARED != 0, flags & 3 == MAP_PRIVATE);
    if !shared && !private {
        return -EINVAL;
    }
    // The file, if it's one's: open to be read.
    let file = match flags & MAP_ANONYMOUS {
        0 => {
            if !offset.is_multiple_of(4096) {
                return -EINVAL;
            }
            let Some(handle) = files::open_handle(process, fd as i32) else { return -EBADF };
            match &*handle {
                Handle::File { read: false, .. } => return -EACCES,
                Handle::File { write, .. } if shared && prot & PROT_WRITE != 0 => {
                    return if *write { -ENODEV } else { -EACCES };
                }
                Handle::File { .. } => Some(handle),
                _ => return -ENODEV,
            }
        }
        _ if shared => return -EINVAL,
        _ => None,
    };
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
    // Nor will what's handed out after: past what MAP_FIXED maps.
    claim(&mut memory, start, end);
    // PROT_NONE: reserved, and left unmapped so any access faults.
    if prot == 0 {
        return start as i64;
    }
    // Reading the file takes the tree, which mustn't wait with this held:
    // /proc takes the two the other way round.
    if let Some(handle) = &file
        && prot & PROT_WRITE == 0
    {
        drop(memory);
        if let Err(errno) = map_file(process, handle, offset, start, len, page_flags(prot)) {
            process.memory.lock().space.unmap(pages(start, end));
            return -errno;
        }
        return start as i64;
    }
    if memory.space.map(pages(start, end), page_flags(prot)).is_none() {
        // What memory there was goes back: the program, told it got none,
        // has no address to unmap it by.
        memory.space.unmap(pages(start, end));
        return -ENOMEM;
    }
    drop(memory);
    if let Some(handle) = file
        && let Err(errno) = fill(process, &handle, offset, start, len)
    {
        process.memory.lock().space.unmap(pages(start, end));
        return -errno;
    }
    start as i64
}

/// Map the `len` bytes of the file `handle` is open on from `offset` on at
/// `start`, never writable, with `flags`: its own pages, from the page
/// cache, shared with whatever else maps them, so that what's written to
/// the file shows, as on Linux; zeros past its end.
fn map_file(
    process: &Process,
    handle: &Handle,
    offset: u64,
    start: u64,
    len: u64,
    flags: PageTableFlags,
) -> Result<(), i64> {
    let Handle::File { file, .. } = handle else { return Ok(()) };
    let size = {
        let _tree = crate::fs::lock();
        file.size().map_err(crate::fs::errno)?
    };
    for done in (0..len).step_by(4096) {
        let page = page(start + done);
        let frame = match offset + done < size {
            true => {
                let _tree = crate::fs::lock();
                Some(crate::cache::page(file, (offset + done) / 4096).map_err(crate::fs::errno)?)
            }
            false => None,
        };
        let mut memory = process.memory.lock();
        let mapped = match frame {
            Some(frame) => memory.space.map_shared(page, frame, flags),
            None => memory.space.map(Page::range(page, page + 1), flags),
        };
        mapped.ok_or(ENOMEM)?;
    }
    Ok(())
}

/// Copy `len` bytes of the file `handle` is open on, from `offset` on, into
/// the program's memory at `start`, already mapped: as many as there are,
/// zeros past its end.
fn fill(process: &Process, handle: &Handle, offset: u64, start: u64, len: u64) -> Result<(), i64> {
    const EIO: i64 = 5;
    let Handle::File { file, .. } = handle else { return Ok(()) };
    let mut piece = vec![0; 64 << 10];
    let mut done = 0;
    while done < len {
        let want = (len - done).min(piece.len() as u64) as usize;
        let read = {
            let _tree = crate::fs::lock();
            crate::cache::read(file, offset + done, &mut piece[..want]).map_err(|_| EIO)?
        };
        if read == 0 {
            break;
        }
        process.memory.lock().space.load(start + done, &piece[..read]).ok_or(EIO)?;
        done += read as u64;
    }
    Ok(())
}

/// Advice on how the program will use the memory at `addr`: taken, and of
/// it MADV_DONTNEED and MADV_FREE acted on, which say it needs nothing that
/// is there, which is zeroed, as Linux does to private anonymous memory.
///
/// A page of a file that's mapped read-only holds what the file does, and
/// stays as it is, as Linux would have it read from the file again.
///
/// shortcut: a file's page that's mapped writable is a copy, which is
/// zeroed too, where Linux reads it from the file again; and memory that's
/// PROT_NONE, which here isn't mapped, is ENOMEM.
fn madvise(process: &Process, addr: u64, len: u64, advice: u64) -> i64 {
    const MADV_DONTNEED: u64 = 4;
    const MADV_FREE: u64 = 8;
    // The advice there is, as Linux has it.
    if !matches!(advice, 0..=4 | 8..=25) {
        return -EINVAL;
    }
    if len == 0 {
        return if addr.is_multiple_of(4096) { 0 } else { -EINVAL };
    }
    let Some((start, end)) = user_range(addr, len) else { return -EINVAL };
    let mut memory = process.memory.lock();
    if pages(start, end).any(|page| memory.space.flags(page).is_none()) {
        return -ENOMEM;
    }
    if advice == MADV_DONTNEED || advice == MADV_FREE {
        memory.space.zero(pages(start, end));
    }
    0
}

/// Hand nothing out from `mmap_next` that `start..end` now maps: so all
/// that's mapped among what `mmap` hands out is below `mmap_next`.
fn claim(memory: &mut process::Memory, start: u64, end: u64) {
    if start < MMAP_END && end > memory.mmap_next {
        memory.mmap_next = end.min(MMAP_END);
    }
}

/// Make the mapping of `old_len` bytes at `old` `new_len` long: shorter
/// where it is; longer where it is if nothing's mapped after it, its new
/// pages zeroed as `mmap`'s; or else, with MREMAP_MAYMOVE, where there's
/// room, its pages taken there rather than copied, as musl's `realloc`
/// asks for of big blocks; or with MREMAP_FIXED too, to `new_addr`, what
/// was there going first. Where it is now.
///
/// shortcut: no MREMAP_DONTUNMAP; all of it must be mapped, so a part that
/// is PROT_NONE, which here is unmapped, is EFAULT; and it grows where it
/// is only if it's the last `mmap` handed out, where Linux grows into any
/// gap, so without MREMAP_MAYMOVE it's ENOMEM more often.
fn mremap(process: &Process, old: u64, old_len: u64, new_len: u64, flags: u64, to: u64) -> i64 {
    const MREMAP_MAYMOVE: u64 = 1;
    const MREMAP_FIXED: u64 = 2;
    let (may_move, fixed) = (flags & MREMAP_MAYMOVE != 0, flags & MREMAP_FIXED != 0);
    if flags & !(MREMAP_MAYMOVE | MREMAP_FIXED) != 0 || fixed && !may_move {
        return -EINVAL;
    }
    let Some((old, old_end)) = user_range(old, old_len) else { return -EINVAL };
    let Some(new_len) = new_len.checked_next_multiple_of(4096).filter(|&len| len > 0) else {
        return -EINVAL;
    };
    let mut memory = process.memory.lock();
    // What its new pages are mapped with: what its first is.
    let Some(prot) = memory.space.flags(page(old)) else { return -EFAULT };
    let prot = prot & (PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE);
    if pages(old, old_end).any(|page| memory.space.flags(page).is_none()) {
        return -EFAULT;
    }
    let old_len = old_end - old;
    let to = if fixed {
        let Some((to, to_end)) = user_range(to, new_len) else { return -EINVAL };
        if to < old_end && old < to_end {
            return -EINVAL;
        }
        memory.space.unmap(pages(to, to_end));
        claim(&mut memory, to, to_end);
        to
    } else if new_len <= old_len {
        memory.space.unmap(pages(old + new_len, old_end));
        return old as i64;
    } else if old_end == memory.mmap_next {
        // Where it is: nothing's mapped after the last handed out.
        let Some(end) = old.checked_add(new_len).filter(|&end| end <= MMAP_END) else {
            return -ENOMEM;
        };
        if memory.space.map(pages(old_end, end), prot).is_none() {
            memory.space.unmap(pages(old_end, end));
            return -ENOMEM;
        }
        memory.mmap_next = end;
        return old as i64;
    } else if !may_move {
        return -ENOMEM;
    } else {
        let to = memory.mmap_next;
        match to.checked_add(new_len).filter(|&end| end <= MMAP_END) {
            Some(end) => memory.mmap_next = end,
            None => return -ENOMEM,
        }
        to
    };
    // Its pages taken along, but for those past `new_len`, and new ones
    // after them, if it's grown.
    let taken = old_len.min(new_len);
    let new = pages(to + taken, to + new_len);
    if new_len > taken && memory.space.map(new, prot).is_none() {
        memory.space.unmap(new);
        return -ENOMEM;
    }
    if memory.space.move_pages(page(old), page(to), taken / 4096).is_none() {
        memory.space.unmap(new);
        return -ENOMEM;
    }
    memory.space.unmap(pages(old + taken, old_end));
    to as i64
}

/// The page at `addr`, an address in the program's half.
fn page(addr: u64) -> Page {
    Page::containing_address(VirtAddr::new(addr))
}

/// What the program may do with the pages at `addr`: with PROT_NONE,
/// nothing, but what's in them is kept for when it may again, as Linux
/// keeps it. Otherwise what `prot` says, of pages that `mmap` reserved with
/// PROT_NONE, which here aren't mapped, too: mapped now, zeroed, as musl
/// does with a thread's stack, and its guard page kept out of reach.
///
/// shortcut: a page nothing reserved is mapped as well, where Linux says
/// ENOMEM; keep track of what's reserved when something needs telling.
fn mprotect(process: &Process, addr: u64, len: u64, prot: u64) -> i64 {
    let Some((start, end)) = user_range(addr, len) else { return -EINVAL };
    let mut memory = process.memory.lock();
    if prot == 0 {
        memory.space.hide(pages(start, end));
        return 0;
    }
    // Those mapped get these flags and more; then just these.
    if memory.space.map(pages(start, end), page_flags(prot)).is_none() {
        return -ENOMEM;
    }
    if memory.space.protect(pages(start, end), page_flags(prot)).is_none() {
        return -ENOMEM;
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
            // As for `mmap`: the heap stays where it was, and so does memory.
            memory.space.unmap(pages(old_end, new_end));
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
const CLOCK_BOOTTIME: u64 = 7;

/// Sleep for the `struct timespec` at `request`, or with TIMER_ABSTIME until
/// `clock` says that time: what `std::thread::sleep` does, through
/// `clock_nanosleep`, and `nanosleep` too. A signal ends it early, with what
/// was left of a sleep for a while at `remain`.
///
/// shortcut: one that stops the program and continues it starts the whole
/// sleep again, where Linux sleeps for what's left.
fn clock_nanosleep(process: &Process, clock: u64, flags: u64, request: u64, remain: u64) -> i64 {
    const TIMER_ABSTIME: u64 = 1;
    if flags & !TIMER_ABSTIME != 0
        || ![CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME].contains(&clock)
    {
        return -EINVAL;
    }
    let ms = match flags {
        TIMER_ABSTIME => timespec_ns(process, request).and_then(|until| {
            Ok(until.saturating_sub(clocks::now(process, clock)?).div_ceil(1_000_000))
        }),
        _ => timespec_ms(process, request),
    };
    let ms = match ms {
        // Already past.
        Ok(0) if flags == TIMER_ABSTIME => return 0,
        Ok(ms) => ms,
        Err(errno) => return -errno,
    };
    let deadline = thread::ticks_after(ms);
    if thread::poll_until(Some(deadline), || false).is_ok() {
        return 0;
    }
    if remain != 0 && flags != TIMER_ABSTIME {
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

/// The `struct timeval` at `addr`, in milliseconds, rounded up: a million
/// microseconds and more are seconds, as Linux's `select` takes them.
fn timeval_ms(process: &Process, addr: u64) -> Result<u64, i64> {
    let mut timeval = [0u8; 16];
    process.memory.lock().space.read(addr, &mut timeval).ok_or(EFAULT)?;
    let seconds = i64::from_le_bytes(timeval[..8].try_into().unwrap());
    let micros = i64::from_le_bytes(timeval[8..].try_into().unwrap());
    if seconds < 0 || micros < 0 {
        return Err(EINVAL);
    }
    Ok((seconds as u64).saturating_mul(1000).saturating_add((micros as u64).div_ceil(1000)))
}

/// The `struct timespec` at `addr`, in milliseconds, rounded up.
fn timespec_ms(process: &Process, addr: u64) -> Result<u64, i64> {
    Ok(timespec_ns(process, addr)?.div_ceil(1_000_000))
}

/// The `struct timespec` at `addr`, in nanoseconds.
fn timespec_ns(process: &Process, addr: u64) -> Result<u64, i64> {
    let mut timespec = [0u8; 16];
    process.memory.lock().space.read(addr, &mut timespec).ok_or(EFAULT)?;
    let seconds = i64::from_le_bytes(timespec[..8].try_into().unwrap());
    let nanos = i64::from_le_bytes(timespec[8..].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000_000).contains(&nanos) {
        return Err(EINVAL);
    }
    Ok((seconds as u64).saturating_mul(1_000_000_000).saturating_add(nanos as u64))
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
