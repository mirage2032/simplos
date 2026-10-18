//! Hard disks: find them at boot and read or write raw 512-byte sectors.
//!
//! Two kinds of controller, both polled (no disk interrupts):
//! - [`ata`]: legacy IDE ports, what QEMU's default `pc` machine gives every `-drive`;
//! - [`ahci`]: SATA controllers found on PCI, as on most real PCs and QEMU's q35.
//!
//! Disks are numbered in the order they were found, IDE first. Nothing stops
//! a caller writing to the disk the kernel booted from.

pub mod ahci;
pub mod ata;

use crate::io::hpet;
use crate::serial_println;
use alloc::string::String;
use alloc::vec::Vec;
use spin::Mutex;

pub const SECTOR: usize = 512;

#[derive(Clone, Debug, Default)]
pub struct DiskInfo {
    /// `"ata"` or `"ahci"`.
    pub kind: &'static str,
    pub model: String,
    pub sectors: u64,
    /// The reads and writes made since boot, and their sectors.
    pub reads: u64,
    pub sectors_read: u64,
    pub writes: u64,
    pub sectors_written: u64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum DiskError {
    NoSuchDisk,
    /// The buffer is empty or not a whole number of sectors.
    BadLength,
    /// The transfer would run past the last sector.
    OutOfRange,
    /// The device refused the command; the value is its error register.
    Device(u8),
    Timeout,
}

/// How long a disk may take over one command: a flush waits for everything
/// in the disk's cache, and under QEMU for its image to reach the host's disk,
/// which can take far longer than a read, and still far less than this.
const TIMEOUT_MS: u64 = 10_000;

/// Poll `done` until it says so, or for `TIMEOUT_MS` by the HPET's clock.
fn poll(done: impl FnMut() -> bool) -> Result<(), DiskError> {
    poll_for(TIMEOUT_MS, done)
}

fn poll_for(ms: u64, mut done: impl FnMut() -> bool) -> Result<(), DiskError> {
    let start = hpet::elapsed_millis();
    let mut tries: u32 = 0;
    while !done() {
        tries = tries.wrapping_add(1);
        let late = match start {
            // The clock is a slow read, so not on every try.
            Some(start) => {
                tries.is_multiple_of(64)
                    && hpet::elapsed_millis().is_some_and(|now| now - start >= ms)
            }
            // shortcut: without an HPET, a million tries, which a fast CPU
            // makes in a few milliseconds; count the PIT's ticks instead if
            // a machine without one turns up.
            None => tries == 1_000_000,
        };
        if late {
            return Err(DiskError::Timeout);
        }
    }
    Ok(())
}

enum Disk {
    Ata(ata::Drive),
    Ahci(ahci::Port),
}

static DISKS: Mutex<Vec<(DiskInfo, Disk)>> = Mutex::new(Vec::new());

/// Find every disk and log one line for each. Called once from `pre_init`.
pub fn init() {
    let mut disks = DISKS.lock();
    disks.extend(ata::probe().into_iter().map(|(info, drive)| (info, Disk::Ata(drive))));
    disks.extend(ahci::probe().into_iter().map(|(info, port)| (info, Disk::Ahci(port))));
    for (index, (info, _)) in disks.iter().enumerate() {
        serial_println!(
            "simplos: disk {}: {} {:?} {} sectors",
            index,
            info.kind,
            info.model,
            info.sectors
        );
    }
}

pub fn list() -> Vec<DiskInfo> {
    DISKS.lock().iter().map(|(info, _)| info.clone()).collect()
}

/// Read `buf.len() / 512` sectors starting at `lba`.
pub fn read(disk: usize, lba: u64, buf: &mut [u8]) -> Result<(), DiskError> {
    let mut disks = DISKS.lock();
    let (info, disk) = disks.get_mut(disk).ok_or(DiskError::NoSuchDisk)?;
    check(info, lba, buf.len())?;
    match disk {
        Disk::Ata(drive) => drive.read(lba, buf),
        Disk::Ahci(port) => port.read(lba, buf),
    }?;
    info.reads += 1;
    info.sectors_read += (buf.len() / SECTOR) as u64;
    Ok(())
}

/// Write `buf.len() / 512` sectors starting at `lba`, and flush the disk's
/// write cache so they are really stored when this returns.
pub fn write(disk: usize, lba: u64, buf: &[u8]) -> Result<(), DiskError> {
    let mut disks = DISKS.lock();
    let (info, disk) = disks.get_mut(disk).ok_or(DiskError::NoSuchDisk)?;
    check(info, lba, buf.len())?;
    match disk {
        Disk::Ata(drive) => drive.write(lba, buf),
        Disk::Ahci(port) => port.write(lba, buf),
    }?;
    info.writes += 1;
    info.sectors_written += (buf.len() / SECTOR) as u64;
    Ok(())
}

fn check(info: &DiskInfo, lba: u64, len: usize) -> Result<(), DiskError> {
    if len == 0 || !len.is_multiple_of(SECTOR) {
        return Err(DiskError::BadLength);
    }
    match lba.checked_add((len / SECTOR) as u64) {
        Some(end) if end <= info.sectors => Ok(()),
        _ => Err(DiskError::OutOfRange),
    }
}

/// Model and sector count from an ATA IDENTIFY DEVICE response, or `None`
/// for a disk whose sectors aren't 512 bytes, which nothing here can address.
fn parse_identify(id: &[u16; 256]) -> Option<(String, u64)> {
    // Word 106 is only meaningful when bit 14 is set and bit 15 clear; bit 12
    // then says a logical sector is longer than 256 words.
    if id[106] & 0xC000 == 0x4000 && id[106] & (1 << 12) != 0 {
        return None;
    }
    // Two ASCII characters per word, the first in the high byte.
    let model: String = id[27..47].iter().flat_map(|w| w.to_be_bytes()).map(char::from).collect();
    let sectors = if id[83] & (1 << 10) != 0 {
        // LBA48: words 100-103, least significant first.
        id[100..104].iter().rev().fold(0, |acc, &w| acc << 16 | u64::from(w))
    } else {
        u64::from(id[60]) | u64::from(id[61]) << 16
    };
    Some((String::from(model.trim()), sectors))
}

/// IDENTIFY data as 256 words from the 512 bytes a disk sends.
fn words(bytes: &[u8]) -> [u16; 256] {
    let mut id = [0u16; 256];
    for (word, pair) in id.iter_mut().zip(bytes.as_chunks::<2>().0) {
        *word = u16::from_le_bytes(*pair);
    }
    id
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn a_wait_lasts_its_time_however_quick_each_try() {
        // A million of these tries take a few milliseconds; it's the clock
        // that has to say when to stop.
        let start = hpet::elapsed_millis().expect("QEMU's machines have an HPET");
        assert_eq!(poll_for(50, || false), Err(DiskError::Timeout));
        let took = hpet::elapsed_millis().unwrap() - start;
        assert!((50..1000).contains(&took), "took {took} ms");
        assert_eq!(poll_for(50, || true), Ok(()));
    }
}
