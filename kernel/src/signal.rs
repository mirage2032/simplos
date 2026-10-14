//! Signals, as Linux has them: what a program is sent, by another with
//! `kill`, by its terminal (^C), or by the kernel (a child ending, a pipe
//! no one reads), and what becomes of it: the action the program set with
//! `rt_sigaction`, a function of its own run on its stack, with what it was
//! doing saved below it for `rt_sigreturn` to put back; or else the signal's
//! default, which ends the program, stops it, or does nothing.
//!
//! A signal is acted on as its program goes back to ring 3: as a system call
//! returns, which it ends early if the call was waiting, and as the timer
//! interrupts the program.
//!
//! shortcut: a fault ends its program with the signal it raises even if it
//! has a handler for it; handlers for SIGSEGV and the like aren't run. And
//! real-time signals are kept as the others are, one of each at a time,
//! where Linux queues them.

use crate::io::interrupts::gdt;
use crate::process::{self, Process, Registers};
use crate::thread::{self, WaitQueue};
use alloc::sync::Arc;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicPtr, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

pub const SIGHUP: usize = 1;
pub const SIGINT: usize = 2;
pub const SIGQUIT: usize = 3;
pub const SIGILL: usize = 4;
pub const SIGTRAP: usize = 5;
pub const SIGABRT: usize = 6;
pub const SIGBUS: usize = 7;
pub const SIGFPE: usize = 8;
pub const SIGKILL: usize = 9;
pub const SIGSEGV: usize = 11;
pub const SIGPIPE: usize = 13;
pub const SIGCHLD: usize = 17;
pub const SIGCONT: usize = 18;
pub const SIGSTOP: usize = 19;
pub const SIGTSTP: usize = 20;
pub const SIGTTIN: usize = 21;
pub const SIGTTOU: usize = 22;
pub const SIGURG: usize = 23;
pub const SIGXCPU: usize = 24;
pub const SIGXFSZ: usize = 25;
pub const SIGWINCH: usize = 28;
pub const SIGSYS: usize = 31;
/// Signals are 1 to this.
pub const COUNT: usize = 64;

/// The errnos a call a signal ends early returns, which never reach the
/// program: `deliver` makes them EINTR if a handler ran, or else starts the
/// call again; ERESTARTSYS's starts again after a handler too, if the
/// handler's SA_RESTART asks.
pub const ERESTARTSYS: i64 = 512;
pub const ERESTARTNOHAND: i64 = 514;
const EINTR: i64 = 4;

const SIG_DFL: u64 = 0;
const SIG_IGN: u64 = 1;

const SA_NOCLDSTOP: u64 = 1;
const SA_NOCLDWAIT: u64 = 2;
const SA_ONSTACK: u64 = 0x0800_0000;
const SA_RESTART: u64 = 0x1000_0000;
const SA_NODEFER: u64 = 0x4000_0000;
const SA_RESETHAND: u64 = 0x8000_0000;

/// `si_code`s: sent by `kill`, by `tkill`, by the kernel; and why a child's
/// SIGCHLD came.
pub const SI_USER: i32 = 0;
pub const SI_TKILL: i32 = -6;
pub const SI_KERNEL: i32 = 0x80;
pub const CLD_EXITED: i32 = 1;
pub const CLD_KILLED: i32 = 2;
pub const CLD_STOPPED: i32 = 5;
pub const CLD_CONTINUED: i32 = 6;

/// What `wait4` reports of a child that continued.
pub const CONTINUED: i32 = 0xffff;

/// Signal `signal`'s bit in a set.
pub const fn bit(signal: usize) -> u64 {
    1 << (signal - 1)
}

/// What can't be blocked, caught or ignored.
const UNBLOCKABLE: u64 = bit(SIGKILL) | bit(SIGSTOP);
const STOPS: u64 = bit(SIGSTOP) | bit(SIGTSTP) | bit(SIGTTIN) | bit(SIGTTOU);

/// What a signal does when its program hasn't said.
#[derive(PartialEq, Eq)]
enum Default {
    End,
    Stop,
    Ignore,
}

