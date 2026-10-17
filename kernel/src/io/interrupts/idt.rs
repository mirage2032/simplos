use crate::io::interrupts::pic::InterruptIndex;
use crate::io::interrupts::{gdt, pic};
use crate::io::ps2::PS2_CONTROLLER;
use crate::process::{self, Registers};
use crate::signal::{SIGBUS, SIGFPE, SIGILL, SIGSEGV, SIGTRAP};
use crate::{serial_println, thread};
use core::fmt;
use core::sync::atomic::{AtomicUsize, Ordering};
use spin::{Lazy, Mutex};
use x86_64::PrivilegeLevel;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame, PageFaultErrorCode};
pub static IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    idt.breakpoint.set_handler_fn(breakpoint_handler);
    idt.page_fault.set_handler_fn(page_fault_handler);
    // The faults a user program can cause. Without a handler each would
    // become a double fault, and one program could stop the machine.
    idt.divide_error.set_handler_fn(divide_error_handler);
    idt.debug.set_handler_fn(debug_handler);
    idt.invalid_opcode.set_handler_fn(invalid_opcode_handler);
    idt.x87_floating_point.set_handler_fn(x87_floating_point_handler);
    idt.simd_floating_point.set_handler_fn(simd_floating_point_handler);
    idt.segment_not_present.set_handler_fn(segment_not_present_handler);
    idt.stack_segment_fault.set_handler_fn(stack_segment_fault_handler);
    idt.general_protection_fault.set_handler_fn(general_protection_fault_handler);
    idt.alignment_check.set_handler_fn(alignment_check_handler);
    unsafe {
        idt.double_fault
            .set_handler_fn(double_fault_handler)
            .set_stack_index(gdt::DOUBLE_FAULT_IST_INDEX);
    };
    // Safety: `timer_entry` is an interrupt handler, saving what it uses.
    unsafe {
        let entry = timer_entry as *const () as u64;
        idt[InterruptIndex::Timer.as_u8()].set_handler_addr(x86_64::VirtAddr::new(entry));
    }
    idt[InterruptIndex::Keyboard.as_u8()].set_handler_fn(keyboard_interrupt_handler);
    // What an 8259 sends on its last line, masked or not, when an interrupt
    // went before it was taken. With no handler, a fault: real hardware's
    // halted the kernel, or ended whichever program was running.
    idt[InterruptIndex::Parallel1.as_u8()].set_handler_fn(spurious_at_master);
    idt[InterruptIndex::SecondaryAta.as_u8()].set_handler_fn(spurious_at_slave);
    idt
});
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

/// What `timer_entry` saves, from its last push up: every general register,
/// then what the CPU pushed. A signal handler for the program it interrupted
/// needs them all, to put back once it's done.
#[repr(C)]
struct TrapFrame {
    r15: u64,
    r14: u64,
    r13: u64,
    r12: u64,
    r11: u64,
    r10: u64,
    r9: u64,
    r8: u64,
    rbp: u64,
    rdi: u64,
    rsi: u64,
    rdx: u64,
    rcx: u64,
    rbx: u64,
    rax: u64,
    rip: u64,
    cs: u64,
    rflags: u64,
    rsp: u64,
    ss: u64,
}

impl TrapFrame {
    fn registers(&self) -> Registers {
        Registers {
            rax: self.rax,
            rbx: self.rbx,
            rcx: self.rcx,
            rdx: self.rdx,
            rsi: self.rsi,
            rdi: self.rdi,
            rbp: self.rbp,
            r8: self.r8,
            r9: self.r9,
            r10: self.r10,
            r11: self.r11,
            r12: self.r12,
            r13: self.r13,
            r14: self.r14,
            r15: self.r15,
            rip: self.rip,
            rflags: self.rflags,
            rsp: self.rsp,
        }
    }
}

/// The timer interrupt's entry. The CPU's push of five words onto a 16-byte
/// aligned stack, then fifteen here, leave it aligned for the call.
#[unsafe(naked)]
unsafe extern "C" fn timer_entry() {
    core::arch::naked_asm!(
        "cld",
        "push rax",
        "push rbx",
        "push rcx",
        "push rdx",
        "push rsi",
        "push rdi",
        "push rbp",
        "push r8",
        "push r9",
        "push r10",
        "push r11",
        "push r12",
        "push r13",
        "push r14",
        "push r15",
        "mov rdi, rsp",
        "call {handler}",
        "pop r15",
        "pop r14",
        "pop r13",
        "pop r12",
        "pop r11",
        "pop r10",
        "pop r9",
        "pop r8",
        "pop rbp",
        "pop rdi",
        "pop rsi",
        "pop rdx",
        "pop rcx",
        "pop rbx",
        "pop rax",
        "iretq",
        handler = sym timer_interrupt_handler,
    );
}

