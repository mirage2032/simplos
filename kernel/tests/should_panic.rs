//! A failing assertion really does panic, and the panic really does stop the
//! kernel. The whole test suite's idea of "failed" depends on it.
//!
//! There is no harness here (`harness = false` in Cargo.toml): a panic is the
//! expected outcome, so the panic handler is the one that reports success, and
//! reaching the end of the run is the failure.

#![no_std]
#![no_main]

use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::io::utils::qemu::{QemuExitCode, exit_qemu};
use simplos::{serial_print, serial_println};

entry_point!(main, config = &simplos::BOOTLOADER_CONFIG);

// Deliberately without `pre_init`: the only panic that should be able to reach
// the handler below is the one this test causes. Serial output needs no setup.
fn main(_boot_info: &'static mut BootInfo) -> ! {
    should_fail();
    serial_println!("[test did not panic]");
    exit_qemu(QemuExitCode::Failed);
    simplos::hlt_loop()
}

fn should_fail() {
    serial_print!("should_panic::should_fail...\t");
    assert_eq!(0, 1);
}

#[panic_handler]
fn panic(_info: &PanicInfo) -> ! {
    serial_println!("[ok]");
    exit_qemu(QemuExitCode::Success);
    simplos::hlt_loop()
}