fn default(signal: usize) -> Default {
    match signal {
        SIGCHLD | SIGCONT | SIGURG | SIGWINCH => Default::Ignore,
        SIGSTOP | SIGTSTP | SIGTTIN | SIGTTOU => Default::Stop,
        // shortcut: those whose default dumps core just end the program,
        // as there's nowhere to dump it.
        _ => Default::End,
    }
}

/// Linux's `struct sigaction`, as `rt_sigaction` passes it.
#[derive(Clone, Copy, PartialEq, Eq)]
pub struct Action {
    handler: u64,
    flags: u64,
    restorer: u64,
    mask: u64,
}

impl Action {
    const DEFAULT: Action = Action { handler: SIG_DFL, flags: 0, restorer: 0, mask: 0 };
    pub const SIZE: usize = 32;

    pub fn to_bytes(self) -> [u8; Action::SIZE] {
        let mut bytes = [0; Action::SIZE];
        for (at, word) in [self.handler, self.flags, self.restorer, self.mask].iter().enumerate() {
            bytes[at * 8..at * 8 + 8].copy_from_slice(&word.to_le_bytes());
        }
        bytes
    }

    pub fn from_bytes(bytes: &[u8; Action::SIZE]) -> Action {
        let word = |at: usize| u64::from_le_bytes(bytes[at * 8..at * 8 + 8].try_into().unwrap());
        Action { handler: word(0), flags: word(1), restorer: word(2), mask: word(3) & !UNBLOCKABLE }
    }

    /// Whether `signal` with this action is thrown away.
    fn ignores(&self, signal: usize) -> bool {
        bit(signal) & UNBLOCKABLE == 0
            && (self.handler == SIG_IGN
                || (self.handler == SIG_DFL && default(signal) == Default::Ignore))
    }
}

/// Who sent a signal, and why: what goes in the handler's `siginfo_t`.
#[derive(Clone, Copy, Default)]
pub struct Info {
    pub code: i32,
    pub pid: u64,
    /// A child's exit status, or the signal that ended or stopped it.
    pub status: i32,
}

impl Info {
    pub fn user(pid: u64) -> Info {
        Info { code: SI_USER, pid, status: 0 }
    }

    pub fn kernel() -> Info {
        Info { code: SI_KERNEL, pid: 0, status: 0 }
    }
}

/// `sigaltstack`'s: where handlers that ask to are run instead of on the
/// program's stack.
#[derive(Clone, Copy)]
pub struct AltStack {
    pub sp: u64,
    pub size: u64,
    pub enabled: bool,
}

impl AltStack {
    const NONE: AltStack = AltStack { sp: 0, size: 0, enabled: false };

    /// Whether `rsp` is on it.
    pub fn holds(&self, rsp: u64) -> bool {
        self.enabled && rsp > self.sp && rsp - self.sp <= self.size
    }
}

/// A program's signals: what's pending and blocked, what each does, and
/// whether it's stopped.
pub struct Signals {
    /// By bit (`bit`); taken by anyone, with interrupts on or off.
    pending: AtomicU64,
    blocked: AtomicU64,
    /// Taken with interrupts on, as are the next three.
    actions: Mutex<[Action; COUNT]>,
    /// Each pending signal's sender.
    info: Mutex<[Info; COUNT]>,
    altstack: Mutex<AltStack>,
    /// `rt_sigsuspend`'s: the mask to go back to once a handler has run.
    saved_mask: Mutex<Option<u64>>,
    /// The queue its thread waits in, while it waits in a way a signal ends,
    /// for `send` to wake it.
    waiting_on: AtomicPtr<WaitQueue>,
    /// Woken by nothing but signals: what `pause` waits on.
    pub(crate) arrived: WaitQueue,
    stopped: AtomicBool,
    /// What `wait4` hasn't yet told the parent: that it stopped, or 0xffff
    /// that it continued; 0 for nothing.
    report: AtomicI32,
    continued: WaitQueue,
}

