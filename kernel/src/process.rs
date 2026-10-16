//! User programs: Linux x86-64 executables, run in ring 3 in an address space
//! of their own. Each runs on threads of its own, one to start with and more
//! as it makes them (`spawn_thread`), which share all of it but their
//! registers, `fs` and signal mask; whose system calls `syscall.rs` handles;
//! and which a fault ends, with the rest of their program, without touching
//! anything else. A program ends as its last thread does, or all at once with
//! `exit_group` or a fatal signal. It can copy itself (`fork`), replace
//! itself with another (`exec`), and wait for the copies it made to end. Programs are in process groups, which are
//! in sessions, as on Linux: a session's leader has a terminal, and one of
//! its groups is in the terminal's foreground, which ^C signals.
//!
//! The lower half of a program's address space:
//!
//!   0x0040_0000          the program (a PIE is loaded here), then its heap (`brk`)
//!   0x1000_0000_0000     `mmap`, growing up
//!   0x7FFF_FBFF_0000     where the stack can grow down to, as it's used
//!   0x7FFF_FFFF_0000     the top of the stack, which grows down. The 64 KiB above
//!                        it are never mapped, so no `syscall` can sit at the very
//!                        end of the lower half (see `syscall.rs`).

use crate::elf::{self, Elf, Source};
use crate::fs;
use crate::futex;
use crate::io::interrupts::gdt;
use crate::memory::{AddressSpace, USER_END};
use crate::signal::{self, AltStack, Info, Signals, ThreadSignals};
use crate::syscall;
use crate::thread::{self, WaitQueue};
use crate::time;
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
/// The stack mapped as a program starts, which its arguments go on.
const STACK_SIZE: u64 = 256 * 1024;
/// The most the stack grows to, mapped a page at a time as the program
/// reaches it: as GCC's does, compiling.
pub const STACK_MOST: u64 = 64 << 20;
/// Where `mmap` stops: a gap below where the stack can grow to.
pub const MMAP_END: u64 = STACK_TOP - STACK_MOST - 0x10_0000;

/// What `status` holds until the program ends, and `ending` until it starts
/// to; and what `ending` holds while one of its threads execs.
const RUNNING: i32 = i32::MIN;
const EXECING: i32 = i32::MIN + 1;

/// The exit status of a program a segmentation fault ended, as a shell
/// reports it: 128 plus the signal.
pub const KILLED_SIGSEGV: i32 = 128 + signal::SIGSEGV as i32;

/// Why a program couldn't be loaded, for the system call's errno.
pub const NO_MEMORY: &str = "out of memory";
pub const TOO_LONG: &str = "arguments too long for the stack";

const ENOMEM: i64 = 12;

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
    /// How nice it is to others, from -20 to 19, as `setpriority` set it.
    ///
    /// shortcut: kept and reported, but every thread gets the same turns;
    /// weigh them by this when the scheduler has priorities.
    pub nice: AtomicI32,
    /// Its resource limits, soft and hard, by `RLIMIT_*` number. Taken with
    /// interrupts on.
    pub limits: Mutex<[(u64, u64); syscall::LIMITS]>,
    /// The CPU time it has had, and its children it waited for have.
    pub times: Times,
    /// Its alarm, which the alarm thread sends it when due. Taken with
    /// interrupts on, as are its timers.
    pub alarm: Mutex<time::Alarm>,
    /// Its timers, by id, as `timer_create` makes them; `None` for one
    /// deleted, whose id is free again.
    pub timers: Mutex<Vec<Option<time::Timer>>>,
    /// The program it's a copy of; none for one the kernel started.
    parent: Weak<Process>,
    /// What it runs, as /proc tells it. Taken with interrupts on.
    pub command: Mutex<Command>,
    /// When it started, in nanoseconds since boot.
    pub started: u64,
    /// The copies it made that it hasn't waited for, ended or not. Taken only
    /// with interrupts off, as `wait4` looks at them from
    /// `WaitQueue::wait_until`.
    ///
    /// shortcut: one that ends keeps its memory until it's waited for, and
    /// those never waited for go with their parent rather than to an init.
    pub children: Mutex<Vec<Arc<Process>>>,
    /// Woken as each of them ends.
    pub child_exited: WaitQueue,
    /// Its threads that haven't ended, the one it started with first while
    /// it's there. Taken only with interrupts off, as `exec` looks at them
    /// from `WaitQueue::wait_until`.
    threads: Mutex<Vec<Arc<Task>>>,
    /// Woken as each of them ends.
    thread_exited: WaitQueue,
    /// `RUNNING`; or how it ends, once a thread of it has ended it all
    /// (`exit_group`, a fatal signal) and the rest are ending too; or
    /// `EXECING`, while the rest end for a thread that execs.
    ending: AtomicI32,
}

