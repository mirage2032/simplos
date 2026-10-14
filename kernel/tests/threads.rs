//! Kernel threads take turns, each on a stack of its own, can't keep the CPU
//! to themselves, sleep and wait without using it, and give their stacks back
//! when they finish.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use alloc::sync::Arc;
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use simplos::io::hpet;
use simplos::memory::STACKS_START;
use simplos::thread::{SleepLock, WaitQueue, sleep, spawn, yield_now};
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
    // Each thread's turns in order. Not strictly a0 b0 a1 b1: a tick can
    // preempt a thread between starting and logging, giving the other two
    // turns in a row.
    let log = log.lock();
    for name in ['a', 'b'] {
        let turns: Vec<_> = log.iter().filter(|(n, _)| *n == name).map(|(_, t)| *t).collect();
        assert_eq!(turns, [0, 1, 2], "{name}'s turns, in {log:?}");
    }
}

#[test_case]
fn a_thread_that_never_yields_is_preempted() {
    let stop = Arc::new(AtomicBool::new(false));
    let spinning = stop.clone();
    spawn(move || {
        while !spinning.load(Ordering::Relaxed) {
            core::hint::spin_loop();
        }
    })
    .expect("memory for a stack");
    // Hands the CPU to the spinner. Only a timer tick gets it back: without
    // preemption this never returns, and the test times out.
    yield_now();
    stop.store(true, Ordering::Relaxed);
    wait_for(&stop);
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

#[test_case]
fn threads_switch_while_another_holds_the_heap() {
    // A thread that never yields and spends its time in the allocator, so a
    // tick usually preempts it holding the allocator's lock. Each round, the
    // thread that runs next allocates with interrupts off (`exit`, and the
    // spawn that follows, both do), and would wait for that lock forever
    // unless the heap only takes it with interrupts off.
    static FINISHED: AtomicUsize = AtomicUsize::new(0);
    let stop = Arc::new(AtomicBool::new(false));
    let hammering = stop.clone();
    spawn(move || {
        while !hammering.load(Ordering::Relaxed) {
            core::hint::black_box(Box::new([0u8; 64]));
        }
    })
    .expect("memory for a stack");
    for round in 1..=50 {
        // Captures nothing, so it neither allocates nor frees: once it is
        // back from its yield, right after the hammer's preemption, the next
        // heap access is `exit`'s.
        spawn(|| {
            yield_now();
            FINISHED.fetch_add(1, Ordering::Relaxed);
        })
        .expect("memory for a stack");
        while FINISHED.load(Ordering::Relaxed) < round {
            yield_now();
        }
    }
    stop.store(true, Ordering::Relaxed);
    wait_for(&stop);
}

#[test_case]
fn sleep_waits_at_least_as_long_as_asked() {
    // With no other thread ready, so the idle thread has to run meanwhile:
    // without one, there's nothing to switch to.
    let start = hpet::elapsed_millis().expect("QEMU has an HPET");
    sleep(50);
    let slept = hpet::elapsed_millis().unwrap() - start;
    assert!((50..250).contains(&slept), "asked for 50 ms, slept {slept}");
}

#[test_case]
fn a_waiting_thread_sleeps_until_woken() {
    static QUEUE: WaitQueue = WaitQueue::new();
    let go = Arc::new(AtomicBool::new(false));
    let done = Arc::new(AtomicBool::new(false));
    let (waiter_go, waiter_done) = (go.clone(), done.clone());
    spawn(move || {
        QUEUE.wait_until(|| waiter_go.load(Ordering::Relaxed));
        waiter_done.store(true, Ordering::Relaxed);
    })
    .expect("memory for a stack");
    for _ in 0..5 {
        yield_now();
    }
    // True now, but the waiter only looks again when woken: if it were
    // polling instead of asleep, it would be done after these turns.
    go.store(true, Ordering::Relaxed);
    for _ in 0..5 {
        yield_now();
    }
    assert!(!done.load(Ordering::Relaxed), "the waiter ran without being woken");
    QUEUE.wake_all();
    wait_for(&done);
    assert!(done.load(Ordering::Relaxed));
}

#[test_case]
fn an_endless_sleep_is_only_a_long_one() {
    // Programs can ask to sleep for practically ever (nanosleep takes up to
    // i64::MAX seconds), and working out when to wake mustn't overflow.
    let asleep = Arc::new(AtomicBool::new(false));
    let sleeper = asleep.clone();
    spawn(move || {
        sleeper.store(true, Ordering::Relaxed);
        sleep(u64::MAX);
    })
    .expect("memory for a stack");
    while !asleep.load(Ordering::Relaxed) {
        yield_now();
    }
}

#[test_case]
fn a_sleep_lock_lets_one_thread_in_at_a_time() {
    // Each thread reads the count, gives the CPU away while holding the lock,
    // and writes it back one more: had two been in at once, counts would be
    // lost.
    let count = Arc::new(SleepLock::new(0));
    for _ in 0..4 {
        let count = count.clone();
        spawn(move || {
            for _ in 0..20 {
                let mut held = count.lock();
                let seen = *held;
                yield_now();
                *held = seen + 1;
            }
        })
        .expect("memory for a stack");
    }
    wait_for(&count);
    assert_eq!(*count.lock(), 80);
}
