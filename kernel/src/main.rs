#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

use core::panic::PanicInfo;
use bootloader_api::{entry_point, BootInfo};
#[allow(unused_imports)]
use simplos::{print, println, serial_print, serial_println};
use simplos::BOOTLOADER_CONFIG;
use simplos::io::video::vga_buffer::{Color, ColorCode, WRITER};

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);
#[allow(unreachable_code)]
#[unsafe(no_mangle)]
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    simplos::init();
    #[cfg(test)]
    test_main();
    start(boot_info);
}

#[unsafe(no_mangle)]
fn start(boot_info: &'static mut BootInfo) -> ! {
    // let ptr = 0xdeadbeaf as *mut u8;
    // unsafe { *ptr = 42; }
    WRITER.lock().set_color(ColorCode::new(Color::Yellow, Color::Black));
    // WRITER.lock().clear();
    println!("Hello, World!");
    WRITER.lock().set_color(ColorCode::new(Color::White, Color::Red));
    // panic!("Some panic message");
    simplos::hlt_loop();
}

#[cfg(test)]
#[panic_handler]
#[allow(unreachable_code)]
fn panic(info: &PanicInfo) -> ! {
    use simplos::io::utils::qemu::{exit_qemu, QemuExitCode};
    serial_println!("[failed]\n");
    serial_println!("Error: {}\n", info);
    exit_qemu(QemuExitCode::Failed);
    loop {}
}
#[cfg(not(test))]
#[panic_handler]
#[allow(unreachable_code)]
fn panic(info: &PanicInfo) -> ! {
    let title_color = ColorCode::new(Color::Yellow, Color::Red);
    let message_color = ColorCode::new(Color::Yellow, Color::DarkGray);
    WRITER.lock().set_color(title_color);
    WRITER.lock().new_line();
    println!("Kernel panic!");
    WRITER.lock().set_color(message_color);
    WRITER.lock().new_line();
    println!("{}", info);
    loop {}
}

#[test_case]
fn trivial_assertion() {
    assert_eq!(1, 1);
}
