//! Programs start programs: user/probe copies itself with fork, replaces
//! itself with execve, and waits with wait4, running itself again from
//! /bin/probe on the ext4 disk `tools/boot-test` attaches.

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
use simplos::{fs, process};
use spin::Mutex;

entry_point!(main, config = &simplos::BOOTLOADER_CONFIG);

fn main(boot_info: &'static mut BootInfo) -> ! {
    simplos::pre_init(boot_info);
    fs::init();
    test_main();
    simplos::hlt_loop()
}

#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    simplos::test_panic_handler(info)
}

/// Run probe from the ramdisk with `args`: its exit status, and everything
/// it and whatever it started wrote.
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

/// How probe says what went wrong with `path`.
fn failed(path: &str, error: &str) -> (i32, String) {
    (1, alloc::format!("probe: {path}: {error}\n"))
}

#[test_case]
fn a_copy_has_memory_of_its_own() {
    // The child sees what the parent had, changes it, and is told apart from
    // it; the parent's is as it was, and it learns how the child ended.
    assert_eq!(run(&["fork"]), ok("child: 2 2 true true\nparent: 1 1 true exit status: 7\n"));
}

#[test_case]
fn a_fault_ends_only_the_copy() {
    assert_eq!(run(&["fork-fault"]), ok("signal: 11\n"));
}

#[test_case]
fn copies_give_their_memory_back() {
    // Each copies all of probe's memory, over a megabyte: 200 of them are
    // more than the machine's 128 MiB, unless each is freed once waited for.
    assert_eq!(run(&["forks", "200"]), ok("200\n"));
}

#[test_case]
fn wait4_waits_for_children_and_not_for_none() {
    // Not ended yet, with WNOHANG; then it has; then there are none: ECHILD.
    assert_eq!(run(&["waits"]), ok("0 true exit status: 3 -10\n"));
}

#[test_case]
fn bad_calls_are_refused_and_the_program_goes_on() {
    // A thread, EINVAL; and execve's path, arguments and an argument in the
    // kernel's half, EFAULT.
    assert_eq!(run(&["bad-processes"]), ok("-22 -14 -14 -14\n"));
}

#[test_case]
fn execve_replaces_the_program() {
    assert_eq!(
        run(&["exec", "/bin/probe", "env", "a b", "c"]),
        ok("[\"env\", \"a b\", \"c\"]\nFROM=exec\n")
    );
}

#[test_case]
fn execve_refuses_what_isnt_a_program() {
    let refused = |path, error| (path, failed(path, error));
    for (path, expected) in [
        refused("/missing", "No such file or directory (os error 2)"),
        // Not executable, a directory, and executable but not ELF.
        refused("/hello.txt", "Permission denied (os error 13)"),
        refused("/dir", "Permission denied (os error 13)"),
        refused("/dev/null", "Permission denied (os error 13)"),
        refused("/bin/script", "Exec format error (os error 8)"),
    ] {
        assert_eq!(run(&["exec", path]), expected, "{path}");
    }
}

#[test_case]
fn a_program_runs_another_and_waits_for_it() {
    // Rust's Command, through musl's posix_spawn: clone, then execve.
    assert_eq!(
        run(&["spawn", "/bin/probe", "hello"]),
        ok("hello from user space\nexit status: 0\n")
    );
    assert_eq!(run(&["spawn", "/bin/probe", "exit", "5"]), ok("exit status: 5\n"));
    assert_eq!(run(&["spawn", "/bin/probe", "fault"]), ok("signal: 11 (SIGSEGV)\n"));
    // Told through a pipe that closes on exec, which then never does.
    assert_eq!(
        run(&["spawn", "/missing"]),
        failed("/missing", "No such file or directory (os error 2)")
    );
}

#[test_case]
fn pipes_join_a_program_to_the_one_it_runs() {
    // Its standard input and output are pipes whose other ends the parent
    // holds, and which are close-on-exec: were they left open in the child,
    // `cat` would wait for itself to close its input, forever.
    assert_eq!(
        run(&["feed", "fed through\n", "/bin/probe", "cat"]),
        ok("fed through\nexit status: 0\n")
    );
}
