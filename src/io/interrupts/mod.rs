use core::cell::OnceCell;
use spin::Lazy;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use crate::println;
use crate::vga_buffer::{Color, ColorCode, WRITER};

pub static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt =InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt
});
pub fn init_idt() {
    IDT.load();
}

const EXCEPTION_COLOR: Lazy<ColorCode> = Lazy::new(|| {
    ColorCode::new(Color::LightRed, Color::Black)
});
extern "x86-interrupt" fn breakpoint_handler(
    stack_frame: InterruptStackFrame)
{
    let default_color = WRITER.lock().get_color();
    WRITER.lock().set_color(*EXCEPTION_COLOR);
    WRITER.lock().new_line();
    println!("EXCEPTION: BREAKPOINT");
    println!("{:#?}", stack_frame);
    WRITER.lock().set_color(default_color);
    WRITER.lock().new_line();
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}