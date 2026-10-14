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
use simplos::fs::{self, Handle, ROOT};
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
    run_with(args, console())
}

/// Standard input, output and error on the console.
fn console() -> Vec<Option<Arc<Handle>>> {
    vec![Some(Arc::new(Handle::Console)); 3]
}

/// Start probe with `args` and `files` as its descriptors, and what it
/// writes to the console goes to `output`.
fn start(
    args: &[&str],
    files: Vec<Option<Arc<Handle>>>,
    output: Arc<Mutex<Vec<u8>>>,
) -> Arc<process::Process> {
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    process::spawn_with(elf, &argv, move |bytes| output.lock().extend_from_slice(bytes), files)
        .expect("probe loads")
}

/// `run_bytes`, with `files` as its descriptors.
fn run_with(args: &[&str], files: Vec<Option<Arc<Handle>>>) -> (i32, Vec<u8>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    let status = start(args, files, output.clone()).wait();
    let output = output.lock().clone();
    (status, output)
}

/// A pipe's two ends, ready for a program's descriptors.
fn pipe() -> (Option<Arc<Handle>>, Option<Arc<Handle>>) {
    let (read, write) = simplos::pipe::new();
    (Some(Arc::new(Handle::Pipe(read))), Some(Arc::new(Handle::Pipe(write))))
}

/// How long /big.bin is: 3 MiB and some.
const BIG: usize = (3 << 20) + 5;

/// What /big.bin holds: no two neighbouring bytes alike. Made as it's needed
/// rather than kept, as the kernel's heap is shared with the screen's copy.
fn big(range: core::ops::Range<usize>) -> Vec<u8> {
    range.map(|i| (i % 251) as u8).collect()
}

/// Write /big.bin, from the kernel.
fn big_file() {
    let root = ROOT.lock();
    let fs = root.as_ref().expect("the ext4 disk, mounted");
    let file = fs.create("big.bin").unwrap();
    let mut done = 0;
    while done < BIG {
        let chunk = big(done..BIG.min(done + (64 << 10)));
        let mut written = 0;
        while written < chunk.len() {
            written += file.write_at((done + written) as u64, &chunk[written..]).unwrap();
        }
        done += chunk.len();
    }
}