/// One of a program's threads, as the program's others and the kernel see it.
/// What else it has, its registers and `fs`, is the scheduler's (`UserState`).
pub struct Task {
    /// Its thread id, from the same numbers as programs': its program's own
    /// for the thread it started with, and for one that execs, which takes
    /// the program over.
    id: AtomicU64,
    pub signals: ThreadSignals,
    /// Where it ends, 0 is written, and a futex there woken, as
    /// CLONE_CHILD_CLEARTID and `set_tid_address` ask: how `pthread_join`
    /// hears of it. 0 for nowhere.
    pub clear_tid: AtomicU64,
}

impl Task {
    fn new(id: u64, signals: ThreadSignals) -> Arc<Task> {
        Arc::new(Task { id: AtomicU64::new(id), signals, clear_tid: AtomicU64::new(0) })
    }

    pub fn id(&self) -> u64 {
        self.id.load(Ordering::Relaxed)
    }
}

/// What a process runs: its program's name, as Linux's `comm` is, the last
/// part of the path it was run by, at most 15 bytes; and its arguments,
/// each ending in a NUL, as /proc/PID/cmdline has them.
#[derive(Clone, Default)]
pub struct Command {
    pub name: String,
    pub args: Vec<u8>,
}

impl Command {
    pub fn new(path: &[u8], args: &[&[u8]]) -> Command {
        let name = path.rsplit(|&byte| byte == b'/').next().unwrap_or_default();
        let name = String::from_utf8_lossy(&name[..name.len().min(15)]).into_owned();
        Command {
            name,
            args: args.iter().flat_map(|arg| arg.iter().chain(&[0])).copied().collect(),
        }
    }
}

/// The CPU's time, in timer ticks, as /proc/stat counts it: running
/// programs, in the kernel for them, and neither.
pub struct CpuTime {
    pub user: AtomicU64,
    pub system: AtomicU64,
    pub idle: AtomicU64,
}

pub static CPU_TIME: CpuTime =
    CpuTime { user: AtomicU64::new(0), system: AtomicU64::new(0), idle: AtomicU64::new(0) };

/// CPU time, in timer ticks: a program's own, running and in the kernel for
/// it; and that of the children it has waited for, theirs and their own
/// children's.
#[derive(Default)]
pub struct Times {
    pub user: AtomicU64,
    pub system: AtomicU64,
    pub children_user: AtomicU64,
    pub children_system: AtomicU64,
}

impl Times {
    /// Count a child's, as its parent waits for it.
    pub fn add_child(&self, child: &Times) {
        let all = |own: &AtomicU64, children: &AtomicU64| {
            own.load(Ordering::Relaxed) + children.load(Ordering::Relaxed)
        };
        self.children_user.fetch_add(all(&child.user, &child.children_user), Ordering::Relaxed);
        self.children_system
            .fetch_add(all(&child.system, &child.children_system), Ordering::Relaxed);
    }
}

