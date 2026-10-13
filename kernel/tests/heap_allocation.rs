//! The heap hands memory back out after it is freed, and keeps what is still
//! in use intact.
//!
//! Every `Box`, `Vec` and `String` in the kernel goes through the allocator in
//! `allocator/fixed_size_block.rs`: unsafe free lists, with a fallback heap for
//! anything too big for them. A bug there shows up as corruption somewhere
//! else entirely, so it is caught here instead.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use alloc::vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::allocator::HEAP_SIZE;

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

#[test_case]
fn freed_blocks_are_reused() {
    const BLOCK: usize = 4096;
    let long_lived = Box::new(42u64);
    // More blocks in total than the heap holds: this only finishes if freeing
    // one makes room for the next.
    for i in 0..HEAP_SIZE / BLOCK + 1000 {
        let block = Box::new([i as u8; BLOCK]);
        assert_eq!(block[BLOCK - 1], i as u8);
    }
    assert_eq!(*long_lived, 42, "a live allocation was handed out again");
}

#[test_case]
fn large_allocation() {
    // Too big for any block list, so this is the fallback heap's job.
    let buffer = vec![7u8; 1 << 20];
    assert!(buffer.iter().all(|&b| b == 7));
}
