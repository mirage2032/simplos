//! User programs: Linux x86-64 executables, run in ring 3 in an address space
//! of their own. Each runs on one thread, whose system calls `syscall.rs`
//! handles, and which a fault ends without touching anything else. A program
//! can copy itself (`fork`), replace itself with another (`exec`), and wait
//! for the copies it made to end. Programs are in process groups, which are
//! in sessions, as on Linux: a session's leader has a terminal, and one of
//! its groups is in the terminal's foreground, which ^C signals.
//!
//! The lower half of a program's address space:
//!
//!   0x0040_0000          the program (a PIE is loaded here), then its heap (`brk`)
//!   0x1000_0000_0000     `mmap`, growing up
//!   0x7FFF_FFFF_0000     the top of the stack, which grows down. The 64 KiB above
//!                        it are never mapped, so no `syscall` can sit at the very
//!                        end of the lower half (see `syscall.rs`).

use crate::elf::{self, Elf};
use crate::fs;
use crate::io::interrupts::gdt;
use crate::memory::{AddressSpace, USER_END};
use crate::signal::{self, Info, Signals};
use crate::syscall;
use crate::thread::{self, WaitQueue};
use crate::tty::Tty;
use alloc::boxed::Box;
use alloc::collections::BTreeMap;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicI32, AtomicU32, AtomicU64, Ordering};
use spin::Mutex;
use x86_64::VirtAddr;
use x86_64::instructions::interrupts;
use x86_64::registers::control::{Cr0, Cr0Flags, Cr3, Cr3Flags, Cr4, Cr4Flags};
use x86_64::registers::model_specific::FsBase;
use x86_64::structures::paging::page::PageRange;
use x86_64::structures::paging::{Page, PageTableFlags, PhysFrame};

/// Where a position-independent program is loaded.
const PIE_BASE: u64 = 0x40_0000;
/// Nothing is mapped below this, so a null pointer and small offsets from one
/// always fault.
pub const LOWEST: u64 = 0x1_0000;
pub const MMAP_START: u64 = 0x0000_1000_0000_0000;
const STACK_TOP: u64 = 0x0000_7FFF_FFFF_0000;
/// shortcut: a fixed 256 KiB, mapped up front; grow it on page faults when a
/// program needs more.
pub const STACK_SIZE: u64 = 256 * 1024;
/// Where `mmap` stops: a gap below the stack.
pub const MMAP_END: u64 = STACK_TOP - STACK_SIZE - 0x10_0000;

/// What `status` holds until the program ends.
const RUNNING: i32 = i32::MIN;

/// The exit status of a program a segmentation fault ended, as a shell
/// reports it: 128 plus the signal.
pub const KILLED_SIGSEGV: i32 = 128 + signal::SIGSEGV as i32;

/// Why a program couldn't be loaded, for the system call's errno.
pub const NO_MEMORY: &str = "out of memory";
pub const TOO_LONG: &str = "arguments too long for the stack";

pub struct Process {
    pub id: u64,
    /// Taken only with interrupts on, by the program's own system calls.
    pub memory: Mutex<Memory>,
    /// `RUNNING`, then how it ended, as `wait4` reports it: what it passed
    /// to `exit` shifted up a byte, or the signal that ended it.
    status: AtomicI32,
    exited: WaitQueue,
    /// Its process group, and its session.
    pgid: AtomicU64,
    sid: AtomicU64,
    /// Its session's terminal, which /dev/tty opens, if it has one. Taken
    /// with interrupts on.
    pub tty: Mutex<Option<Arc<Tty>>>,
    pub signals: Signals,
    /// Its file descriptors, by number; `None` for one that's closed. Taken
    /// only with interrupts on, by the program's own system calls.
    pub files: Mutex<Vec<Option<Fd>>>,
    /// Where its relative paths start: a path from the root, without the
    /// leading slash, as `fs::resolve` makes them.
    pub cwd: Mutex<String>,
    /// The permission bits taken off what it creates.
    pub umask: AtomicU32,
    /// Its resource limits, soft and hard, by `RLIMIT_*` number. Taken with
    /// interrupts on.
    pub limits: Mutex<[(u64, u64); syscall::LIMITS]>,
    /// The program it's a copy of; none for one the kernel started.
    parent: Weak<Process>,
    /// The copies it made that it hasn't waited for, ended or not. Taken only
    /// with interrupts off, as `wait4` looks at them from
    /// `WaitQueue::wait_until`.
    ///
    /// shortcut: one that ends keeps its memory until it's waited for, and
    /// those never waited for go with their parent rather than to an init.
    pub children: Mutex<Vec<Arc<Process>>>,
    /// Woken as each of them ends.
    pub child_exited: WaitQueue,
}