impl Signals {
    pub fn new() -> Signals {
        Signals::with([Action::DEFAULT; COUNT], 0, AltStack::NONE)
    }

    fn with(actions: [Action; COUNT], blocked: u64, altstack: AltStack) -> Signals {
        Signals {
            pending: AtomicU64::new(0),
            blocked: AtomicU64::new(blocked),
            actions: Mutex::new(actions),
            info: Mutex::new([Info::default(); COUNT]),
            altstack: Mutex::new(altstack),
            saved_mask: Mutex::new(None),
            waiting_on: AtomicPtr::new(core::ptr::null_mut()),
            arrived: WaitQueue::new(),
            stopped: AtomicBool::new(false),
            report: AtomicI32::new(0),
            continued: WaitQueue::new(),
        }
    }

    /// A child's, as `fork` makes it: the same actions, mask and alternate
    /// stack, and nothing pending.
    pub fn fork(&self) -> Signals {
        let blocked = self.blocked.load(Ordering::Relaxed);
        Signals::with(*self.actions.lock(), blocked, *self.altstack.lock())
    }

    /// As `execve` leaves them: handlers back to the default, as the code they
    /// were in is gone, and no alternate stack; what's ignored, blocked and
    /// pending stays.
    pub fn exec(&self) {
        for action in self.actions.lock().iter_mut() {
            if action.handler != SIG_IGN {
                *action = Action::DEFAULT;
            }
        }
        *self.altstack.lock() = AltStack::NONE;
    }

    /// Whether a signal is pending that isn't blocked.
    pub fn has_signal(&self) -> bool {
        self.pending.load(Ordering::Acquire) & !self.blocked.load(Ordering::Relaxed) != 0
    }

    pub fn pending(&self) -> u64 {
        self.pending.load(Ordering::Acquire)
    }

    pub fn blocked(&self) -> u64 {
        self.blocked.load(Ordering::Relaxed)
    }

    /// Block `mask`, all but what can't be.
    pub fn set_blocked(&self, mask: u64) {
        self.blocked.store(mask & !UNBLOCKABLE, Ordering::Relaxed);
    }

    /// For `rt_sigsuspend`: block `mask` until a handler has run, then what
    /// was blocked before.
    pub fn suspend_with(&self, mask: u64) {
        *self.saved_mask.lock() = Some(self.blocked());
        self.set_blocked(mask);
    }

    pub fn action(&self, signal: usize) -> Action {
        self.actions.lock()[signal - 1]
    }

    /// Set what `signal` does. One that's now ignored and pending goes, as
    /// POSIX says.
    pub fn set_action(&self, signal: usize, action: Action) {
        self.actions.lock()[signal - 1] = action;
        if action.ignores(signal) {
            self.pending.fetch_and(!bit(signal), Ordering::AcqRel);
        }
    }

    /// Whether the parent asked not to hear when its children stop or
    /// continue (SA_NOCLDSTOP).
    pub fn ignores_child_stops(&self) -> bool {
        self.action(SIGCHLD).flags & SA_NOCLDSTOP != 0
    }

    /// Whether children that end are freed at once, never waited for: as
    /// asked by ignoring SIGCHLD, or with SA_NOCLDWAIT.
    pub fn reaps_children(&self) -> bool {
        let action = self.action(SIGCHLD);
        action.handler == SIG_IGN || action.flags & SA_NOCLDWAIT != 0
    }

    pub fn altstack(&self) -> AltStack {
        *self.altstack.lock()
    }

    pub fn set_altstack(&self, altstack: AltStack) {
        *self.altstack.lock() = altstack;
    }

    /// What `wait4` hasn't told the parent: the stop, if `stops`; the
    /// continuing, if `continues`. Taken, so it's told once.
    pub fn take_report(&self, stops: bool, continues: bool) -> Option<i32> {
        let report = self.report.load(Ordering::Acquire);
        let wanted = match report {
            0 => false,
            CONTINUED => continues,
            _ => stops,
        };
        (wanted
            && self.report.compare_exchange(report, 0, Ordering::AcqRel, Ordering::Acquire).is_ok())
        .then_some(report)
    }

