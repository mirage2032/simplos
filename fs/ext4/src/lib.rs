//! ext4 file systems (and ext3 and ext2) for simplos.
//!
//! For now the work is done by lwext4 (`vendor/lwext4`), in C. Nothing of it
//! shows through this interface, so it can be replaced by Rust code without
//! touching callers or tests.
//!
//! lwext4 keeps its state in globals (mounts, devices, caches) and isn't
//! built for threads, so every call into it holds one lock. In the kernel,
//! callers should wait on a sleeping lock of their own first, so that a
//! thread waiting on the disk doesn't leave others spinning on this one.
//!
//! Open files and directories own their place in the mount, so they can be
//! kept by whatever outlives a borrow, such as a program's file descriptors.
//!
//! Every change is on the device when the call making it returns, but for
//! small writes to a file, one after another: lwext4 makes each call a
//! transaction of its own, a handful of the device's writes however little
//! it writes, so those are held in memory (`Pending`) and written as one,
//! before anything else is done on the file system, as the file is closed,
//! or on `sync`. With a journal, each change is a transaction there, so a
//! crash leaves it whole or not made, and lwext4 writes the blocks to their
//! places in its own time; without one, lwext4's cache writes them through.
//! (The data in files isn't journaled, as with Linux's default
//! `data=ordered`: a crash in the middle of overwriting a file can leave it
//! part old, part new. And removing a file over 16 MiB takes a transaction
//! per 16 MiB: a crash can leave it there, shorter.)
//!
//! The file system on a device is trusted for now: lwext4 is C, parsing it
//! in ring 0. Fuzz this before mounting disks from elsewhere.

#![no_std]

extern crate alloc;

mod memory;

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;
use core::ffi::{CStr, c_char, c_int, c_void};
use core::sync::atomic::{AtomicU32, Ordering};
use spin::{Mutex, MutexGuard};

pub const SECTOR: usize = 512;

/// Where a file system lives: a disk, a partition, an image in memory.
pub trait BlockDevice: Send {
    /// How many 512-byte sectors it has.
    fn sectors(&self) -> u64;
    /// Fill `buf`, a whole number of sectors, from `sector` on. A device
    /// that fails says `Error::Io`.
    fn read(&mut self, sector: u64, buf: &mut [u8]) -> Result<(), Error>;
    /// Write `buf`, a whole number of sectors, from `sector` on.
    fn write(&mut self, sector: u64, buf: &[u8]) -> Result<(), Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Error {
    NotFound,
    NotADirectory,
    IsADirectory,
    /// The file system uses a feature this driver can't handle.
    Unsupported,
    /// Not unmounted cleanly: its journal holds changes still to be applied,
    /// which only a writable mount does.
    NeedsRecovery,
    AlreadyExists,
    /// The file system is mounted read-only, or the file was opened only
    /// for reading.
    ReadOnly,
    NoSpace,
    /// Such as moving a directory into itself.
    InvalidArgument,
    /// Such as removing a file that's open.
    Busy,
    /// A directory that would have to be empty isn't.
    NotEmpty,
    /// Such as a hard link to a directory.
    NotPermitted,
    /// As many file systems are mounted as can be.
    TooManyMounts,
    OutOfMemory,
    /// The device failed, or the file system is damaged.
    Io,
    /// Any other errno lwext4 returned.
    Other(i32),
}

fn check(errno: c_int) -> Result<(), Error> {
    // lwext4's own errno values, which are Linux's.
    Err(match errno {
        0 => return Ok(()),
        1 => Error::NotPermitted,
        2 => Error::NotFound,
        5 => Error::Io,
        12 => Error::OutOfMemory,
        16 => Error::Busy,
        17 => Error::AlreadyExists,
        20 => Error::NotADirectory,
        21 => Error::IsADirectory,
        22 => Error::InvalidArgument,
        28 => Error::NoSpace,
        30 => Error::ReadOnly,
        39 => Error::NotEmpty,
        95 => Error::Unsupported,
        other => Error::Other(other),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    File,
    Directory,
    Symlink,
    /// Devices, pipes and sockets: nothing reads them through a file system.
    Other,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    pub kind: Kind,
    pub inode: u32,
}

/// What `stat` tells of a file.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub inode: u32,
    pub kind: Kind,
    /// The permission bits: `0o755` and the like.
    pub permissions: u32,
    pub links: u32,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    /// Space it takes, in 512-byte units, as `stat` counts it.
    pub blocks: u64,
    /// Seconds since 1970, by the clock `set_clock` gave, or by whatever
    /// set them last; 0 for never.
    pub accessed: u32,
    pub modified: u32,
    pub changed: u32,
}

/// How big a file system is and how much of it is free, as `statfs` tells
/// it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Usage {
    pub block_size: u32,
    pub blocks: u64,
    pub free_blocks: u64,
    pub inodes: u32,
    pub free_inodes: u32,
}

/// What to change of a file, each `None` left as it is. Its change time is
/// set too, as Linux sets it.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
pub struct Changes {
    pub permissions: Option<u32>,
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    /// Seconds since 1970: not 0, which leaves it.
    pub accessed: Option<u32>,
    pub modified: Option<u32>,
}

/// What tells the time, in seconds since 1970, for files' times.
static CLOCK: Mutex<Option<fn() -> u32>> = Mutex::new(None);

/// Have files' times set by `now` from here on: all three as one is made,
/// its modification time as it's written to or cut, its change time as
/// anything about it changes. Until then they're left as they are.
pub fn set_clock(now: fn() -> u32) {
    *CLOCK.lock() = Some(now);
}

fn now() -> Option<u32> {
    CLOCK.lock().map(|now| now())
}

/// Which of lwext4's mount slots are taken. Every call into lwext4 holds
/// this lock, not only mounting.
static LWEXT4: Mutex<[bool; MOUNTS]> = Mutex::new([false; MOUNTS]);

/// How many file systems can be mounted at once: lwext4's
/// CONFIG_EXT4_MOUNTPOINTS_COUNT and CONFIG_EXT4_BLOCKDEVS_COUNT, set in
/// build.rs.
pub const MOUNTS: usize = 4;

/// A mounted file system. Files and directories opened on it keep it
/// mounted until they're closed too.
pub struct Ext4 {
    mount: Arc<Mount>,
}

/// What a mount holds, unmounted when the last `Ext4`, `File` or `Dir`
/// holding it is gone.
struct Mount {
    slot: usize,
    /// lwext4's names for the device and where it is mounted.
    device_name: CString,
    mount_point: CString,
    device: *mut ffi::BlockDev,
    writable: bool,
    /// How far `attach` got, for `Drop` to undo.
    registered: bool,
    mounted: bool,
    journaling: bool,
    /// Whether written blocks can wait in memory: with a journal.
    write_back: bool,
    /// Boxed twice so lwext4 can hold a thin pointer to it.
    _ours: Box<Box<dyn BlockDevice>>,
    /// The inodes of the files and directories open on it, once for each
    /// time one's open.
    open: Mutex<Vec<u32>>,
    /// The inodes of the open files `Ext4::keep` keeps, whose other names
    /// are gone.
    kept: Mutex<Vec<u32>>,
    /// Small writes to a file, held to be written as one.
    pending: Mutex<Option<Pending>>,
    /// Why what was held couldn't be written, for the next `sync` to tell.
    failed: Mutex<Option<Error>>,
}

/// Writes to an open file, one after another from `offset` on, not made
/// yet: by `Mount::lock`, before anything else is done on the mount, so
/// nothing sees the file without them.
///
/// shortcut: one file's at a time, so writing two in turn writes each as
/// it comes; hold some for each open file if a program needs that.
struct Pending {
    file: *mut ffi::File,
    inode: u32,
    offset: u64,
    data: Vec<u8>,
}

impl Pending {
    fn end(&self) -> u64 {
        self.offset + self.data.len() as u64
    }
}

/// How much `Pending` holds at most: more is written as it comes.
const HOLD: usize = 64 << 10;

// Safety: lwext4's state is only touched with `LWEXT4` held.
unsafe impl Send for Mount {}
unsafe impl Sync for Mount {}

impl Ext4 {
    /// Mount the file system on `device`, read-only. Nothing is written to
    /// it, so one that needs its journal recovered is refused.
    pub fn mount(device: Box<dyn BlockDevice>) -> Result<Ext4, Error> {
        Ext4::mount_as(device, false)
    }