/// A file descriptor: what it refers to, shared with its copies, and whether
/// `exec` closes it.
#[derive(Clone)]
pub struct Fd {
    pub handle: Arc<fs::Handle>,
    pub cloexec: bool,
}

pub struct Memory {
    pub space: AddressSpace,
    /// Where the heap starts, and where it ends now.
    pub brk_start: u64,
    pub brk: u64,
    /// Where the next `mmap` without an address goes.
    pub mmap_next: u64,
}

impl Process {
    /// Block until the program ends, and return its exit status as a shell
    /// reports it: what it passed to `exit`, or 128 plus the signal that
    /// ended it.
    pub fn wait(&self) -> i32 {
        self.exited.wait_until(|| self.ended().is_some());
        let status = self.ended().expect("it ended");
        match status & 0x7F {
            0 => status >> 8,
            signal => 128 + signal,
        }
    }

    /// How it ended, as `wait4` reports it, if it has.
    pub fn ended(&self) -> Option<i32> {
        Some(self.status.load(Ordering::Acquire)).filter(|&status| status != RUNNING)
    }

    /// Its parent's id; 0 if it has none.
    pub fn parent_id(&self) -> u64 {
        self.parent.upgrade().map_or(0, |parent| parent.id)
    }

    pub fn pgid(&self) -> u64 {
        self.pgid.load(Ordering::Relaxed)
    }

    pub fn set_pgid(&self, pgid: u64) {
        self.pgid.store(pgid, Ordering::Relaxed);
    }

    pub fn sid(&self) -> u64 {
        self.sid.load(Ordering::Relaxed)
    }

    /// `setsid`: lead a new session, its own group's, with no terminal.
    pub fn lead_session(&self) {
        self.sid.store(self.id, Ordering::Relaxed);
        self.pgid.store(self.id, Ordering::Relaxed);
        *self.tty.lock() = None;
    }

    /// Tell its parent, if it has one, that it ended, stopped or continued:
    /// SIGCHLD, unless the parent asked not to hear of stops, and whoever
    /// waits for it with `wait4`.
    pub fn notify_parent(&self, code: i32, status: i32) {
        let Some(parent) = self.parent.upgrade() else { return };
        let ended = code != signal::CLD_STOPPED && code != signal::CLD_CONTINUED;
        if ended || !parent.signals.ignores_child_stops() {
            signal::send(&parent, signal::SIGCHLD, Info { code, pid: self.id, status });
        }
        parent.child_exited.wake_all();
    }
}

impl Drop for Process {
    fn drop(&mut self) {
        TABLE.lock().remove(&self.id);
    }
}

/// Every process there is, by id, for `kill` to find. Taken with interrupts
/// on.
static TABLE: Mutex<BTreeMap<u64, Weak<Process>>> = Mutex::new(BTreeMap::new());

fn register(process: &Arc<Process>) {
    TABLE.lock().insert(process.id, Arc::downgrade(process));
}

/// The process numbered `id`, if there is one.
pub fn find(id: u64) -> Option<Arc<Process>> {
    TABLE.lock().get(&id).and_then(Weak::upgrade)
}

/// Every process there is.
pub fn all() -> Vec<Arc<Process>> {
    TABLE.lock().values().filter_map(Weak::upgrade).collect()
}

