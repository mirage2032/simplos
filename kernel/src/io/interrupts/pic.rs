use core::ops::Deref;
use pic8259::ChainedPics;
use spin::{Lazy, Mutex};
use x86_64::instructions::port::Port;
// use crate::println;

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
    pub fn as_usize(self) -> usize {
        self as usize
    }
}

static PICS: Lazy<Mutex<ChainedPics>> = Lazy::new(|| unsafe {
    Mutex::new( ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) )
});

pub fn init_pics() {
    unsafe {
        // Step 1: Initialize the PICs and unmask IRQ2 (cascade) and IRQ12 (mouse)
        PICS.lock().initialize();
    }
}

pub fn config_pics(){
    // Read the current interrupt masks
    let mut masks = unsafe { PICS.lock().read_masks() };
    // println!("{:#b}-{:#b}",masks[0],masks[1]);
    
    // Unmask IRQ2 on PIC1 (cascade line) and IRQ12 on PIC2 (mouse interrupt)
    let timer_bit = !(1 << 0); // Bit 0 corresponds to IRQ0 (timer)
    let pic2_bit = !(1 << 2);  // Bit 2 corresponds to IRQ2
    let mouse_bit = !(1 << 4); // Bit 4 corresponds to IRQ12
    masks[0] &= timer_bit;     // Clear the mask for IRQ0 on PIC1
    masks[0] &= pic2_bit;      // Clear the mask for IRQ2 on PIC1
    masks[1] &= mouse_bit;     // Clear the mask for IRQ12 on PIC2
    // println!("{:#b}-{:#b}",masks[0],masks[1]);
    
    // Write the updated masks back to the PICs
    unsafe { PICS.lock().write_masks(masks[0], masks[1]) };
}

pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

pub fn notify_end_of_interrupt(interrupt_index: &InterruptIndex) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_index.as_u8());
    }
}