    /// Mount the file system on `device` to read and write, first applying
    /// whatever its journal holds.
    pub fn mount_writable(device: Box<dyn BlockDevice>) -> Result<Ext4, Error> {
        let fs = Ext4::mount_as(device, true)?;
        // What `keep` kept as the power went, its files closed by it. One
        // that won't go only takes space.
        for entry in fs.read_dir(KEPT_IN).unwrap_or_default() {
            if entry.name.starts_with(KEPT_AS) && entry.kind == Kind::File {
                let _ = fs.remove_file(&format!("{KEPT_IN}/{}", entry.name));
            }
        }
        Ok(fs)
    }

    fn mount_as(device: Box<dyn BlockDevice>, writable: bool) -> Result<Ext4, Error> {
        let mut slots = LWEXT4.lock();
        let slot = slots.iter().position(|used| !used).ok_or(Error::TooManyMounts)?;
        let ours = Box::new(device);
        let pointer = (&*ours as *const Box<dyn BlockDevice>).cast_mut().cast();
        // Safety: `ours` outlives the device, which `Drop` frees.
        let device = unsafe { ffi::simplos_ext4_device_new(pointer, ours.sectors()) };
        if device.is_null() {
            return Err(Error::OutOfMemory);
        }
        let mut mount = Mount {
            slot,
            device_name: CString::new(format!("sd{slot}")).unwrap(),
            mount_point: CString::new(format!("/mp{slot}/")).unwrap(),
            device,
            writable,
            registered: false,
            mounted: false,
            journaling: false,
            write_back: false,
            _ours: ours,
            open: Mutex::new(Vec::new()),
            kept: Mutex::new(Vec::new()),
            pending: Mutex::new(None),
            failed: Mutex::new(None),
        };
        // The slot stays taken until `mount`'s `Drop` has undone whatever
        // `attach` managed, if it fails: that takes the lock, so it runs
        // after this function lets go of it.
        slots[slot] = true;
        let mounted = mount.attach();
        drop(slots);
        mounted.map(|()| Ext4 { mount: Arc::new(mount) })
    }

    /// lwext4's path for `path`, which is relative to the file system's root.
    fn path(&self, path: &str) -> Result<CString, Error> {
        let mount_point = self.mount.mount_point.to_str().unwrap();
        let full = format!("{mount_point}{}", path.trim_start_matches('/'));
        CString::new(full).map_err(|_| Error::NotFound)
    }

    /// What's known of the file at `path`. A symbolic link is described
    /// itself, not followed.
    pub fn metadata(&self, path: &str) -> Result<Metadata, Error> {
        let path = self.path(path)?;
        let _lwext4 = self.mount.lock();
        let mut inode = 0;
        // Safety: a NUL-terminated path on our mount.
        check(unsafe { ffi::simplos_ext4_inode_of(path.as_ptr(), &mut inode) })?;
        self.mount.stat(inode)
    }

    /// The entries of the directory at `path`, without `.` and `..`.
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, Error> {
        Ok(core::mem::take(&mut self.open_dir(path)?.entries))
    }

