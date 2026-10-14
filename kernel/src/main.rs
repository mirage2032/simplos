#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;
use alloc::format;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_7X13;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Baseline, Text};
use simplos::BOOTLOADER_CONFIG;
use simplos::CONSOLE;
use simplos::DISPLAY;
use simplos::io::video::Display;
use simplos::pre_init;
use simplos::shell::{PROMPT, Redraw, Shell};

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
    let (rows, cols) = {
        let size = DISPLAY.lock().bounding_box().size;
        let text = |px: u32, cell: u32| (px.saturating_sub(2 * MARGIN as u32) / cell) as usize;
        (text(size.height, LINE_HEIGHT as u32), text(size.width, CHAR_WIDTH as u32))
    };
    let mut shell = Shell::new(rows, cols);
    let mut redraw = Redraw::All;

    loop {
        // Keys arrive from the keyboard interrupt through `CONSOLE`.
        while let Some(msg) = CONSOLE.pop() {
            for c in msg.chars() {
                redraw = redraw.max(shell.key(c));
            }
        }

        // Nothing changed: sleep until the next interrupt rather than spin,
        // which is ruinous when the CPU is emulated.
        if redraw == Redraw::None {
            x86_64::instructions::hlt();
            continue;
        }

        let mut display = DISPLAY.lock();
        let input = format!("{PROMPT}{}_", shell.line);
        if redraw == Redraw::All {
            display.clear(BACKGROUND).expect("Failed to clear screen");
            for (row, line) in shell.history.iter().enumerate() {
                draw_line(&mut display, row, line, cols);
            }
        }
        draw_line(&mut display, shell.history.len(), &input, cols);
        display.present();
        redraw = Redraw::None;
    }
}

const BACKGROUND: Rgb888 = Rgb888::new(23, 114, 243);
const FOREGROUND: Rgb888 = Rgb888::new(94, 243, 23);
const MARGIN: i32 = 8;
const LINE_HEIGHT: i32 = 13;
const CHAR_WIDTH: i32 = 7;

/// Paint one text row, erasing what was there. Text past `cols` is cut off.
fn draw_line(display: &mut Display, row: usize, text: &str, cols: usize) {
    let y = MARGIN + row as i32 * LINE_HEIGHT;
    let width = display.bounding_box().size.width;
    display.fill_rect(
        &Rectangle::new(Point::new(0, y), Size::new(width, LINE_HEIGHT as u32)),
        BACKGROUND,
    );
    let text = text.char_indices().nth(cols).map_or(text, |(end, _)| &text[..end]);
    let style = MonoTextStyle::new(&FONT_7X13, FOREGROUND);
    Text::with_baseline(text, Point::new(MARGIN, y), style, Baseline::Top)
        .draw(display)
        .expect("Failed to draw text");
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

// No `#[test_case]`s here on purpose. What this binary does — drawing the shell —
// needs a screen to be worth asserting about; that it boots at all is covered by
// tests/basic_boot.rs.
