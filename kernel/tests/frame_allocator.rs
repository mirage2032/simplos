//! Physical frames can be given back, and come out again.
//!
//! Processes will come and go, each taking frames for its memory and page
//! tables, so a frame that is freed and never reused is memory lost for good.
//! A test kernel of its own because it takes every frame there is.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::vec::Vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::memory::FRAMES;
use x86_64::structures::paging::{FrameAllocator, FrameDeallocator};

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
fn a_freed_frame_is_handed_out_next() {
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().expect("pre_init keeps the frame allocator");
    let frame = frames.allocate_frame().expect("memory left");
    // Safety: just allocated, and used for nothing.
    unsafe { frames.deallocate_frame(frame) };
    assert_eq!(frames.allocate_frame(), Some(frame));
    unsafe { frames.deallocate_frame(frame) };
}

#[test_case]
fn all_of_memory_comes_back() {
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().expect("pre_init keeps the frame allocator");
    // Take every frame there is, give them all back and take them all again:
    // the same frames both times, or freeing loses some or hands some out twice.
    // The heap is mapped up front, so this Vec doesn't need frames to grow.
    let mut taken = Vec::new();
    while let Some(frame) = frames.allocate_frame() {
        taken.push(frame);
    }
    assert!(taken.len() > 1000, "only {} frames free after boot", taken.len());
    let mut first = taken.clone();
    for frame in taken.drain(..) {
        // Safety: every one was just allocated and used for nothing.
        unsafe { frames.deallocate_frame(frame) };
    }
    while let Some(frame) = frames.allocate_frame() {
        taken.push(frame);
    }
    first.sort_unstable();
    taken.sort_unstable();
    assert!(first == taken, "{} frames the first time, {} the second", first.len(), taken.len());
    for frame in taken {
        unsafe { frames.deallocate_frame(frame) };
    }
}