    /// Open the directory at `path`, as it is now.
    pub fn open_dir(&self, path: &str) -> Result<Dir, Error> {
        // lwext4 says ENOENT for a file; Linux says ENOTDIR.
        let metadata = self.metadata(path)?;
        if metadata.kind != Kind::Directory {
            return Err(Error::NotADirectory);
        }
        let path = self.path(path)?;
        let _lwext4 = self.mount.lock();
        let mut error = 0;
        // Safety: as in `metadata`.
        let dir = unsafe { ffi::simplos_ext4_dir_open(path.as_ptr(), &mut error) };
        if dir.is_null() {
            return Err(check(error).err().unwrap_or(Error::Io));
        }
        let mut entries = Vec::new();
        let mut name = [0u8; 255];
        let (mut kind, mut inode) = (0, 0);
        loop {
            // Safety: `dir` is open; `name` holds the longest name ext4 has.
            let length = unsafe {
                let name = name.as_mut_ptr().cast();
                ffi::simplos_ext4_dir_next(dir, name, 255, &mut kind, &mut inode)
            };
            let Ok(length) = usize::try_from(length) else { break };
            let name = &name[..length];
            if name == b"." || name == b".." {
                continue;
            }
            let kind = match kind {
                1 => Kind::File,
                2 => Kind::Directory,
                7 => Kind::Symlink,
                _ => Kind::Other,
            };
            let name = String::from_utf8_lossy(name).into_owned();
            entries.push(DirEntry { name, kind, inode });
        }
        // Safety: opened above, closed once.
        unsafe { ffi::simplos_ext4_dir_close(dir) };
        self.mount.open.lock().push(metadata.inode);
        Ok(Dir { mount: self.mount.clone(), inode: metadata.inode, entries })
    }

    /// Open the file at `path` for reading.
    pub fn open(&self, path: &str) -> Result<File, Error> {
        self.open_with(path, O_RDONLY)
    }

    /// Open the file at `path` for reading and writing.
    pub fn open_writable(&self, path: &str) -> Result<File, Error> {
        self.open_with(path, O_RDWR)
    }

    /// Open the file at `path` for reading and writing, emptied first, or
    /// made if there's none.
    pub fn create(&self, path: &str) -> Result<File, Error> {
        self.open_with(path, O_RDWR | O_CREAT | O_TRUNC)
    }

    /// `flags` are open(2)'s.
    fn open_with(&self, path: &str, flags: c_int) -> Result<File, Error> {
        let writable = flags & O_RDWR != 0;
        if writable && !self.mount.writable {
            return Err(Error::ReadOnly);
        }
        let made = match self.metadata(path) {
            Ok(Metadata { kind: Kind::File, .. }) => false,
            Ok(Metadata { kind: Kind::Directory, .. }) => return Err(Error::IsADirectory),
            Ok(_) => return Err(Error::Unsupported),
            Err(Error::NotFound) if flags & O_CREAT != 0 => {
                self.check_parent(path)?;
                true
            }
            Err(error) => return Err(error),
        };
        let path = self.path(path)?;
        let _lwext4 = self.mount.lock();
        let mut error = 0;
        // Safety: as in `metadata`.
        let raw = unsafe { ffi::simplos_ext4_file_open(path.as_ptr(), flags, &mut error) };
        if raw.is_null() {
            return Err(check(error).err().unwrap_or(Error::Io));
        }
        // Safety: just opened.
        let inode = unsafe { ffi::simplos_ext4_file_inode(raw) };
        self.mount.open.lock().push(inode);
        let file = File { mount: self.mount.clone(), raw, inode, writable, touched: 0.into() };
        // A new file has all its times set; one emptied, when.
        match (made, flags & O_TRUNC != 0) {
            (true, _) => self.mount.stamp(inode)?,
            (false, true) => file.touch()?,
            (false, false) => {}
        }
        // Not dropped while the lock is held: closing it takes the lock.
        drop(_lwext4);
        Ok(file)
    }

    /// Make a directory at `path`.
    pub fn create_dir(&self, path: &str) -> Result<(), Error> {
        // lwext4 says nothing to a directory that's there already.
        if self.metadata(path).is_ok() {
            return Err(Error::AlreadyExists);
        }
        self.check_parent(path)?;
        let at = self.path(path)?;
        let lwext4 = self.mount.lock();
        // Safety: as in `metadata`.
        check(unsafe { ffi::ext4_dir_mk(at.as_ptr()) })?;
        drop(lwext4);
        self.stamp(path)
    }

    /// Set all of a new file's times to now.
    fn stamp(&self, path: &str) -> Result<(), Error> {
        let inode = self.metadata(path)?.inode;
        let _lwext4 = self.mount.lock();
        self.mount.stamp(inode)
    }

    /// Change what `changes` says of the file at `path`, and its change
    /// time. A symbolic link is changed itself, not followed.
    pub fn change(&self, path: &str, changes: Changes) -> Result<(), Error> {
        let inode = self.metadata(path)?.inode;
        let _lwext4 = self.mount.lock();
        self.mount.change(inode, changes)
    }

    /// Remove the file at `path`. One that's open stays until it's closed,
    /// as on Linux: see `keep`.
    pub fn remove_file(&self, path: &str) -> Result<(), Error> {
        let metadata = self.metadata(path)?;
        // lwext4 says nothing to a directory, and leaves it.
        if metadata.kind == Kind::Directory {
            return Err(Error::IsADirectory);
        }
        let path = self.path(path)?;
        let _lwext4 = self.mount.lock();
        self.keep(&path, &metadata)?;
        // Safety: as in `metadata`.
        check(unsafe { ffi::ext4_fremove(path.as_ptr()) })
    }

