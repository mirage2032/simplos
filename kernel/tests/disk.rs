//! Disks are found on both controllers, and raw sectors read and write.
//!
//! `tools/boot-test` attaches two scratch drives to every test run besides the
//! boot disk: 1 MiB on the IDE channel and 2 MiB on an AHCI controller. Their
//! sizes are how this test tells them apart. Only they are written to.

#![no_std]
#![no_main]
#![feature(custom_test_frameworks)]
#![test_runner(simplos::test_runner)]
#![reexport_test_harness_main = "test_main"]

extern crate alloc;

use alloc::vec;
use bootloader_api::{BootInfo, entry_point};
use core::panic::PanicInfo;
use simplos::io::disk::{self, DiskError, SECTOR};

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

const ATA_SCRATCH_SECTORS: u64 = 1024 * 1024 / SECTOR as u64;
const AHCI_SCRATCH_SECTORS: u64 = 2 * 1024 * 1024 / SECTOR as u64;

/// The index of the disk of this kind and size.
fn find(kind: &str, sectors: u64) -> usize {
    disk::list()
        .iter()
        .position(|d| d.kind == kind && d.sectors == sectors)
        .unwrap_or_else(|| panic!("no {kind} disk of {sectors} sectors in {:?}", disk::list()))
}

#[test_case]
fn both_controllers_are_found() {
    find("ata", ATA_SCRATCH_SECTORS);
    find("ahci", AHCI_SCRATCH_SECTORS);
}

#[test_case]
fn boot_disk_has_a_boot_signature() {
    // The boot image is disk 0: the first drive on the first IDE channel.
    let mut sector = [0u8; SECTOR];
    disk::read(0, 0, &mut sector).unwrap();
    assert_eq!(sector[510..], [0x55, 0xAA]);
}

#[test_case]
fn written_sectors_read_back() {
    // 21 sectors: not a multiple of the AHCI bounce buffer, so the last
    // command is a partial one.
    const SECTORS: usize = 21;
    for index in [find("ata", ATA_SCRATCH_SECTORS), find("ahci", AHCI_SCRATCH_SECTORS)] {
        let pattern: alloc::vec::Vec<u8> =
            (0..SECTORS * SECTOR).map(|i| (i * 7 + i / SECTOR) as u8).collect();
        disk::write(index, 100, &pattern).unwrap();
        let mut back = vec![0u8; pattern.len()];
        disk::read(index, 100, &mut back).unwrap();
        assert!(back == pattern, "disk {index} read back different data");
    }
}

#[test_case]
fn bad_requests_are_refused() {
    let index = find("ata", ATA_SCRATCH_SECTORS);
    let sector = [0u8; SECTOR];
    assert_eq!(disk::write(index, ATA_SCRATCH_SECTORS, &sector), Err(DiskError::OutOfRange));
    assert_eq!(disk::write(index, 0, &sector[..100]), Err(DiskError::BadLength));
    assert_eq!(disk::write(99, 0, &sector), Err(DiskError::NoSuchDisk));
}
