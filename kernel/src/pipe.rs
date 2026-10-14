//! Pipes: what one program writes in, another reads out, each waiting for
//! the other while it's full or empty.

use crate::thread::WaitQueue;
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

impl PipeEnd {
    /// Up to `most` bytes, waiting until there are some; none once every
    /// writing end has closed and all that was written has been read.
    pub fn read(&self, most: usize) -> Vec<u8> {
        let mut taken = Vec::new();
        // Checked and taken in one go, so that two readers can't both see
        // the same bytes waiting.
        self.pipe.changed.wait_until(|| {
            let mut state = self.pipe.state.lock();
            if state.data.is_empty() {
                return state.writers == 0;
            }
            let count = most.min(state.data.len());
            taken.extend(state.data.drain(..count));
            true
        });
        self.pipe.changed.wake_all();
        taken
    }

    /// Write all of `data`, waiting for room as it's read out: how much
    /// went in, fewer only if every reading end closed meanwhile, and
    /// `None` if they had before any did.
    pub fn write(&self, data: &[u8]) -> Option<usize> {
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
                count > 0
            });
            self.pipe.changed.wake_all();
        }
        (written > 0 || !broken).then_some(written)
    }
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
    }
}
