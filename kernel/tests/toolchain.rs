//! GCC: gcc and g++, as `scripts/toolchain.sh` builds them and
//! `tools/boot-test` puts them on this kernel's disk, as it does for a
//! kernel whose name starts with `toolchain`, compiling programs on simplos
//! that then run on it: linked dynamically, to shared libraries musl's
//! loader loads, or statically.

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

/// The environment `/sbin/init` gives zsh, with its PATH: gcc finds where
/// it is on it, and from there what it runs, cc1 and the rest.
const ENVIRONMENT: [&str; 3] = ["HOME=/", "TERM=linux", "PATH=/sbin:/usr/sbin:/bin:/usr/bin"];

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
fn gcc_compiles_c_that_runs() {
    // cc1, as, collect2 and ld, read from the disk each time they run, under
    // emulation: half a minute on a fast machine.
    simplos::take_longer(270);
    write("tmp/add.h", "int add(int a, int b);\n");
    write("tmp/add.c", "#include \"add.h\"\nint add(int a, int b) { return a + b; }\n");
    write(
        "tmp/main.c",
        "#include <stdio.h>\n#include \"add.h\"\n\n\
         int main(void) {\n    printf(\"%d\\n\", add(40, 2));\n    return 0;\n}\n",
    );
    // Compiled and linked apart, then statically from an archive, and run
    // each time: the first to musl's libc.so, which its loader, named in
    // its headers, loads; the second with what it needs of libc.a in it.
    let script = "cd /tmp && gcc -O2 -c add.c && gcc -O2 -o sum main.c add.o && ./sum && \
                  ar rcs libadd.a add.o && cc -static -o sum2 main.c -L. -ladd && ./sum2 && \
                  readelf -l sum sum2 | grep interpreter";
    let loader = "      [Requesting program interpreter: /lib/ld-musl-x86_64.so.1]\n";
    assert_eq!(sh(script), ok(&format!("42\n42\n{loader}")));
}

#[test_case]
fn g_plus_plus_compiles_cxx_that_runs() {
    // cc1plus, and the headers <iostream> takes: as long.
    simplos::take_longer(270);
    write(
        "tmp/hello.cc",
        r#"#include <iostream>
#include <stdexcept>
#include <string>
#include <vector>

int main() {
    std::vector<std::string> words{"hello", "from", "g++"};
    std::string all;
    for (const auto& word : words) all += word + " ";
    try {
        throw std::runtime_error(all);
    } catch (const std::exception& e) {
        std::cout << e.what() << words.size() << std::endl;
    }
}
"#,
    );
    // To libstdc++.so, and libgcc_s.so, which unwinds; and statically.
    let script = "cd /tmp && g++ -O2 -o hello hello.cc && ./hello && \
                  g++ -O2 -static -o hello-static hello.cc && ./hello-static && \
                  readelf -d hello | grep NEEDED";
    let needed = [
        " 0x0000000000000001 (NEEDED)             Shared library: [libstdc++.so.6]\n",
        " 0x0000000000000001 (NEEDED)             Shared library: [libgcc_s.so.1]\n",
        " 0x0000000000000001 (NEEDED)             Shared library: [libc.so]\n",
    ];
    assert_eq!(sh(script), ok(&format!("hello from g++ 3\nhello from g++ 3\n{}", needed.concat())));
}

#[test_case]
fn gcc_makes_a_shared_library_programs_load() {
    simplos::take_longer(270);
    write("tmp/greet.h", "const char *greet(void);\n");
    write("tmp/greet.c", "#include \"greet.h\"\nconst char *greet(void) { return \"greeted\"; }\n");
    write(
        "tmp/greeted.c",
        "#include <stdio.h>\n#include \"greet.h\"\nint main(void) { puts(greet()); }\n",
    );
    // Not where musl's loader looks, until LD_LIBRARY_PATH says it is; and
    // what ldd, the loader by that name, says it loads, but where.
    let script = "cd /tmp && gcc -O2 -fPIC -shared -o libgreet.so greet.c && \
                  gcc -O2 -o greeted greeted.c -L. -lgreet && { ./greeted; echo $?; } && \
                  LD_LIBRARY_PATH=/tmp ./greeted && \
                  LD_LIBRARY_PATH=/tmp ldd ./greeted | sed 's/ (0x[0-9a-f]*)$//'";
    let expected = "Error loading shared library libgreet.so: No such file or directory \
                    (needed by ./greeted)\n\
                    Error relocating ./greeted: greet: symbol not found\n\
                    127\n\
                    greeted\n\
                    \t/lib/ld-musl-x86_64.so.1\n\
                    \tlibgreet.so => /tmp/libgreet.so\n\
                    \tlibc.so => /lib/ld-musl-x86_64.so.1\n";
    assert_eq!(sh(script), ok(expected));
}

