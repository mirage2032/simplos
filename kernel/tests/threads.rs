//! Kernel threads take turns, each on a stack of its own, and give their
//! stacks back when they finish.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::sync::Arc;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicU64, Ordering};
use simplos::memory::STACKS_START;
use simplos::thread::{spawn, yield_now};
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

/// Yield until every thread holding a clone of `shared` has finished with it.
fn wait_for<T>(shared: &Arc<T>) {
    while Arc::strong_count(shared) > 1 {
        yield_now();
    }
}

#[test_case]
fn threads_take_turns() {
    let log = Arc::new(Mutex::new(Vec::new()));
    for name in ['a', 'b'] {
        let log = log.clone();
        spawn(move || {
            for turn in 0..3 {
                log.lock().push((name, turn));
                yield_now();
            }
        })
        .expect("memory for a stack");
    }
    wait_for(&log);
    assert_eq!(*log.lock(), [('a', 0), ('b', 0), ('a', 1), ('b', 1), ('a', 2), ('b', 2)]);
}

#[test_case]
fn a_thread_runs_on_a_stack_of_its_own() {
    let seen = Arc::new(AtomicU64::new(0));
    let report = seen.clone();
    spawn(move || {
        let local = 0u8;
        report.store(&raw const local as u64, Ordering::Relaxed);
    })
    .expect("memory for a stack");
    wait_for(&seen);
    let address = seen.load(Ordering::Relaxed);
    // The stacks' PML4 entry covers 512 GiB.
    assert!((STACKS_START..STACKS_START + (1 << 39)).contains(&address), "stack at {address:#x}");
}

#[test_case]
fn finished_threads_give_their_stacks_back() {
    // 2000 stacks of 80 KiB is 160 MiB, more than the tests' 128 MiB machine
    // has: this only finishes if each finished thread's stack is freed.
    for _ in 0..2000 {
        let done = Arc::new(());
        let held = done.clone();
        spawn(move || drop(held)).expect("memory for a stack, so stacks aren't being freed");
        wait_for(&done);
    }
}
