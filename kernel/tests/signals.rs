//! Signals: user/probe sends them to itself and to copies of itself, with
//! handlers and without, blocked and not, as programs on Linux do.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::process;
use spin::Mutex;

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

/// Run probe from the ramdisk with `args`: its exit status, and everything
/// it and its copies wrote.
fn run(args: &[&str]) -> (i32, String) {
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let program = process::spawn(elf, &argv, move |bytes| sink.lock().extend_from_slice(bytes));
    let status = program.expect("probe loads").wait();
    let output = output.lock().clone();
    (status, String::from_utf8(output).unwrap())
}

fn ok(text: &str) -> (i32, String) {
    (0, String::from(text))
}

#[test_case]
fn a_program_handles_blocks_and_ignores_signals() {
    // A handler, run before kill returns, told the sender and SI_USER; a
    // blocked signal pending, then taken as it's unblocked; tkill's SI_TKILL,
    // and SA_RESETHAND's handler gone after once; an ignored one dropped;
    // SIGKILL neither caught nor blocked; and EINVAL, ESRCH, and 0 for
    // signal 0.
    assert_eq!(
        run(&["signals"]),
        ok("0 0 (1, 10) true 0; true 1 2; -6 0 3; -22 false (-22, -3, 0)\n")
    );
}

#[test_case]
fn a_signal_comes_to_a_program_that_never_calls_and_takes_nothing() {
    // Through the timer interrupt; and the handler, which changes every
    // register it may, leaves the program's as they were.
    assert_eq!(run(&["signal-registers"]), ok("exit status: 0\n"));
}

#[test_case]
fn a_signal_ends_a_wait_or_starts_it_again() {
    // A read, EINTR; with SA_RESTART, read again and given what came; a
    // sleep, EINTR, with what was left of five seconds.
    assert_eq!(run(&["interrupted"]), ok("-4 1 -4 true\n"));
}

#[test_case]
fn signals_end_programs_unless_ignored() {
    assert_eq!(run(&["endings"]), ok("signal: 15, exit status: 5, signal: 9, signal: 13\n"));
}

#[test_case]
fn a_program_stops_and_continues() {
    // Stopped by SIGSTOP, as WUNTRACED says, and CLD_STOPPED; continued,
    // as WCONTINUED says, and CLD_CONTINUED; then killed, and CLD_KILLED.
    assert_eq!(run(&["stops"]), ok("0x137f 5 0xffff 6 9 2\n"));
}

#[test_case]
fn sigsuspend_lets_a_blocked_signal_through_and_blocks_it_again() {
    assert_eq!(run(&["suspend"]), ok("-4 1 true\n"));
}

#[test_case]
fn process_groups_and_sessions() {
    // Its own group and session, as the kernel started it, so no new one;
    // a copy in a group of its own, which a group's signal ends; and no
    // joining a group that isn't there, nor signalling one that's gone.
    assert_eq!(run(&["groups"]), ok("true true -1 true -1 0 signal: 15 -3\n"));
}

#[test_case]
fn a_handler_runs_on_the_alternate_stack() {
    assert_eq!(run(&["altstack"]), ok("0 true 1 0\n"));
}
