//! A stack overflow is caught as a double fault instead of tripling into a
//! reset.
//!
//! Overflowing the kernel stack runs off the end of its guard page. The page
//! fault that follows cannot be handled on that stack, which escalates to a
//! double fault — and handling *that* needs a stack of its own, which is why
//! the GDT sets up an interrupt stack table entry for it. This test is what
//! proves that entry is really in place: without it the double fault escalates
//! again, and a triple fault resets the machine with nothing reported.

#![feature(abi_x86_interrupt)]
#![no_std]
#![no_main]

use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::io::utils::qemu::{QemuExitCode, exit_qemu};
use simplos::{serial_print, serial_println};
use spin::Lazy;
use x86_64::structures::idt::{InterruptDescriptorTable, InterruptStackFrame};

static TEST_IDT: Lazy<InterruptDescriptorTable> = Lazy::new(|| {
    let mut idt = InterruptDescriptorTable::new();
    unsafe {
        idt.double_fault
            .set_handler_fn(test_double_fault_handler)
            .set_stack_index(simplos::io::interrupts::gdt::DOUBLE_FAULT_IST_INDEX);
    }
    idt
});

extern "x86-interrupt" fn test_double_fault_handler(
    _stack_frame: InterruptStackFrame,
    _error_code: u64,
) -> ! {
    serial_println!("[ok]");
    exit_qemu(QemuExitCode::Success);
    simplos::hlt_loop()
}

entry_point!(main, config = &simplos::BOOTLOADER_CONFIG);

fn main(_boot_info: &'static mut BootInfo) -> ! {
    serial_print!("stack_overflow::stack_overflow...\t");

    // Only the GDT, not the whole `pre_init`: the IST entry it installs is what
    // is under test, and the IDT below has to be this test's own, with nothing
    // but a double fault handler.
    simplos::io::interrupts::gdt::init_gdt();
    TEST_IDT.load();

    stack_overflow();

    panic!("Execution continued after stack overflow");
}

#[allow(unconditional_recursion)]
fn stack_overflow() {
    stack_overflow(); // for each recursion, the return address is pushed
    // A volatile read after the call, so the recursion cannot be turned into a
    // jump: a tail call would reuse the frame and never exhaust the stack.
    let marker = 0u8;
    unsafe { core::ptr::read_volatile(&raw const marker) };
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    simplos::test_panic_handler(info)
}