/// The processes in group `pgid`.
pub fn group(pgid: u64) -> Vec<Arc<Process>> {
    all().into_iter().filter(|process| process.pgid() == pgid).collect()
}

/// Send `signal` to every process in group `pgid`, from `info`.
pub fn signal_group(pgid: u64, signal: usize, info: Info) {
    for process in group(pgid) {
        signal::send(&process, signal, info);
    }
}

/// Whether group `pgid` is orphaned: no one in it has a parent in another
/// group of its session, who could continue it if it stopped. A group the
/// kernel started is, having no parent at all.
pub fn group_orphaned(pgid: u64) -> bool {
    !group(pgid).iter().any(|process| {
        process.parent.upgrade().is_some_and(|parent| {
            parent.pgid() != pgid && parent.sid() == process.sid() && parent.ended().is_none()
        })
    })
}

static NEXT_ID: AtomicU64 = AtomicU64::new(1);

/// Load `elf` into a new address space and start it on a thread of its own,
/// with `args` as its arguments (the first being its name, by convention).
/// Its standard input, output and error are a terminal no one types on,
/// whose output goes to `output` as it's written (`Tty::capture`).
pub fn spawn(
    elf: &[u8],
    args: &[&str],
    output: impl FnMut(&[u8]) + Send + 'static,
) -> Result<Arc<Process>, &'static str> {
    let tty = Arc::new(fs::Handle::tty(Arc::new(Tty::capture(Box::new(output)))));
    spawn_with(elf, args, &[], vec![Some(tty); 3])
}

/// `spawn`, with `env` as the program's environment, and `files` as its
/// descriptors to start with, by number: what its standard input, output
/// and error are, for one.
pub fn spawn_with(
    elf: &[u8],
    args: &[&str],
    env: &[&str],
    files: Vec<Option<Arc<fs::Handle>>>,
) -> Result<Arc<Process>, &'static str> {
    let args: Vec<&[u8]> = args.iter().map(|arg| arg.as_bytes()).collect();
    let env: Vec<&[u8]> = env.iter().map(|variable| variable.as_bytes()).collect();
    let (memory, start) = program(elf, &args, &env)?;
    let pml4 = memory.space.pml4();
    let tty = files.iter().flatten().find_map(|handle| match &**handle {
        fs::Handle::Tty { tty, .. } => Some(tty.clone()),
        _ => None,
    });
    let files = files.into_iter().map(|file| file.map(|handle| Fd { handle, cloexec: false }));
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    // It leads a session of its own, its own group in its terminal's
    // foreground, as if `setsid` and TIOCSCTTY had made it so.
    if let Some(tty) = &tty {
        tty.control(id, id);
    }
    let process = Arc::new(Process {
        id,
        memory: Mutex::new(memory),
        status: AtomicI32::new(RUNNING),
        exited: WaitQueue::new(),
        pgid: AtomicU64::new(id),
        sid: AtomicU64::new(id),
        tty: Mutex::new(tty),
        signals: Signals::new(),
        files: Mutex::new(files.collect()),
        cwd: Mutex::new(String::new()),
        umask: AtomicU32::new(0o022),
        limits: Mutex::new(syscall::DEFAULT_LIMITS),
        parent: Weak::new(),
        children: Mutex::new(Vec::new()),
        child_exited: WaitQueue::new(),
    });
    register(&process);
    let user = UserState {
        process: process.clone(),
        start,
        pml4,
        kernel_stack_top: VirtAddr::zero(),
        fs_base: 0,
        fpu: FpuState::new(),
    };
    thread::spawn_user(Box::new(user)).ok_or(NO_MEMORY)?;
    Ok(process)
}

/// `spawn_with`, for the program at `path` on the disk: or the errno
/// `execve` would give for it.
pub fn spawn_program(
    path: &str,
    args: &[&str],
    env: &[&str],
    files: Vec<Option<Arc<fs::Handle>>>,
) -> Result<Arc<Process>, i64> {
    let elf = fs::read_program(&fs::resolve("", path))?;
    spawn_with(&elf, args, env, files).map_err(load_errno)
}

