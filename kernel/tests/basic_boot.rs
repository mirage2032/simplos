//! The kernel boots and hands control to the tests.
//!
//! Everything else in `tests/` presumes this: that the bootloader's handoff,
//! `pre_init` and the framebuffer all work. When this fails, nothing else is
//! worth reading.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::boxed::Box;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::{BootMode, DISPLAY};

// Not a bare `_start`: the bootloader reads the kernel's configuration from a
// `.bootloader-config` ELF section and refuses to load an image without one,
// and `entry_point!` is what emits it.
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
fn display_is_initialized() {
    let display = DISPLAY.lock();
    assert!(display.is_initialized(), "pre_init left the display without a framebuffer");
}

#[test_case]
fn framebuffer_is_not_empty() {
    let area = {
        use embedded_graphics::geometry::Dimensions;
        DISPLAY.lock().bounding_box().size
    };
    assert!(area.width > 0 && area.height > 0, "framebuffer is {}x{}", area.width, area.height);
}

#[test_case]
fn boot_mode_is_the_firmware_that_ran() {
    // tools/boot-test boots every test under both firmwares and says which one
    // this is, so detection is checked against the truth, and a run meant for
    // one firmware can't quietly have tested the other.
    let mut name = [0u8; 8];
    let len = simplos::io::fwcfg::read_file("opt/simplos/firmware", &mut name)
        .expect("tools/boot-test passes opt/simplos/firmware");
    let expected = core::str::from_utf8(&name[..len]).unwrap().trim_end_matches('\0');
    assert_ne!(simplos::boot_mode(), BootMode::Unknown);
    assert_eq!(simplos::boot_mode().as_str(), expected);
}

#[test_case]
fn kernel_is_in_the_upper_half() {
    // Code, statics, the heap and the stack: everything the kernel runs on.
    let heap = Box::new(0u8);
    let stack = 0u8;
    let addresses = [
        simplos::pre_init as fn(_) as usize,
        &raw const DISPLAY as usize,
        &raw const *heap as usize,
        &raw const stack as usize,
    ];
    for address in addresses {
        assert!(
            address >= simplos::memory::PHYS_MAP_BASE as usize,
            "{address:#x} is in the lower half"
        );
    }
}

#[test_case]
fn lower_half_is_unmapped() {
    // Left to user programs. Anything the kernel still needed there would have
    // faulted long before this test, but a stray mapping wouldn't.
    let (pml4, _) = x86_64::registers::control::Cr3::read();
    let pml4 = simplos::memory::phys_ptr(pml4.start_address()).unwrap();
    // Safety: the active PML4, read through the physical-memory map.
    let pml4 = unsafe { &*(pml4 as *const x86_64::structures::paging::PageTable) };
    assert!(pml4.iter().take(256).all(|entry| entry.is_unused()));
}

#[test_case]
fn timer_ticks_at_timer_hz() {
    use core::sync::atomic::Ordering::Relaxed;
    use simplos::io::hpet;
    use simplos::io::interrupts::{idt::TIMER_COUNTER, pit::TIMER_HZ};

    // Count ticks across half a second of HPET time, a separate clock. Loose
    // bounds, since an emulated CPU can take a tick late; the PIT's power-on
    // rate would give about 9 here, far outside them.
    let start_ms = hpet::elapsed_millis().expect("QEMU has an HPET");
    let start_ticks = TIMER_COUNTER.load(Relaxed);
    while hpet::elapsed_millis().unwrap() - start_ms < 500 {
        x86_64::instructions::hlt();
    }
    let ticks = TIMER_COUNTER.load(Relaxed) - start_ticks;
    let expected = TIMER_HZ as usize / 2;
    assert!(
        (expected * 6 / 10..=expected * 14 / 10).contains(&ticks),
        "{ticks} ticks in 500 ms of HPET time, expected about {expected}"
    );
}

#[test_case]
fn ramdisk_holds_an_x86_64_executable() {
    // tools/boot-test attaches user/probe to every test boot, as fullkernel
    // does to the real images. Checked by hand here; the loader will parse it.
    let elf = simplos::ramdisk().expect("the image has a ramdisk");
    assert!(elf.len() > 64, "a {}-byte ramdisk", elf.len());
    assert_eq!(elf[..4], *b"\x7fELF", "not an ELF file");
    assert_eq!((elf[4], elf[5]), (2, 1), "not 64-bit little-endian");
    let kind = u16::from_le_bytes([elf[16], elf[17]]);
    assert!(kind == 2 || kind == 3, "ELF type {kind}, not an executable");
    assert_eq!(u16::from_le_bytes([elf[18], elf[19]]), 62, "not x86-64");
}
