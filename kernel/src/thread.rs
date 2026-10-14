//! Kernel threads: each with a stack of its own, taking turns on the one CPU.
//!
//! Cooperative for now. A thread runs until it calls [`yield_now`] or its
//! entry function returns; the timer doesn't preempt anyone yet.
//!
//! Every switch happens with interrupts off, inside [`yield_now`] or [`exit`],
//! and the scheduler's lock is released before it. The thread switched to
//! resumes inside its own earlier call, and turns interrupts back on as that
//! call returns, or in [`start`] if it has never run.

use crate::memory::KernelStack;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec::Vec;
use spin::{Lazy, Mutex};
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
        dead: Vec::new(),
    })
});

/// Start `entry` on a thread of its own. It first runs when the current
/// thread yields. `None` when there's no memory for its stack.
pub fn spawn(entry: impl FnOnce() + Send + 'static) -> Option<()> {
    let stack = KernelStack::new()?;
    // What the first `switch` to this thread pops: the six registers it saves,
    // then `start` as the address to return to, then a dummy return address
    // for `start` itself, so it begins with the stack aligned as any called
    // function expects (8 below a 16-byte boundary).
    let rsp = stack.top().as_u64() - 8 * 8;
    let frame: [u64; 8] = [0, 0, 0, 0, 0, 0, start as *const () as u64, 0];
    // Safety: the top of a fresh stack, mapped and used by nothing yet.
    unsafe { (rsp as *mut [u64; 8]).write(frame) };

    let thread = Thread { rsp, _stack: Some(stack), entry: Some(Box::new(entry)) };
    interrupts::without_interrupts(|| SCHEDULER.lock().ready.push_back(Box::new(thread)));
    Some(())
}

/// Let the next ready thread run. Returns when this thread's turn comes round
/// again, or at once if no one else is ready.
pub fn yield_now() {
    interrupts::without_interrupts(|| {
        let mut scheduler = SCHEDULER.lock();
        let Some(next) = scheduler.ready.pop_front() else { return };
        let to = next.rsp;
        let mut previous = core::mem::replace(&mut scheduler.current, next);
        // Into the Box, whose contents stay put while the Box itself moves.
        let from = &raw mut previous.rsp;
        scheduler.ready.push_back(previous);
        drop(scheduler);
        // Safety: `to` is where `switch` left a ready thread, or a new
        // thread's prepared stack, and `from` outlives the switch.
        unsafe { switch(from, to) };
        reap();
    });
}

/// End the current thread. Its stack is freed by whichever thread runs next.
///
/// # Panics
///
/// If no other thread is ready to run.
pub fn exit() -> ! {
    interrupts::disable();
    let mut scheduler = SCHEDULER.lock();
    let next = scheduler.ready.pop_front().expect("the last thread exited");
    let to = next.rsp;
    let mut previous = core::mem::replace(&mut scheduler.current, next);
    let from = &raw mut previous.rsp;
    scheduler.dead.push(previous);
    drop(scheduler);
    // Safety: as in `yield_now`; `from` is kept alive in `dead`.
    unsafe { switch(from, to) };
    unreachable!("an exited thread was switched back to");
}

/// Free the stacks of threads that have exited. Never called by one of them.
fn reap() {
    let dead = core::mem::take(&mut SCHEDULER.lock().dead);
    drop(dead);
}

/// Where a new thread begins, returned to by its first [`switch`].
extern "C" fn start() -> ! {
    // Whoever switched here may have just exited.
    reap();
    let entry = SCHEDULER.lock().current.entry.take().expect("a new thread has an entry");
    interrupts::enable();
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
