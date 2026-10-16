//! Programs use files: user/probe, through Rust's std::fs, reads, writes and
//! lists the ext4 disk `tools/boot-test` attaches, which starts out holding
//! `hello.txt` and `dir/nested/deep.txt` and is checked by e2fsck afterwards.

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

/// Run probe with `args`: its exit status, and everything it wrote.
fn run_bytes(args: &[&str]) -> (i32, Vec<u8>) {
    let output = Arc::new(Mutex::new(Vec::new()));
    run_with(args, console(&output), &output)
}

/// Standard input, output and error on a terminal no one types on, whose
/// output goes to `output`.
fn console(output: &Arc<Mutex<Vec<u8>>>) -> Vec<Option<Arc<Handle>>> {
    let sink = output.clone();
    let tty = Tty::capture(Box::new(move |bytes| sink.lock().extend_from_slice(bytes)));
    vec![Some(Arc::new(Handle::tty(Arc::new(tty)))); 3]
}

/// Start probe with `args` and `files` as its descriptors.
fn start(args: &[&str], files: Vec<Option<Arc<Handle>>>) -> Arc<process::Process> {
    let mut argv = vec!["probe"];
    argv.extend(args);
    let elf = simplos::ramdisk().expect("tools/boot-test attaches user/probe");
    process::spawn_with(elf, &argv, &[], files).expect("probe loads")
}

/// Run probe with `args` and `files` as its descriptors: its exit status,
/// and what's in `output` once it's done.
fn run_with(
    args: &[&str],
    files: Vec<Option<Arc<Handle>>>,
    output: &Arc<Mutex<Vec<u8>>>,
) -> (i32, Vec<u8>) {
    let status = start(args, files).wait();
    let output = output.lock().clone();
    (status, output)
}

