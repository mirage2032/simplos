#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;
use alloc::format;
use alloc::string::ToString;
use bootloader_api::{BootInfo, entry_point};
use core::ops::DerefMut;
use core::panic::PanicInfo;
use core::sync::atomic::Ordering;
use embedded_graphics::mono_font::ascii::FONT_7X13;
// use embedded_graphics::mono_font::ascii::FONT_7X13;
// use haxorfont::FONT_HAX_ITALICRMEDIUM11;
use alloc::string::String;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Alignment, Text};
use simplos::BOOTLOADER_CONFIG;
use simplos::CONSOLE;
use simplos::DISPLAY;
use simplos::boot_mode;
use simplos::io::hpet;
use simplos::io::interrupts::idt::TIMER_COUNTER;
use simplos::pre_init;
use x86_rtc::Rtc;

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
    const BACKGROUND: Rgb888 = Rgb888::new(23, 114, 243);
    let character_style = MonoTextStyle::new(&FONT_7X13, Rgb888::new(94, 243, 23));
    let rtc = Rtc::new();
    let mut keyboard_input = String::new();

    // Read CPUID once. The answer cannot change, and the brand string lives in
    // extended leaves (0x8000_0002-4) that not every emulator implements, so it
    // must not be able to take the kernel down mid-frame.
    let cpu_id = x86::cpuid::CpuId::new();
    let cpu_brand = cpu_id
        .get_processor_brand_string()
        .map_or_else(|| String::from("unknown"), |brand| brand.as_str().to_string());
    let cpu_vendor = cpu_id
        .get_vendor_info()
        .map_or_else(|| String::from("unknown"), |vendor| vendor.as_str().to_string());
    let firmware = boot_mode().as_str();

    // Paint the background once. From here on only the status block is
    // repainted, so a frame costs a few hundred scanlines instead of the screen.
    {
        let mut display = DISPLAY.lock();
        display.clear(BACKGROUND).expect("Failed to clear screen");
        display.present();
    }

    let mut val: u64 = 0;
    let mut previous = Rectangle::new(Point::zero(), Size::zero());
    let mut last_tick = usize::MAX;

    loop {
        // Drain console messages from interrupts (keyboard input)
        let mut typed = false;
        while let Some(msg) = CONSOLE.pop() {
            keyboard_input.push_str(&msg);
            // Keep last 40 chars for display
            if keyboard_input.len() > 40 {
                keyboard_input = keyboard_input[keyboard_input.len() - 40..].to_string();
            }
            typed = true;
        }

        // One repaint per timer tick, and halt in between. Unthrottled this loop
        // redraws as fast as the CPU allows, which is merely wasteful on real
        // hardware and ruinous when the CPU is emulated.
        let tick = TIMER_COUNTER.load(Ordering::Relaxed);
        if tick == last_tick && !typed {
            x86_64::instructions::hlt();
            continue;
        }
        last_tick = tick;

        let time = rtc.get_unix_timestamp();
        let hpet_ms = hpet::elapsed_millis().unwrap_or(0);
        let hpet_secs = hpet_ms / 1000;
        let hpet_frac = hpet_ms % 1000;
        let status = format!(
            "\
            Update: {val}\n\
            Boot: {firmware}\n\
            Timer: {tick}\n\
            RTC: {time}\n\
            HPET: {hpet_secs}.{hpet_frac:03}\n\
            CPU: {cpu_brand}\n\
            Vendor: {cpu_vendor}\n\
            \n\
            Keyboard: {keyboard_input}\n\
            "
        );

        {
            let mut display = DISPLAY.lock();
            let center = display.bounding_box().center();
            let text =
                Text::with_alignment(status.as_str(), center, character_style, Alignment::Center);
            // Erase only what the last frame drew, then draw and blit the union.
            display.fill_rect(&previous, BACKGROUND);
            previous = text.bounding_box();
            text.draw(display.deref_mut()).expect("Failed to draw text");
            display.present();
        }

        val += 1;
    }
}

// Under `cargo test` the failure has to reach the host, which is what
// `test_panic_handler` does: report over serial, then tell QEMU to exit with a
// failing status.
#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    simplos::test_panic_handler(info)
}
#[cfg(not(test))]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    // Say something. A silent `loop {}` here means every boot failure looks
    // identical from the outside: a pegged CPU and a blank screen. Interrupts
    // off first, and a lock-free writer, so this can't deadlock on a lock the
    // panicking code already holds.
    x86_64::instructions::interrupts::disable();
    simplos::serial::panic_print(format_args!("\n*** KERNEL PANIC ***\n{info}\n"));
    simplos::hlt_loop();
}

// No `#[test_case]`s here on purpose. What this binary does — the render loop —
// needs a screen to be worth asserting about; that it boots at all is covered by
// tests/basic_boot.rs.
