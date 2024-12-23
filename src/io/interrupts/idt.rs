use spin::Lazy;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};
use crate::io::interrupts::{gdt, pic};
use crate::io::interrupts::pic::InterruptIndex;
use crate::{print, println};
use crate::vga_buffer::{Color, ColorCode, WRITER};

pub static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt =InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    unsafe {
        idt.double_fault.set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    };
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
    idt
});
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

pub fn init_idt() {
    IDT.load();
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame, _error_code: u64) -> !
{
    let default_color = WRITER.lock().get_color();
    WRITER.lock().set_color(*EXCEPTION_COLOR);
    WRITER.lock().new_line();
    println!("EXCEPTION: DOUBLE FAULT");
    println!("{:#?}", stack_frame);
    WRITER.lock().set_color(default_color);
    WRITER.lock().new_line();
    loop {}
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}

extern "x86-interrupt" fn timer_interrupt_handler(
    _stack_frame: InterruptStackFrame)
{
    print!(".");
    pic::notify_end_of_interrupt(&InterruptIndex::Timer);
}