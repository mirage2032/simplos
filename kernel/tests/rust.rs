//! Rust: rustc and cargo, as `scripts/rust.sh` unpacks them and
//! `tools/boot-test` puts them on this kernel's disk, with GCC to link with,
//! as it does for a kernel whose name starts with `rust`: compiling programs
//! on simplos that then run on it. Each start of rustc reads its 230 MB
//! library, which under emulation is most of the time these take.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::fs::{self, Handle, ROOT};
use simplos::process;
use simplos::tty::Tty;
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

/// The environment `/sbin/init` gives zsh: cargo keeps what it downloads and
/// its records in $HOME/.cargo, and finds rustc, and cc to link with, on
/// PATH.
const ENVIRONMENT: [&str; 3] = ["HOME=/root", "TERM=linux", "PATH=/sbin:/usr/sbin:/bin:/usr/bin"];

/// Run `script` with busybox's sh, on a terminal no one types on: how it
/// ended, and what it wrote, newlines as they are.
fn sh(script: &str) -> (i32, String) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let tty = Tty::capture(Box::new(move |bytes| sink.lock().extend_from_slice(bytes)));
    let files = vec![Some(Arc::new(Handle::tty(Arc::new(tty)))); 3];
    let argv = ["/bin/sh", "-c", script];
    let program = process::spawn_program(argv[0], &argv, &ENVIRONMENT, files);
    let status = program.expect("it runs").wait();
    let output = output.lock().clone();
    (status, String::from_utf8_lossy(&output).into_owned())
}

/// Put `text` in the file at `path`, a path from the root, as an editor
/// would have.
fn write(path: &str, text: &str) {
    let root = ROOT.lock();
    let file = root.as_ref().unwrap().create(path).unwrap();
    let mut done = 0;
    while done < text.len() {
        done += file.write_at(done as u64, &text.as_bytes()[done..]).unwrap();
    }
}

fn ok(text: &str) -> (i32, String) {
    (0, String::from(text))
}

#[test_case]
fn rustc_compiles_rust_that_runs() {
    // One start of rustc, and gcc to link: a minute and more under
    // emulation.
    simplos::take_longer(600);
    // std's threads and hash maps, which ask the kernel for threads and
    // random numbers.
    write(
        "tmp/hello.rs",
        r#"use std::collections::HashMap;

fn main() {
    let counts: HashMap<&str, u32> = [("rustc", 1)].into();
    let answer = std::thread::spawn(|| 40 + 2).join().unwrap();
    println!("hello from rustc, {} {answer}", counts["rustc"]);
}
"#,
    );
    assert_eq!(sh("cd /tmp && rustc -O hello.rs && ./hello"), ok("hello from rustc, 1 42\n"));
}

#[test_case]
fn cargo_makes_a_package_builds_it_and_runs_it() {
    // cargo, then rustc three times: to ask what it is, what it makes, and
    // to compile. Several minutes under emulation.
    simplos::take_longer(1200);
    // Quiet, so that anything it warns of, such as locks it couldn't take,
    // shows as a difference here.
    let script = "cd /tmp && cargo new -q --vcs none app && cd app && cargo run -q";
    assert_eq!(sh(script), ok("Hello, world!\n"));
}