/// The errno for why a program couldn't be loaded.
pub fn load_errno(error: &str) -> i64 {
    const E2BIG: i64 = 7;
    const ENOEXEC: i64 = 8;
    const ENOMEM: i64 = 12;
    match error {
        NO_MEMORY => ENOMEM,
        TOO_LONG => E2BIG,
        _ => ENOEXEC,
    }
}

/// `elf` loaded into a new address space, with `args` and `env` on its
/// stack, and the registers to start it with.
fn program(elf: &[u8], args: &[&[u8]], env: &[&[u8]]) -> Result<(Memory, Registers), &'static str> {
    let mut space = AddressSpace::new().ok_or(NO_MEMORY)?;
    let image = load(&mut space, elf)?;
    let rsp = build_stack(&mut space, args, env, &image)?;
    let memory = Memory { space, brk_start: image.end, brk: image.end, mmap_next: MMAP_START };
    Ok((memory, Registers { rip: image.entry, rsp, ..Registers::default() }))
}

/// A copy of `parent`, the current thread's program, as `fork` makes: its
/// memory, descriptors, working directory and umask, started on a thread of
/// its own with `registers`. Its id, or `None` when memory runs out.
pub fn fork(parent: &Arc<Process>, registers: Registers) -> Option<u64> {
    let memory = {
        let memory = parent.memory.lock();
        Memory { space: memory.space.duplicate()?, ..*memory }
    };
    let pml4 = memory.space.pml4();
    let child = Arc::new(Process {
        id: NEXT_ID.fetch_add(1, Ordering::Relaxed),
        memory: Mutex::new(memory),
        status: AtomicI32::new(RUNNING),
        exited: WaitQueue::new(),
        pgid: AtomicU64::new(parent.pgid()),
        sid: AtomicU64::new(parent.sid()),
        tty: Mutex::new(parent.tty.lock().clone()),
        signals: parent.signals.fork(),
        files: Mutex::new(parent.files.lock().clone()),
        cwd: Mutex::new(parent.cwd.lock().clone()),
        umask: AtomicU32::new(parent.umask.load(Ordering::Relaxed)),
        limits: Mutex::new(*parent.limits.lock()),
        parent: Arc::downgrade(parent),
        children: Mutex::new(Vec::new()),
        child_exited: WaitQueue::new(),
    });
    register(&child);
    let user = thread::with_current_user(|user| user.fork(child.clone(), pml4, registers))?;
    thread::spawn_user(Box::new(user))?;
    // Even if it has already ended, it's there to be waited for: the parent
    // can't wait until this returns.
    interrupts::without_interrupts(|| parent.children.lock().push(child.clone()));
    Some(child.id)
}

/// Replace `process`, the current thread's program, with `elf`, given `args`
/// and `env`: its memory, `fs` and SSE registers start afresh. The registers
/// to enter it with; or why it couldn't be loaded, and nothing has changed.
pub fn exec(
    process: &Process,
    elf: &[u8],
    args: &[&[u8]],
    env: &[&[u8]],
) -> Result<Registers, &'static str> {
    let (memory, start) = program(elf, args, env)?;
    let pml4 = memory.space.pml4();
    let old = core::mem::replace(&mut *process.memory.lock(), memory);
    thread::with_current_user(|user| user.start_over(pml4));
    // Only now that the CPU is off its page tables.
    drop(old);
    process.signals.exec();
    Ok(start)
}

/// End the current thread's program as `exit` does, with `code`, from one of
/// its system calls.
pub fn exit_current(code: i32) -> ! {
    end_current((code & 0xFF) << 8)
}

/// End the current thread's program as `signal` does: from a fault it
/// caused, or a signal it was sent.
pub fn kill_current(signal: usize) -> ! {
    end_current(signal as i32)
}