    /// Before the file at `at`, lwext4's path, loses its last name: if it's
    /// open, give it another, which `File`'s `drop` takes away as the last
    /// of it closes. lwext4 would free it at once, under whoever has it
    /// open. Busy if that other name is the one going, or can't be given;
    /// `LWEXT4` is held.
    ///
    /// shortcut: Linux keeps such a file on ext4's orphan list, which lwext4
    /// knows nothing of; here it's in /lost+found as `.open-` and its inode
    /// while open, and taken out as the disk is next mounted if the power
    /// went first. Upgrade if anything minds it there.
    fn keep(&self, at: &CStr, metadata: &Metadata) -> Result<(), Error> {
        let open = self.mount.open.lock();
        let mut kept = self.mount.kept.lock();
        if metadata.links > 1 || !open.contains(&metadata.inode) {
            return Ok(());
        }
        if kept.contains(&metadata.inode) {
            return Err(Error::Busy);
        }
        let keeper = self.mount.keeper(metadata.inode);
        // Safety: as in `rename`.
        check(unsafe { ffi::ext4_flink(at.as_ptr(), keeper.as_ptr()) }).map_err(|_| Error::Busy)?;
        kept.push(metadata.inode);
        Ok(())
    }

    /// Remove the directory at `path`, which must be empty and not open.
    pub fn remove_dir(&self, path: &str) -> Result<(), Error> {
        const ROOT: u32 = 2;
        let metadata = self.metadata(path)?;
        if metadata.kind != Kind::Directory {
            return Err(Error::NotADirectory);
        }
        if metadata.inode == ROOT {
            return Err(Error::Busy);
        }
        // lwext4 would remove whatever is in it too.
        if !self.read_dir(path)?.is_empty() {
            return Err(Error::NotEmpty);
        }
        let path = self.path(path)?;
        let _lwext4 = self.mount.lock();
        if self.mount.open.lock().contains(&metadata.inode) {
            return Err(Error::Busy);
        }
        // Safety: as in `metadata`.
        check(unsafe { ffi::ext4_dir_rm(path.as_ptr()) })
    }

    /// Move the file or directory at `from` to `to`. Whatever is at `to`
    /// already goes: a file in the same transaction, as Linux does it; an
    /// empty directory, when a directory replaces it, just before.
    ///
    /// shortcut: a directory replacing one isn't one transaction, so a crash
    /// in between leaves neither at `to`; Linux has nothing of the kind.
    pub fn rename(&self, from: &str, to: &str) -> Result<(), Error> {
        let (from, to) = (from.trim_matches('/'), to.trim_matches('/'));
        // The root stays where it is.
        if from.is_empty() || to.is_empty() {
            return Err(Error::Busy);
        }
        // lwext4 would cut the directory off from the root, in a loop.
        if to.strip_prefix(from).is_some_and(|rest| rest.starts_with('/')) {
            return Err(Error::InvalidArgument);
        }
        let source = self.metadata(from)?;
        let replaced = match self.metadata(to) {
            Err(Error::NotFound) => None,
            Err(error) => return Err(error),
            // Another name for the same file: nothing to do, as on Linux.
            Ok(target) if target.inode == source.inode => return Ok(()),
            Ok(target) => match (source.kind, target.kind) {
                (Kind::Directory, Kind::Directory) => {
                    self.remove_dir(to)?;
                    None
                }
                (Kind::Directory, _) => return Err(Error::NotADirectory),
                (_, Kind::Directory) => return Err(Error::IsADirectory),
                _ => Some(target),
            },
        };
        self.check_parent(to)?;
        let (from, to) = (self.path(from)?, self.path(to)?);
        let _lwext4 = self.mount.lock();
        if let Some(replaced) = replaced {
            self.keep(&to, &replaced)?;
        }
        // Safety: as in `metadata`.
        check(unsafe { ffi::ext4_frename(from.as_ptr(), to.as_ptr()) })
    }

    /// Set the permission bits of the file at `path`: `0o644` and the like.
    pub fn set_permissions(&self, path: &str, permissions: u32) -> Result<(), Error> {
        self.change(path, Changes { permissions: Some(permissions), ..Changes::default() })
    }

    /// Make a symbolic link at `path` to `target`, which can be anything:
    /// it's only read as the link is followed.
    pub fn symlink(&self, target: &[u8], path: &str) -> Result<(), Error> {
        if self.metadata(path).is_ok() {
            return Err(Error::AlreadyExists);
        }
        self.check_parent(path)?;
        let target = CString::new(target).map_err(|_| Error::InvalidArgument)?;
        let at = self.path(path)?;
        let lwext4 = self.mount.lock();
        // Safety: two NUL-terminated strings, the second a path on our mount.
        check(unsafe { ffi::ext4_fsymlink(target.as_ptr(), at.as_ptr()) })?;
        drop(lwext4);
        self.stamp(path)
    }

    /// How big it is and how much of it is free.
    pub fn usage(&self) -> Result<Usage, Error> {
        let _lwext4 = self.mount.lock();
        self.mount.usage()
    }

    /// Write the small writes held to be made as one: every change made on
    /// it is on the device once this returns. An error if any held since the
    /// last `sync` couldn't be, here or as something else wrote them first,
    /// as Linux's `fsync` tells what its writing back couldn't write.
    pub fn sync(&self) -> Result<(), Error> {
        let _lwext4 = self.mount.lock();
        self.mount.failed.lock().take().map_or(Ok(()), Err)
    }

    /// Write the small writes held to be made as one, as `sync` does, but
    /// leave telling whether they could be to it.
    pub fn flush(&self) {
        drop(self.mount.lock());
    }