    /// Wake its thread if it waits in a way a signal ends.
    fn wake(&self) {
        interrupts::without_interrupts(|| {
            let queue = self.waiting_on.load(Ordering::Acquire);
            // Safety: set only while its thread is in the queue's
            // `wait_until`, which the queue outlives, and with interrupts
            // off on the one CPU that thread can't run and leave it now.
            if let Some(queue) = unsafe { queue.as_ref() } {
                queue.wake_all();
            }
        });
    }

    /// For the queue's waiter, before it waits: `queue` is what to wake it
    /// in; none once it's done waiting.
    pub(crate) fn waiting_in(&self, queue: Option<&WaitQueue>) {
        let queue = queue.map_or(core::ptr::null_mut(), |queue| queue as *const _ as *mut _);
        self.waiting_on.store(queue, Ordering::Release);
    }
}

impl core::default::Default for Signals {
    fn default() -> Signals {
        Signals::new()
    }
}

/// Send `signal` to `target`, from `info`. One the target ignores goes, but
/// for one it blocks, which waits; SIGCONT continues it now, if it's stopped,
/// whatever it does with SIGCONT.
pub fn send(target: &Process, signal: usize, info: Info) {
    let signals = &target.signals;
    if signal == SIGCONT {
        signals.pending.fetch_and(!STOPS, Ordering::AcqRel);
        if signals.stopped.swap(false, Ordering::AcqRel) {
            signals.report.store(CONTINUED, Ordering::Release);
            target.notify_parent(CLD_CONTINUED, SIGCONT as i32);
            signals.continued.wake_all();
        }
    } else if bit(signal) & STOPS != 0 {
        signals.pending.fetch_and(!bit(SIGCONT), Ordering::AcqRel);
    }
    if signals.action(signal).ignores(signal) && signals.blocked() & bit(signal) == 0 {
        return;
    }
    signals.info.lock()[signal - 1] = info;
    signals.pending.fetch_or(bit(signal), Ordering::AcqRel);
    signals.wake();
}

/// As the timer interrupt goes back to the current thread's program: if it
/// has a signal to take, act on it, `registers` being what the program had,
/// and go back to it from here.
pub fn on_interrupt_return(registers: impl FnOnce() -> Registers) {
    let Some(process) = thread::current_process() else { return };
    if !process.signals.has_signal() {
        return;
    }
    // As a system call runs: what follows can take locks and wait.
    interrupts::enable();
    let mut registers = registers();
    deliver(process, &mut registers, None);
    process::enter_user(registers)
}

/// What became of the system call a program is back from: its number, and
/// what it returned, minus an errno if it failed.
pub struct Syscall {
    pub number: u64,
    pub result: i64,
}

impl Syscall {
    /// Whether a signal ended it early.
    fn interrupted(&self) -> bool {
        self.result == -ERESTARTSYS || self.result == -ERESTARTNOHAND
    }
}