/// From the timer interrupt: the tick that's ending is the running program's,
/// its own (`user`) or the kernel's for it. Nothing for the kernel's threads.
pub fn charge_tick(user: bool) {
    let charged = thread::with_current_user(|current| {
        let times = &current.process.times;
        let count = if user { &times.user } else { &times.system };
        count.fetch_add(1, Ordering::Relaxed);
    });
    // The machine's: with no program's thread running, it was idle.
    let count = match (charged, user) {
        (None, _) => &CPU_TIME.idle,
        (Some(()), true) => &CPU_TIME.user,
        (Some(()), false) => &CPU_TIME.system,
    };
    count.fetch_add(1, Ordering::Relaxed);
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

    /// Its threads that haven't ended.
    pub fn threads(&self) -> Vec<Arc<Task>> {
        interrupts::without_interrupts(|| self.threads.lock().clone())
    }

    /// Start ending it all, with `status`, as `exit_group` and fatal signals
    /// do: its threads but `me`, the current one, are sent SIGKILL, to end
    /// as they next can. Unless it's ending already, or a thread execs: then
    /// that status stands, or the program goes on.
    fn end_all(&self, me: &Task, status: i32) {
        let ended = interrupts::without_interrupts(|| {
            let threads = self.threads.lock();
            let ending =
                self.ending.compare_exchange(RUNNING, status, Ordering::AcqRel, Ordering::Acquire);
            ending.is_ok().then(|| threads.clone())
        });
        self.kill_others(me, ended.unwrap_or_default());
    }

    /// For `exec`: end its threads but `me`, the current one, and wait until
    /// they have; `me` then has the program's id, as the thread it started
    /// with did. False if it's ending already, or another thread execs, and
    /// `me` is to end too.
    fn alone(&self, me: &Task) -> bool {
        let others = interrupts::without_interrupts(|| {
            let threads = self.threads.lock();
            let execing =
                self.ending.compare_exchange(RUNNING, EXECING, Ordering::AcqRel, Ordering::Acquire);
            execing.is_ok().then(|| threads.clone())
        });
        let Some(others) = others else { return false };
        self.kill_others(me, others);
        self.thread_exited.wait_until(|| self.threads.lock().len() == 1);
        me.id.store(self.id, Ordering::Relaxed);
        me.clear_tid.store(0, Ordering::Relaxed);
        self.ending.store(RUNNING, Ordering::Release);
        true
    }

    /// Send SIGKILL to each of `threads` but `me`.
    fn kill_others(&self, me: &Task, threads: Vec<Arc<Task>>) {
        for task in threads.iter().filter(|task| !core::ptr::eq(&***task, me)) {
            signal::send_thread(self, task, signal::SIGKILL, Info::kernel());
        }
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

/// The thread numbered `tid`, if there is one, and the process it's a thread
/// of.
pub fn find_thread(tid: u64) -> Option<(Arc<Process>, Arc<Task>)> {
    // Most often the thread a process started with, numbered as it is.
    let first = find(tid).into_iter();
    first.chain(all()).find_map(|process| {
        let task = process.threads().into_iter().find(|task| task.id() == tid)?;
        Some((process, task))
    })
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
    elf: &(impl Source + ?Sized),
    args: &[&str],
    output: impl FnMut(&[u8]) + Send + 'static,
) -> Result<Arc<Process>, i64> {
    let tty = Arc::new(fs::Handle::tty(Arc::new(Tty::capture(Box::new(output)))));
    spawn_with(elf, args, &[], vec![Some(tty); 3])
}

/// `spawn`, with `env` as the program's environment, and `files` as its
/// descriptors to start with, by number: what its standard input, output
/// and error are, for one. In the root, where an interpreter it names by a
/// relative path is looked for.
pub fn spawn_with(
    elf: &(impl Source + ?Sized),
    args: &[&str],
    env: &[&str],
    files: Vec<Option<Arc<fs::Handle>>>,
) -> Result<Arc<Process>, i64> {
    let args: Vec<&[u8]> = args.iter().map(|arg| arg.as_bytes()).collect();
    let env: Vec<&[u8]> = env.iter().map(|variable| variable.as_bytes()).collect();
    let (memory, start) = program(elf, &args, &env, "")?;
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
    let task = Task::new(id, ThreadSignals::new(0, AltStack::NONE));
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
        nice: AtomicI32::new(0),
        limits: Mutex::new(syscall::DEFAULT_LIMITS),
        times: Times::default(),
        alarm: Mutex::new(time::Alarm::default()),
        timers: Mutex::new(Vec::new()),
        parent: Weak::new(),
        children: Mutex::new(Vec::new()),
        child_exited: WaitQueue::new(),
        command: Mutex::new(Command::new(args.first().copied().unwrap_or_default(), &args)),
        started: time::since_boot(),
        threads: Mutex::new(vec![task.clone()]),
        thread_exited: WaitQueue::new(),
        ending: AtomicI32::new(RUNNING),
    });
    register(&process);
    let user = UserState {
        process: process.clone(),
        task,
        start,
        pml4,
        kernel_stack_top: VirtAddr::zero(),
        fs_base: 0,
        fpu: FpuState::new(),
    };
    thread::spawn_user(Box::new(user)).ok_or(ENOMEM)?;
    Ok(process)
}

/// `spawn_with` for the program at `path`, a path from the root, read from
/// the disk: an ELF program, or a script, run as `execve` runs them (see
/// `program_for`). The errno saying why not if it can't be.
pub fn spawn_program(
    path: &str,
    args: &[&str],
    env: &[&str],
    files: Vec<Option<Arc<fs::Handle>>>,
) -> Result<Arc<Process>, i64> {
    let mut args: Vec<Vec<u8>> = args.iter().map(|arg| arg.as_bytes().to_vec()).collect();
    let elf = program_for("", path, &mut args)?;
    let command =
        Command::new(path.as_bytes(), &args.iter().map(Vec::as_slice).collect::<Vec<_>>());
    let args: Vec<String> = args.iter().map(|arg| String::from_utf8_lossy(arg).into()).collect();
    let args: Vec<&str> = args.iter().map(String::as_str).collect();
    let process = spawn_with(&elf, &args, env, files)?;
    *process.command.lock() = command;
    Ok(process)
}