/// End the current thread's program, which ended as `status` says. Its memory
/// is freed once its thread is reaped and whoever waits for it lets go.
fn end_current(status: i32) -> ! {
    if let Some(process) = thread::current_process() {
        // Closing files can wait on the disk, and a fault comes here with
        // interrupts off.
        interrupts::enable();
        // Now, not when it's freed: whoever reads its pipes waits for them
        // to close.
        let tree = fs::lock();
        let files = core::mem::take(&mut *process.files.lock());
        drop(files);
        drop(tree);
        process.status.store(status, Ordering::Release);
        process.exited.wake_all();
        let (code, how) = match status & 0x7f {
            0 => (signal::CLD_EXITED, status >> 8),
            signal => (signal::CLD_KILLED, signal),
        };
        process.notify_parent(code, how);
        // A parent that said it won't wait for its children doesn't keep
        // them once they've ended.
        if let Some(parent) = process.parent.upgrade()
            && parent.signals.reaps_children()
        {
            let me = interrupts::without_interrupts(|| {
                let mut children = parent.children.lock();
                let at = children.iter().position(|child| child.id == process.id);
                at.map(|at| children.remove(at))
            });
            drop(me);
        }
        hang_up(&process);
    }
    thread::exit()
}

/// As a session's leader ends: its terminal is no one's, and whoever's in
/// its foreground is told so, with SIGHUP, and continued to hear it.
fn hang_up(process: &Process) {
    if process.sid() != process.id {
        return;
    }
    let Some(tty) = process.tty.lock().clone() else { return };
    if tty.session() != process.id {
        return;
    }
    let foreground = tty.foreground();
    tty.control(0, 0);
    if foreground != 0 {
        signal_group(foreground, signal::SIGHUP, Info::kernel());
        signal_group(foreground, signal::SIGCONT, Info::kernel());
    }
}

/// What the scheduler swaps in and out for a thread that runs a program: the
/// parts of the CPU's state that are the program's and that a switch would
/// otherwise leave for the next thread to see or to clobber.
pub struct UserState {
    pub process: Arc<Process>,
    /// What the thread first enters ring 3 with.
    pub start: Registers,
    /// The program's page table.
    pml4: PhysFrame,
    /// The top of the thread's kernel stack, where the CPU switches to from
    /// ring 3. Set by `thread::spawn_user`.
    pub kernel_stack_top: VirtAddr,
    /// Where `fs` points: musl keeps its thread-local storage there.
    fs_base: u64,
    fpu: FpuState,
}

impl UserState {
    /// The thread is about to run: give the CPU its page table, its `fs`, its
    /// stack for entering the kernel and its SSE registers.
    pub fn switch_in(&self) {
        if Cr3::read().0 != self.pml4 {
            // Safety: the program's page table, whose upper half is the kernel's.
            unsafe { Cr3::write(self.pml4, Cr3Flags::empty()) };
        }
        FsBase::write(VirtAddr::new(self.fs_base));
        gdt::set_kernel_stack(self.kernel_stack_top);
        crate::syscall::set_kernel_stack(self.kernel_stack_top);
        // Safety: a 512-byte, 16-byte aligned area filled by `fxsave` or `new`.
        unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) &self.fpu, options(nostack)) };
    }

    /// The thread is about to stop running: keep its SSE registers. The kernel
    /// itself never uses them, so they still hold the program's.
    pub fn switch_out(&mut self) {
        // Safety: as in `switch_in`.
        unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) &mut self.fpu, options(nostack)) };
    }

    pub fn fs_base(&self) -> u64 {
        self.fs_base
    }

    /// Set `fs`, for `arch_prctl`. Only for the running thread.
    pub fn set_fs_base(&mut self, base: u64) {
        self.fs_base = base;
        FsBase::write(VirtAddr::new(base));
    }

    /// The state for a copy of this thread, which must be the running one,
    /// running `process` on page table `pml4` from `start`: its `fs` and SSE
    /// registers as they are now.
    fn fork(&self, process: Arc<Process>, pml4: PhysFrame, start: Registers) -> UserState {
        let mut fpu = FpuState::new();
        // Safety: as in `switch_out`; the CPU holds this thread's registers.
        unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) &mut fpu, options(nostack)) };
        let kernel_stack_top = VirtAddr::zero();
        UserState { process, start, pml4, kernel_stack_top, fs_base: self.fs_base, fpu }
    }

    /// Start afresh on page table `pml4`, as `exec` does, with `fs` and the
    /// SSE registers as a new program's. Only for the running thread.
    fn start_over(&mut self, pml4: PhysFrame) {
        self.pml4 = pml4;
        // Safety: the program's new page table, whose upper half is the kernel's.
        unsafe { Cr3::write(pml4, Cr3Flags::empty()) };
        self.set_fs_base(0);
        self.fpu = FpuState::new();
        // Safety: as in `switch_in`.
        unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) &self.fpu, options(nostack)) };
    }
}

