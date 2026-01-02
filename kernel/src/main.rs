#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;
use alloc::format;
use alloc::string::ToString;
use core::ops::DerefMut;
use core::panic::PanicInfo;
use core::sync::atomic::Ordering;
use bootloader_api::{entry_point, BootInfo};
use embedded_graphics::mono_font::ascii::FONT_7X13;
// use embedded_graphics::mono_font::ascii::FONT_7X13;
// use haxorfont::FONT_HAX_ITALICRMEDIUM11;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::{Rgb888};
use embedded_graphics::prelude::*;
use embedded_graphics::text::{Alignment, Text};
use alloc::string::String;
use simplos::BOOTLOADER_CONFIG;
use simplos::DISPLAY;
use simplos::CONSOLE;
use simplos::io::interrupts::idt::TIMER_COUNTER;
use simplos::io::hpet;
use simplos::pre_init;
use x86_rtc::Rtc;
use x86;
use simplos::badoo;

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
        let character_style = MonoTextStyle::new(&FONT_7X13, Rgb888::new(94, 243, 23));
        let mut val = 0;
        let rtc = Rtc::new();
        let mut keyboard_input = String::new();
        
        loop {
            // Drain console messages from interrupts (keyboard input)
            while let Some(msg) = CONSOLE.pop() {
                keyboard_input.push_str(&msg);
                // Keep last 40 chars for display
                if keyboard_input.len() > 40 {
                    keyboard_input = keyboard_input[keyboard_input.len() - 40..].to_string();
                }
            }
            
            let bounding_box = DISPLAY.lock().bounding_box();
            let text_pospos = bounding_box.center();
            let timer_counter = TIMER_COUNTER.load(Ordering::Relaxed);
            DISPLAY.lock().clear(Rgb888::new(23, 114, 243)).expect("Failed to clear screen");
            let time = rtc.get_unix_timestamp();
            let hpet_ms = hpet::elapsed_millis().unwrap_or(0);
            let hpet_secs = hpet_ms / 1000;
            let hpet_frac = hpet_ms % 1000;
            let cpu_id = x86::cpuid::CpuId::new();
            let cpu_brand = cpu_id.get_processor_brand_string().expect("Failed to get cpu brand string").as_str().to_string();
            let cpu_vendor = cpu_id.get_vendor_info().expect("No vendor info for CPU").as_str().to_string();
            
            Text::with_alignment(
                format!("\
                Update: {val}\n\
                Timer: {timer_counter}\n\
                RTC: {time}\n\
                HPET: {hpet_secs}.{hpet_frac:03}\n\
                CPU: {cpu_brand}\n\
                Vendor: {cpu_vendor}\n\
                \n\
                Keyboard: {keyboard_input}\n\
                ").as_str(),
                text_pospos,
                character_style,
                Alignment::Center,
            ).draw(DISPLAY.lock().deref_mut()).expect("Failed to draw text");
            
            // Present back buffer to screen
            DISPLAY.lock().present();
            
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