/// The ELF program that runs for `path`, from the directory `cwd` (a path
/// from the root, as `fs::resolve` makes them), with `args`: the file's
/// own; or for a script, what its first line names, given the line's
/// argument if it has one and the script's path, in place of the first of
/// `args`, as Linux runs them. That can be a script too, up to where Linux
/// gives up (ELOOP).
pub fn program_for(cwd: &str, path: &str, args: &mut Vec<Vec<u8>>) -> Result<fs::Program, i64> {
    const ENOENT: i64 = 2;
    const EIO: i64 = 5;
    const ELOOP: i64 = 40;
    if path.is_empty() {
        return Err(ENOENT);
    }
    let mut path = String::from(path);
    let mut elf = fs::open_program(&fs::lookup(cwd, &path, true)?)?;
    for depth in 0.. {
        // As much of its first line as `interpreter` looks at.
        let mut head = vec![0; elf.size().min(LONGEST_LINE as u64) as usize];
        elf.read_at(0, &mut head).ok_or(EIO)?;
        let Some((interpreter, argument)) = interpreter(&head) else { break };
        if depth == 5 {
            return Err(ELOOP);
        }
        let interpreter = String::from_utf8(interpreter.to_vec()).map_err(|_| ENOENT)?;
        let mut first = Vec::from([interpreter.clone().into_bytes()]);
        first.extend(argument.map(<[u8]>::to_vec));
        first.push(path.into_bytes());
        args.splice(..args.len().min(1), first);
        elf = fs::open_program(&fs::lookup(cwd, &interpreter, true)?)?;
        path = interpreter;
    }
    Ok(elf)
}

/// How much of a script's first line Linux reads.
const LONGEST_LINE: usize = 256;

/// What runs the script `program`, as Linux reads its first line: `#!`, then
/// the interpreter's path, then what's left of the line as one argument if
/// anything is, past spaces and tabs. Within the first 256 bytes, where a
/// line that goes on past them must at least have the path whole: all of
/// `program` that's needed. `None` if it isn't a script, or names nothing.
fn interpreter(program: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    const LONGEST: usize = LONGEST_LINE;
    let blank = |byte: &u8| matches!(byte, b' ' | b'\t');
    let head = program.strip_prefix(b"#!")?;
    let head = &head[..head.len().min(LONGEST - 2)];
    let line = match head.iter().position(|&byte| byte == b'\n') {
        Some(end) => &head[..end],
        None if program.len() < LONGEST => head,
        None => {
            let line = &head[..head.len() - 1];
            let path = line.iter().position(|byte| !blank(byte))?;
            line[path..].iter().position(|byte| blank(byte) || *byte == 0)?;
            line
        }
    };
    let line = line.split(|&byte| byte == 0).next()?;
    let start = line.iter().position(|byte| !blank(byte))?;
    let end = line.iter().rposition(|byte| !blank(byte))? + 1;
    let line = &line[start..end];
    let (path, rest) = line.split_at(line.iter().position(blank).unwrap_or(line.len()));
    let argument = rest.iter().position(|byte| !blank(byte)).map(|at| &rest[at..]);
    Some((path, argument))
}

/// The errno for why a program couldn't be loaded.
fn load_errno(error: &str) -> i64 {
    const E2BIG: i64 = 7;
    const ENOEXEC: i64 = 8;
    const EIO: i64 = 5;
    match error {
        NO_MEMORY => ENOMEM,
        TOO_LONG => E2BIG,
        elf::UNREADABLE => EIO,
        _ => ENOEXEC,
    }
}

/// `elf` loaded into a new address space, with `args` and `env` on its
/// stack, and the registers to start it with; or the errno saying why it
/// couldn't be. A program that names an interpreter (PT_INTERP), a dynamic
/// loader such as musl's, starts in it, as Linux starts it: loaded too,
/// where `mmap` starts handing memory out, and told where both are
/// (AT_BASE, AT_PHDR, AT_ENTRY), to load what else the program needs and
/// start it. Its path is looked for from `cwd` if it's relative.
fn program(
    elf: &(impl Source + ?Sized),
    args: &[&[u8]],
    env: &[&[u8]],
    cwd: &str,
) -> Result<(Memory, Registers), i64> {
    let mut space = AddressSpace::new().ok_or(ENOMEM)?;
    space.grow_stack(STACK_TOP - STACK_MOST..STACK_TOP);
    let parsed = Elf::parse(elf).map_err(load_errno)?;
    let image = load(&mut space, &parsed, elf, PIE_BASE, MMAP_START).map_err(load_errno)?;
    let interpreter = match parsed.interpreter(elf).map_err(load_errno)? {
        Some(path) => Some(load_interpreter(&mut space, cwd, &path)?),
        None => None,
    };
    let base = interpreter.as_ref().map_or(0, |interpreter| interpreter.base);
    let rsp = build_stack(&mut space, args, env, &image, base).map_err(load_errno)?;
    let mmap_next = interpreter.as_ref().map_or(MMAP_START, |interpreter| interpreter.end);
    let entry = interpreter.map_or(image.entry, |interpreter| interpreter.entry);
    let memory = Memory { space, brk_start: image.end, brk: image.end, mmap_next };
    Ok((memory, Registers { rip: entry, rsp, ..Registers::default() }))
}