fn run(args: &[&str]) -> (i32, String) {
    let (status, output) = run_bytes(args);
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
    big_file();
    let (status, output) = run_bytes(&["cat", "/big.bin"]);
    assert_eq!(status, 0);
    assert!(output == big(0..BIG), "big.bin reads back different: {} bytes", output.len());
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

#[test_case]
fn a_program_makes_and_removes_directories() {
    assert_eq!(run(&["mkdir", "/made"]), ok(""));
    // As on Linux: what a program asks for, less the umask's 022. Two
    // blocks, not one: lwext4 makes every directory indexed from the start.
    assert_eq!(run(&["stat", "/made"]), ok("8192 dir 755\n8192 dir 755\n"));
    assert_eq!(run(&["mkdir", "/made"]), failed("/made", "File exists (os error 17)"));
    assert_eq!(run(&["mkdir-p", "/made/a/b/c"]), ok(""));
    assert_eq!(run(&["write", "/made/a/b/c/file.txt", "deep\n"]), ok(""));
    assert_eq!(run(&["stat", "/made/a/b/c/file.txt"]), ok("5 file 644\n5 file 644\n"));
    assert_eq!(run(&["rmdir", "/made/a"]), failed("/made/a", "Directory not empty (os error 39)"));
    assert_eq!(run(&["rm", "/made/a"]), failed("/made/a", "Is a directory (os error 21)"));
    assert_eq!(
        run(&["rmdir", "/made/a/b/c/file.txt"]),
        failed("/made/a/b/c/file.txt", "Not a directory (os error 20)")
    );
    assert_eq!(
        run(&["rmdir", "/made/a/b/c/."]),
        failed("/made/a/b/c/.", "Invalid argument (os error 22)")
    );
    assert_eq!(run(&["rm", "/made/a/b/c/file.txt"]), ok(""));
    assert_eq!(run(&["rmdir", "/made/a/b/c"]), ok(""));
    // The rest, as std::fs::remove_dir_all does it: openat, getdents64 and
    // unlinkat, each relative to the directory above.
    assert_eq!(run(&["write", "/made/a/b/again.txt", "x"]), ok(""));
    assert_eq!(run(&["rm-r", "/made"]), ok(""));
    assert_eq!(run(&["ls", "/"]), ok("dir/\nhello.txt\nlost+found/\n"));
}

#[test_case]
fn a_program_moves_files_and_replaces_them() {
    assert_eq!(run(&["mkdir", "/moves"]), ok(""));
    assert_eq!(run(&["write", "/moves/first.txt", "first\n"]), ok(""));
    assert_eq!(run(&["write", "/moves/second.txt", "second\n"]), ok(""));
    assert_eq!(run(&["mv", "/moves/first.txt", "/moves/moved.txt"]), ok(""));
    assert_eq!(run(&["cat", "/moves/moved.txt"]), ok("first\n"));
    assert_eq!(
        run(&["cat", "/moves/first.txt"]),
        failed("/moves/first.txt", "No such file or directory (os error 2)")
    );
    // Not over what's there, when asked not to.
    assert_eq!(run(&["mv-n", "/moves/second.txt", "/moves/moved.txt"]), ok("-17\n"));
    // Over it, which goes, as a program saving a file whole expects.
    assert_eq!(run(&["mv", "/moves/second.txt", "/moves/moved.txt"]), ok(""));
    assert_eq!(run(&["cat", "/moves/moved.txt"]), ok("second\n"));
    assert_eq!(run(&["ls", "/moves"]), ok("moved.txt\n"));
    // A directory, with what's in it.
    assert_eq!(run(&["mv", "/moves", "/dir/moved"]), ok(""));
    assert_eq!(run(&["cat", "/dir/moved/moved.txt"]), ok("second\n"));
    assert_eq!(
        run(&["mv", "/dir", "/dir/moved/dir"]),
        failed("/dir", "Invalid argument (os error 22)")
    );
    assert_eq!(run(&["rm-r", "/dir/moved"]), ok(""));
}

#[test_case]
fn a_program_has_a_working_directory() {
    // Says where it went, then reads a path from there.
    assert_eq!(run(&["cd", "/dir", "nested/deep.txt"]), ok("/dir\ndeep\n"));
    assert_eq!(
        run(&["cd", "/dir/nested/", "../../hello.txt"]),
        ok("/dir/nested\nhello from ext4\n")
    );
    assert_eq!(
        run(&["cd", "/hello.txt", "x"]),
        failed("/hello.txt", "Not a directory (os error 20)")
    );
    assert_eq!(
        run(&["cd", "/missing", "x"]),
        failed("/missing", "No such file or directory (os error 2)")
    );
}

#[test_case]
fn a_program_pipes_to_itself() {
    // Out the other end, then the end once the writer's gone; then nothing
    // reads, and writing says so.
    assert_eq!(run(&["pipe"]), ok("through a pipe\nBroken pipe (os error 32)\n"));
}

#[test_case]
fn two_programs_talk_through_a_pipe() {
    // More than a pipe holds, so each waits for the other in turn.
    big_file();
    let (read, write) = pipe();
    let mut reader_files = console();
    reader_files[0] = read;
    let mut writer_files = console();
    writer_files[1] = write;
    let output = Arc::new(Mutex::new(Vec::with_capacity(BIG)));
    let reader = start(&["cat"], reader_files, output.clone());
    let writer = start(&["cat", "/big.bin"], writer_files, Arc::new(Mutex::new(Vec::new())));
    assert_eq!(writer.wait(), 0);
    // Which closed the writing end as it ended: the reader sees the end.
    assert_eq!(reader.wait(), 0);
    let output = output.lock();
    assert!(*output == big(0..BIG), "{} bytes came through, not {BIG}", output.len());
    ROOT.lock().as_ref().unwrap().remove_file("big.bin").unwrap();
}

#[test_case]
fn a_program_writing_to_no_one_is_told() {
    let (read, write) = pipe();
    let mut reader_files = console();
    reader_files[0] = read;
    assert_eq!(run_with(&["exit"], reader_files).0, 0);
    let mut writer_files = console();
    writer_files[1] = write;
    let (status, output) = run_with(&["cat", "/hello.txt"], writer_files);
    assert_eq!(
        (status, String::from_utf8(output).unwrap()),
        failed("/hello.txt", "Broken pipe (os error 32)")
    );
}

#[test_case]
fn a_program_sends_its_output_elsewhere() {
    // dup2 onto standard output, which then writes into the file.
    assert_eq!(run(&["dup-out", "/out.txt", "into the file"]), ok(""));
    assert_eq!(run(&["cat", "/out.txt"]), ok("into the file 1\n"));
    ROOT.lock().as_ref().unwrap().remove_file("out.txt").unwrap();
}

#[test_case]
fn a_program_finds_the_devices_it_expects() {
    assert_eq!(run(&["cat", "/dev/null"]), ok(""));
    assert_eq!(run(&["write", "/dev/null", "gone"]), ok(""));
    assert_eq!(run_bytes(&["read-at", "/dev/zero", "0", "8"]), (0, vec![0; 8]));
    assert_eq!(run(&["write", "/dev/tty", "on the console\n"]), ok("on the console\n"));
    // Character devices: not directories, and not much else.
    assert_eq!(run(&["stat", "/dev/null"]), ok("0 file 666\n0 file 666\n"));
}

#[test_case]
fn bad_pipe_calls_are_refused() {
    // EBADF for a descriptor that isn't open and one past the most there can
    // be, EFAULT for ends to go in the kernel's half, EINVAL for O_NONBLOCK.
    assert_eq!(run(&["bad-pipes"]), ok("-9 -9 -14 -22\n"));
}
