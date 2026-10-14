//! Kernel threads: each with a stack of its own, taking turns on the one CPU.
//!
//! A thread runs until it calls [`yield_now`], blocks ([`sleep`],
//! [`WaitQueue::wait_until`]), its entry function returns, or a timer tick
//! preempts it: the timer interrupt calls [`timer_tick`], which wakes sleepers
//! and yields, so no thread keeps the CPU for more than a tick (10 ms) while
//! others are ready. When no one is ready, the idle thread halts the CPU until
//! the next interrupt.
//!
//! Every switch happens with interrupts off and the scheduler's lock released.
//! The thread switched to resumes inside its own earlier call, and turns
//! interrupts back on as that call returns, or in [`start`] if it has never run.
//!
//! The scheduler's lock is only ever taken with interrupts off, and so must
//! be every lock taken while holding it (the heap's and the wait queues', for
//! a start): a lock a preempted thread was holding would otherwise never come
//! free.

use crate::io::interrupts::{idt::TIMER_COUNTER, pit::TIMER_HZ};
use crate::memory::KernelStack;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::{Lazy, Mutex, MutexGuard};
use x86_64::instructions::interrupts;

struct Thread {
    /// Where [`switch`] left the stack pointer while the thread isn't running.
    rsp: u64,
    /// Kept for its `Drop`, which frees the stack. `None` for the boot thread,
    /// which runs on the bootloader's stack.
    _stack: Option<KernelStack>,
    /// What a new thread runs, taken when it starts.
    entry: Option<Box<dyn FnOnce() + Send>>,
}

struct Scheduler {
    current: Box<Thread>,
    /// Waiting for their turn, next first.
    ready: VecDeque<Box<Thread>>,
    /// Runs when no one else can, and never waits in `ready`. `None` while it
    /// is the current thread, and before [`init`].
    idle: Option<Box<Thread>>,
    /// Whether `current` is the idle thread.
    idling: bool,
    /// Asleep, each with the tick it wakes on.
    sleeping: Vec<(u64, Box<Thread>)>,
    /// Exited, and kept until another thread frees their stacks: a thread
    /// can't unmap the stack it is running on.
    #[expect(clippy::vec_box, reason = "`switch` writes through a pointer into the Box")]
    dead: Vec<Box<Thread>>,
}

static SCHEDULER: Lazy<Mutex<Scheduler>> = Lazy::new(|| {
    Mutex::new(Scheduler {
        // Whoever first touches the scheduler is the boot thread.
        current: Box::new(Thread { rsp: 0, _stack: None, entry: None }),
        ready: VecDeque::new(),
        idle: None,
        idling: false,
        sleeping: Vec::new(),
        dead: Vec::new(),
    })
});

/// Create the idle thread. `pre_init` calls it before turning interrupts on;
/// without it, a thread that blocks with no one else ready has nowhere to go.
pub fn init() {
    let idle = new_thread(Box::new(idle)).expect("memory for the idle thread's stack");
    interrupts::without_interrupts(|| SCHEDULER.lock().idle = Some(idle));
}

/// Start `entry` on a thread of its own. It first runs when the current
/// thread yields or blocks. `None` when there's no memory for its stack.
pub fn spawn(entry: impl FnOnce() + Send + 'static) -> Option<()> {
    let thread = new_thread(Box::new(entry))?;
    interrupts::without_interrupts(|| SCHEDULER.lock().ready.push_back(thread));
    Some(())
}

fn new_thread(entry: Box<dyn FnOnce() + Send>) -> Option<Box<Thread>> {
    let stack = KernelStack::new()?;
    // What the first `switch` to this thread pops: the six registers it saves,
    // then `start` as the address to return to, then a dummy return address
    // for `start` itself, so it begins with the stack aligned as any called
    // function expects (8 below a 16-byte boundary).
    let rsp = stack.top().as_u64() - 8 * 8;
    let frame: [u64; 8] = [0, 0, 0, 0, 0, 0, start as *const () as u64, 0];
    // Safety: the top of a fresh stack, mapped and used by nothing yet.
    unsafe { (rsp as *mut [u64; 8]).write(frame) };
    Some(Box::new(Thread { rsp, _stack: Some(stack), entry: Some(entry) }))
}

/// Let the next ready thread run. Returns when this thread's turn comes round
/// again, or at once if no one else is ready.
pub fn yield_now() {
    interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();
        let Some(next) = scheduler.ready.pop_front() else { return };
        switch_to(scheduler, next, false, |scheduler, previous| {
            scheduler.ready.push_back(previous)
        });
    });
    reap();
}

/// Sleep for at least `ms` milliseconds, letting other threads run meanwhile.
pub fn sleep(ms: u64) {
    // Rounded up to whole ticks, and one more: the current tick is partly over.
    let wake = ticks() + (ms * TIMER_HZ as u64).div_ceil(1000) + 1;
    interrupts::without_interrupts(|| {
        while ticks() < wake {
            block(|scheduler, me| scheduler.sleeping.push((wake, me)));
        }
    });
    reap();
}

/// The timer interrupt's part: wake the sleepers whose tick has come, then
/// preempt whichever thread was interrupted.
pub fn timer_tick() {
    let now = ticks();
    let mut scheduler = SCHEDULER.lock();
    let due: Vec<_> = scheduler.sleeping.extract_if(.., |(wake, _)| *wake <= now).collect();
    scheduler.ready.extend(due.into_iter().map(|(_, thread)| thread));
    drop(scheduler);
    yield_now();
}

