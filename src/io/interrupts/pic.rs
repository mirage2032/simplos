use core::ops::Deref;
use pic8259::ChainedPics;
use spin::{Lazy, Mutex};

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
}

impl InterruptIndex {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn as_usize(self) -> usize {
        self as usize
    }
}

static PICS: Lazy<Mutex<ChainedPics>> = Lazy::new(|| unsafe {
    Mutex::new( ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) )
});

pub fn init_pics() {
    unsafe {
        PICS.lock().initialize();
    }
}

pub fn enable_interrupts() {
    x86_64::instructions::interrupts::enable();
}

pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

pub fn notify_end_of_interrupt(interrupt_index: &InterruptIndex) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_index.as_u8());
    }
}