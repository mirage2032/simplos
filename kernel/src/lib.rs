#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![feature(abi_x86_interrupt)]
#![test_runner(crate::test_runner)]
#![reexport_test_harness_main = "test_main"]
extern crate alloc;

pub mod allocator;
pub mod io;
pub mod memory;
pub mod utils;

use core::ops::{Deref, DerefMut};
// pub use io::video::vga_buffer;
pub use io::serial;

use crate::io::{init_io, video};
use crate::memory::BootInfoFrameAllocator;
use crate::utils::imutex::IMutex;
use bootloader_api::config::{BootloaderConfig, Mapping};
use bootloader_api::info::FrameBuffer;
use bootloader_api::{entry_point, BootInfo};
use core::panic::PanicInfo;
use io::utils::qemu::{exit_qemu, QemuExitCode};
use spin::Lazy;
use x86_64::VirtAddr;

pub static BOOTLOADER_CONFIG: BootloaderConfig = {
    let mut config = BootloaderConfig::new_default();
    config.mappings.physical_memory = Some(Mapping::Dynamic);
    config.mappings.kernel_base = Mapping::FixedAddress(0x8000000000);
    config
};
pub static mut FRAMEBUFFER: Lazy<IMutex<Option<FrameBuffer>>> = Lazy::new(|| IMutex::new(None));
pub fn pre_init(boot_info: &'static mut BootInfo) {
    unsafe {
        init_io();
        let phys_mem_offset = VirtAddr::new(
            boot_info
                .physical_memory_offset
                .take()
                .expect("No physical memory offset found"),
        );
        let mut mapper = unsafe { memory::init(phys_mem_offset) };
        let mut frame_allocator =
            unsafe { BootInfoFrameAllocator::init(&boot_info.memory_regions) };
        allocator::init_heap(&mut mapper, &mut frame_allocator)
            .expect("heap initialization failed");
        let fb = boot_info.framebuffer.take().expect("No framebuffer found");
        video::init_video(&fb);
        
        #[allow(static_mut_refs)]
        FRAMEBUFFER
            .lock()
            .replace(fb);
        x86_64::instructions::interrupts::enable();
    }
}

#[cfg(test)]
entry_point!(test_kernel_main, config = &BOOTLOADER_CONFIG);
#[cfg(test)]
fn test_kernel_main(boot_info: &'static mut BootInfo) -> ! {
    pre_init(boot_info);
    test_main();
    hlt_loop()
}

pub trait Testable {
    fn run(&self) -> ();
}
impl<T> Testable for T
where
    T: Fn(),
{
    fn run(&self) {
        serial_print!("{}...\t", core::any::type_name::<T>());
        self();
        serial_println!("[ok]");
    }
}

pub fn test_runner(tests: &[&dyn Testable]) {
    serial_println!("Running {} tests", tests.len());
    for test in tests {
        test.run();
    }
    exit_qemu(QemuExitCode::Success);
}

pub fn test_panic_handler(info: &PanicInfo) -> ! {
    serial_println!("[failed]\n");
    serial_println!("Error: {}\n", info);
    exit_qemu(QemuExitCode::Failed);
    loop {}
}
#[cfg(test)]
#[panic_handler]
fn panic(info: &PanicInfo) -> ! {
    test_panic_handler(info)
}

pub fn hlt_loop() -> ! {
    loop {
        x86_64::instructions::hlt();
    }
}


#[unsafe(no_mangle)]
pub fn badoo() {
    let a = 1;
    let b = 2;
    let c = a + b;
    core::hint::black_box(c);
}