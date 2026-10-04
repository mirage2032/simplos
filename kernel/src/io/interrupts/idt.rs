use crate::io::interrupts::pic::InterruptIndex;
use crate::io::interrupts::{gdt, pic};
use crate::io::ps2::{PS2_CONTROLLER, Ps2InterruptCause};
use crate::serial_println;
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
/// A breakpoint is a trap, not a fault: `int3` is how a debugger (or a test)
/// asks to be noticed, and execution continues at the instruction after it.
/// Reporting it used to go to a VGA text console that no longer exists, and in
/// the meantime it had become a `panic!` — which made every `int3` fatal.
extern "x86-interrupt" fn breakpoint_handler(stack_frame: InterruptStackFrame) {
    serial_println!("EXCEPTION: BREAKPOINT\n{:#?}", stack_frame);
}

pub fn init_idt() {
    IDT.load();
}

/// Unrecoverable: the CPU could not deliver an earlier exception, and if this
/// handler faults in turn the machine triple faults and resets with nothing
/// reported. `panic_print` takes no locks for that reason.
extern "x86-interrupt" fn double_fault_handler(
    stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    crate::serial::panic_print(format_args!(
        "\n*** EXCEPTION: DOUBLE FAULT ***\n{stack_frame:#?}\n"
    ));
    crate::hlt_loop();
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
    use crate::io::console::CONSOLE;
    use alloc::format;
    use pc_keyboard::{DecodedKey, HandleControl, Keyboard, ScancodeSet1, layouts};

    static KEYBOARD: Lazy<Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>>> = Lazy::new(|| {
        Mutex::new(Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, HandleControl::Ignore))
    });

    let mut controller = PS2_CONTROLLER.lock();
    let data_available = controller.output_has_data();
    let from_keyboard = controller.interrupt_cause() == Ps2InterruptCause::Keyboard;
    if data_available && from_keyboard {
        let mut keyboard = KEYBOARD.lock();
        if let Ok(scancode) = controller.controller_mut().read_data()
            && let Ok(Some(key_event)) = keyboard.add_byte(scancode)
            && let Some(key) = keyboard.process_keyevent(key_event)
        {
            // Log key to console buffer (interrupt-safe!)
            match key {
                DecodedKey::Unicode(c) => {
                    CONSOLE.push(&format!("{}", c));
                }
                DecodedKey::RawKey(k) => {
                    CONSOLE.push(&format!("{:?}", k));
                }
            }
        }
    }
    pic::notify_end_of_interrupt(&InterruptIndex::Keyboard);
}
/// Also unrecoverable as things stand — nothing here grows a mapping on demand —
/// so the useful thing is to say which address was touched and stop, rather than
/// halt silently and look like a freeze.
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    crate::serial::panic_print(format_args!(
        "\n*** EXCEPTION: PAGE FAULT ***\naddress: {:?}\nerror: {:?}\n{:#?}\n",
        x86_64::registers::control::Cr2::read(),
        error_code,
        stack_frame
    ));
    crate::hlt_loop();
}
extern "x86-interrupt" fn mouse_interrupt_handler(_stack_frame: InterruptStackFrame) {
    let mut controller = PS2_CONTROLLER.lock();
    let data_available = controller.output_has_data();
    let from_mouse = controller.interrupt_cause() == Ps2InterruptCause::Mouse;
    if data_available && from_mouse {
        // The packet has to be read whether or not anything wants it: leaving a
        // byte in the output buffer stops the controller raising further
        // interrupts. Nothing moves a cursor yet, so it is read and dropped.
        let _ = controller.controller_mut().mouse().read_data_packet();
    }
    pic::notify_end_of_interrupt(&InterruptIndex::Ps2Mouse);
    // });
}
