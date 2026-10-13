//! Lock-free console buffer for interrupt-safe logging
//!
//! Interrupts can push messages to this buffer without blocking.
//! The main loop can drain and display these messages.

use alloc::string::String;
use core::cell::UnsafeCell;
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};

/// Maximum number of messages in the buffer
const BUFFER_SIZE: usize = 64;

/// Maximum length of a single message
const MAX_MSG_LEN: usize = 128;

/// A single message slot
struct MessageSlot {
    data: UnsafeCell<[u8; MAX_MSG_LEN]>,
    len: AtomicUsize,
}

impl MessageSlot {
    const fn new() -> Self {
        Self { data: UnsafeCell::new([0; MAX_MSG_LEN]), len: AtomicUsize::new(0) }
    }
}

/// Formats straight into a slot, cutting off whatever doesn't fit.
struct SlotWriter<'a> {
    buf: &'a mut [u8; MAX_MSG_LEN],
    len: usize,
}

impl fmt::Write for SlotWriter<'_> {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        let n = s.len().min(MAX_MSG_LEN - self.len);
        self.buf[self.len..self.len + n].copy_from_slice(&s.as_bytes()[..n]);
        self.len += n;
        Ok(())
    }
}

/// Lock-free ring buffer for console messages
pub struct ConsoleBuffer {
    slots: [MessageSlot; BUFFER_SIZE],
    write_pos: AtomicUsize,
    read_pos: AtomicUsize,
}

// Safety: one producer (interrupt handlers, which don't nest) and one consumer
// (the main loop). A slot's bytes are only touched by the side that owns it,
// and ownership is handed over through the Release/Acquire positions.
unsafe impl Sync for ConsoleBuffer {}

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
    ///
    /// Takes `format_args!` rather than a string so that callers never touch
    /// the heap: an interrupt that lands while the main loop holds the
    /// allocator's spin lock would wait for it forever.
    pub fn push(&self, args: fmt::Arguments) -> bool {
        let write = self.write_pos.load(Ordering::Acquire);
        let read = self.read_pos.load(Ordering::Acquire);

        // Check if buffer is full
        let next_write = (write + 1) % BUFFER_SIZE;
        if next_write == read {
            return false; // Buffer full, drop message
        }

        let slot = &self.slots[write];
        // Safety: the reader won't look at this slot until `write_pos` moves past it.
        let mut writer = SlotWriter { buf: unsafe { &mut *slot.data.get() }, len: 0 };
        let _ = fmt::Write::write_fmt(&mut writer, args);
        slot.len.store(writer.len, Ordering::Release);

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

        // Safety: the writer won't touch this slot again until `read_pos` moves past it.
        let data = unsafe { &*slot.data.get() };
        let msg = String::from_utf8_lossy(&data[..len]).into_owned();

        // Clear slot and advance read position
        slot.len.store(0, Ordering::Release);
        self.read_pos.store((read + 1) % BUFFER_SIZE, Ordering::Release);

        Some(msg)
    }
}

/// Global console buffer - safe to use from interrupts
pub static CONSOLE: ConsoleBuffer = ConsoleBuffer::new();

impl Default for ConsoleBuffer {
    fn default() -> Self {
        Self::new()
    }
}
