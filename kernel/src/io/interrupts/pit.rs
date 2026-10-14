//! The PIT (8253/8254), the timer behind IRQ 0.

use x86_64::instructions::port::Port;

/// Timer interrupts per second. Left alone the PIT runs at about 18.2 Hz, too
/// coarse to share the CPU between programs. 100 Hz, a 10 ms tick, is Linux's
/// lowest `CONFIG_HZ`: fine enough to schedule with, and cheap when every
/// interrupt is emulated, as in the browser build.
pub const TIMER_HZ: u32 = 100;

/// The PIT's input clock.
const PIT_INPUT_HZ: u32 = 1_193_182;

/// What the input clock is divided by, rounded to nearest: 11932 for 100 Hz,
/// which is 99.998 Hz. 16 bits, so `TIMER_HZ` can't go below 19 Hz.
const DIVISOR: u16 = {
    let divisor = (PIT_INPUT_HZ + TIMER_HZ / 2) / TIMER_HZ;
    assert!(divisor > 1 && divisor <= u16::MAX as u32, "TIMER_HZ out of the PIT's range");
    divisor as u16
};

/// Start channel 0 ticking at `TIMER_HZ`.
pub fn init_pit() {
    // Safety: 0x43 and 0x40 are the PIT's command and channel 0 ports, and
    // nothing else drives them.
    unsafe {
        // Channel 0, low byte then high byte, mode 2 (rate generator), binary.
        Port::<u8>::new(0x43).write(0b0011_0100);
        let mut channel_0 = Port::<u8>::new(0x40);
        channel_0.write(DIVISOR as u8);
        channel_0.write((DIVISOR >> 8) as u8);
    }
}
