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
use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::allocator;

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
    for i in 0..allocator::size() / BLOCK + 1000 {
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

#[test_case]
fn small_blocks_freed_can_be_had_again_in_one_piece() {
    // Three quarters of what's free, in small blocks, as the page cache's
    // index takes it; then freed, and half of it had again as one buffer,
    // which only works if the blocks went back to be joined up.
    let free = allocator::free();
    let count = free / 4 * 3 / 512;
    let mut blocks = Vec::with_capacity(count);
    for i in 0..count {
        blocks.push(Box::new([i as u8; 512]));
    }
    drop(blocks);
    let buffer = vec![7u8; free / 2];
    assert!(buffer.iter().all(|&b| b == 7));
}
