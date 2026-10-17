//! Time: user/probe reads the clocks, counts the CPU time it and a copy of
//! itself have had, and sets alarms.

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
    simplos::fs::init();
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
fn a_program_tells_the_time() {
    // The time of day within minutes of the test disk's files, which were
    // stamped as it was made; the three calls that tell it agreeing; sleeps
    // never short, for a while and until a time of day, and none at all for
    // a time past;
    // to the nanosecond, by the HPET; and EINVAL for a clock there isn't.
    assert_eq!(run(&["clocks"]), ok("true true true 0 true (0, true) [0, 1] -22\n"));
}

#[test_case]
fn cpu_time_is_counted() {
    // A program's own, computing; the kernel's for it, in system calls; and
    // a copy's, told by wait4 and counted as its parent's children's.
    assert_eq!(run(&["cpu-time"]), ok("true true true true true true true true\n"));
}

#[test_case]
fn alarms_go_off() {
    // pause ends with EINTR as the alarm's handler runs; one repeating; what
    // is left of one, and alarm(0) saying so as it turns it off; a copy
    // without one; SIGALRM ending a program that doesn't handle it; and a
    // read it interrupts.
    assert_eq!(
        run(&["alarms"]),
        ok("-4 true true 0 true 10 exit status: 0 signal: 14 Err(Some(4))\n")
    );
}

#[test_case]
fn alarms_too_far_off_to_count_to_never_come() {
    // One due in 2^63 seconds, which is more nanoseconds than there are:
    // not now, nor in a while, and still far off; and one due now that
    // repeats that far apart, once.
    assert_eq!(run(&["far-alarms"]), ok("0 0 true 1\n"));
}

#[test_case]
fn timers_send_their_signals_when_they_come() {
    // What Linux says, from the same program: a timer's SIGUSR1, no sooner
    // than set, with SI_TIMER, its id and its value; three more 20 ms apart;
    // off; one by the time of day already past, at once; one that sends
    // nothing, counting down; SIGALRM with its id for none given; one for
    // the thread alone, with all 8 bytes of its value, a pointer, where 4
    // came; deleted, then not there; and EINVAL for a clock, a timer and a
    // signal there aren't.
    assert_eq!(
        run(&["timers"]),
        ok("0 10 true (-2, true, 42) [10, 10, 10] true true true true 0 true [0, -22] \
            [-22, -22, -22, -22]\n")
    );
}
