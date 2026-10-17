use pic8259::ChainedPics;
use spin::{Lazy, Mutex};

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
    PicSlave,
    Com2,
    Com1,
    Parallel23,
    Floppy,
    Parallel1,
    CmosRtc,
    Acpi,
    Free1,
    Free2,
    Ps2Mouse,
    Fpu,
    PrimaryAta,
    SecondaryAta,
}

impl InterruptIndex {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
}

static PICS: Lazy<Mutex<ChainedPics>> =
    Lazy::new(|| unsafe { Mutex::new(ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET)) });

pub fn init_pics() {
    unsafe {
        // Step 1: Initialize the PICs
        PICS.lock().initialize();
    }
}

pub fn config_pics() {
    // The lines with handlers, and no others, whatever the firmware left
    // open: an interrupt with none is a fault. IRQ 0 (timer), 1 (keyboard)
    // and 2 (the slave) on the master, and none on the slave.
    unsafe { PICS.lock().write_masks(!0b0000_0111, !0) };
}

/// End an interrupt at the master alone, as the slave's spurious one is.
pub fn end_at_master() {
    // Safety: 0x20 is the master's command port, and 0x20 there is EOI.
    unsafe { x86_64::instructions::port::Port::<u8>::new(0x20).write(0x20u8) };
}

pub fn notify_end_of_interrupt(interrupt_index: &InterruptIndex) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_index.as_u8());
    }
}
