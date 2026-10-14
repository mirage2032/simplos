//! Programs on a terminal: user/probe reads what's typed into one, as lines
//! or byte by byte, asks it what it is, and waits with poll.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::fs::{self, Handle};
use simplos::init;
use simplos::process::{self, Process};
use simplos::thread;
use simplos::tty::{Termios, Tty};
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

type Screen = Arc<Mutex<Vec<u8>>>;

/// A terminal of 24 rows and 80 columns, as Linux starts one, and what's
/// written to it.
fn terminal() -> (Arc<Tty>, Screen) {
    let screen = Arc::new(Mutex::new(Vec::new()));
    let sink = screen.clone();
    let output = Box::new(move |bytes: &[u8]| sink.lock().extend_from_slice(bytes));
    (Arc::new(Tty::new(output, 24, 80)), screen)
}

/// Start probe with `args`, its standard input, output and error `tty`.
fn start(tty: &Arc<Tty>, args: &[&str]) -> Arc<Process> {
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    let files = vec![Some(Arc::new(Handle::tty(tty.clone()))); 3];
    process::spawn_with(elf, &argv, &[], files).expect("probe loads")
}

/// Run probe with `args` on `tty`: its exit status, and what's on `screen`
/// once it's done.
fn run(tty: &Arc<Tty>, screen: &Screen, args: &[&str]) -> (i32, String) {
    let status = start(tty, args).wait();
    (status, text(screen))
}

fn ok(text: &str) -> (i32, String) {
    (0, String::from(text))
}

fn text(screen: &Screen) -> String {
    String::from_utf8(screen.lock().clone()).unwrap()
}

/// Wait until `screen` ends with `end`.
fn wait_for(screen: &Screen, end: &str) {
    for _ in 0..1000 {
        if text(screen).ends_with(end) {
            return;
        }
        thread::sleep(10);
    }
    panic!("{end:?} never came; the screen has {:?}", text(screen));
}

/// What echoing an erase does to one character on the screen.
fn erased(count: usize) -> String {
    "\x08 \x08".repeat(count)
}

#[test_case]
fn a_program_reads_what_is_typed_a_line_at_a_time() {
    let (tty, screen) = terminal();
    // Typed ahead, so all of it's echoed before probe reads any: erasing a
    // character, the line, and words, then the end of file.
    tty.input(b"hello\rwrong\x7f\x7f\x7f\x7f\x7fright\rgone\x15kept\rtwo words\x17\x17one\r\x04");
    let echo = format!(
        "hello\r\nwrong{}right\r\ngone{}kept\r\ntwo words{}one\r\n",
        erased(5),
        erased(4),
        erased(9)
    );
    assert_eq!(run(&tty, &screen, &["cat"]), ok(&(echo + "hello\r\nright\r\nkept\r\none\r\n")));
}

#[test_case]
fn the_end_of_file_ends_a_line_or_what_is_typed() {
    let (tty, screen) = terminal();
    tty.input(b"partial\x04\x04");
    // Echoed, then read without a newline; then the end.
    assert_eq!(run(&tty, &screen, &["cat"]), ok("partialpartial"));
}

#[test_case]
fn a_program_in_raw_mode_gets_bytes_as_they_come() {
    let (tty, screen) = terminal();
    let probe = start(&tty, &["raw", "3"]);
    wait_for(&screen, "raw\n");
    // Not echoed, nor turned into a newline, nor taken as an interrupt.
    tty.input(b"a\r\x03");
    // Without OPOST, newlines go out as they are, until it's put back.
    assert_eq!((probe.wait(), text(&screen)), ok("raw\n[97, 13, 3]\ncooked\r\n"));
    assert_eq!(tty.termios(), Termios::DEFAULT);
}

#[test_case]
fn a_program_knows_its_terminal() {
    let (tty, screen) = terminal();
    assert_eq!(run(&tty, &screen, &["tty"]), ok("true true false 24x80 0\r\n"));
    // /dev/tty is it, and without one there's none to open: ENXIO.
    let (tty, screen) = terminal();
    assert_eq!(run(&tty, &screen, &["write", "/dev/tty", "x"]), ok("x"));
    let elf = simplos::ramdisk().unwrap();
    let files = vec![Some(Arc::new(Handle::Null)); 3];
    let probe = process::spawn_with(elf, &["probe", "write", "/dev/tty", "x"], &[], files);
    assert_eq!(probe.unwrap().wait(), 1);
}

#[test_case]
fn the_first_program_there_is_runs_as_linux_starts_it() {
    let (tty, screen) = terminal();
    // None of Linux's are on the disk, so the kernel's shell would be next.
    init::run(&tty);
    assert!(init::start(&tty, &init::PROGRAMS, &[]).is_none());
    // One that isn't there, one that isn't a program, which is said, and
    // then probe, with Linux's environment.
    let programs = ["/sbin/init", "/hello.txt", "/bin/probe"];
    let (path, first) = init::start(&tty, &programs, &["env"]).expect("one of them runs");
    assert_eq!((path.as_str(), first.wait()), ("/bin/probe", 0));
    assert_eq!(
        text(&screen),
        "/hello.txt is there but couldn't be run: Permission denied\r\n\
         [\"env\"]\r\nHOME=/\r\nTERM=linux\r\n"
    );
}

#[test_case]
fn poll_waits_for_what_it_asks_and_no_longer() {
    let (tty, screen) = terminal();
    // At once and after 50 ms, nothing; then POLLIN once the copy writes,
    // and POLLHUP once it's gone.
    assert_eq!(run(&tty, &screen, &["poll"]), ok("0 0 (1, 1) (1, 16)\r\n"));
}

#[test_case]
fn o_nonblocking_makes_what_would_wait_fail_instead() {
    // Nothing to read in an empty pipe, EAGAIN; written until it holds all
    // it can, 64 KiB, then EAGAIN; F_GETFL says so of the writing end, as
    // O_WRONLY | O_NONBLOCK; and nothing typed yet, EAGAIN.
    let (tty, screen) = terminal();
    assert_eq!(run(&tty, &screen, &["nonblocking"]), ok("-11 65536 -11 2049 -11\r\n"));
}

#[test_case]
fn a_program_collects_another_s_output() {
    // Rust's Command::output: both of its pipes at once, through poll.
    let (tty, screen) = terminal();
    assert_eq!(
        run(&tty, &screen, &["output", "/bin/probe", "cat", "/hello.txt", "/missing"]),
        ok("hello from ext4\r\n\"probe: /missing: No such file or directory (os error 2)\\n\" \
            exit status: 1\r\n")
    );
}