/// Act on the signals the current thread's program, `process`, has pending
/// and doesn't block, before it goes back to ring 3 with `registers`, maybe
/// from `syscall`. A handler is run by changing `registers` to its start; the
/// next waits for its `rt_sigreturn`. A default action may end the program,
/// and then this doesn't return, having let go of `process` first.
pub fn deliver(process: Arc<Process>, registers: &mut Registers, mut syscall: Option<Syscall>) {
    let signals = &process.signals;
    loop {
        let ready = signals.pending() & !signals.blocked();
        if ready == 0 {
            break;
        }
        // SIGKILL first; then the lowest, as Linux does.
        let signal = match ready & bit(SIGKILL) {
            0 => ready.trailing_zeros() as usize + 1,
            _ => SIGKILL,
        };
        signals.pending.fetch_and(!bit(signal), Ordering::AcqRel);
        let action = signals.action(signal);
        if action.handler == SIG_IGN && bit(signal) & UNBLOCKABLE == 0 {
            continue;
        }
        if action.handler == SIG_DFL || bit(signal) & UNBLOCKABLE != 0 {
            match default(signal) {
                Default::Ignore => continue,
                Default::Stop => {
                    // Not one sent by a terminal to a group that's orphaned:
                    // no one outside it is left to continue it, as POSIX
                    // says.
                    if signal == SIGSTOP || !process::group_orphaned(process.pgid()) {
                        stop(&process, signal);
                    }
                    continue;
                }
                Default::End => {
                    drop(process);
                    process::kill_current(signal);
                }
            }
        }
        // A handler.
        if let Some(call) = syscall.take().filter(Syscall::interrupted) {
            match call.result == -ERESTARTSYS && action.flags & SA_RESTART != 0 {
                true => restart(registers, call.number),
                false => registers.rax = -EINTR as u64,
            }
        }
        let info = signals.info.lock()[signal - 1];
        let old_mask = signals.saved_mask.lock().take().unwrap_or(signals.blocked());
        if setup_frame(&process, registers, signal, action, info, old_mask).is_none() {
            // Nowhere to run it: as Linux does, the program ends.
            drop(process);
            process::kill_current(SIGSEGV);
        }
        let deferred = if action.flags & SA_NODEFER != 0 { 0 } else { bit(signal) };
        signals.set_blocked(signals.blocked() | action.mask | deferred);
        if action.flags & SA_RESETHAND != 0 {
            signals.actions.lock()[signal - 1] = Action::DEFAULT;
        }
        return;
    }
    // No handler ran: a call a signal ended starts again, as if it hadn't
    // been, and `rt_sigsuspend`'s mask goes back.
    if let Some(call) = syscall.filter(Syscall::interrupted) {
        restart(registers, call.number);
    }
    if let Some(mask) = signals.saved_mask.lock().take() {
        signals.set_blocked(mask);
    }
}

/// Back to the `syscall` instruction, two bytes back, with its number again.
fn restart(registers: &mut Registers, number: u64) {
    registers.rax = number;
    registers.rip -= 2;
}

/// Stop `process`, the current thread's, as `signal` would: until SIGCONT
/// continues it or SIGKILL comes.
fn stop(process: &Process, signal: usize) {
    let signals = &process.signals;
    signals.stopped.store(true, Ordering::Release);
    signals.report.store(((signal as i32) << 8) | 0x7f, Ordering::Release);
    process.notify_parent(CLD_STOPPED, signal as i32);
    signals.waiting_in(Some(&signals.continued));
    signals.continued.wait_until(|| {
        !signals.stopped.load(Ordering::Acquire) || signals.pending() & bit(SIGKILL) != 0
    });
    signals.waiting_in(None);
}

/// A signal frame's parts and size: the restorer's address, the
/// `ucontext`, then the `siginfo_t`.
const UCONTEXT: usize = 304;
const SIGINFO: usize = 128;
const FRAME: usize = 8 + UCONTEXT + SIGINFO;
/// Where the general registers are in the `ucontext`: `uc_mcontext`, as
/// `struct sigcontext` has them; then the FPU state's address, and the mask.
const GREGS: usize = 40;
const FPSTATE: usize = GREGS + 23 * 8;
const SIGMASK: usize = GREGS + 256;

