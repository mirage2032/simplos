//! File systems. For now that's one: the first ext4 disk found, mounted to
//! read and write by [`init`], as the root of every path.
//!
//! Every call into it, through a path or through an open file, holds
//! [`ROOT`]'s lock, and whoever waits for it sleeps.

use crate::io::disk;
use crate::serial_println;
use crate::thread::SleepLock;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, AtomicUsize};
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

/// What a file descriptor refers to. Copies of a descriptor share one, and
/// with it the place they're at in a file.
pub enum Handle {
    /// Standard input, output and error: the program's `output`, and no
    /// input.
    Console,
    File {
        file: ext4::File,
        read: bool,
        write: bool,
        /// Every write goes at the end.
        append: bool,
        offset: AtomicU64,
    },
    Dir {
        dir: ext4::Dir,
        /// From the root, for paths relative to it (`openat` and the like).
        path: String,
        /// The next entry `getdents64` hands out.
        next: AtomicUsize,
    },
}

/// `path` made absolute against the directory `base`, and with `.`, `..`
/// and repeated slashes taken out: a path from the root, without the leading
/// slash, as `Ext4` takes it.
///
/// shortcut: `..` is taken out by its name alone, which is right as long as
/// nothing on the way is a symbolic link; none are followed yet.
pub fn resolve(base: &str, path: &str) -> String {
    let mut parts: Vec<&str> = match path.starts_with('/') {
        true => Vec::new(),
        false => base.split('/').filter(|part| !part.is_empty()).collect(),
    };
    for part in path.split('/') {
        match part {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            name => parts.push(name),
        }
    }
    parts.join("/")
}

/// Linux's errno for `error`.
pub fn errno(error: ext4::Error) -> i64 {
    use ext4::Error::*;
    match error {
        NotFound => 2,
        Io | NeedsRecovery => 5,
        OutOfMemory | TooManyMounts => 12,
        Busy => 16,
        AlreadyExists => 17,
        NotADirectory => 20,
        IsADirectory => 21,
        InvalidArgument => 22,
        NoSpace => 28,
        ReadOnly => 30,
        Unsupported => 95,
        Other(errno) => errno.into(),
    }
}

#[test_case]
fn paths_resolve_against_where_they_start() {
    assert_eq!(resolve("", "/a//b/./c/"), "a/b/c");
    assert_eq!(resolve("a/b", "c"), "a/b/c");
    assert_eq!(resolve("a/b", "../c"), "a/c");
    assert_eq!(resolve("a", "/../.."), "");
    assert_eq!(resolve("", "."), "");
}
