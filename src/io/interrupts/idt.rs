use spin::{Lazy, Mutex};
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
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
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

extern "x86-interrupt" fn keyboard_interrupt_handler(
    _stack_frame: InterruptStackFrame)
{
    use x86_64::instructions::port::Port;
    use pc_keyboard::{layouts, DecodedKey, HandleControl, Keyboard, ScancodeSet1};

    static KEYBOARD: Lazy<Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>>> = Lazy::new(|| {
        Mutex::new(Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore))
    });
    let mut keyboard = KEYBOARD.lock(); 
    let mut port = Port::new(0x60);
    let scancode: u8 = unsafe { port.read() };
    if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
        if let Some(key) = keyboard.process_keyevent(key_event) {
            match key {
                DecodedKey::Unicode(character) => print!("{}", character),
                DecodedKey::RawKey(key) => print!("{:?}", key),
            }
        }
    }
    pic::notify_end_of_interrupt(&InterruptIndex::Keyboard);
}