    /// Where the symbolic link at `path` points, as it was given.
    pub fn read_link(&self, path: &str) -> Result<Vec<u8>, Error> {
        let metadata = self.metadata(path)?;
        if metadata.kind != Kind::Symlink {
            return Err(Error::InvalidArgument);
        }
        let path = self.path(path)?;
        let mut target = alloc::vec![0u8; metadata.size as usize];
        let mut read = 0;
        let _lwext4 = self.mount.lock();
        // Safety: as in `metadata`, and a buffer of the length given.
        check(unsafe {
            ffi::ext4_readlink(path.as_ptr(), target.as_mut_ptr().cast(), target.len(), &mut read)
        })?;
        target.truncate(read);
        Ok(target)
    }

    /// Another name, `to`, for the file at `from`, which can't be a
    /// directory, as on Linux.
    pub fn link(&self, from: &str, to: &str) -> Result<(), Error> {
        if self.metadata(from)?.kind == Kind::Directory {
            return Err(Error::NotPermitted);
        }
        if self.metadata(to).is_ok() {
            return Err(Error::AlreadyExists);
        }
        self.check_parent(to)?;
        let (from_at, to_at) = (self.path(from)?, self.path(to)?);
        let lwext4 = self.mount.lock();
        // Safety: as in `rename`.
        check(unsafe { ffi::ext4_flink(from_at.as_ptr(), to_at.as_ptr()) })?;
        drop(lwext4);
        // One more name: a change to it.
        self.change(from, Changes::default())
    }

    /// That the directory `path` would go in is there: lwext4 makes any
    /// that are missing, where Linux says ENOENT.
    fn check_parent(&self, path: &str) -> Result<(), Error> {
        let parent = path.trim_end_matches('/').rsplit_once('/').map_or("", |(parent, _)| parent);
        match self.metadata(parent)?.kind {
            Kind::Directory => Ok(()),
            _ => Err(Error::NotADirectory),
        }
    }
}

// open(2)'s flags, which lwext4 takes too.
/// Where `Ext4::keep` keeps open files whose names are gone, and what it
/// names them, before their inodes.
const KEPT_IN: &str = "lost+found";
const KEPT_AS: &str = ".open-";

const O_RDONLY: c_int = 0o0;
const O_RDWR: c_int = 0o2;
const O_CREAT: c_int = 0o100;
const O_TRUNC: c_int = 0o1000;

impl Mount {
    /// `LWEXT4`, to do anything on this mount but hold a write: what's held
    /// is written first.
    fn lock(&self) -> MutexGuard<'static, [bool; MOUNTS]> {
        let lwext4 = LWEXT4.lock();
        self.flush();
        lwext4
    }

    /// Write what `Pending` holds, if anything; `LWEXT4` is held.
    fn flush(&self) {
        self.write_held(self.pending.lock().take());
    }

    /// Write `held`, what `Pending` held, all of it; `LWEXT4` is held. Why
    /// it couldn't be is kept for the next `sync`: whoever's call wrote it
    /// didn't make it, and the program that did was told it was written.
    fn write_held(&self, held: Option<Pending>) {
        let Some(held) = held else { return };
        let error = match write(held.file, held.offset, &held.data) {
            Ok(written) if written == held.data.len() => return,
            Ok(_) => Error::NoSpace,
            Err(error) => error,
        };
        self.failed.lock().get_or_insert(error);
    }

    /// As `Ext4::usage`; `LWEXT4` is held.
    fn usage(&self) -> Result<Usage, Error> {
        let mut stats = ffi::MountStats::default();
        // Safety: the mount point is NUL-terminated and mounted.
        check(unsafe { ffi::ext4_mount_point_stats(self.mount_point.as_ptr(), &mut stats) })?;
        Ok(Usage {
            block_size: stats.block_size,
            blocks: stats.blocks_count,
            free_blocks: stats.free_blocks_count,
            inodes: stats.inodes_count,
            free_inodes: stats.free_inodes_count,
        })
    }

    /// lwext4's path for the name `Ext4::keep` gives inode `inode`.
    fn keeper(&self, inode: u32) -> CString {
        let mount_point = self.mount_point.to_str().unwrap();
        CString::new(format!("{mount_point}{KEPT_IN}/{KEPT_AS}{inode}")).unwrap()
    }

    /// Register the device with lwext4 and mount it; `LWEXT4` is held.
    fn attach(&mut self) -> Result<(), Error> {
        let mount_point = self.mount_point.as_ptr();
        // Safety: the names are NUL-terminated, the device is ours.
        unsafe {
            check(ffi::ext4_device_register(self.device, self.device_name.as_ptr()))?;
            self.registered = true;
            check(ffi::ext4_mount(self.device_name.as_ptr(), mount_point, !self.writable))?;
            self.mounted = true;
            let needs_recovery = ffi::simplos_ext4_needs_recovery(mount_point) != 0;
            if !self.writable {
                return if needs_recovery { Err(Error::NeedsRecovery) } else { Ok(()) };
            }
            if ffi::simplos_ext4_read_only(mount_point) != 0 {
                return Err(Error::Unsupported);
            }
            if needs_recovery {
                check(ffi::ext4_recover(mount_point))?;
            }
            check(ffi::ext4_journal_start(mount_point))?;
            self.journaling = true;
            // With a journal, blocks can wait in memory to be written where
            // they go, as each change is in the journal before the call
            // that made it returns: lwext4 writes the journal's blocks
            // through, and those it holds once the journal has them.
            if ffi::simplos_ext4_journaled(mount_point) != 0 {
                check(ffi::ext4_cache_write_back(mount_point, true))?;
                self.write_back = true;
            }
        }
        Ok(())
    }

