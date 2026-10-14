//! File systems, as one tree: at the root, the first ext4 disk found, mounted
//! to read and write by [`init`], or with no disk an empty directory that
//! can't be written; and at /dev, the devices.
//!
//! Every call into them, through a path or through an open file, holds
//! [`ROOT`]'s lock ([`lock`]), and whoever waits for it sleeps.

use crate::io::disk;
use crate::serial_println;
use crate::thread::{SleepLock, SleepLockGuard};
use crate::tty::Tty;
use alloc::boxed::Box;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize};
use ext4::{BlockDevice, DirEntry, Error, Ext4, Kind, Metadata};

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
    /// A terminal: standard input, output and error, and /dev/tty.
    Tty {
        tty: Arc<Tty>,
        /// O_NONBLOCK: a read that would wait fails instead.
        nonblocking: AtomicBool,
    },
    /// /dev/null: nothing to read, and what's written goes nowhere.
    Null,
    /// /dev/zero: as many zeros as are read, and what's written goes nowhere.
    Zero,
    Pipe {
        end: crate::pipe::PipeEnd,
        /// O_NONBLOCK: a read or write that would wait fails instead.
        nonblocking: AtomicBool,
    },
    File {
        file: ext4::File,
        read: bool,
        write: bool,
        /// Every write goes at the end.
        append: bool,
        offset: AtomicU64,
    },
    Dir {
        /// The disk's handle on it, if it's on the disk.
        dir: Option<ext4::Dir>,
        /// As they were when it was opened, with what's mounted in it.
        entries: Vec<DirEntry>,
        /// From the root, for paths relative to it (`openat` and the like).
        path: String,
        /// The next entry `getdents64` hands out.
        next: AtomicUsize,
    },
}

impl Handle {
    pub fn tty(tty: Arc<Tty>) -> Handle {
        Handle::Tty { tty, nonblocking: AtomicBool::new(false) }
    }

    pub fn pipe(end: crate::pipe::PipeEnd) -> Handle {
        Handle::Pipe { end, nonblocking: AtomicBool::new(false) }
    }

    /// Its O_NONBLOCK, for what can make a program wait: none for the rest.
    pub fn nonblocking(&self) -> Option<&AtomicBool> {
        match self {
            Handle::Tty { nonblocking, .. } | Handle::Pipe { nonblocking, .. } => Some(nonblocking),
            _ => None,
        }
    }
}

/// What's in /dev: each device's name and inode number. The numbers are
/// ones ext4 keeps for itself and never shows (its bad-blocks list, quotas
/// and boot loader), so no file on the disk has them.
///
/// shortcut: just these three; device nodes on the disk aren't looked at.
const DEVICES: [(&str, u32); 3] = [("null", 3), ("tty", 4), ("zero", 5)];
/// /dev's own.
const DEV_INODE: u32 = 1;

#[derive(Clone, Copy)]
pub enum Device {
    Null,
    Zero,
    /// The terminal of whoever opens it.
    Tty,
}

/// The device at `path`, a path from the root, if it's one.
pub fn device(path: &str) -> Option<Device> {
    match on(path) {
        On::Devices("null") => Some(Device::Null),
        On::Devices("zero") => Some(Device::Zero),
        On::Devices("tty") => Some(Device::Tty),
        _ => None,
    }
}

/// What a device is, as `stat` tells it.
fn device_metadata(device: Device) -> Metadata {
    let (inode, mode) = match device {
        Device::Null => (3, 0o020666),
        Device::Zero => (5, 0o020666),
        Device::Tty => (4, 0o020620),
    };
    special_metadata(inode, mode)
}

/// What a handle that isn't a file on the disk is, as `stat` tells it.
pub fn special(handle: &Handle) -> Option<Metadata> {
    match handle {
        Handle::Tty { .. } => Some(device_metadata(Device::Tty)),
        Handle::Null => Some(device_metadata(Device::Null)),
        Handle::Zero => Some(device_metadata(Device::Zero)),
        // A FIFO.
        Handle::Pipe { .. } => Some(special_metadata(0, 0o010600)),
        Handle::File { .. } | Handle::Dir { .. } => None,
    }
}