extern "sysv64" fn timer_interrupt_handler(frame: &TrapFrame) {
    let now = TIMER_COUNTER.fetch_add(1, Ordering::Relaxed) as u64 + 1;
    crate::check_test_deadline(now);
    crate::process::charge_tick(frame.cs & 3 == 3);
    pic::notify_end_of_interrupt(&InterruptIndex::Timer);
    // After the end-of-interrupt: this thread may not get back here for a
    // while, and the PIC sends no more ticks until the last one is acknowledged.
    crate::thread::timer_tick();
    // Back to a program, which may have been sent a signal meanwhile.
    if frame.cs & 3 == 3 {
        crate::signal::on_interrupt_return(|| frame.registers());
    }
}
/// Keys go to `CONSOLE` as the Linux console sends them: Enter as `\r`,
/// Backspace as DEL, Ctrl and a letter as its control character, and the
/// arrows, the keys above them and the function keys as escape sequences.
extern "x86-interrupt" fn keyboard_interrupt_handler(_stack_frame: InterruptStackFrame) {
    use crate::io::console::{CONSOLE, CONSOLE_READERS};
    use pc_keyboard::{DecodedKey, HandleControl, KeyCode, Keyboard, ScancodeSet1, layouts};

    static KEYBOARD: Lazy<Mutex<Keyboard<layouts::Us104Key, ScancodeSet1>>> = Lazy::new(|| {
        let control = HandleControl::MapLettersToUnicode;
        Mutex::new(Keyboard::new(ScancodeSet1::new(), layouts::Us104Key, control))
    });

    if let Some(controller) = PS2_CONTROLLER.lock().as_mut()
        && controller.output_has_data()
    {
        let mut keyboard = KEYBOARD.lock();
        if let Ok(scancode) = controller.controller_mut().read_data()
            && let Ok(Some(key_event)) = keyboard.add_byte(scancode)
            && let Some(key) = keyboard.process_keyevent(key_event)
        {
            // The rest of what Ctrl makes control characters of, as on a
            // terminal: ^@, ^[, ^\, ^], ^^ and ^_.
            let key = match key {
                DecodedKey::Unicode(c) if keyboard.get_modifiers().is_ctrl() => {
                    DecodedKey::Unicode(match c {
                        '2' | '@' | ' ' => '\0',
                        '[' => '\x1b',
                        '\\' => '\x1c',
                        ']' => '\x1d',
                        '6' | '^' => '\x1e',
                        '-' | '_' => '\x1f',
                        c => c,
                    })
                }
                key => key,
            };
            let mut utf8 = [0; 4];
            let sent = match key {
                DecodedKey::Unicode('\n') => "\r",
                DecodedKey::Unicode('\u{8}') => "\u{7f}",
                // The Delete key.
                DecodedKey::Unicode('\u{7f}') => "\x1b[3~",
                DecodedKey::Unicode(c) => c.encode_utf8(&mut utf8),
                DecodedKey::RawKey(KeyCode::ArrowUp) => "\x1b[A",
                DecodedKey::RawKey(KeyCode::ArrowDown) => "\x1b[B",
                DecodedKey::RawKey(KeyCode::ArrowRight) => "\x1b[C",
                DecodedKey::RawKey(KeyCode::ArrowLeft) => "\x1b[D",
                DecodedKey::RawKey(KeyCode::Home) => "\x1b[1~",
                DecodedKey::RawKey(KeyCode::Insert) => "\x1b[2~",
                DecodedKey::RawKey(KeyCode::End) => "\x1b[4~",
                DecodedKey::RawKey(KeyCode::PageUp) => "\x1b[5~",
                DecodedKey::RawKey(KeyCode::PageDown) => "\x1b[6~",
                DecodedKey::RawKey(KeyCode::F1) => "\x1b[[A",
                DecodedKey::RawKey(KeyCode::F2) => "\x1b[[B",
                DecodedKey::RawKey(KeyCode::F3) => "\x1b[[C",
                DecodedKey::RawKey(KeyCode::F4) => "\x1b[[D",
                DecodedKey::RawKey(KeyCode::F5) => "\x1b[[E",
                DecodedKey::RawKey(KeyCode::F6) => "\x1b[17~",
                DecodedKey::RawKey(KeyCode::F7) => "\x1b[18~",
                DecodedKey::RawKey(KeyCode::F8) => "\x1b[19~",
                DecodedKey::RawKey(KeyCode::F9) => "\x1b[20~",
                DecodedKey::RawKey(KeyCode::F10) => "\x1b[21~",
                DecodedKey::RawKey(KeyCode::F11) => "\x1b[23~",
                DecodedKey::RawKey(KeyCode::F12) => "\x1b[24~",
                // Shift and the like, which only change other keys.
                DecodedKey::RawKey(_) => "",
            };
            if !sent.is_empty() {
                CONSOLE.push(format_args!("{sent}"));
                CONSOLE_READERS.wake_all();
            }
        }
    }
    pic::notify_end_of_interrupt(&InterruptIndex::Keyboard);
}
/// A fault in a user program ends that program, as the signal Linux would
/// have sent it would. In the kernel it is a bug: say what and where, and
/// stop, rather than halt silently and look like a freeze.
fn fault(frame: &InterruptStackFrame, what: fmt::Arguments, signal: usize) -> ! {
    if frame.code_segment.rpl() == PrivilegeLevel::Ring3 {
        let id = thread::current_process().map_or(0, |process| process.id);
        serial_println!(
            "simplos: process {}: {} at {:#x}, ended",
            id,
            what,
            frame.instruction_pointer.as_u64()
        );
        process::kill_current(signal);
    }
    crate::serial::panic_print(format_args!("\n*** EXCEPTION: {what} ***\n{frame:#?}\n"));
    crate::hlt_loop();
}