/// What `fxsave` stores: the x87 and SSE registers.
#[repr(C, align(16))]
struct FpuState([u8; 512]);

/// The CPU's x87 and SSE registers, which while a program's thread runs are
/// the program's: for a signal handler's frame.
pub fn save_fpu() -> [u8; 512] {
    let mut area = FpuState([0; 512]);
    // Safety: a 512-byte, 16-byte aligned area.
    unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) &mut area, options(nostack)) };
    area.0
}

/// Load `area`, from a signal frame, into the CPU's x87 and SSE registers,
/// for the running thread's program; MXCSR's reserved bits cleared first, as
/// loading them would fault.
pub fn load_fpu(area: &[u8; 512]) {
    let mask = match u32::from_le_bytes(save_fpu()[28..32].try_into().unwrap()) {
        0 => 0xFFBF,
        mask => mask,
    };
    let mut state = FpuState(*area);
    let mxcsr = u32::from_le_bytes(state.0[24..28].try_into().unwrap()) & mask;
    state.0[24..28].copy_from_slice(&mxcsr.to_le_bytes());
    // Safety: as in `save_fpu`, and every bit it loads is allowed.
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) &state, options(nostack)) };
}

/// The CPU's x87 and SSE registers as a new program's, for the running
/// thread's program.
pub fn reset_fpu() {
    let state = FpuState::new();
    // Safety: as in `save_fpu`.
    unsafe { core::arch::asm!("fxrstor64 [{}]", in(reg) &state, options(nostack)) };
}

impl FpuState {
    /// As after `fninit`: x87 control word 0x37F and MXCSR 0x1F80, every
    /// exception masked, round to nearest.
    fn new() -> FpuState {
        let mut area = [0; 512];
        area[0..2].copy_from_slice(&0x037Fu16.to_le_bytes());
        area[24..28].copy_from_slice(&0x1F80u32.to_le_bytes());
        FpuState(area)
    }
}

/// Make the CPU ready for user programs: SSE, which every x86-64 program
/// uses, and the `syscall` instruction. `pre_init` calls it.
pub fn init() {
    // Safety: SSE on, its exceptions reported as #XM, and x87 instructions
    // executed rather than trapped.
    unsafe {
        Cr0::update(|flags| {
            flags.remove(Cr0Flags::EMULATE_COPROCESSOR);
            flags.insert(Cr0Flags::MONITOR_COPROCESSOR);
        });
        Cr4::update(|flags| flags.insert(Cr4Flags::OSFXSR | Cr4Flags::OSXMMEXCPT_ENABLE));
    }
    crate::syscall::init();
}

/// A program's registers: what it enters ring 3 with.
#[derive(Clone, Copy, Default)]
pub struct Registers {
    pub rax: u64,
    pub rbx: u64,
    pub rcx: u64,
    pub rdx: u64,
    pub rsi: u64,
    pub rdi: u64,
    pub rbp: u64,
    pub r8: u64,
    pub r9: u64,
    pub r10: u64,
    pub r11: u64,
    pub r12: u64,
    pub r13: u64,
    pub r14: u64,
    pub r15: u64,
    pub rip: u64,
    /// Only the arithmetic flags, direction and single-stepping are taken;
    /// interrupts are always on.
    pub rflags: u64,
    pub rsp: u64,
}