/// The interpreter at `path`, from `cwd`, loaded into `space` where `mmap`
/// starts handing memory out: a program, as `execve` would take it, and an
/// ELF one for x86-64, else ELIBBAD, as Linux has it; whose own PT_INTERP,
/// if it has one, is passed over, as Linux passes it over.
///
/// shortcut: it must be position-independent, as every loader is, rather
/// than at addresses of its own, which could be the program's.
fn load_interpreter(space: &mut AddressSpace, cwd: &str, path: &[u8]) -> Result<Image, i64> {
    const ENOENT: i64 = 2;
    const EIO: i64 = 5;
    const ELIBBAD: i64 = 80;
    let path = core::str::from_utf8(path).map_err(|_| ENOENT)?;
    if path.is_empty() {
        return Err(ENOENT);
    }
    let file = fs::open_program(&fs::lookup(cwd, path, true)?)?;
    // Linux reads its ELF header whole first: a shorter file is EIO.
    if file.size() < 64 {
        return Err(EIO);
    }
    let elf = match Elf::parse(&file) {
        Ok(elf) if elf.pie => elf,
        Err(elf::UNREADABLE) => return Err(EIO),
        _ => return Err(ELIBBAD),
    };
    load(space, &elf, &file, MMAP_START, MMAP_END).map_err(load_errno)
}

/// Where `clone` is to write a new thread's id, those that aren't 0: in the
/// parent's memory (CLONE_PARENT_SETTID); in the child's (CLONE_CHILD_SETTID);
/// and 0, as the thread ends, in the child's (CLONE_CHILD_CLEARTID).
#[derive(Clone, Copy, Default)]
pub struct ThreadIds {
    pub parent: u64,
    pub child: u64,
    pub clear: u64,
}

/// Write `id` at `at` in `memory`, if `at` isn't 0, as `clone` writes a new
/// thread's: or not, if it can't be, as Linux doesn't say.
fn write_id(memory: &mut Memory, at: u64, id: u64) {
    if at != 0 {
        let _ = memory.space.write(at, &(id as u32).to_le_bytes());
    }
}

/// A copy of `parent`, the current thread's program, as `fork` makes: its
/// memory, descriptors, working directory and umask, started on a thread of
/// its own with `registers`, and `fs` at `tls` if that's given, its id
/// written where `ids` say. Its id, or `None` when memory runs out.
pub fn fork(
    parent: &Arc<Process>,
    registers: Registers,
    tls: Option<u64>,
    ids: ThreadIds,
) -> Option<u64> {
    let mut memory = {
        let memory = parent.memory.lock();
        Memory { space: memory.space.duplicate()?, ..*memory }
    };
    let pml4 = memory.space.pml4();
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    write_id(&mut memory, ids.child, id);
    write_id(&mut parent.memory.lock(), ids.parent, id);
    // Blocking what the thread that made it blocks, its alternate stack too.
    let task = thread::with_current_user(|user| Task::new(id, user.task.signals.fork()))?;
    task.clear_tid.store(ids.clear, Ordering::Relaxed);
    let child = Arc::new(Process {
        id,
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
        nice: AtomicI32::new(parent.nice.load(Ordering::Relaxed)),
        limits: Mutex::new(*parent.limits.lock()),
        times: Times::default(),
        alarm: Mutex::new(time::Alarm::default()),
        timers: Mutex::new(Vec::new()),
        parent: Arc::downgrade(parent),
        children: Mutex::new(Vec::new()),
        child_exited: WaitQueue::new(),
        command: Mutex::new(parent.command.lock().clone()),
        started: time::since_boot(),
        threads: Mutex::new(vec![task.clone()]),
        thread_exited: WaitQueue::new(),
        ending: AtomicI32::new(RUNNING),
    });
    register(&child);
    let mut user =
        thread::with_current_user(|user| user.fork(child.clone(), task, pml4, registers))?;
    user.fs_base = tls.unwrap_or(user.fs_base);
    thread::spawn_user(Box::new(user))?;
    // Even if it has already ended, it's there to be waited for: the parent
    // can't wait until this returns.
    interrupts::without_interrupts(|| parent.children.lock().push(child.clone()));
    Some(child.id)
}

