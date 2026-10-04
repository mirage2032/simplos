//! The kernel boots and hands control to the tests.
//!
//! Everything else in `tests/` presumes this: that the bootloader's handoff,
//! `pre_init` and the framebuffer all work. When this fails, nothing else is
//! worth reading.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::{BootMode, DISPLAY};

// Not a bare `_start`: the bootloader reads the kernel's configuration from a
// `.bootloader-config` ELF section and refuses to load an image without one,
// and `entry_point!` is what emits it.
entry_point!(main, config = &simplos::BOOTLOADER_CONFIG);

fn main(boot_info: &'static mut BootInfo) -> ! {
    simplos::pre_init(boot_info);
    test_main();
    simplos::hlt_loop()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    simplos::test_panic_handler(info)
}

#[test_case]
fn display_is_initialized() {
    let display = DISPLAY.lock();
    assert!(display.is_initialized(), "pre_init left the display without a framebuffer");
}

#[test_case]
fn framebuffer_is_not_empty() {
    let area = {
        use embedded_graphics::geometry::Dimensions;
        DISPLAY.lock().bounding_box().size
    };
    assert!(area.width > 0 && area.height > 0, "framebuffer is {}x{}", area.width, area.height);
}

#[test_case]
fn boot_mode_was_detected() {
    // Whichever firmware ran, the memory map should have told us which it was —
    // `Unknown` means the detection no longer recognises either.
    assert_ne!(simplos::boot_mode(), BootMode::Unknown);
}
