//! The ext4 file system on a real (emulated) disk, through the disk drivers.
//!
//! `tools/boot-test` attaches an ext4 disk made by mke2fs to every test run,
//! holding `hello.txt` and `dir/nested/deep.txt`, and checks it with e2fsck
//! after a run that passes, by which time the test runner has unmounted it.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::format;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use ext4::{Ext4, File, Kind};
use simplos::fs::{self, ROOT};
use simplos::thread::{spawn, yield_now};

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

fn read_all(fs: &Ext4, path: &str) -> Vec<u8> {
    let file = fs.open(path).unwrap();
    let mut contents = vec![0; file.size().unwrap() as usize];
    let mut done = 0;
    while done < contents.len() {
        let read = file.read_at(done as u64, &mut contents[done..]).unwrap();
        assert_ne!(read, 0, "{path} ended early, at {done}");
        done += read;
    }
    contents
}

fn write_all(file: &File, mut offset: u64, mut buf: &[u8]) {
    while !buf.is_empty() {
        let written = file.write_at(offset, buf).unwrap();
        offset += written as u64;
        buf = &buf[written..];
    }
}

/// 1 MiB and some, no two neighbouring bytes alike.
fn pattern() -> Vec<u8> {
    (0..(1 << 20) + 300).map(|i: u32| (i % 251) as u8).collect()
}

#[test_case]
fn reads_what_linux_wrote() {
    let root = ROOT.lock();
    let fs = root.as_ref().expect("the ext4 disk, mounted");
    let mut names: Vec<_> =
        fs.read_dir("/").unwrap().into_iter().map(|e| (e.name, e.kind)).collect();
    names.sort_by(|a, b| a.0.cmp(&b.0));
    assert_eq!(
        names,
        [
            ("bin".into(), Kind::Directory),
            ("dir".into(), Kind::Directory),
            ("hello.txt".into(), Kind::File),
            ("lost+found".into(), Kind::Directory)
        ]
    );
    assert_eq!(read_all(fs, "hello.txt"), b"hello from ext4\n");
    assert_eq!(read_all(fs, "dir/nested/deep.txt"), b"deep\n");
}

#[test_case]
fn writes_what_reads_back_after_mounting_again() {
    {
        let root = ROOT.lock();
        let fs = root.as_ref().expect("the ext4 disk, mounted");
        write_all(&fs.create("big.bin").unwrap(), 0, &pattern());
        write_all(&fs.open_writable("big.bin").unwrap(), 4000, &[0xaa; 200]);
        fs.create_dir("made").unwrap();
        fs.rename("hello.txt", "made/hello.txt").unwrap();
        fs.remove_file("dir/nested/deep.txt").unwrap();
    }
    // From the disk, not from anything kept in memory.
    fs::unmount();
    fs::init();

    let root = ROOT.lock();
    let fs = root.as_ref().expect("the ext4 disk, mounted again");
    let mut expected = pattern();
    expected[4000..4200].fill(0xaa);
    assert!(read_all(fs, "big.bin") == expected, "big.bin reads back different");
    assert_eq!(read_all(fs, "made/hello.txt"), b"hello from ext4\n");
    assert!(fs.metadata("hello.txt").is_err());
    assert!(fs.read_dir("dir/nested").unwrap().is_empty());
}

#[test_case]
fn threads_take_turns_with_it() {
    // Each writes files of its own, a few times over, while the others do:
    // the lock keeps them out of lwext4 one at a time, and those waiting for
    // the disk sleep.
    let done = Arc::new(());
    for thread in 0..4 {
        let done = done.clone();
        spawn(move || {
            let _done = done;
            for round in 0..5 {
                let path = format!("thread-{thread}");
                let contents = format!("thread {thread}, round {round}\n").repeat(100);
                let root = ROOT.lock();
                let fs = root.as_ref().expect("the ext4 disk, mounted");
                write_all(&fs.create(&path).unwrap(), 0, contents.as_bytes());
                assert_eq!(read_all(fs, &path), contents.as_bytes());
                drop(root);
                yield_now();
            }
        })
        .expect("memory for a stack");
    }
    while Arc::strong_count(&done) > 1 {
        yield_now();
    }
    let root = ROOT.lock();
    let fs = root.as_ref().expect("the ext4 disk, mounted");
    for thread in 0..4 {
        let contents = format!("thread {thread}, round 4\n").repeat(100);
        assert_eq!(read_all(fs, &format!("thread-{thread}")), contents.as_bytes());
    }
}
