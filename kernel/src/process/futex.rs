//! Futexes: words of a program's memory that its threads wait on until
//! another wakes them, as Linux's `futex` has them. musl's and Rust's
//! locks, condition variables and `pthread_join` are made of them.
//!
//! shortcut: a futex is its program's alone, found by its address, as
//! FUTEX_PRIVATE_FLAG's are. None is shared between programs, which have no
//! shared memory to put one in.

use crate::process::Process;
use crate::thread::{self, Interrupted, WaitQueue};
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, Ordering};
use spin::Mutex;

/// The bitset that every waiter's matches.
pub const ANY: u32 = u32::MAX;

/// A thread waiting on a futex.
struct Waiter {
    /// The futex: its program's id, and its address there.
    process: u64,
    addr: u64,
    /// FUTEX_WAIT_BITSET's: only a wake whose bitset shares a bit with this
    /// one wakes it.
    bitset: u32,
    state: Arc<State>,
}

/// What a waiter and whoever wakes it share.
struct State {
    woken: AtomicBool,
    /// What it waits in, if it waits without a deadline. With one, it waits
    /// as `poll` does, as the timer wakes those.
    queue: WaitQueue,
    timed: bool,
}

/// Every thread that waits on a futex, the first to wait first. Taken with
/// interrupts on, and before a program's memory.
static WAITERS: Mutex<Vec<Waiter>> = Mutex::new(Vec::new());

/// Why a wait ended without a wake, or didn't start.
pub enum Error {
    /// The word wasn't what the waiter expected: it changed already.
    Again,
    /// The word isn't in the program's memory.
    Fault,
    TimedOut,
    Interrupted,
}

/// The 32-bit word at `addr` in `process`'s memory.
fn word(process: &Process, addr: u64) -> Result<u32, Error> {
    let mut word = [0; 4];
    process.read(addr, &mut word).ok_or(Error::Fault)?;
    Ok(u32::from_le_bytes(word))
}

/// Wait on the futex at `addr` in `process`, the current thread's program,
/// if the word there is `expected`, until a wake whose bitset shares a bit
/// with `bitset`, or the tick `deadline` if there is one, or a signal.
pub fn wait(
    process: &Process,
    addr: u64,
    expected: u32,
    deadline: Option<u64>,
    bitset: u32,
) -> Result<(), Error> {
    let timed = deadline.is_some();
    let state = Arc::new(State { woken: AtomicBool::new(false), queue: WaitQueue::new(), timed });
    // Read first without them, so that its page, if the program hasn't
    // touched it, isn't read in from the disk with them held.
    word(process, addr)?;
    {
        // Read with the waiters held: whoever changes the word and then
        // wakes it either changes it before this reads it, or wakes this.
        let mut waiters = WAITERS.lock();
        if word(process, addr)? != expected {
            return Err(Error::Again);
        }
        let waiter = Waiter { process: process.id, addr, bitset, state: state.clone() };
        waiters.push(waiter);
    }
    let woken = || state.woken.load(Ordering::Acquire);
    let waited = match deadline {
        None => state.queue.wait_until_interruptible(woken),
        Some(deadline) => thread::poll_until(Some(deadline), woken),
    };
    // Unless a wake came, even one just now, which then counts.
    let mut waiters = WAITERS.lock();
    if woken() {
        return Ok(());
    }
    waiters.retain(|waiter| !Arc::ptr_eq(&waiter.state, &state));
    match waited {
        Ok(()) => Err(Error::TimedOut),
        Err(Interrupted) => Err(Error::Interrupted),
    }
}

/// Wake up to `count` of the threads waiting on the futex at `addr` in the
/// program numbered `process`, the first to wait first, of those whose
/// bitset shares a bit with `bitset`: how many it woke.
pub fn wake(process: u64, addr: u64, count: usize, bitset: u32) -> usize {
    wake_from(&mut WAITERS.lock(), process, addr, count, bitset)
}

fn wake_from(
    waiters: &mut Vec<Waiter>,
    process: u64,
    addr: u64,
    count: usize,
    bitset: u32,
) -> usize {
    let on = |waiter: &mut Waiter| {
        waiter.process == process && waiter.addr == addr && waiter.bitset & bitset != 0
    };
    let woken: Vec<Waiter> = waiters.extract_if(.., on).take(count).collect();
    for waiter in &woken {
        waiter.state.woken.store(true, Ordering::Release);
        waiter.state.queue.wake_all();
    }
    if woken.iter().any(|waiter| waiter.state.timed) {
        thread::wake_pollers();
    }
    woken.len()
}

/// Wake up to `count` of the threads waiting on the futex at `addr` in
/// `process`, the current thread's program, and move up to `moved` more of
/// them to the one at `to`, as FUTEX_REQUEUE does; and with `expected`, as
/// FUTEX_CMP_REQUEUE does, only if the word at `addr` is that. How many it
/// woke and moved, as Linux counts them.
pub fn requeue(
    process: &Process,
    addr: u64,
    count: usize,
    to: u64,
    moved: usize,
    expected: Option<u32>,
) -> Result<usize, Error> {
    // Read in, if it must be, without the waiters held: as in `wait`.
    if expected.is_some() {
        word(process, addr)?;
    }
    let mut waiters = WAITERS.lock();
    if let Some(expected) = expected
        && word(process, addr)? != expected
    {
        return Err(Error::Again);
    }
    let woken = wake_from(&mut waiters, process.id, addr, count, ANY);
    let on = |waiter: &&mut Waiter| waiter.process == process.id && waiter.addr == addr;
    let mut requeued = 0;
    for waiter in waiters.iter_mut().filter(on).take(moved) {
        waiter.addr = to;
        requeued += 1;
    }
    Ok(woken + requeued)
}
