//! Pipes: what one program writes in, another reads out, each waiting for
//! the other while it's full or empty.

use crate::thread::{self, WaitQueue};
use alloc::collections::VecDeque;
use alloc::sync::Arc;
use alloc::vec::Vec;
use spin::Mutex;
use x86_64::instructions::interrupts;

/// What a pipe holds before its writers wait: Linux's default.
const CAPACITY: usize = 64 * 1024;

struct Pipe {
    /// Taken only with interrupts off, as `WaitQueue::wait_until` checks it.
    state: Mutex<State>,
    /// Woken whenever data goes in or out, or an end closes.
    changed: WaitQueue,
}

struct State {
    data: VecDeque<u8>,
    /// Ends of each kind still open.
    readers: usize,
    writers: usize,
}

/// One end of a pipe. Copies of a descriptor share it, and it closes when
/// the last of them does.
pub struct PipeEnd {
    pipe: Arc<Pipe>,
    pub write: bool,
}

/// A new pipe's reading end and writing end.
pub fn new() -> (PipeEnd, PipeEnd) {
    let state = State { data: VecDeque::new(), readers: 1, writers: 1 };
    let pipe = Arc::new(Pipe { state: Mutex::new(state), changed: WaitQueue::new() });
    (PipeEnd { pipe: pipe.clone(), write: false }, PipeEnd { pipe, write: true })
}

/// Why a write put nothing in.
#[derive(Debug, PartialEq, Eq)]
pub enum WriteError {
    /// Every reading end has closed.
    Broken,
    /// It's full, and the writer won't wait.
    Full,
}

impl PipeEnd {
    /// Up to `most` bytes, waiting until there are some if `wait`; none once
    /// every writing end has closed and all that was written has been read.
    /// `None` if there are none yet and it wouldn't wait.
    pub fn read(&self, most: usize, wait: bool) -> Option<Vec<u8>> {
        let mut taken = Vec::new();
        let mut empty = false;
        // Checked and taken in one go, so that two readers can't both see
        // the same bytes waiting.
        self.pipe.changed.wait_until(|| {
            let mut state = self.pipe.state.lock();
            if state.data.is_empty() {
                empty = state.writers > 0;
                return !empty || !wait;
            }
            let count = most.min(state.data.len());
            taken.extend(state.data.drain(..count));
            empty = false;
            true
        });
        self.pipe.changed.wake_all();
        thread::wake_pollers();
        (!empty).then_some(taken)
    }

    /// Write all of `data`, waiting for room as it's read out if `wait`, or
    /// else as much as there's room for: how much went in, fewer than all
    /// if every reading end closed meanwhile.
    ///
    /// shortcut: a write that doesn't wait can go in part even if it's as
    /// small as PIPE_BUF, which Linux puts in whole or not at all.
    pub fn write(&self, data: &[u8], wait: bool) -> Result<usize, WriteError> {
        let mut written = 0;
        let mut broken = false;
        while written < data.len() && !broken {
            self.pipe.changed.wait_until(|| {
                let mut state = self.pipe.state.lock();
                if state.readers == 0 {
                    broken = true;
                    return true;
                }
                let room = CAPACITY - state.data.len();
                let count = room.min(data.len() - written);
                state.data.extend(&data[written..written + count]);
                written += count;
                count > 0 || !wait
            });
            self.pipe.changed.wake_all();
            thread::wake_pollers();
            if !wait {
                break;
            }
        }
        match (written, broken) {
            (0, true) => Err(WriteError::Broken),
            (0, false) if !data.is_empty() => Err(WriteError::Full),
            _ => Ok(written),
        }
    }

    /// What `poll` sees of it: whether a read or a write wouldn't wait, and
    /// whether the other end has closed. Takes the pipe's lock, so only with
    /// interrupts off.
    pub fn poll(&self) -> Ready {
        let state = self.pipe.state.lock();
        match self.write {
            false => Ready { now: !state.data.is_empty(), closed: state.writers == 0 },
            true => Ready { now: state.data.len() < CAPACITY, closed: state.readers == 0 },
        }
    }
}

pub struct Ready {
    pub now: bool,
    pub closed: bool,
}

impl Drop for PipeEnd {
    fn drop(&mut self) {
        interrupts::without_interrupts(|| {
            let mut state = self.pipe.state.lock();
            match self.write {
                true => state.writers -= 1,
                false => state.readers -= 1,
            }
        });
        self.pipe.changed.wake_all();
        thread::wake_pollers();
    }
}