    /// Change what `changes` says of inode `inode`, and set its change time
    /// to now; `LWEXT4` is held.
    fn change(&self, inode: u32, changes: Changes) -> Result<(), Error> {
        const LEAVE: u32 = u32::MAX;
        // Safety: the mount point is NUL-terminated, and `inode` came from
        // lwext4.
        check(unsafe {
            ffi::ext4_inode_attrs_set(
                self.mount_point.as_ptr(),
                inode,
                changes.permissions.map_or(LEAVE, |permissions| permissions & 0o7777),
                changes.uid.unwrap_or(LEAVE),
                changes.gid.unwrap_or(LEAVE),
                changes.accessed.unwrap_or(0),
                changes.modified.unwrap_or(0),
                now().unwrap_or(0),
            )
        })
    }

    /// Set all of inode `inode`'s times to now, as it's made; `LWEXT4` is
    /// held. Without a clock, nothing.
    fn stamp(&self, inode: u32) -> Result<(), Error> {
        match now() {
            Some(now) => self.change(
                inode,
                Changes { accessed: Some(now), modified: Some(now), ..Changes::default() },
            ),
            None => Ok(()),
        }
    }

    /// What's known of inode `inode`; `LWEXT4` is held.
    fn stat(&self, inode: u32) -> Result<Metadata, Error> {
        let mut stat = ffi::Stat::default();
        // Safety: the mount point is NUL-terminated, and `inode` came from
        // lwext4.
        check(unsafe { ffi::simplos_ext4_stat(self.mount_point.as_ptr(), inode, &mut stat) })?;
        let kind = match stat.mode & 0o170000 {
            0o100000 => Kind::File,
            0o040000 => Kind::Directory,
            0o120000 => Kind::Symlink,
            _ => Kind::Other,
        };
        Ok(Metadata {
            inode,
            kind,
            permissions: stat.mode & 0o7777,
            links: stat.links,
            uid: stat.uid,
            gid: stat.gid,
            size: stat.size,
            blocks: stat.blocks,
            accessed: stat.accessed,
            modified: stat.modified,
            changed: stat.changed,
        })
    }
}

impl Drop for Mount {
    fn drop(&mut self) {
        let mut slots = LWEXT4.lock();
        let mount_point = self.mount_point.as_ptr();
        // Safety: undoes what `attach` did, and only that: lwext4 would
        // unmount a mount that failed, and crash on its half-made state.
        unsafe {
            // What waits in memory, first.
            if self.write_back {
                ffi::ext4_cache_write_back(mount_point, false);
            }
            if self.journaling {
                ffi::ext4_journal_stop(mount_point);
            }
            if self.mounted {
                ffi::ext4_umount(mount_point);
            }
            if self.registered {
                ffi::ext4_device_unregister(self.device_name.as_ptr());
            }
            ffi::simplos_ext4_device_free(self.device);
        }
        slots[self.slot] = false;
    }
}

/// An open directory, with its entries as they were when it was opened.
pub struct Dir {
    mount: Arc<Mount>,
    inode: u32,
    entries: Vec<DirEntry>,
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _lwext4 = LWEXT4.lock();
        let mut open = self.mount.open.lock();
        let at = open.iter().position(|inode| *inode == self.inode).expect("it was opened");
        open.swap_remove(at);
    }
}

impl Dir {
    /// Without `.` and `..`.
    pub fn entries(&self) -> &[DirEntry] {
        &self.entries
    }

    pub fn metadata(&self) -> Result<Metadata, Error> {
        let _lwext4 = self.mount.lock();
        self.mount.stat(self.inode)
    }
}

/// An open file. Reads and writes say where, as `pread` and `pwrite` do:
/// whoever opened it keeps its own place in it.
pub struct File {
    mount: Arc<Mount>,
    raw: *mut ffi::File,
    inode: u32,
    /// lwext4 would write to a file opened only for reading.
    writable: bool,
    /// The second its modification time was last set to through this
    /// handle: a stream of writes changes it once a second, not each time.
    touched: AtomicU32,
}

// Safety: as for `Mount`.
unsafe impl Send for File {}
unsafe impl Sync for File {}

impl File {
    pub fn metadata(&self) -> Result<Metadata, Error> {
        let _lwext4 = self.mount.lock();
        self.mount.stat(self.inode)
    }

    /// Its size, with what's held to be written to it, without writing it:
    /// appending asks before each write.
    pub fn size(&self) -> Result<u64, Error> {
        let _lwext4 = LWEXT4.lock();
        let size = self.mount.stat(self.inode)?.size;
        Ok(match &*self.mount.pending.lock() {
            Some(held) if held.inode == self.inode => size.max(held.end()),
            _ => size,
        })
    }

    /// Read into `buf` from `offset` on: how many bytes, 0 at or past the
    /// end.
    pub fn read_at(&self, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
        let _lwext4 = self.mount.lock();
        // lwext4 would read on past the end of a file another handle has
        // made shorter since.
        if offset >= self.mount.stat(self.inode)?.size {
            return Ok(0);
        }
        let mut read = 0;
        // Safety: open until `drop`; `buf` is writable for its length.
        check(unsafe {
            ffi::simplos_ext4_file_seek(self.raw, offset);
            ffi::ext4_fread(self.raw, buf.as_mut_ptr().cast(), buf.len(), &mut read)
        })?;
        Ok(read)
    }