fn ticks() -> u64 {
    TIMER_COUNTER.load(Ordering::Relaxed) as u64
}

/// Threads waiting for something that another thread or an interrupt handler
/// announces with [`wake_all`](WaitQueue::wake_all).
pub struct WaitQueue {
    waiting: Mutex<VecDeque<Box<Thread>>>,
}

impl WaitQueue {
    pub const fn new() -> WaitQueue {
        WaitQueue { waiting: Mutex::new(VecDeque::new()) }
    }

    /// Block until `done` returns true, checking it again each time the
    /// queue is woken. It runs with interrupts off, so it must be quick and
    /// mustn't block; in exchange, a `wake_all` from an interrupt handler
    /// can't slip in between the check and going to sleep.
    pub fn wait_until(&self, mut done: impl FnMut() -> bool) {
        interrupts::without_interrupts(|| {
            while !done() {
                block(|_, me| self.waiting.lock().push_back(me));
            }
        });
        reap();
    }

    /// Make every waiting thread ready to run again. Safe to call from an
    /// interrupt handler.
    pub fn wake_all(&self) {
        interrupts::without_interrupts(|| {
            let woken = core::mem::take(&mut *self.waiting.lock());
            SCHEDULER.lock().ready.extend(woken);
        });
    }
}

impl Default for WaitQueue {
    fn default() -> WaitQueue {
        WaitQueue::new()
    }
}

/// End the current thread. Its stack is freed by the next thread to come
/// back from a yield or a block, or to start.
pub fn exit() -> ! {
    interrupts::disable();
    block(|scheduler, me| scheduler.dead.push(me));
    unreachable!("an exited thread was switched back to");
}

/// Switch away from the current thread, handing it to `park` until something
/// makes it ready again, to the next ready thread or else the idle one.
/// Interrupts must be off.
fn block(park: impl FnOnce(&mut Scheduler, Box<Thread>)) {
    let mut scheduler = SCHEDULER.lock();
    let (next, idle) = match scheduler.ready.pop_front() {
        Some(next) => (next, false),
        None => (scheduler.idle.take().expect("an idle thread to run, from thread::init"), true),
    };
    switch_to(scheduler, next, idle, park);
}

/// Make `next` the current thread and switch to it, handing the previous one
/// to `park`, or back to the idle slot if it was the idle thread. Returns when
/// the previous thread is switched back to. Interrupts must be off.
fn switch_to(
    mut scheduler: MutexGuard<'_, Scheduler>,
    next: Box<Thread>,
    next_is_idle: bool,
    park: impl FnOnce(&mut Scheduler, Box<Thread>),
) {
    let to = next.rsp;
    let mut previous = core::mem::replace(&mut scheduler.current, next);
    // Into the Box, whose contents stay put while the Box itself moves.
    let from = &raw mut previous.rsp;
    if core::mem::replace(&mut scheduler.idling, next_is_idle) {
        scheduler.idle = Some(previous);
    } else {
        park(&mut scheduler, previous);
    }
    drop(scheduler);
    // Safety: `to` is where `switch` left a parked thread, or a new thread's
    // prepared stack, and `from` lives in a Box the scheduler keeps.
    unsafe { switch(from, to) };
}

/// Free the stacks of threads that have exited. Never called by one of them.
/// Does nothing with interrupts off, as in the timer interrupt: freeing a
/// stack takes the page table's and the frames' locks, which a preempted
/// thread may be holding.
fn reap() {
    if !interrupts::are_enabled() {
        return;
    }
    let dead = interrupts::without_interrupts(|| core::mem::take(&mut SCHEDULER.lock().dead));
    drop(dead);
}

/// What the idle thread runs: free dead threads' stacks, then halt until an
/// interrupt makes someone ready.
fn idle() {
    loop {
        reap();
        interrupts::disable();
        if SCHEDULER.lock().ready.is_empty() {
            // `sti; hlt` back to back, so an interrupt that makes a thread
            // ready can't land between the check and the halt and leave the
            // CPU stopped until the next tick.
            interrupts::enable_and_hlt();
        } else {
            interrupts::enable();
            yield_now();
        }
    }
}

/// Where a new thread begins, returned to by its first [`switch`].
extern "C" fn start() -> ! {
    let entry = SCHEDULER.lock().current.entry.take().expect("a new thread has an entry");
    interrupts::enable();
    // Whoever switched here may have just exited.
    reap();
    entry();
    exit()
}

/// Save the callee-saved registers on the current stack and the stack pointer
/// in `*from`, then load `to` and pop the registers saved there. The `ret`
/// returns into whichever thread `to` belongs to, as if its own call to
/// `switch` had just finished. The caller-saved registers need no saving: the
/// compiler already assumes a call clobbers them.
///
/// # Safety
///
/// `to` must be a stack pointer saved by `switch`, or prepared by [`spawn`].
#[unsafe(naked)]
unsafe extern "sysv64" fn switch(from: *mut u64, to: u64) {
    core::arch::naked_asm!(
        "push rbp",
        "push rbx",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov [rdi], rsp",
        "mov rsp, rsi",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop rbx",
        "pop rbp",
        "ret",
    );
}
