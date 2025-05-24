#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;

use alloc::format;
use core::ops::DerefMut;
use core::panic::PanicInfo;
use bootloader_api::{entry_point, BootInfo};
use embedded_graphics::mono_font::ascii::FONT_10X20;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::{BinaryColor, Rgb888, RgbColor};
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::PrimitiveStyleBuilder;
use embedded_graphics::text::{Alignment, Text};
use simplos::BOOTLOADER_CONFIG;
use simplos::io::interrupts::idt::TIMER_COUNTER;
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
        let clear_style = PrimitiveStyleBuilder::new().fill_color(Rgb888::new(23,114,243)).build();
        let character_style = MonoTextStyle::new(&FONT_10X20, Rgb888::new(94, 243, 23));
        let mut val =0;
        loop{
            let bounding_box = VIDEO.lock().bounding_box();
            let text_pospos = bounding_box.center();
            let timer_counter = *TIMER_COUNTER.lock();
            bounding_box.into_styled(clear_style).draw(VIDEO.lock().deref_mut()).expect("Failed to clear screen");
            Text::with_alignment(
                format!("Update: {val}\n Timer: {timer_counter}").as_str(),
                text_pospos,
                character_style,
                Alignment::Center,
            ).draw(VIDEO.lock().deref_mut()).expect("Failed to draw text");
            VIDEO.lock().swap_buffers();
            val += 1;
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
