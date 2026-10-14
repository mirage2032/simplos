//! File systems. For now that's one: the first ext4 disk found, mounted to
//! read and write by [`init`].

use crate::io::disk;
use crate::serial_println;
use crate::thread::SleepLock;
use alloc::boxed::Box;
use ext4::{BlockDevice, Ext4};

/// The mounted file system, if a disk had one. Calls into it can wait on the
/// disk for a long time, so whoever waits for them sleeps.
pub static ROOT: SleepLock<Option<Ext4>> = SleepLock::new(None);

/// One of `io::disk`'s disks, by its number.
struct Disk {
    index: usize,
    sectors: u64,
}

impl BlockDevice for Disk {
    fn sectors(&self) -> u64 {
        self.sectors
    }

    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), ext4::Error> {
        disk::read(self.index, sector, buf).map_err(|_| ext4::Error::Io)
    }

    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), ext4::Error> {
        disk::write(self.index, sector, buf).map_err(|_| ext4::Error::Io)
    }
}

/// Mount the first disk with an ext4 (or ext3, or ext2) file system on it,
/// replaying its journal if it needs it. Disks without one are left alone:
/// mounting reads their superblock, and gives up there.
pub fn init() {
    for (index, info) in disk::list().into_iter().enumerate() {
        if let Ok(fs) = Ext4::mount_writable(Box::new(Disk { index, sectors: info.sectors })) {
            serial_println!("simplos: ext4 on disk {}", index);
            *ROOT.lock() = Some(fs);
            return;
        }
    }
}

/// Unmount it, leaving the disk clean: before the machine goes off.
pub fn unmount() {
    ROOT.lock().take();
}
