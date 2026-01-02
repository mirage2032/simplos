use crate::io::interrupts::pic::InterruptIndex;
use crate::io::interrupts::{gdt, pic};
use crate::io::ps2::{Ps2InterruptCause, PS2_CONTROLLER};
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Lazy, Mutex};
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
pub static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    };
    idt[InterruptIndex::Timer.as_u8()].set_handler_fn(timer_interrupt_handler);
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
    idt[InterruptIndex::Ps2Mouse.as_u8()].set_handler_fn(mouse_interrupt_handler);
    idt
});
// const EXCEPTION_COLOR: Lazy<ColorCode> =
//     Lazy::new(|| ColorCode::new(Color::LightRed, Color::Black));
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    // let default_color = WRITER.lock().get_color();
    // WRITER.lock().set_color(*EXCEPTION_COLOR);
    // WRITER.lock().new_line();
    // println!("EXCEPTION: BREAKPOINT");
    // println!("{:#?}", stack_frame);
    // WRITER.lock().set_color(default_color);
    // WRITER.lock().new_line();
    panic!("EXCEPTION: BREAKPOINT");
}

pub fn init_idt() {
    IDT.load();
}

extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    // let default_color = WRITER.lock().get_color();
    // WRITER.lock().set_color(*EXCEPTION_COLOR);
    // WRITER.lock().new_line();
    // println!("EXCEPTION: DOUBLE FAULT");
    // println!("{:#?}", stack_frame);
    // WRITER.lock().set_color(default_color);
    // WRITER.lock().new_line();
    loop {}
}

#[test_case]
fn test_breakpoint_exception() {
    // invoke a breakpoint exception
    x86_64::instructions::interrupts::int3();
}

pub static TIMER_COUNTER: AtomicUsize = AtomicUsize::new(0);

extern "x86-interrupt" fn timer_interrupt_handler(_stack_frame: InterruptStackFrame) {
    TIMER_COUNTER.fetch_add(1, Ordering::Relaxed);
    pic::notify_end_of_interrupt(&InterruptIndex::Timer);
}
extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use pc_keyboard::{layouts, HandleControl, Keyboard, ScancodeSet1};

    // x86_64::instructions::interrupts::without_interrupts(|| {
    static KEYBOARD: Lazy<Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>>> = Lazy::new(|| {
        Mutex::new(Keyboard::new(
            ScancodeSet1::new(),
            layouts::Us104Key,
            HandleControl::Ignore,
        ))
    });
    let mut controller = PS2_CONTROLLER.lock();
    let data_available = controller.output_has_data();
    let from_keyboard = controller.interrupt_cause() == Ps2InterruptCause::Keyboard;
    if data_available && from_keyboard {
        let mut keyboard = KEYBOARD.lock();
        if let Ok(scancode) = controller.controller_mut().read_data() {
            if let Ok(Some(key_event)) = keyboard.add_byte(scancode) {
                if let Some(key) = keyboard.process_keyevent(key_event) {
                    // match key {
                    //     DecodedKey::Unicode(character) => print!("{}", character),
                    //     DecodedKey::RawKey(key) => print!("{:?}", key),
                    // }
                }
            }
        }
    }
    pic::notify_end_of_interrupt(&InterruptIndex::Keyboard);
    // });
}
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    // println!("EXCEPTION: PAGE FAULT");
    // println!("Accessed Address: {:?}", Cr2::read());
    // println!("Error Code: {:?}", error_code);
    // println!("{:#?}", stack_frame);
    crate::hlt_loop();
}
extern "x86-interrupt" fn mouse_interrupt_handler(stack_frame: InterruptStackFrame) {
    // x86_64::instructions::interrupts::without_interrupts(|| {
    // let writer_color = WRITER.lock().get_color();
    // WRITER.lock().set_color(*EXCEPTION_COLOR);
    let mut controller = PS2_CONTROLLER.lock();
    let data_available = controller.output_has_data();
    let from_mouse = controller.interrupt_cause() == Ps2InterruptCause::Mouse;
    if data_available && from_mouse {
        match controller.controller_mut().mouse().read_data_packet() {
            Ok((flags, x, y)) => {
                // println!("x:{},y:{}", x, y);
            }
            Err(err) => {
                // FIXME: WHY also getting a bad response
                // println!("DAFUQ:{:?}", err);
            }
        }
    }
    // WRITER.lock().set_color(writer_color);
    pic::notify_end_of_interrupt(&InterruptIndex::Ps2Mouse);
    // });
}