/// A pipe's two ends, ready for a program's descriptors.
fn pipe() -> (Option<Arc<Handle>>, Option<Arc<Handle>>) {
    let (read, write) = simplos::pipe::new();
    (Some(Arc::new(Handle::pipe(read))), Some(Arc::new(Handle::pipe(write))))
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

/// Put `contents` in the file at `path`, from the root, made or emptied
/// first.
fn write(path: &str, contents: &[u8]) {
    let root = ROOT.lock();
    let file = root.as_ref().unwrap().create(path).unwrap();
    let mut done = 0;
    while done < contents.len() {
        done += file.write_at(done as u64, &contents[done..]).unwrap();
    }
}

/// How probe says what went wrong with `path`.
fn failed(path: &str, error: &str) -> (i32, String) {
    (1, alloc::format!("probe: {path}: {error}\n"))
}

#[test_case]
fn a_program_reads_files_and_lists_directories() {
    assert_eq!(run(&["cat", "/hello.txt", "/dir/nested/deep.txt"]), ok("hello from ext4\ndeep\n"));
    assert_eq!(run(&["ls", "/"]), ok("bin/\ndev/\ndir/\nhello.txt\nlost+found/\nproc/\n"));
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
    // Kept in lost+found if the program's file were still open.
    let root = ROOT.lock();
    let fs = root.as_ref().unwrap();
    fs.remove_file("held.txt").unwrap();
    assert!(fs.read_dir("lost+found").unwrap().is_empty());
    drop((root, process));
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
    assert_eq!(run(&["ls", "/"]), ok("bin/\ndev/\ndir/\nhello.txt\nlost+found/\nproc/\n"));
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
fn mmap_maps_files() {
    // What Linux says, from the same program: from a page on, with zeros
    // past the end; MAP_SHARED read-only; a private copy written, which
    // leaves the file as it was; and EINVAL, EBADF, ENODEV, EACCES and
    // EACCES. Then mprotect making a MAP_SHARED page of a file open to be
    // written writable: EACCES, where Linux writes what's written there to
    // the file, which nothing here would; and again once it's been made
    // read-only.
    let refused = "[-22, -9, -19, -13, -13, -13, -13]";
    assert_eq!(
        run(&["file-mmap", "/mapped.bin"]),
        ok(&alloc::format!("true true true {refused}\n"))
    );
    ROOT.lock().as_ref().unwrap().remove_file("mapped.bin").unwrap();
}

#[test_case]
fn a_file_s_pages_mapped_are_the_file_s() {
    // What Linux says, from the same program: mapped read-only, privately or
    // shared, they're the page cache's, so what's written to the file then
    // shows in both, and still does after MADV_DONTNEED; but a page mapped
    // writable and written is the program's own, as is the private one made
    // writable with mprotect.
    assert_eq!(run(&["mapped-writes", "/mapped.bin"]), ok("true true true true true\n"));
    ROOT.lock().as_ref().unwrap().remove_file("mapped.bin").unwrap();
}

#[test_case]
fn a_file_read_again_is_read_from_memory() {
    // The page cache keeps what's read of a file: read again, none of it
    // comes from the disk; removed, it's not kept.
    let contents: String = (0..64 << 10).map(|i| char::from(b'a' + (i % 26) as u8)).collect();
    write("cached.txt", contents.as_bytes());
    let sectors = || simplos::io::disk::list().iter().map(|disk| disk.sectors_read).sum::<u64>();
    let kept = simplos::cache::pages();
    assert_eq!(run(&["cat", "/cached.txt"]), ok(&contents));
    assert!(
        simplos::cache::pages() >= kept + 16,
        "{} pages kept, from {kept}",
        simplos::cache::pages()
    );
    let read = sectors();
    assert_eq!(run(&["cat", "/cached.txt"]), ok(&contents));
    assert_eq!(sectors() - read, 0, "sectors read from the disk the second time");
    // Gone, it takes them with it.
    ROOT.lock().as_ref().unwrap().remove_file("cached.txt").unwrap();
    assert!(simplos::cache::pages() <= kept, "{} pages kept, from {kept}", simplos::cache::pages());
}

#[test_case]
fn what_s_read_again_is_what_was_written_since() {
    // Each read first, so the page cache keeps it: then rewritten shorter,
    // emptied as it's opened; cut short and made longer, with zeros, by the
    // kernel itself; renamed over by another file; and gone, its inode's
    // number free for the next file made, which is made longer with zeros.
    let cat = |path| run(&["cat", path]);
    let resize = |path, size| {
        let root = ROOT.lock();
        root.as_ref().unwrap().open_writable(path).unwrap().set_len(size).unwrap();
    };
    assert_eq!(run(&["write", "/again.txt", "a longer first text"]), ok(""));
    assert_eq!(cat("/again.txt"), ok("a longer first text"));
    assert_eq!(run(&["write", "/again.txt", "short"]), ok(""));
    assert_eq!(cat("/again.txt"), ok("short"));
    resize("again.txt", 2);
    resize("again.txt", 4);
    assert_eq!(cat("/again.txt"), ok("sh\0\0"));
    assert_eq!(run(&["write", "/other.txt", "the other"]), ok(""));
    assert_eq!(run(&["mv", "/other.txt", "/again.txt"]), ok(""));
    assert_eq!(cat("/again.txt"), ok("the other"));
    assert_eq!(run(&["rm", "/again.txt"]), ok(""));
    assert_eq!(run(&["write", "/new.txt", "new"]), ok(""));
    resize("new.txt", 9);
    assert_eq!(cat("/new.txt"), ok("new\0\0\0\0\0\0"));
    ROOT.lock().as_ref().unwrap().remove_file("new.txt").unwrap();
}

#[test_case]
fn fchdir_moves_to_the_directory_open() {
    // And not to a file, ENOTDIR, nor a descriptor not open, EBADF.
    assert_eq!(run(&["fchdir", "/dir", "/hello.txt"]), ok("0 /dir (-20, -9)\n"));
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
    let output = Arc::new(Mutex::new(Vec::with_capacity(BIG)));
    let mut reader_files = console(&output);
    reader_files[0] = read;
    let mut writer_files = console(&Arc::new(Mutex::new(Vec::new())));
    writer_files[1] = write;
    let reader = start(&["cat"], reader_files);
    let writer = start(&["cat", "/big.bin"], writer_files);
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
    let output = Arc::new(Mutex::new(Vec::new()));
    let mut reader_files = console(&output);
    reader_files[0] = read;
    assert_eq!(run_with(&["exit"], reader_files, &output).0, 0);
    let mut writer_files = console(&output);
    writer_files[1] = write;
    let (status, output) = run_with(&["cat", "/hello.txt"], writer_files, &output);
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
fn a_program_gets_random_bytes() {
    // What Linux says, from the same program: getrandom's bytes, as many as
    // asked for, different each time, as Rust's std needs for its hash maps;
    // EINVAL for flags there aren't and for GRND_RANDOM with GRND_INSECURE,
    // EFAULT into the kernel's half; and /dev/urandom and /dev/random, read,
    // written to, and character devices anyone can use.
    let devices = r#"["true 3 true 666", "true 3 true 666"]"#;
    let expected = alloc::format!("[32, 32, 32, 32, 32] true [-22, -22, -14] {devices} 42\n");
    assert_eq!(run(&["random"]), ok(&expected));
    assert_eq!(run(&["stat", "/dev/urandom"]), ok("0 file 666\n0 file 666\n"));
}

#[test_case]
fn bad_pipe_calls_are_refused() {
    // EBADF for a descriptor that isn't open and one past the most there can
    // be, EFAULT for ends to go in the kernel's half, EINVAL for a flag
    // pipe2 doesn't take.
    assert_eq!(run(&["bad-pipes"]), ok("-9 -9 -14 -22\n"));
}

#[test_case]
fn programs_waiting_to_write_into_a_pipe_leave_the_kernel_room() {
    // Sixteen copies each writing a megabyte into a full pipe that no one
    // reads: each waits, with what it's writing in the kernel's heap, which
    // has less room left than sixteen megabytes.
    assert_eq!(run(&["pipe-writers", "16"]), ok("16\n"));
}

#[test_case]
fn the_devices_are_in_dev() {
    // Wherever the disk has it or not: mounted over it, as on Linux.
    assert_eq!(
        run(&["ls", "/dev"]),
        ok("fd/\nnull\nrandom\nstderr\nstdin\nstdout\ntty\nurandom\nzero\n")
    );
    assert_eq!(run(&["stat", "/dev"]), ok("0 dir 755\n0 dir 755\n"));
    assert_eq!(run(&["cd", "/dev", "null"]), ok("/dev\n"));
    assert_eq!(run(&["cd", "/dev/..", "hello.txt"]), ok("/\nhello from ext4\n"));
    // Nothing to make, remove or move there.
    let read_only = "Read-only file system (os error 30)";
    assert_eq!(run(&["mkdir", "/dev/new"]), failed("/dev/new", read_only));
    assert_eq!(run(&["rm", "/dev/null"]), failed("/dev/null", read_only));
    assert_eq!(run(&["write", "/dev/new", "x"]), failed("/dev/new", read_only));
    assert_eq!(run(&["mv", "/hello.txt", "/dev/hello.txt"]), failed("/hello.txt", read_only));
}

#[test_case]
fn without_a_disk_there_is_still_a_root() {
    fs::unmount();
    // All of it before the disk goes back, whatever it says.
    let seen = [
        run(&["ls", "/"]),
        run(&["stat", "/"]),
        run(&["cat", "/dev/null"]),
        run(&["cat", "/hello.txt"]),
        run(&["write", "/new.txt", "x"]),
    ];
    fs::init();
    assert_eq!(
        seen,
        [
            ok("dev/\nproc/\n"),
            ok("0 dir 755\n0 dir 755\n"),
            ok(""),
            failed("/hello.txt", "No such file or directory (os error 2)"),
            failed("/new.txt", "Read-only file system (os error 30)"),
        ]
    );
}

#[test_case]
fn a_program_moves_several_buffers_in_one_call() {
    // Through a pipe: writev's three, one empty, as one write; readv into
    // two, the first filled first. Then too many buffers, and one that
    // isn't there.
    assert_eq!(run(&["vectors"]), ok("13 13 \"one \" \"two three\" -22 -14\n"));
}

#[test_case]
fn symbolic_and_hard_links_lead_where_they_should() {
    // Read back, followed, and lstat'd as itself; through a directory, with
    // `..` the directory's parent; a link to nothing, which O_CREAT makes;
    // a loop and O_NOFOLLOW, ELOOP; a program run through one; a hard link
    // outliving the other name. Then refused: a hard link to a directory
    // (EPERM), a link where there's a file (EEXIST), readlink of a file
    // (EINVAL), a hard link to nothing (ENOENT), and a file on the way, even
    // with `..` after it (ENOTDIR).
    let expected = r#""/hello.txt" "hello from ext4\n" true true "deep\n" true "made\n" Err(40) Err(40) "hello from user space\n" 2 "shared\n" [Err(1), Err(17), Err(22), Err(2), Err(20)]"#;
    assert_eq!(run(&["links"]), ok(&(String::from(expected) + "\n")));
}

#[test_case]
fn a_program_changes_a_file_s_permissions_owner_times_and_length() {
    // Made now; chmod by path and by descriptor; chown, owner and group
    // apart, and lchown of a link as itself; utimensat's times, one left as
    // it was; a write making it modified now, and no times making both now;
    // cut short and grown with zeros, through a descriptor
    // and by path, but not through one open only to read (EINVAL). Then
    // access: read and write, run what may not be (EACCES) and what may,
    // write a device and /dev (EROFS), a missing file, a bad mode (EINVAL).
    // And sendfile: from where the file's at, from a place given, which
    // moves on and leaves the file's own where it was, at its end.
    let expected = r#"true 600 [1000, 100, 7] 640 [1000, 2000] [1000, 3000] true true "x123" [120, 49, 50, 51, 0, 0] 0 2 -22 [0, -13, 0, 0, -30, -2, -22] 16 4 10 0 "hello from ext4\nfrom""#;
    assert_eq!(run(&["attributes"]), ok(&(String::from(expected) + "\n")));
}

#[test_case]
fn a_file_removed_or_replaced_while_open_stays_until_closed() {
    // As zsh does a here-document: written, opened, removed, read. And one
    // renamed over while open still reads as it was to whoever has it.
    // Closed as the program ends, they're gone, from lost+found too.
    assert_eq!(run(&["unlinked"]), ok("true \"here\\n\" \"old\\n\" \"new\\n\"\n"));
    let root = ROOT.lock();
    assert!(root.as_ref().unwrap().read_dir("lost+found").unwrap().is_empty());
}

#[test_case]
fn a_program_reads_of_itself_in_proc() {
    // Its pages, 256 more as it maps a megabyte and none once it unmaps it;
    // ext4's type and block size by path and by descriptor, alike; its name.
    assert_eq!(run(&["proc"]), ok("256 0 0xef53 true 4096 probe\n"));
}

#[test_case]
fn mounts_are_in_proc_as_each_process_sees_them() {
    // /proc/self/mounts, as /proc/mounts; and mountinfo, where uutils' df
    // finds them.
    assert_eq!(run(&["cat", "/proc/self/mounts"]), run(&["cat", "/proc/mounts"]));
    assert_eq!(
        run(&["cat", "/proc/self/mountinfo"]),
        ok("1 1 8:0 / / rw,relatime - ext4 /dev/root rw\n\
            2 1 0:5 / /dev rw - devtmpfs devtmpfs rw\n3 1 0:4 / /proc rw - proc proc rw\n")
    );
}

#[test_case]
fn calls_take_at_fdcwd_as_an_int() {
    // What Linux says, from the same program: openat, symlinkat,
    // readlinkat and unlinkat given AT_FDCWD in its low 32 bits only, as
    // rustix, and so uutils, passes it: by a relative path, which an
    // absolute one would ignore.
    assert_eq!(
        run(&["at-fdcwd", "at.txt"]),
        ok("true \"through the working directory\" 0 true [0, 0]\n")
    );
}

#[test_case]
fn files_have_no_extended_attributes() {
    // As on a file system without them on Linux: getting one is EOPNOTSUPP
    // and listing them lists none, by path, by path to a link and by
    // descriptor; setting or removing one is EOPNOTSUPP too, as Linux says
    // to root, though to anyone else EACCES can come first; and ENOENT and
    // EBADF for what isn't there.
    assert_eq!(run(&["xattrs", "/hello.txt"]), ok("[-95, -95, -95, 0, 0, 0, -95, -95, -2, -9]\n"));
}

#[test_case]
fn fadvise_takes_advice() {
    // What Linux says, from the same program: advice taken; EINVAL for
    // advice there isn't, ESPIPE for a pipe and EBADF for a descriptor not
    // open.
    assert_eq!(run(&["fadvise", "/hello.txt"]), ok("[0, -22, -29, -9]\n"));
}

#[test_case]
fn socket_pairs_connect_their_two_ends() {
    // What Linux says, from the same program: written one way and sent the
    // other, close-on-exec, a socket to fstat, ready to be written; once the
    // other end closes, readable, writable and hung up to poll, nothing to
    // read and EPIPE to send; and EAFNOSUPPORT, EPROTONOSUPPORT,
    // ESOCKTNOSUPPORT, EINVAL and ENOTSOCK twice for what it refuses.
    assert_eq!(
        run(&["socketpair"]),
        ok("0 4 4 4 4 \"pingpong\" 1 true 4 21 0 -32 [-97, -93, -94, -22, -88, -88]\n")
    );
}

#[test_case]
fn splice_moves_what_s_read_into_or_out_of_a_pipe() {
    // What Linux says, from the same program: 4 bytes from the place given,
    // moved on, into a pipe, the file's own place left; out of the pipe
    // into another file; the rest from the file's own place; 0 at its end;
    // and EINVAL for neither end a pipe, ESPIPE for a place for a pipe,
    // EINVAL for flags there aren't and EBADF for a descriptor not open.
    assert_eq!(
        run(&["splice", "/spliced.txt"]),
        ok("4 6 0 4 10 10 0 \"23450123456789\" [-22, -29, -22, -9]\n")
    );
}

#[test_case]
fn fsync_writes_what_waits_in_memory() {
    // A small write waits to be made with what follows it; fsync makes it,
    // and sync, as /proc/diskstats shows.
    for call in ["fsync", "sync"] {
        assert_eq!(run(&[call, "/synced.txt"]), ok("synced\n"), "{call}");
    }
    ROOT.lock().as_ref().unwrap().remove_file("synced.txt").unwrap();
}

#[test_case]
fn files_are_locked_whole_and_in_ranges() {
    // What Linux says, from the same program: flock's lock is its open
    // file's, so another open file of the same file is refused, shared or
    // not, and its dup and its copy aren't; let go of, another has it; a
    // copy's, waited for. fcntl's write lock on bytes 0 to 9, which a copy
    // is told of, F_GETLK, and refused, F_SETLK, but not bytes past it;
    // gone as the process closes another descriptor of the file; and an
    // open file's, F_OFD_SETLK, in a copy's process lock's way.
    let expected = "[0, -11, -11, 0, 0, 0, 0] exit status: 0 (0, true); 0 1 0 10 true -11 0; \
                    exit status: 0 0 exit status: 11\n";
    assert_eq!(run(&["locks", "/locked"]), ok(expected));
    ROOT.lock().as_ref().unwrap().remove_file("locked").unwrap();
}

#[test_case]
fn preadv_and_pwritev_read_and_write_where_asked() {
    // What Linux says, from the same program: three bytes written at 2 and
    // five read from 1, the file's place left at 0; two written there by
    // pwritev2, and one with RWF_APPEND at the end, the place moved to 9;
    // and a flag there isn't, EOPNOTSUPP.
    assert_eq!(run(&["vectors-at", "/at.txt"]), ok("3 5 \"bXY\" \"Zf\" 0 2 9 12XYZfgh! -95\n"));
    ROOT.lock().as_ref().unwrap().remove_file("at.txt").unwrap();
}