    /// Write `buf` from `offset` on: how many bytes, fewer if the file
    /// system fills up. Past the end, the gap is filled with zeros.
    ///
    /// shortcut: lwext4 can't leave the gap a hole that takes no space, as
    /// Linux does; a write far past the end can fill the disk with zeros.
    pub fn write_at(&self, offset: u64, buf: &[u8]) -> Result<usize, Error> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let _lwext4 = LWEXT4.lock();
        let written = match self.hold(offset, buf)? {
            true => buf.len(),
            false => {
                // lwext4 would put what's written past the end at the end.
                let end = self.mount.stat(self.inode)?.size;
                self.fill(end, offset)?;
                write(self.raw, offset, buf)?
            }
        };
        // It's written: a time that couldn't be set isn't worth failing the
        // write over, and having it written again.
        if written > 0 {
            let _ = self.touch();
        }
        Ok(written)
    }

    /// Hold `buf` to write at `offset` later, if it goes on from what's held
    /// for this file already, with room for both; else, what's held is
    /// written first, and `buf` held from scratch. Not past the end, which
    /// zeros fill first; or with too little room left on the file system to
    /// be sure it can be written: running out is told as it's written, and
    /// nothing is held then. `LWEXT4` is held.
    fn hold(&self, offset: u64, buf: &[u8]) -> Result<bool, Error> {
        let mut pending = self.mount.pending.lock();
        let follows = matches!(&*pending, Some(held) if held.file == self.raw
            && held.end() == offset
            && held.data.len() + buf.len() <= HOLD);
        if !follows {
            self.mount.write_held(pending.take());
            if buf.len() > HOLD || offset > self.mount.stat(self.inode)?.size {
                return Ok(false);
            }
        }
        let held = pending.as_ref().map_or(0, |held| held.data.len());
        let usage = self.mount.usage()?;
        // With blocks to spare for those that map them: an extent tree's,
        // or ext2's indirect ones.
        if usage.free_blocks < (held + buf.len()) as u64 / u64::from(usage.block_size) + 16 {
            self.mount.write_held(pending.take());
            return Ok(false);
        }
        let new = || Pending { file: self.raw, inode: self.inode, offset, data: Vec::new() };
        pending.get_or_insert_with(new).data.extend_from_slice(buf);
        Ok(true)
    }

    /// Zeros from `end`, the end of the file, to `to`; `LWEXT4` is held.
    /// `HOLD` of them at a time: each write is a transaction, which costs
    /// the device about as many writes for 4 KiB as for 64.
    fn fill(&self, mut end: u64, to: u64) -> Result<(), Error> {
        static ZEROS: [u8; HOLD] = [0; HOLD];
        while end < to {
            let gap = (to - end).min(HOLD as u64) as usize;
            end += write(self.raw, end, &ZEROS[..gap])? as u64;
        }
        Ok(())
    }

    /// Make the file `size` bytes long: cut short, or longer with zeros (not
    /// a hole: see `write_at`).
    pub fn set_len(&self, size: u64) -> Result<(), Error> {
        if !self.writable {
            return Err(Error::ReadOnly);
        }
        let _lwext4 = self.mount.lock();
        let end = self.mount.stat(self.inode)?.size;
        if size < end {
            // Safety: open until `drop`.
            check(unsafe { ffi::ext4_ftruncate(self.raw, size) })?;
        }
        self.fill(end, size)?;
        self.touch()
    }

    /// Change what `changes` says of the file, and its change time.
    pub fn change(&self, changes: Changes) -> Result<(), Error> {
        let _lwext4 = self.mount.lock();
        self.mount.change(self.inode, changes)
    }

    /// Its modification time to now, as it's written to; `LWEXT4` is held.
    fn touch(&self) -> Result<(), Error> {
        match now() {
            Some(now) if self.touched.swap(now, Ordering::Relaxed) != now => {
                self.mount.change(self.inode, Changes { modified: Some(now), ..Changes::default() })
            }
            _ => Ok(()),
        }
    }
}

/// Write `buf` to `file` from `offset` on, within it or at its end: how
/// many bytes; `LWEXT4` is held.
fn write(file: *mut ffi::File, offset: u64, buf: &[u8]) -> Result<usize, Error> {
    let mut written = 0;
    // Safety: `file` is open, as a `File` is until `drop`, which writes what
    // `Pending` holds for it first; `buf` is readable for its length.
    check(unsafe {
        ffi::simplos_ext4_file_seek(file, offset);
        ffi::ext4_fwrite(file, buf.as_ptr().cast(), buf.len(), &mut written)
    })?;
    // lwext4 loses why a write fell short (its `Finish:` overwrites the
    // error); running out of space is the likely reason.
    if written == 0 && !buf.is_empty() {
        return Err(Error::NoSpace);
    }
    Ok(written)
}

impl Drop for File {
    fn drop(&mut self) {
        let _lwext4 = LWEXT4.lock();
        // What's held, while it can be written: if it can't, the next
        // `sync` says so.
        self.mount.flush();
        // Safety: opened in `Ext4::open_with`, closed once.
        unsafe { ffi::simplos_ext4_file_close(self.raw) };
        let mut open = self.mount.open.lock();
        let at = open.iter().position(|inode| *inode == self.inode).expect("it was opened");
        open.swap_remove(at);
        let mut kept = self.mount.kept.lock();
        if !open.contains(&self.inode)
            && let Some(at) = kept.iter().position(|inode| *inode == self.inode)
        {
            kept.swap_remove(at);
            // The last of a file whose other names are gone: it goes too.
            // Safety: as in `Ext4::metadata`.
            unsafe { ffi::ext4_fremove(self.mount.keeper(self.inode).as_ptr()) };
        }
    }
}

/// Called by lwext4, through `glue.c`, with the device `Ext4::mount` gave it.
#[unsafe(no_mangle)]
extern "C" fn simplos_ext4_read(
    device: *mut c_void,
    buf: *mut u8,
    sector: u64,
    count: u32,
) -> c_int {
    // Safety: the `Box<dyn BlockDevice>` behind `Ext4::_ours`, used only with
    // `LWEXT4` held; `buf` holds `count` sectors.
    let (device, buf) = unsafe {
        (
            &mut *device.cast::<Box<dyn BlockDevice>>(),
            core::slice::from_raw_parts_mut(buf, count as usize * SECTOR),
        )
    };
    match device.read(sector, buf) {
        Ok(()) => 0,
        Err(_) => 5,
    }
}