/// Devices and pipes: their type goes in `permissions`, as ext4's `Kind`
/// has nothing for them.
fn special_metadata(inode: u32, mode: u32) -> Metadata {
    Metadata { kind: Kind::Other, links: 1, ..directory(inode, mode) }
}

/// A directory that isn't on the disk.
fn directory(inode: u32, permissions: u32) -> Metadata {
    Metadata {
        inode,
        kind: Kind::Directory,
        permissions,
        links: 2,
        uid: 0,
        gid: 0,
        size: 0,
        blocks: 0,
        accessed: 0,
        modified: 0,
        changed: 0,
    }
}

/// Which file system a path from the root is on: the devices, with the
/// name within /dev ("" for /dev itself), or else the disk.
enum On<'p> {
    Devices(&'p str),
    Disk,
}

fn on(path: &str) -> On<'_> {
    match path.strip_prefix("dev") {
        Some("") => On::Devices(""),
        Some(rest) => rest.strip_prefix('/').map_or(On::Disk, On::Devices),
        None => On::Disk,
    }
}

/// The whole tree, held: every call into a file system holds it, so that
/// what a system call looks at stays as it was while it acts on it.
pub fn lock() -> Tree<'static> {
    Tree { disk: ROOT.lock() }
}

pub struct Tree<'a> {
    disk: SleepLockGuard<'a, Option<Ext4>>,
}

impl Tree<'_> {
    /// What's at `path`, a path from the root.
    pub fn metadata(&self, path: &str) -> Result<Metadata, Error> {
        match on(path) {
            On::Devices("") => Ok(directory(DEV_INODE, 0o755)),
            On::Devices(_) => device(path).map(device_metadata).ok_or(Error::NotFound),
            On::Disk => match self.disk.as_ref() {
                Some(disk) => disk.metadata(path),
                // The empty root, with ext4's root's inode number.
                None if path.is_empty() => Ok(directory(2, 0o755)),
                None => Err(Error::NotFound),
            },
        }
    }

    /// The directory at `path`: the disk's handle on it if it's on the disk,
    /// and what's in it, with whatever is mounted there.
    pub fn open_dir(&self, path: &str) -> Result<(Option<ext4::Dir>, Vec<DirEntry>), Error> {
        let (dir, mut entries) = match on(path) {
            On::Devices("") => {
                let entry = |(name, inode): (&str, u32)| DirEntry {
                    name: name.into(),
                    kind: Kind::Other,
                    inode,
                };
                (None, DEVICES.into_iter().map(entry).collect())
            }
            On::Devices(_) => {
                return Err(self.metadata(path).err().unwrap_or(Error::NotADirectory));
            }
            On::Disk => match self.disk.as_ref() {
                Some(disk) => {
                    let dir = disk.open_dir(path)?;
                    let entries = dir.entries().to_vec();
                    (Some(dir), entries)
                }
                None if path.is_empty() => (None, Vec::new()),
                None => return Err(Error::NotFound),
            },
        };
        if path.is_empty() && !entries.iter().any(|entry| entry.name == "dev") {
            entries.push(DirEntry { name: "dev".into(), kind: Kind::Directory, inode: DEV_INODE });
        }
        Ok((dir, entries))
    }

    /// The disk, for what only it can do: making, changing and removing
    /// files. Neither /dev nor the root without a disk can be written.
    pub fn disk(&self, path: &str) -> Result<&Ext4, Error> {
        match on(path) {
            On::Devices(_) => Err(Error::ReadOnly),
            On::Disk => self.disk.as_ref().ok_or(Error::ReadOnly),
        }
    }
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
        NotEmpty => 39,
        Unsupported => 95,
        Other(errno) => errno.into(),
    }
}

#[test_case]
fn paths_go_to_the_file_system_they_are_on() {
    assert!(matches!(on("dev"), On::Devices("")));
    assert!(matches!(on("dev/null"), On::Devices("null")));
    assert!(matches!(on("devices/x"), On::Disk));
    assert!(matches!(on(""), On::Disk));
}

#[test_case]
fn paths_resolve_against_where_they_start() {
    assert_eq!(resolve("", "/a//b/./c/"), "a/b/c");
    assert_eq!(resolve("a/b", "c"), "a/b/c");
    assert_eq!(resolve("a/b", "../c"), "a/c");
    assert_eq!(resolve("a", "/../.."), "");
    assert_eq!(resolve("", "."), "");
}