/// A new thread of `process`, the current thread's program, as `clone` makes
/// one with CLONE_THREAD: sharing all of it, and started with `registers`,
/// its `fs` at `tls` if that's given, else where the current thread's is,
/// and its id written where `ids` say before it runs. Blocking what the
/// current thread blocks, but with no alternate stack, as that one's is the
/// current thread's. Its id; or the errno saying why not: EAGAIN while the
/// program is ending, ENOMEM when memory runs out.
pub fn spawn_thread(
    process: &Arc<Process>,
    registers: Registers,
    tls: Option<u64>,
    ids: ThreadIds,
) -> Result<u64, i64> {
    const EAGAIN: i64 = 11;
    const ENOMEM: i64 = 12;
    let id = NEXT_ID.fetch_add(1, Ordering::Relaxed);
    let user = thread::with_current_user(|user| {
        let task = Task::new(id, user.task.signals.sibling());
        task.clear_tid.store(ids.clear, Ordering::Relaxed);
        let mut new = user.fork(process.clone(), task, user.pml4, registers);
        new.fs_base = tls.unwrap_or(new.fs_base);
        new
    })
    .ok_or(ENOMEM)?;
    {
        let mut memory = process.memory.lock();
        write_id(&mut memory, ids.parent, id);
        write_id(&mut memory, ids.child, id);
    }
    // One of its threads, unless it's ending, which then has none to end.
    let task = user.task.clone();
    let joined = interrupts::without_interrupts(|| {
        let mut threads = process.threads.lock();
        let running = process.ending.load(Ordering::Acquire) == RUNNING;
        if running {
            threads.push(task.clone());
        }
        running
    });
    if !joined {
        return Err(EAGAIN);
    }
    if thread::spawn_user(Box::new(user)).is_none() {
        let leave =
            |threads: &mut Vec<Arc<Task>>| threads.retain(|other| !Arc::ptr_eq(other, &task));
        interrupts::without_interrupts(|| leave(&mut process.threads.lock()));
        return Err(ENOMEM);
    }
    Ok(id)
}

/// Replace `process`, the current thread's program, with `elf`, given `args`
/// and `env`: its memory, `fs` and SSE registers start afresh, and its other
/// threads end, the current one taking the program's id, as on Linux. The
/// registers to enter it with; or the errno saying why it couldn't be
/// loaded, and nothing has changed; or ENOEXEC if the program is ending,
/// which the current thread is to do too.
pub fn exec(
    process: &Process,
    elf: &(impl Source + ?Sized),
    args: &[&[u8]],
    env: &[&[u8]],
) -> Result<Registers, i64> {
    const ENOEXEC: i64 = 8;
    let cwd = process.cwd.lock().clone();
    let (memory, start) = program(elf, args, env, &cwd)?;
    let task = thread::current_task().expect("a program execs");
    if !process.alone(&task) {
        return Err(ENOEXEC);
    }
    let pml4 = memory.space.pml4();
    let old = core::mem::replace(&mut *process.memory.lock(), memory);
    thread::with_current_user(|user| user.start_over(pml4));
    // Only now that the CPU is off its page tables, as its other threads,
    // gone, are.
    drop(old);
    process.signals.exec();
    task.signals.exec();
    // Its timers go, as POSIX says; its alarm stays.
    process.timers.lock().clear();
    Ok(start)
}

/// From a page fault in ring 3 at `addr`, not mapped: map it if it's where
/// the current thread's program's stack grows to. Whether it did, and the
/// instruction can run again.
pub fn grow_stack(addr: u64) -> bool {
    // As a system call runs: the locks this takes are taken with them on.
    interrupts::enable();
    let Some(process) = thread::current_process() else { return false };
    process.memory.lock().space.grow(addr)
}

/// End the current thread as `exit` does, with `code`, from one of its
/// system calls; and its program, if it's the last of its threads.
pub fn exit_thread(code: i32) -> ! {
    leave((code & 0xFF) << 8)
}

/// End the current thread's program as `exit_group` does, with `code`, from
/// one of its system calls.
pub fn exit_current(code: i32) -> ! {
    end_current((code & 0xFF) << 8)
}

/// End the current thread's program as `signal` does: from a fault it
/// caused, or a signal it was sent.
pub fn kill_current(signal: usize) -> ! {
    end_current(signal as i32)
}

/// End the current thread's program, which ended as `status` says: its other
/// threads as they next can, and the current one now.
fn end_current(status: i32) -> ! {
    if let Some((process, task)) = thread::current_user() {
        // Sending signals takes locks taken with interrupts on, and a fault
        // comes here with them off.
        interrupts::enable();
        process.end_all(&task, status);
    }
    leave(status)
}