#[unsafe(no_mangle)]
extern "C" fn simplos_ext4_write(
    device: *mut c_void,
    buf: *const u8,
    sector: u64,
    count: u32,
) -> c_int {
    // Safety: as in `simplos_ext4_read`.
    let (device, buf) = unsafe {
        (
            &mut *device.cast::<Box<dyn BlockDevice>>(),
            core::slice::from_raw_parts(buf, count as usize * SECTOR),
        )
    };
    match device.write(sector, buf) {
        Ok(()) => 0,
        Err(_) => 5,
    }
}

/// lwext4's `assert`, through `src/include/assert.h`.
#[unsafe(no_mangle)]
extern "C" fn simplos_ext4_assert_failed(file: *const c_char, line: c_int) {
    // Safety: `__FILE__`, a string literal.
    let file = unsafe { CStr::from_ptr(file) }.to_str().unwrap_or("?");
    panic!("lwext4: assertion failed at {file}:{line}");
}

/// lwext4's functions, and `glue.c`'s around them. Its structures stay opaque.
mod ffi {
    use core::ffi::{c_char, c_int, c_void};

    #[repr(C)]
    pub struct BlockDev {
        _opaque: [u8; 0],
    }
    #[repr(C)]
    pub struct File {
        _opaque: [u8; 0],
    }
    #[repr(C)]
    pub struct Dir {
        _opaque: [u8; 0],
    }

    /// `struct simplos_ext4_stat` in `glue.c`.
    #[repr(C)]
    #[derive(Default)]
    pub struct Stat {
        pub mode: u32,
        pub uid: u32,
        pub gid: u32,
        pub links: u32,
        pub size: u64,
        pub blocks: u64,
        pub accessed: u32,
        pub modified: u32,
        pub changed: u32,
    }

    /// lwext4's `struct ext4_mount_stats`.
    #[repr(C)]
    #[derive(Default)]
    pub struct MountStats {
        pub inodes_count: u32,
        pub free_inodes_count: u32,
        pub blocks_count: u64,
        pub free_blocks_count: u64,
        pub block_size: u32,
        pub block_group_count: u32,
        pub blocks_per_group: u32,
        pub inodes_per_group: u32,
        pub volume_name: [u8; 16],
    }

    unsafe extern "C" {
        pub fn ext4_mount_point_stats(mount_point: *const c_char, stats: *mut MountStats) -> c_int;
        pub fn simplos_ext4_device_new(device: *mut c_void, sectors: u64) -> *mut BlockDev;
        pub fn simplos_ext4_device_free(device: *mut BlockDev);
        pub fn simplos_ext4_needs_recovery(mount_point: *const c_char) -> c_int;
        pub fn simplos_ext4_read_only(mount_point: *const c_char) -> c_int;
        pub fn simplos_ext4_journaled(mount_point: *const c_char) -> c_int;
        pub fn ext4_cache_write_back(mount_point: *const c_char, on: bool) -> c_int;
        pub fn simplos_ext4_file_open(
            path: *const c_char,
            flags: c_int,
            error: *mut c_int,
        ) -> *mut File;
        pub fn simplos_ext4_file_inode(file: *mut File) -> u32;
        pub fn simplos_ext4_file_seek(file: *mut File, offset: u64);
        pub fn simplos_ext4_file_close(file: *mut File) -> c_int;
        pub fn simplos_ext4_inode_of(path: *const c_char, inode: *mut u32) -> c_int;
        pub fn simplos_ext4_stat(mount_point: *const c_char, inode: u32, stat: *mut Stat) -> c_int;
        pub fn simplos_ext4_dir_open(path: *const c_char, error: *mut c_int) -> *mut Dir;
        pub fn simplos_ext4_dir_close(dir: *mut Dir);
        pub fn simplos_ext4_dir_next(
            dir: *mut Dir,
            name: *mut c_char,
            capacity: usize,
            kind: *mut u8,
            inode: *mut u32,
        ) -> c_int;

        pub fn ext4_device_register(device: *mut BlockDev, name: *const c_char) -> c_int;
        pub fn ext4_device_unregister(name: *const c_char) -> c_int;
        pub fn ext4_mount(
            device: *const c_char,
            mount_point: *const c_char,
            read_only: bool,
        ) -> c_int;
        pub fn ext4_umount(mount_point: *const c_char) -> c_int;
        pub fn ext4_recover(mount_point: *const c_char) -> c_int;
        pub fn ext4_journal_start(mount_point: *const c_char) -> c_int;
        pub fn ext4_journal_stop(mount_point: *const c_char) -> c_int;
        pub fn ext4_dir_mk(path: *const c_char) -> c_int;
        pub fn ext4_dir_rm(path: *const c_char) -> c_int;
        pub fn ext4_fremove(path: *const c_char) -> c_int;
        pub fn ext4_frename(path: *const c_char, new_path: *const c_char) -> c_int;
        pub fn ext4_fsymlink(target: *const c_char, path: *const c_char) -> c_int;
        pub fn ext4_readlink(
            path: *const c_char,
            buf: *mut c_char,
            bufsize: usize,
            rcnt: *mut usize,
        ) -> c_int;
        pub fn ext4_flink(path: *const c_char, hardlink_path: *const c_char) -> c_int;
        pub fn ext4_ftruncate(file: *mut File, size: u64) -> c_int;
        pub fn ext4_inode_attrs_set(
            mount_point: *const c_char,
            inode: u32,
            mode: u32,
            uid: u32,
            gid: u32,
            atime: u32,
            mtime: u32,
            ctime: u32,
        ) -> c_int;
        pub fn ext4_fread(
            file: *mut File,
            buf: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> c_int;
        pub fn ext4_fwrite(
            file: *mut File,
            buf: *const c_void,
            size: usize,
            written: *mut usize,
        ) -> c_int;
    }
}