#[test_case]
fn a_program_loads_a_library_as_it_runs() {
    simplos::take_longer(270);
    write("tmp/twice.c", "int twice(int n) { return 2 * n; }\n");
    write(
        "tmp/dl.c",
        r#"#include <dlfcn.h>
#include <stdio.h>

int main(void) {
    void *library = dlopen("/tmp/libtwice.so", RTLD_NOW);
    if (!library) return puts(dlerror()), 1;
    int (*twice)(int) = (int (*)(int))dlsym(library, "twice");
    printf("twice 21 is %d\n", twice(21));
    dlclose(library);
    if (!dlopen("/tmp/missing.so", RTLD_NOW)) puts(dlerror());
}
"#,
    );
    let script = "cd /tmp && gcc -O2 -fPIC -shared -o libtwice.so twice.c && \
                  gcc -O2 -o dl dl.c && ./dl";
    let expected = "twice 21 is 42\n\
                    Error loading shared library /tmp/missing.so: No such file or directory\n";
    assert_eq!(sh(script), ok(expected));
}

#[test_case]
fn gcc_compiles_threads_that_run() {
    // musl's pthreads, as a C program uses them: four threads under a
    // mutex, joined for what they return; a detached one that posts a
    // semaphore; pthread_kill, to a thread that sleeps until its handler
    // runs; and pthread_cancel, of one in pthread_cond_wait, which musl does
    // with a signal whose handler moves it out of the futex's system call.
    simplos::take_longer(270);
    write(
        "tmp/threads.c",
        r#"#include <pthread.h>
#include <semaphore.h>
#include <signal.h>
#include <stdio.h>
#include <unistd.h>

static pthread_mutex_t lock = PTHREAD_MUTEX_INITIALIZER;
static pthread_cond_t changed = PTHREAD_COND_INITIALIZER;
static long total;
static int waiting;
static sem_t posted;
static volatile sig_atomic_t poked;

static void *add(void *arg) {
    long n = (long)arg;
    for (int i = 0; i < 10000; i++) {
        pthread_mutex_lock(&lock);
        total += n;
        pthread_mutex_unlock(&lock);
    }
    return (void *)(n * 2);
}

static void *post(void *arg) {
    (void)arg;
    sem_post(&posted);
    return 0;
}

static void poke(int signal) {
    (void)signal;
    poked = 1;
}

static void *sleep_until_poked(void *arg) {
    (void)arg;
    while (!poked)
        usleep(1000);
    return (void *)1;
}

static void *wait_forever(void *arg) {
    (void)arg;
    pthread_mutex_lock(&lock);
    waiting = 1;
    pthread_cond_signal(&changed);
    for (;;)
        pthread_cond_wait(&changed, &lock);
    return 0;
}

int main(void) {
    pthread_t threads[4];
    long joined = 0;
    for (long i = 0; i < 4; i++)
        pthread_create(&threads[i], 0, add, (void *)(i + 1));
    for (int i = 0; i < 4; i++) {
        void *got;
        pthread_join(threads[i], &got);
        joined += (long)got;
    }
    printf("total %ld, joined %ld\n", total, joined);

    sem_init(&posted, 0, 0);
    pthread_attr_t detached;
    pthread_attr_init(&detached);
    pthread_attr_setdetachstate(&detached, PTHREAD_CREATE_DETACHED);
    pthread_t poster;
    pthread_create(&poster, &detached, post, 0);
    sem_wait(&posted);
    printf("posted\n");

    signal(SIGUSR1, poke);
    pthread_t sleeper;
    void *slept;
    pthread_create(&sleeper, 0, sleep_until_poked, 0);
    pthread_kill(sleeper, SIGUSR1);
    pthread_join(sleeper, &slept);
    printf("poked %ld\n", (long)slept);

    pthread_t waiter;
    void *how;
    pthread_create(&waiter, 0, wait_forever, 0);
    pthread_mutex_lock(&lock);
    while (!waiting)
        pthread_cond_wait(&changed, &lock);
    pthread_mutex_unlock(&lock);
    pthread_cancel(waiter);
    pthread_join(waiter, &how);
    printf("cancelled %d\n", how == PTHREAD_CANCELED);
    return 0;
}
"#,
    );
    let script = "cd /tmp && gcc -O2 -Wall -pthread -o threads threads.c && ./threads";
    assert_eq!(sh(script), ok("total 100000, joined 20\nposted\npoked 1\ncancelled 1\n"));
}
