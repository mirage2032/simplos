pub mod gdt;
pub mod pic;
pub mod idt;

use spin::Lazy;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use crate::println;
use crate::vga_buffer::{Color, ColorCode, WRITER};
pub fn init_interrupts() {
    gdt::init_gdt();
    idt::init_idt();
    pic::init_pics();
}