/// Drop to ring 3 with `registers`, never to return: the program's thread
/// comes back to the kernel only through system calls and interrupts, each
/// on a fresh stack, and leaves through `exit_current` or `kill_current`. A
/// `rip` outside the program's half, which a signal handler's address or
/// frame can be, ends it with SIGSEGV, as returning there would fault in the
/// kernel.
pub fn enter_user(registers: Registers) -> ! {
    if registers.rip >= USER_END {
        kill_current(signal::SIGSEGV);
    }
    let selectors = gdt::selectors();
    // Carry, parity, adjust, zero, sign, trap, direction and overflow.
    const USER_FLAGS: u64 = 0xDD5;
    // Interrupts on in ring 3, and bit 1, which is always set.
    const ALWAYS: u64 = 0x202;
    let Registers {
        rax, rbx, rcx, rdx, rsi, rdi, rbp, r8, r9, r10, r11, r12, r13, r14, r15, ..
    } = registers;
    // Popped into the registers in this order, then what `iretq` takes.
    let frame = [
        rax,
        rbx,
        rcx,
        rdx,
        rsi,
        rdi,
        rbp,
        r8,
        r9,
        r10,
        r11,
        r12,
        r13,
        r14,
        r15,
        registers.rip,
        u64::from(selectors.user_code_selector.0),
        registers.rflags & USER_FLAGS | ALWAYS,
        registers.rsp,
        u64::from(selectors.user_data_selector.0),
    ];
    // Safety: the thread's address space is active (`switch_in`), with the
    // program loaded. Every register is the program's by the `iretq`, so it
    // sees nothing of the kernel's. Interrupts stay off until then, as the
    // stack is the frame.
    unsafe {
        core::arch::asm!(
            "cli",
            "mov rsp, {}",
            "pop rax",
            "pop rbx",
            "pop rcx",
            "pop rdx",
            "pop rsi",
            "pop rdi",
            "pop rbp",
            "pop r8",
            "pop r9",
            "pop r10",
            "pop r11",
            "pop r12",
            "pop r13",
            "pop r14",
            "pop r15",
            "iretq",
            in(reg) &frame,
            options(noreturn),
        )
    }
}

/// Where the loader put things, for the stack and the heap.
struct Image {
    entry: u64,
    /// The program headers, in the program's memory, and how many.
    phdr: u64,
    phnum: u64,
    /// The first page past the highest segment, where the heap starts.
    end: u64,
}

fn load(space: &mut AddressSpace, file: &[u8]) -> Result<Image, &'static str> {
    let elf = Elf::parse(file)?;
    let base = if elf.pie { PIE_BASE } else { 0 };
    let outside = "a segment outside the program's area";
    let mut end = 0;
    let mut phdr = None;
    for segment in elf.segments() {
        match segment.kind {
            elf::PT_LOAD => {
                let start = base.checked_add(segment.vaddr).ok_or(outside)?;
                let stop = start.checked_add(segment.memsz).ok_or(outside)?;
                if start < LOWEST || stop > MMAP_START {
                    return Err(outside);
                }
                let mut flags = PageTableFlags::empty();
                if segment.flags & elf::PF_W != 0 {
                    flags |= PageTableFlags::WRITABLE;
                }
                if segment.flags & elf::PF_X == 0 {
                    flags |= PageTableFlags::NO_EXECUTE;
                }
                space.map(pages(start, stop), flags).ok_or(NO_MEMORY)?;
                space.load(start, elf.contents(&segment)).ok_or(outside)?;
                end = end.max(stop);
                // Without a PT_PHDR, the headers are in the segment that
                // starts at the beginning of the file, if any is.
                let headers = elf.phoff + u64::from(elf.phnum) * elf::PHENT_SIZE;
                if segment.offset == 0 && headers <= segment.filesz {
                    phdr = phdr.or(Some(start + elf.phoff));
                }
            }
            elf::PT_PHDR => phdr = Some(base.checked_add(segment.vaddr).ok_or(outside)?),
            _ => {}
        }
    }
    let entry = base.checked_add(elf.entry).filter(|&entry| entry >= LOWEST && entry < end);
    Ok(Image {
        entry: entry.ok_or("an entry point outside the program")?,
        phdr: phdr.ok_or("no program headers in the program's memory")?,
        phnum: u64::from(elf.phnum),
        end: end.next_multiple_of(4096),
    })
}