/// A program's stack, reaching a page it hasn't yet, gets it, and goes on.
/// Any other page fault is fatal: to a user program, or to the kernel.
extern "x86-interrupt" fn page_fault_handler(
    stack_frame: InterruptStackFrame,
    error_code: PageFaultErrorCode,
) {
    let address = x86_64::registers::control::Cr2::read_raw();
    if stack_frame.code_segment.rpl() == PrivilegeLevel::Ring3
        && !error_code.contains(PageFaultErrorCode::PROTECTION_VIOLATION)
        && process::grow_stack(address)
    {
        return;
    }
    let what = format_args!("page fault at {address:#x} ({error_code:?})");
    fault(&stack_frame, what, SIGSEGV)
}

extern "x86-interrupt" fn divide_error_handler(frame: InterruptStackFrame) {
    fault(&frame, format_args!("divide error"), SIGFPE)
}

extern "x86-interrupt" fn debug_handler(frame: InterruptStackFrame) {
    // A program can single-step itself by setting the trap flag.
    fault(&frame, format_args!("debug trap"), SIGTRAP)
}

extern "x86-interrupt" fn invalid_opcode_handler(frame: InterruptStackFrame) {
    fault(&frame, format_args!("invalid opcode"), SIGILL)
}

extern "x86-interrupt" fn x87_floating_point_handler(frame: InterruptStackFrame) {
    fault(&frame, format_args!("x87 floating-point exception"), SIGFPE)
}

extern "x86-interrupt" fn simd_floating_point_handler(frame: InterruptStackFrame) {
    fault(&frame, format_args!("SIMD floating-point exception"), SIGFPE)
}

extern "x86-interrupt" fn segment_not_present_handler(frame: InterruptStackFrame, error: u64) {
    fault(&frame, format_args!("segment not present ({error:#x})"), SIGBUS)
}

extern "x86-interrupt" fn stack_segment_fault_handler(frame: InterruptStackFrame, error: u64) {
    fault(&frame, format_args!("stack segment fault ({error:#x})"), SIGBUS)
}

extern "x86-interrupt" fn general_protection_fault_handler(frame: InterruptStackFrame, error: u64) {
    fault(&frame, format_args!("general protection fault ({error:#x})"), SIGSEGV)
}

extern "x86-interrupt" fn alignment_check_handler(frame: InterruptStackFrame, error: u64) {
    fault(&frame, format_args!("alignment check ({error:#x})"), SIGBUS)
}

/// The master's spurious interrupt: not one, so not ended either.
extern "x86-interrupt" fn spurious_at_master(_stack_frame: InterruptStackFrame) {}

/// The slave's: the master, which passed it on, still has to be told it's
/// done with it.
extern "x86-interrupt" fn spurious_at_slave(_stack_frame: InterruptStackFrame) {
    pic::end_at_master();
}
