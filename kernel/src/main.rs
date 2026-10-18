#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;
use alloc::boxed::Box;
use alloc::sync::Arc;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::BOOTLOADER_CONFIG;
use simplos::CONSOLE;
use simplos::DISPLAY;
use simplos::pre_init;
use simplos::shell::Shell;
use simplos::thread;
use simplos::tty::Tty;
use simplos::tty::console::{self, CONSOLE_READERS};
use simplos::tty::terminal::Terminal;

entry_point!(kernel_main, config = &BOOTLOADER_CONFIG);
#[allow(unreachable_code)]
#[unsafe(no_mangle)]
fn kernel_main(boot_info: &'static mut BootInfo) -> ! {
    pre_init(boot_info);
    simplos::fs::init();
    #[cfg(test)]
    test_main();
    start();
}

/// The screen as the console's terminal, what's typed going to it, the
/// first program on it, and then the kernel's shell.
#[unsafe(no_mangle)]
fn start() -> ! {
    let mut terminal = Terminal::fitting(&DISPLAY.lock());
    let (rows, cols) = terminal.size();
    let screen = move |bytes: &[u8]| {
        terminal.write(bytes);
        terminal.draw(&mut DISPLAY.lock());
        // Its answers to programs, as if typed.
        let replies = terminal.take_replies();
        if !replies.is_empty() {
            console::answer(&replies);
        }
    };
    let tty = Arc::new(Tty::new(Box::new(screen), rows as u16, cols as u16));
    // Keys arrive from the keyboard interrupt through `CONSOLE`; the line
    // discipline, and its echo, runs here rather than in the interrupt.
    let keyboard = tty.clone();
    thread::spawn(move || {
        loop {
            CONSOLE_READERS.wait_until(|| !CONSOLE.is_empty());
            while let Some(keys) = CONSOLE.pop() {
                keyboard.input(keys.as_bytes());
            }
        }
    })
    .expect("memory for the keyboard's thread");
    simplos::process::init::run(&tty);
    Shell::new(tty).run()
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