/// End the current thread, with 0 written where its `clear_tid` says and a
/// futex there woken. The last of its program's threads ends the program
/// too: as `status` says, unless one of them ended it all, as that one's
/// does. Its memory is freed once its thread is reaped and whoever waits for
/// it lets go.
fn leave(status: i32) -> ! {
    let Some((process, task)) = thread::current_user() else { thread::exit() };
    // Closing files can wait on the disk, and a fault comes here with
    // interrupts off.
    interrupts::enable();
    let clear = task.clear_tid.swap(0, Ordering::Relaxed);
    if clear != 0 && process.memory.lock().space.write(clear, &0u32.to_le_bytes()).is_some() {
        futex::wake(process.id, clear, 1, futex::ANY);
    }
    // Interrupts off from here until the thread is switched away from for
    // the last time: once it's out of `threads`, a thread that execs can
    // free the page table it runs on.
    interrupts::disable();
    let last = {
        let mut threads = process.threads.lock();
        threads.retain(|other| !Arc::ptr_eq(other, &task));
        threads.is_empty()
    };
    if !last {
        process.thread_exited.wake_all();
        drop((process, task));
        thread::exit()
    }
    interrupts::enable();
    let status = match process.ending.load(Ordering::Acquire) {
        RUNNING | EXECING => status,
        ending => ending,
    };
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
    drop((process, task));
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
    /// Which of the program's threads it is.
    pub task: Arc<Task>,
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
    /// running `process` as `task` on page table `pml4` from `start`: its
    /// `fs` and SSE registers as they are now.
    fn fork(
        &self,
        process: Arc<Process>,
        task: Arc<Task>,
        pml4: PhysFrame,
        start: Registers,
    ) -> UserState {
        let mut fpu = FpuState::new();
        // Safety: as in `switch_out`; the CPU holds this thread's registers.
        unsafe { core::arch::asm!("fxsave64 [{}]", in(reg) &mut fpu, options(nostack)) };
        let kernel_stack_top = VirtAddr::zero();
        UserState { process, task, start, pml4, kernel_stack_top, fs_base: self.fs_base, fpu }
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

/// Where a program's thread first goes to ring 3, at `registers`, from
/// `thread::start`: once it has acted on the signals it has to take, as a
/// thread `clone` made does on Linux as the call returns. One sent SIGKILL
/// as its program ended before it ran ends here.
pub fn start_user(mut registers: Registers) -> ! {
    if let Some((process, task)) = thread::current_user()
        && signal::has_signal(&process, &task)
    {
        signal::deliver(process, task, &mut registers, None);
    }
    enter_user(registers)
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
    /// What was added to the addresses in its headers: where a
    /// position-independent program went, or 0.
    base: u64,
    entry: u64,
    /// The program headers, in the program's memory, and how many.
    phdr: u64,
    phnum: u64,
    /// The first page past the highest segment, where the heap starts.
    end: u64,
}

/// `elf`, read from `file`, loaded into `space`: at `pie_base` if it's
/// position-independent, else where its headers say, all of it below
/// `limit`.
fn load(
    space: &mut AddressSpace,
    elf: &Elf,
    file: &(impl Source + ?Sized),
    pie_base: u64,
    limit: u64,
) -> Result<Image, &'static str> {
    let base = if elf.pie { pie_base } else { 0 };
    let outside = "a segment outside the program's area";
    let mut end = 0;
    let mut phdr = None;
    // What's copied from the file, a page at a time: a program can be
    // bigger than the kernel's heap.
    let mut piece = vec![0; 4096];
    // The pages each loadable segment is in. One that's in two holds what
    // two pages of the file do, and is copied.
    let spans: Vec<_> = elf
        .segments()
        .filter(|segment| segment.kind == elf::PT_LOAD)
        .map(|segment| {
            let start = base.wrapping_add(segment.vaddr);
            (start / 4096, start.wrapping_add(segment.memsz).div_ceil(4096))
        })
        .collect();
    let in_one =
        |page: u64| spans.iter().filter(|span| (span.0..span.1).contains(&page)).count() == 1;
    for segment in elf.segments() {
        match segment.kind {
            elf::PT_LOAD => {
                let start = base.checked_add(segment.vaddr).ok_or(outside)?;
                let stop = start.checked_add(segment.memsz).ok_or(outside)?;
                if start < LOWEST || stop > limit {
                    return Err(outside);
                }
                let mut flags = PageTableFlags::empty();
                if segment.flags & elf::PF_W != 0 {
                    flags |= PageTableFlags::WRITABLE;
                }
                if segment.flags & elf::PF_X == 0 {
                    flags |= PageTableFlags::NO_EXECUTE;
                }
                // A read-only segment's pages are the file's own, from the
                // page cache, shared with whatever else maps them: those
                // wholly its, with nothing past what's in the file, which
                // are zeros. The rest are copies.
                let shareable =
                    segment.flags & elf::PF_W == 0 && segment.vaddr % 4096 == segment.offset % 4096;
                let in_file = start + segment.filesz;
                for page in pages(start, stop) {
                    let at = page.start_address().as_u64();
                    let whole = at + 4096 <= in_file || segment.memsz == segment.filesz;
                    // Where it is in the file, a whole page there, as the
                    // segment starts as far into a page in both.
                    if shareable
                        && whole
                        && in_one(at / 4096)
                        && let Some(frame) = file.page((segment.offset + at - start) / 4096)
                    {
                        space.map_shared(page, frame, flags).ok_or(NO_MEMORY)?;
                        continue;
                    }
                    space.map(Page::range(page, page + 1), flags).ok_or(NO_MEMORY)?;
                    let (from, to) = (at.max(start), (at + 4096).min(in_file));
                    if from < to {
                        let piece = &mut piece[..(to - from) as usize];
                        file.read_at(segment.offset + from - start, piece)
                            .ok_or(elf::UNREADABLE)?;
                        space.load(from, piece).ok_or(outside)?;
                    }
                }
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
        base,
        entry: entry.ok_or("an entry point outside the program")?,
        phdr: phdr.ok_or("no program headers in the program's memory")?,
        phnum: u64::from(elf.phnum),
        end: end.next_multiple_of(4096),
    })
}

/// Map the stack and lay it out as Linux does for a new program: from the
/// stack pointer up, `argc`, the `argv` pointers, the `envp` pointers, and
/// the auxiliary vector, which tells musl where its program headers (and so
/// its thread-local storage) are, a loader where it was loaded itself,
/// `interpreter`, 0 for a program without one, and who it runs as, and that
/// it isn't setuid: without those, musl's loader takes it to be, and
/// ignores LD_LIBRARY_PATH; the strings above them.
fn build_stack(
    space: &mut AddressSpace,
    args: &[&[u8]],
    env: &[&[u8]],
    image: &Image,
    interpreter: u64,
) -> Result<u64, &'static str> {
    const AT_NULL: u64 = 0;
    const AT_PHDR: u64 = 3;
    const AT_PHENT: u64 = 4;
    const AT_PHNUM: u64 = 5;
    const AT_PAGESZ: u64 = 6;
    const AT_BASE: u64 = 7;
    const AT_ENTRY: u64 = 9;
    const AT_UID: u64 = 11;
    const AT_EUID: u64 = 12;
    const AT_GID: u64 = 13;
    const AT_EGID: u64 = 14;
    const AT_SECURE: u64 = 23;
    const AT_RANDOM: u64 = 25;

    let bottom = STACK_TOP - STACK_SIZE;
    let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
    space.map(pages(bottom, STACK_TOP), flags).ok_or(NO_MEMORY)?;

    let mut top = STACK_TOP;
    let mut push = |space: &mut AddressSpace, bytes: &[u8]| {
        top = top.checked_sub(bytes.len() as u64).filter(|&top| top > bottom + 4096)?;
        space.load(top, bytes).map(|()| top)
    };
    let mut random = [0; 16];
    crate::random::fill(&mut random);
    let random = push(space, &random).ok_or(TOO_LONG)?;
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
        AT_BASE,
        interpreter,
        AT_ENTRY,
        image.entry,
        // Root, as everyone is.
        AT_UID,
        0,
        AT_EUID,
        0,
        AT_GID,
        0,
        AT_EGID,
        0,
        AT_SECURE,
        0,
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

/// The pages covering `start..end`.
pub fn pages(start: u64, end: u64) -> PageRange {
    let first = Page::containing_address(VirtAddr::new(start));
    let past = Page::containing_address(VirtAddr::new(end.next_multiple_of(4096)));
    Page::range(first, past)
}

#[cfg(test)]
mod tests {
    use super::interpreter;
    use alloc::vec::Vec;

    /// The interpreter `line` names, and its argument, as text.
    fn named(line: &[u8]) -> Option<(&str, Option<&str>)> {
        let text = |bytes| core::str::from_utf8(bytes).unwrap();
        interpreter(line).map(|(path, argument)| (text(path), argument.map(text)))
    }

    #[test_case]
    fn a_script_s_first_line_names_what_runs_it() {
        assert_eq!(named(b"#!/bin/sh\necho"), Some(("/bin/sh", None)));
        // One argument, however many words, without the blanks around it.
        assert_eq!(named(b"#! \t/bin/sh  -e -x \t\nmore"), Some(("/bin/sh", Some("-e -x"))));
        // The end of the file ends the line too.
        assert_eq!(named(b"#!/bin/sh"), Some(("/bin/sh", None)));
        assert_eq!(named(b"#!\n/bin/sh"), None);
        assert_eq!(named(b"#! \t \n"), None);
        assert_eq!(named(b"\x7fELF"), None);
        // Too long a line: its argument is cut short, but not its path.
        let mut long = b"#!/bin/sh ".to_vec();
        long.resize(300, b'x');
        let cut = core::str::from_utf8(&long[10..255]).unwrap();
        assert_eq!(named(&long), Some(("/bin/sh", Some(cut))));
        let mut path: Vec<u8> = b"#!/".to_vec();
        path.resize(300, b'x');
        assert_eq!(named(&path), None);
    }
}
