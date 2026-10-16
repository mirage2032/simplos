//! Programs start programs: user/probe copies itself with fork, replaces
//! itself with execve, and waits with wait4, running itself again from
//! /bin/probe on the ext4 disk `tools/boot-test` attaches.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::format;
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
fn a_copy_starts_with_every_register_the_call_keeps() {
    assert_eq!(run(&["fork-registers"]), ok("child: true\nparent: true exit status: 0\n"));
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
    // A thread without its handlers, EINVAL, as Linux says; and execve's
    // path, arguments and an argument in the kernel's half, EFAULT.
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
fn a_program_bigger_than_the_kernel_s_heap_runs() {
    // /bin/big, which tools/boot-test makes: user/probe with a segment of
    // 24 MiB more, each byte where it should be, from the PIE's base on.
    assert_eq!(run(&["exec", "/bin/big", "peek", "10400000", "1800000"]), ok("ok\n"));
}

#[test_case]
fn a_program_starts_in_the_interpreter_it_names() {
    // /bin/interp-probe, which tools/boot-test makes: user/probe naming
    // /bin/probe as its interpreter (PT_INTERP), as a dynamically linked
    // program names its loader. It's probe that runs, told where it is
    // (AT_BASE) and where the program is (AT_PHDR, AT_ENTRY), as on Linux.
    assert_eq!(run(&["exec", "/bin/interp-probe", "auxv"]), ok("true true true true\n"));
    // Named by a path from where the program's run: as probe, from /bin.
    let relative = run(&["exec-in", "/bin", "/bin/interp-relative", "auxv"]);
    assert_eq!(relative, ok("true true true true\n"));
    // What it maps comes from past where the interpreter is.
    assert_eq!(run(&["exec", "/bin/interp-probe", "madvise"]), ok("0 true 0 [-22, -12, -22] 0\n"));
    // Without one: AT_BASE is 0, and there's the one copy.
    assert_eq!(run(&["auxv"]), ok("false false true true\n"));
}

#[test_case]
fn execve_refuses_what_isnt_a_program() {
    let refused = |path, error| (path, failed(path, error));
    for (path, expected) in [
        refused("/missing", "No such file or directory (os error 2)"),
        // Not executable, a directory, and a script whose interpreter,
        // /bin/sh, isn't there.
        refused("/hello.txt", "Permission denied (os error 13)"),
        refused("/dir", "Permission denied (os error 13)"),
        refused("/dev/null", "Permission denied (os error 13)"),
        refused("/bin/script", "No such file or directory (os error 2)"),
        // Interpreters (PT_INTERP) that aren't there; too short to be an ELF
        // program, and long enough but not one; that can't be run; and one
        // whose path has no NUL.
        refused("/bin/interp-missing", "No such file or directory (os error 2)"),
        refused("/bin/interp-script", "I/O error (os error 5)"),
        refused("/bin/interp-long-script", "No error information (os error 80)"),
        refused("/bin/interp-text", "Permission denied (os error 13)"),
        refused("/bin/interp-unterminated", "Exec format error (os error 8)"),
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

#[test_case]
fn a_program_learns_about_the_system_and_itself() {
    // Linux, as far as its system calls go; root, and only root; one CPU;
    // its limits, the one it lowered inherited by a copy, which can't raise
    // past the descriptors there are nor have soft over hard; and
    // readlink, EINVAL for a file that isn't a link and ENOENT for none.
    let infinity = u64::MAX;
    let expected = format!(
        "[\"Linux\", \"simplos\", \"6.1.0\", \"simplos {}\", \"x86_64\", \"(none)\"] \
         [0, 0, 0, 0] [0, -1] 8 1 -22 1 [1024, 1024] 0 -1 -22 exit status: 100 \
         [0, {infinity}] -22 -2\n",
        env!("CARGO_PKG_VERSION")
    );
    assert_eq!(run(&["system"]), ok(&expected));
}

#[test_case]
fn a_script_runs_through_what_its_first_line_names() {
    // With the line's argument, the script's path as it was given, and its
    // own arguments; through a script naming another; and a loop (ELOOP),
    // no interpreter, a missing one, and no #! at all.
    let expected = r#"["env", "/script", "a"] | ["env", "/spaced", "a"] | ["env", "/script", "/outer", "a"] | 40 | 8 | 2 | 8 | ["env", "./script", "a"]"#;
    assert_eq!(run(&["scripts"]), ok(&(String::from(expected) + "\n")));
}

#[test_case]
fn a_program_learns_about_the_machine_and_how_nice_it_is() {
    // sysinfo's uptime, memory, processes and unit; no groups besides its
    // own, getgroups refusing a negative count; nice set by process and by
    // group, kept within -20 to 19, inherited, and refused for no such
    // `which` (EINVAL) and no such process (ESRCH); sched_yield.
    assert_eq!(
        run(&["machine"]),
        ok("0 [true, true, true, true] [0, 0, -22] [20, 0, 15, 0, 23, 0, 1, -22, -3] \
            exit status: 15 0\n")
    );
}

#[test_case]
fn threads_share_their_program() {
    // What Linux says, from the same program: 4 x 10,000 sums and counts, a
    // Mutex's and an atomic's; five thread ids, the pid every thread's,
    // thread-locals their own; 1,000 Condvar turns; a wait that times out,
    // after its 50 ms; and park, until unparked.
    assert_eq!(
        run(&["threads"]),
        ok("199980000 40000 5 true true 1000 true true
")
    );
}

#[test_case]
fn futexes_wait_wake_and_requeue() {
    // What Linux says, from the same program: a word that isn't what's
    // expected, EAGAIN; a wait for 10 ms and one until a time past,
    // ETIMEDOUT; no bitset, and a word that isn't aligned, EINVAL; an op
    // there isn't, ENOSYS; a wake for no one, 0; a requeue for a word that
    // isn't, EAGAIN. Then one waiter woken, and two moved and woken there.
    assert_eq!(run(&["futex"]), ok("[-11, -110, -110, -22, -22, -38, 0, -11] 1 0 2 2 [0, 0]\n"));
}

#[test_case]
fn a_thread_ends_alone_and_exit_group_ends_them_all() {
    // What Linux says, from the same program: a thread's exit(5) ends the
    // program while the first waits for it; and the first's own exit, the
    // raw system call, leaves the other to go on and end it all.
    assert_eq!(run(&["thread-exit"]), ok("exit status: 5; after the first, exit status: 3\n"));
}

#[test_case]
fn a_thread_that_execs_takes_its_program_over() {
    // What Linux says, from the same program: the same pid, the thread
    // numbered as it, and the one thread.
    assert_eq!(run(&["thread-exec", "/bin/probe"]), ok("true true Threads:\t1\n"));
}

#[test_case]
fn a_thread_that_forks_makes_a_copy_with_one_thread() {
    assert_eq!(run(&["thread-fork"]), ok("child: true Threads:\t1\nparent: exit status: 4\n"));
}
