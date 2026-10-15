//! User programs: a Linux x86-64 executable (user/probe, from the ramdisk)
//! runs in ring 3, writes through system calls, and ends, cleanly or by a
//! fault, without taking anything else with it.

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
use simplos::io::hpet;
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

/// A running probe and what it has written so far.
type Run = (Arc<process::Process>, Arc<Mutex<Vec<u8>>>);

/// Start probe with `args`.
fn start(args: &[&str]) -> Run {
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    let process = process::spawn(elf, &argv, move |bytes| sink.lock().extend_from_slice(bytes))
        .expect("probe loads");
    (process, output)
}

/// Wait for it: its exit status, and everything it wrote to its standard
/// output and error.
fn finish((process, output): Run) -> (i32, String) {
    let status = process.wait();
    let output = String::from_utf8_lossy(&output.lock()).into_owned();
    (status, output)
}

fn run(args: &[&str]) -> (i32, String) {
    finish(start(args))
}

#[test_case]
fn a_program_runs_prints_and_exits() {
    assert_eq!(run(&["hello"]), (0, String::from("hello from user space\n")));
}

#[test_case]
fn a_program_gets_its_arguments() {
    // On standard error, with the exit status probe picks for it.
    assert_eq!(run(&["bogus"]), (2, String::from("probe: unknown mode \"bogus\"\n")));
}

#[test_case]
fn a_fault_ends_only_the_program() {
    assert_eq!(run(&["fault"]).0, process::KILLED_SIGSEGV);
    assert_eq!(run(&["hello"]).0, 0, "a program after the fault");
}

#[test_case]
fn bad_system_calls_are_refused() {
    // A pointer into the kernel's half and one into nothing are EFAULT, an
    // unknown call ENOSYS, a sleep for 2e9 nanoseconds EINVAL and one whose
    // time is in the kernel EFAULT, as on Linux: never the kernel's memory,
    // never a fault in the kernel.
    assert_eq!(run(&["bad-calls"]), (0, String::from("-14 -14 -38 -22 -14\n")));
}

#[test_case]
fn memory_calls_keep_to_the_program_s_half_and_give_back_what_they_fail_to_get() {
    // The last page of the lower half, which munmap, mprotect and mmap
    // refuse, as Linux's do, and the kernel's half: the end of that page
    // isn't an address, and making it one stopped the kernel. Then a
    // gigabyte, more than the test machine has, which mmap and brk refuse
    // without keeping a page of what they took; and a megabyte, which
    // there still is. And a page MAP_FIXED maps where mmap would next,
    // which mmap then hands out no more.
    assert_eq!(
        run(&["memory-edges"]),
        (0, String::from("[-22, -22, -22, -22] -12 0 true 0 true true\n"))
    );
}

#[test_case]
fn the_stack_grows_as_it_s_used() {
    // What Linux says, from the same program: 4 MiB of calls deep, 16 KiB
    // each; written 4 MiB below, and read(2) into 6 MiB below, where it
    // hadn't been; and SIGSEGV for 100 MiB below, past where it can grow.
    assert_eq!(run(&["deep-stack"]), (0, String::from("32640 true 4 true signal: 11\n")));
}

#[test_case]
fn madvise_zeroes_what_isn_t_needed() {
    // What Linux says, from the same program: MADV_DONTNEED zeroes; and
    // EINVAL, ENOMEM and EINVAL for an address not a page's, memory not
    // mapped, and advice there isn't.
    assert_eq!(run(&["madvise"]), (0, String::from("0 true 0 [-22, -12, -22] 0\n")));
}

#[test_case]
fn mremap_grows_moves_and_shrinks_mappings() {
    // What Linux says, from the same program, but for the last: there,
    // where nothing's been mapped after it, it grows where it is.
    assert_eq!(
        run(&["mremap"]),
        (
            0,
            String::from(
                "-12 true true true [-22, -22, -22, -22, -22, -14, -14, -22, -22] 0 true true\n"
            )
        )
    );
}

#[test_case]
fn programs_keep_their_registers_to_themselves() {
    // Two at once, each preempted many times with its sum in SSE registers
    // and Rust's thread-locals behind `fs`: unless every switch puts both
    // away and brings them back, the two corrupt each other.
    let (a, b) = (start(&["float"]), start(&["float"]));
    // What the same build prints on Linux.
    let expected = (0, String::from("1885617375.8495038\n"));
    assert_eq!(finish(a), expected);
    assert_eq!(finish(b), expected);
}

#[test_case]
fn finished_programs_give_their_memory_back() {
    // Each run maps about 720 KiB: the program, its stack and their page
    // tables. 200 runs is more than the test machine's 128 MiB, so this only
    // finishes if each program's memory is freed when it ends.
    for _ in 0..200 {
        assert_eq!(run(&["exit"]).0, 0);
    }
}

#[test_case]
fn std_thread_sleep_sleeps() {
    // What a Rust program's std::thread::sleep comes to: clock_nanosleep.
    let start = hpet::elapsed_millis().expect("QEMU has an HPET");
    let (status, output) = run(&["sleep", "200"]);
    let took = hpet::elapsed_millis().unwrap() - start;
    assert_eq!((status, output.as_str()), (0, "sleeping for 200 ms\nawake\n"));
    assert!(took >= 200, "asked to sleep 200 ms, the whole run took {took}");
}