/// Put the frame for a handler of `signal` on the program's stack, or its
/// alternate one, as Linux's x86-64 frame is laid out, and point `registers`
/// at the handler. `None` if the stack can't be written.
fn setup_frame(
    process: &Process,
    registers: &mut Registers,
    signal: usize,
    action: Action,
    info: Info,
    old_mask: u64,
) -> Option<()> {
    const SS_ONSTACK: u64 = 1;
    const SS_DISABLE: u64 = 2;
    let altstack = process.signals.altstack();
    let on_altstack = altstack.holds(registers.rsp);
    let top = match action.flags & SA_ONSTACK != 0 && altstack.enabled && !on_altstack {
        true => altstack.sp + altstack.size,
        // Past the red zone the ABI gives code below its stack pointer.
        false => registers.rsp.checked_sub(128)?,
    };
    let fpstate = top.checked_sub(512)? & !63;
    // So that the handler starts as if called: 8 bytes off 16-byte aligned.
    let frame = (fpstate.checked_sub(FRAME as u64)? & !15) - 8;

    let mut bytes = [0u8; FRAME];
    let mut put = |at: usize, value: u64| bytes[at..at + 8].copy_from_slice(&value.to_le_bytes());
    put(0, action.restorer);
    let uc = 8;
    let stack_flags = match (altstack.enabled, altstack.holds(frame)) {
        (_, true) => SS_ONSTACK,
        (false, _) => SS_DISABLE,
        (true, false) => 0,
    };
    put(uc + 16, altstack.sp);
    put(uc + 24, stack_flags);
    put(uc + 32, altstack.size);
    let selectors = gdt::selectors();
    let segments =
        u64::from(selectors.user_code_selector.0) | u64::from(selectors.user_data_selector.0) << 48;
    let r = &*registers;
    let gregs = [
        r.r8, r.r9, r.r10, r.r11, r.r12, r.r13, r.r14, r.r15, r.rdi, r.rsi, r.rbp, r.rbx, r.rdx,
        r.rax, r.rcx, r.rsp, r.rip, r.rflags, segments, 0, 0, old_mask, 0,
    ];
    for (at, value) in gregs.into_iter().enumerate() {
        put(uc + GREGS + at * 8, value);
    }
    put(uc + FPSTATE, fpstate);
    put(uc + SIGMASK, old_mask);
    let si = 8 + UCONTEXT;
    let mut put_int =
        |at: usize, value: i32| bytes[at..at + 4].copy_from_slice(&value.to_le_bytes());
    put_int(si, signal as i32);
    put_int(si + 8, info.code);
    put_int(si + 16, info.pid as i32);
    put_int(si + 24, info.status);

    let fpu = process::save_fpu();
    let mut memory = process.memory.lock();
    memory.space.write(fpstate, &fpu)?;
    memory.space.write(frame, &bytes)?;
    drop(memory);
    // The handler starts with the FPU as a new program's, as on Linux.
    process::reset_fpu();
    *registers = Registers {
        rip: action.handler,
        rsp: frame,
        rdi: signal as u64,
        rsi: frame + si as u64,
        rdx: frame + uc as u64,
        rax: 0,
        // Not single-stepping, and forwards, as a function expects.
        rflags: registers.rflags & !(0x100 | 0x400),
        ..*registers
    };
    Some(())
}

/// `rt_sigreturn`: back from a handler, to the registers, mask and FPU state
/// its frame saved; `uc` is the frame's `ucontext`, where the handler's
/// return to the restorer left the stack pointer. `None` if it can't be
/// read, which on Linux ends the program with SIGSEGV.
pub fn sigreturn(process: &Process, uc: u64) -> Option<Registers> {
    let mut bytes = [0u8; UCONTEXT];
    process.memory.lock().space.read(uc, &mut bytes)?;
    let word = |at: usize| u64::from_le_bytes(bytes[at..at + 8].try_into().unwrap());
    let g = |register: usize| word(GREGS + register * 8);
    let registers = Registers {
        r8: g(0),
        r9: g(1),
        r10: g(2),
        r11: g(3),
        r12: g(4),
        r13: g(5),
        r14: g(6),
        r15: g(7),
        rdi: g(8),
        rsi: g(9),
        rbp: g(10),
        rbx: g(11),
        rdx: g(12),
        rax: g(13),
        rcx: g(14),
        rsp: g(15),
        rip: g(16),
        rflags: g(17),
    };
    match word(FPSTATE) {
        0 => process::reset_fpu(),
        fpstate => {
            let mut fpu = [0u8; 512];
            process.memory.lock().space.read(fpstate, &mut fpu)?;
            process::load_fpu(&fpu);
        }
    }
    process.signals.set_blocked(word(SIGMASK));
    Some(registers)
}
