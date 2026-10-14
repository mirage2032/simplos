//! Programs use files: user/probe, through Rust's std::fs, reads, writes and
//! lists the ext4 disk `tools/boot-test` attaches, which starts out holding
//! `hello.txt` and `dir/nested/deep.txt` and is checked by e2fsck afterwards.

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
use simplos::fs::{self, ROOT};
use simplos::process;
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

/// Run probe with `args`: its exit status, and everything it wrote.
fn run_bytes(args: &[&str]) -> (i32, Vec<u8>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    let process = process::spawn(elf, &argv, move |bytes| sink.lock().extend_from_slice(bytes))
        .expect("probe loads");
    let status = process.wait();
    let output = output.lock().clone();
    (status, output)
}

fn run(args: &[&str]) -> (i32, String) {
    let (status, output) = run_bytes(args);
    (status, String::from_utf8(output).unwrap())
}

fn ok(text: &str) -> (i32, String) {
    (0, String::from(text))
}

#[test_case]
fn a_program_reads_files_and_lists_directories() {
    assert_eq!(run(&["cat", "/hello.txt", "/dir/nested/deep.txt"]), ok("hello from ext4\ndeep\n"));
    assert_eq!(run(&["ls", "/"]), ok("dir/\nhello.txt\nlost+found/\n"));
    assert_eq!(run(&["ls", "/dir/nested"]), ok("deep.txt\n"));
    // Relative to the working directory, the root; and the long way round.
    assert_eq!(run(&["cat", "hello.txt"]), ok("hello from ext4\n"));
    assert_eq!(run(&["cat", "/dir/./nested/../../hello.txt"]), ok("hello from ext4\n"));
    // Relative to a directory the program has open.
    assert_eq!(run(&["cat-at", "/dir", "nested/deep.txt"]), ok("deep\n"));
}

#[test_case]
fn a_program_sees_what_stat_says() {
    // Through the path and through the file open: the same.
    assert_eq!(run(&["stat", "/hello.txt"]), ok("16 file 644\n16 file 644\n"));
    assert_eq!(run(&["stat", "/dir"]), ok("4096 dir 755\n4096 dir 755\n"));
    // pread, which doesn't move the descriptor's place.
    assert_eq!(run(&["read-at", "/hello.txt", "6", "4"]), ok("from"));
    assert_eq!(run(&["read-at", "/hello.txt", "100", "4"]), ok(""));
}

#[test_case]
fn a_program_is_told_what_linux_would_tell_it() {
    let failed = |path: &str, error: &str| (1, alloc::format!("probe: {path}: {error}\n"));
    assert_eq!(
        run(&["cat", "/missing"]),
        failed("/missing", "No such file or directory (os error 2)")
    );
    assert_eq!(run(&["cat", "/dir"]), failed("/dir", "Is a directory (os error 21)"));
    assert_eq!(run(&["ls", "/hello.txt"]), failed("/hello.txt", "Not a directory (os error 20)"));
    assert_eq!(
        run(&["write", "/missing/new.txt", "x"]),
        failed("/missing/new.txt", "No such file or directory (os error 2)")
    );
}

#[test_case]
fn a_program_writes_files_that_stay_written() {
    assert_eq!(run(&["write", "/new.txt", "written by a program\n"]), ok(""));
    assert_eq!(run(&["cat", "/new.txt"]), ok("written by a program\n"));
    // Emptied by the next write, then added to.
    assert_eq!(run(&["write", "/new.txt", "one\n"]), ok(""));
    assert_eq!(run(&["append", "/new.txt", "two\n"]), ok(""));
    assert_eq!(run(&["append", "/log.txt", "made by appending\n"]), ok(""));
    assert_eq!(run(&["cat", "/new.txt", "/log.txt"]), ok("one\ntwo\nmade by appending\n"));

    // The kernel sees the same, through its own way in.
    let root = ROOT.lock();
    let fs = root.as_ref().expect("the ext4 disk, mounted");
    let file = fs.open("new.txt").unwrap();
    let mut contents = [0; 8];
    assert_eq!(file.read_at(0, &mut contents).unwrap(), 8);
    assert_eq!(&contents, b"one\ntwo\n");
    drop(file);
    // The programs closed the files as they ended: they can go.
    fs.remove_file("new.txt").unwrap();
    fs.remove_file("log.txt").unwrap();
}

#[test_case]
fn a_program_reads_a_big_file_whole() {
    // More than one read's worth, which the program has to loop over.
    let pattern: Vec<u8> = (0..(3 << 20) + 5).map(|i: u32| (i % 251) as u8).collect();
    {
        let root = ROOT.lock();
        let fs = root.as_ref().expect("the ext4 disk, mounted");
        let file = fs.create("big.bin").unwrap();
        let mut done = 0;
        while done < pattern.len() {
            done += file.write_at(done as u64, &pattern[done..]).unwrap();
        }
    }
    let (status, output) = run_bytes(&["cat", "/big.bin"]);
    assert_eq!(status, 0);
    assert!(output == pattern, "big.bin reads back different: {} bytes", output.len());
    ROOT.lock().as_ref().unwrap().remove_file("big.bin").unwrap();
}

#[test_case]
fn a_program_lists_a_big_directory() {
    // More entries than musl reads in one getdents64.
    {
        let root = ROOT.lock();
        let fs = root.as_ref().expect("the ext4 disk, mounted");
        fs.create_dir("dir/many").unwrap();
        for i in 0..200 {
            fs.create(&alloc::format!("dir/many/file-{i:03}")).unwrap();
        }
    }
    let expected: String = (0..200).map(|i| alloc::format!("file-{i:03}\n")).collect();
    assert_eq!(run(&["ls", "/dir/many"]), (0, expected));
}

#[test_case]
fn bad_file_calls_are_refused() {
    // EFAULT for a path and a buffer in the kernel's half, EINVAL for a
    // directory read too small for one entry, EISDIR for reading one as a
    // file, EBADF for a descriptor that isn't open.
    assert_eq!(run(&["bad-files"]), ok("-14 -14 -22 -21 -9\n"));
}

#[test_case]
fn a_program_s_files_close_as_it_ends() {
    // Even while whoever waited for it still holds on to it, as a parent
    // will to a child it hasn't reaped.
    ROOT.lock().as_ref().unwrap().create("held.txt").unwrap();
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    let process = process::spawn(elf, &["probe", "hold", "/held.txt"], |_| {}).unwrap();
    assert_eq!(process.wait(), 0);
    // Busy if the program's file were still open.
    ROOT.lock().as_ref().unwrap().remove_file("held.txt").unwrap();
    drop(process);
}