/// Map the stack and lay it out as Linux does for a new program: from the
/// stack pointer up, `argc`, the `argv` pointers, the `envp` pointers, and
/// the auxiliary vector, which tells musl where its program headers (and so
/// its thread-local storage) are; the strings above them.
fn build_stack(
    space: &mut AddressSpace,
    args: &[&[u8]],
    env: &[&[u8]],
    image: &Image,
) -> Result<u64, &'static str> {
    const AT_NULL: u64 = 0;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_ENTRY: u64 = 9;
    const AT_RANDOM: u64 = 25;

    let bottom = STACK_TOP - STACK_SIZE;
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    space.map(pages(bottom, STACK_TOP), flags).ok_or(NO_MEMORY)?;

    let mut top = STACK_TOP;
    let mut push = |space: &mut AddressSpace, bytes: &[u8]| {
        top = top.checked_sub(bytes.len() as u64).filter(|&top| top > bottom + 4096)?;
        space.load(top, bytes).map(|()| top)
    };
    let random = push(space, &random_bytes()).ok_or(TOO_LONG)?;
    let mut words = vec![args.len() as u64];
    for strings in [args, env] {
        for string in strings {
            let mut terminated = vec![0; string.len() + 1];
            terminated[..string.len()].copy_from_slice(string);
            words.push(push(space, &terminated).ok_or(TOO_LONG)?);
        }
        words.push(0);
    }
    words.extend([
        AT_PHDR,
        image.phdr,
        AT_PHENT,
        elf::PHENT_SIZE,
        AT_PHNUM,
        image.phnum,
        AT_PAGESZ,
        4096,
        AT_ENTRY,
        image.entry,
        AT_RANDOM,
        random,
        AT_NULL,
        0,
    ]);
    let bytes: Vec<u8> = words.iter().flat_map(|word| word.to_le_bytes()).collect();
    // `argc` on a 16-byte boundary, as the ABI requires at entry.
    let rsp = (top - bytes.len() as u64) & !0xF;
    if rsp <= bottom + 4096 {
        return Err(TOO_LONG);
    }
    space.load(rsp, &bytes).ok_or(TOO_LONG)?;
    Ok(rsp)
}

/// The 16 bytes behind `AT_RANDOM`, which seed the program's stack canaries
/// and hash maps.
///
/// shortcut: RDRAND when the CPU has it, else the timestamp counter, which is
/// guessable; take entropy from somewhere better once there is somewhere.
fn random_bytes() -> [u8; 16] {
    let rdrand = x86_64::instructions::random::RdRand::new();
    let word = || {
        rdrand
            .and_then(|r| r.get_u64())
            // Safety: rdtsc reads a counter, nothing more.
            .unwrap_or_else(|| unsafe { core::arch::x86_64::_rdtsc() }.rotate_left(17))
    };
    let mut bytes = [0; 16];
    bytes[..8].copy_from_slice(&word().to_le_bytes());
    bytes[8..].copy_from_slice(&word().to_le_bytes());
    bytes
}

/// The pages covering `start..end`.
pub fn pages(start: u64, end: u64) -> PageRange {
    let first = Page::containing_address(VirtAddr::new(start));
    let past = Page::containing_address(VirtAddr::new(end.next_multiple_of(4096)));
    Page::range(first, past)
}
