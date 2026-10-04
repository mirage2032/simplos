//! Lock-free console buffer for interrupt-safe logging
//!
//! Interrupts can push messages to this buffer without blocking.
//! The main loop can drain and display these messages.

use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of messages in the buffer
const BUFFER_SIZE: usize = 64;

/// Maximum length of a single message
const MAX_MSG_LEN: usize = 128;

/// A single message slot
struct MessageSlot {
    data: [u8; MAX_MSG_LEN],
    len: AtomicUsize,
}

impl MessageSlot {
    const fn new() -> Self {
        Self { data: [0; MAX_MSG_LEN], len: AtomicUsize::new(0) }
    }
}

/// Lock-free ring buffer for console messages
pub struct ConsoleBuffer {
    slots: [MessageSlot; BUFFER_SIZE],
    write_pos: AtomicUsize,
    read_pos: AtomicUsize,
}

impl ConsoleBuffer {
    pub const fn new() -> Self {
        // `[MessageSlot::new(); N]` needs the element to be `Copy`, which an
        // `AtomicUsize` is not. `from_fn` in a const context isn't available
        // either, so the array is built by repeating a const — written inline
        // rather than as a named `const`, which would read as a shared value
        // when each slot is in fact its own.
        Self {
            slots: [const { MessageSlot::new() }; BUFFER_SIZE],
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
        }
    }

    /// Push a message to the buffer (safe to call from interrupts)
    /// Returns false if buffer is full
    pub fn push(&self, msg: &str) -> bool {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        // Check if buffer is full
        let next_write = (write + 1) % BUFFER_SIZE;
        if next_write == read {
            return false; // Buffer full, drop message
        }

        let slot = &self.slots[write];
        let bytes = msg.as_bytes();
        let len = bytes.len().min(MAX_MSG_LEN);

        // Copy message to slot
        // Safety: We're the only writer to this slot at this position
        unsafe {
            let data_ptr = slot.data.as_ptr() as *mut u8;
            core::ptr::copy_nonoverlapping(bytes.as_ptr(), data_ptr, len);
        }
        slot.len.store(len, Ordering::Release);

        // Advance write position
        self.write_pos.store(next_write, Ordering::Release);
        true
    }

    /// Pop a message from the buffer (call from main loop only)
    pub fn pop(&self) -> Option<String> {
        let read = self.read_pos.load(Ordering::Acquire);
        let write = self.write_pos.load(Ordering::Acquire);

        if read == write {
            return None; // Buffer empty
        }

        let slot = &self.slots[read];
        let len = slot.len.load(Ordering::Acquire);

        if len == 0 {
            return None;
        }

        // Copy message out
        let msg = unsafe {
            let slice = core::slice::from_raw_parts(slot.data.as_ptr(), len);
            String::from_utf8_lossy(slice).into_owned()
        };

        // Clear slot and advance read position
        slot.len.store(0, Ordering::Release);
        self.read_pos.store((read + 1) % BUFFER_SIZE, Ordering::Release);

        Some(msg)
    }

    /// Check if there are messages to read
    pub fn has_messages(&self) -> bool {
        self.read_pos.load(Ordering::Acquire) != self.write_pos.load(Ordering::Acquire)
    }

    /// Drain all messages into a vector (call from main loop)
    pub fn drain(&self) -> Vec<String> {
        let mut messages = Vec::new();
        while let Some(msg) = self.pop() {
            messages.push(msg);
        }
        messages
    }
}

/// Global console buffer - safe to use from interrupts
pub static CONSOLE: ConsoleBuffer = ConsoleBuffer::new();

/// Log a message from anywhere (interrupt-safe)
#[macro_export]
macro_rules! console_log {
    ($($arg:tt)*) => {
        {
            use alloc::format;
            let msg = format!($($arg)*);
            $crate::io::console::CONSOLE.push(&msg);
        }
    };
}

/// Log from interrupt handler (same as console_log but more explicit name)
#[macro_export]
macro_rules! interrupt_log {
    ($($arg:tt)*) => {
        $crate::console_log!($($arg)*)
    };
}

impl Default for ConsoleBuffer {
    fn default() -> Self {
        Self::new()
    }
}
