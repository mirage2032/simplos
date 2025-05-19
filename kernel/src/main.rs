#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;

use alloc::format;
use alloc::string::ToString;
use core::panic::PanicInfo;
use bootloader_api::{entry_point, BootInfo};
use embedded_graphics::pixelcolor::{Rgb888};
#[allow(unused_imports)]
use simplos::BOOTLOADER_CONFIG;
use simplos::io::video::VIDEO;
use simplos::pre_init;

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);
#[allow(unreachable_code)]
#[unsafe(no_mangle)]
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    pre_init(boot_info);
    #[cfg(test)]
    test_main();
    start();
}

#[unsafe(no_mangle)]
fn start() -> ! {
    {
        let mut v = VIDEO.lock();
        let mut val = 0;
        loop {
            if let Some(ref mut buffers) = v.buffers {
                buffers.framebuffer.lock().clear(&Rgb888::new(val, val, val));
            }
            val = val.wrapping_add(1);
        }
    }
    // let ptr = 0xdeadbeaf as *mut u8;
    // unsafe { *ptr = 42; }
    // WRITER.lock().set_color(ColorCode::new(Color::Yellow, Color::Black));
    // WRITER.lock().clear();
    // println!("Hello, init!");
    // WRITER.lock().set_color(ColorCode::new(Color::White, Color::Red));
    // panic!("Some panic message");
    simplos::hlt_loop();
}

#[cfg(test)]
#[panic_handler]
#[allow(unreachable_code)]
fn panic(info: &PanicInfo) -> ! {
    use simplos::io::utils::qemu::{exit_qemu, QemuExitCode};
    // serial_println!("[failed]\n");
    // serial_println!("Error: {}\n", info);
    exit_qemu(QemuExitCode::Failed);
    loop {}
}
#[cfg(not(test))]
#[panic_handler]
#[allow(unreachable_code)]
fn panic(info: &PanicInfo) -> ! {
    // let title_color = ColorCode::new(Color::Yellow, Color::Red);
    // let message_color = ColorCode::new(Color::Yellow, Color::DarkGray);
    // WRITER.lock().set_color(title_color);
    // WRITER.lock().new_line();
    // println!("Kernel panic!");
    // WRITER.lock().set_color(message_color);
    // WRITER.lock().new_line();
    // println!("{}", info);
    loop {}
}

#[test_case]
fn trivial_assertion() {
    assert_eq!(1, 1);
}
