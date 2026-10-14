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
//! The file system on a device is trusted for now: lwext4 is C, parsing it
//! in ring 0. Fuzz this before mounting disks from elsewhere.

#![no_std]

extern crate alloc;

mod memory;

use alloc::boxed::Box;
use alloc::ffi::CString;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::ffi::{c_int, c_void};
use spin::Mutex;

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
    /// Not unmounted cleanly: its journal holds changes still to be applied.
    NeedsRecovery,
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
        2 => Error::NotFound,
        5 => Error::Io,
        12 => Error::OutOfMemory,
        20 => Error::NotADirectory,
        21 => Error::IsADirectory,
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
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Metadata {
    pub kind: Kind,
    /// The permission bits: `0o755` and the like.
    pub permissions: u32,
}

/// Which of lwext4's mount slots are taken. Every call into lwext4 holds
/// this lock, not only mounting.
static LWEXT4: Mutex<[bool; MOUNTS]> = Mutex::new([false; MOUNTS]);

/// How many file systems can be mounted at once: lwext4's
/// CONFIG_EXT4_MOUNTPOINTS_COUNT and CONFIG_EXT4_BLOCKDEVS_COUNT, set in
/// build.rs.
pub const MOUNTS: usize = 4;

/// A mounted file system.
pub struct Ext4 {
    slot: usize,
    /// lwext4's names for the device and where it is mounted.
    device_name: CString,
    mount_point: String,
    device: *mut ffi::BlockDev,
    /// How far `attach` got, for `Drop` to undo.
    registered: bool,
    mounted: bool,
    /// Boxed twice so lwext4 can hold a thin pointer to it.
    _ours: Box<Box<dyn BlockDevice>>,
}

// Safety: lwext4's state is only touched with `LWEXT4` held.
unsafe impl Send for Ext4 {}
unsafe impl Sync for Ext4 {}

impl Ext4 {
    /// Mount the file system on `device`, read-only.
    pub fn mount(device: Box<dyn BlockDevice>) -> Result<Ext4, Error> {
        let mut slots = LWEXT4.lock();
        let slot = slots.iter().position(|used| !used).ok_or(Error::TooManyMounts)?;
        let ours = Box::new(device);
        let pointer = (&*ours as *const Box<dyn BlockDevice>).cast_mut().cast();
        // Safety: `ours` outlives the device, which `Drop` frees.
        let device = unsafe { ffi::simplos_ext4_device_new(pointer, ours.sectors()) };
        if device.is_null() {
            return Err(Error::OutOfMemory);
        }
        let mut fs = Ext4 {
            slot,
            device_name: CString::new(format!("sd{slot}")).unwrap(),
            mount_point: format!("/mp{slot}/"),
            device,
            registered: false,
            mounted: false,
            _ours: ours,
        };
        // The slot stays taken until `fs`'s `Drop` has undone whatever
        // `attach` managed, if it fails: that takes the lock, so it runs
        // after this function lets go of it.
        slots[slot] = true;
        let mounted = fs.attach();
        drop(slots);
        mounted.map(|()| fs)
    }

    /// Register the device with lwext4 and mount it; `LWEXT4` is held.
    fn attach(&mut self) -> Result<(), Error> {
        let mount_point = CString::new(self.mount_point.as_str()).unwrap();
        // Safety: the names are NUL-terminated, the device is ours.
        unsafe {
            check(ffi::ext4_device_register(self.device, self.device_name.as_ptr()))?;
            self.registered = true;
            check(ffi::ext4_mount(self.device_name.as_ptr(), mount_point.as_ptr(), true))?;
            self.mounted = true;
            if ffi::simplos_ext4_needs_recovery(mount_point.as_ptr()) != 0 {
                return Err(Error::NeedsRecovery);
            }
        }
        Ok(())
    }

    /// lwext4's path for `path`, which is relative to the file system's root.
    fn path(&self, path: &str) -> Result<CString, Error> {
        let full = format!("{}{}", self.mount_point, path.trim_start_matches('/'));
        CString::new(full).map_err(|_| Error::NotFound)
    }

    pub fn metadata(&self, path: &str) -> Result<Metadata, Error> {
        let path = self.path(path)?;
        let _lwext4 = LWEXT4.lock();
        let mut mode = 0;
        // Safety: a NUL-terminated path on our mount.
        check(unsafe { ffi::ext4_mode_get(path.as_ptr(), &mut mode) })?;
        let kind = match mode & 0o170000 {
            0o100000 => Kind::File,
            0o040000 => Kind::Directory,
            0o120000 => Kind::Symlink,
            _ => Kind::Other,
        };
        Ok(Metadata { kind, permissions: mode & 0o7777 })
    }

    /// The entries of the directory at `path`, without `.` and `..`.
    pub fn read_dir(&self, path: &str) -> Result<Vec<DirEntry>, Error> {
        // lwext4 says ENOENT for a file; Linux says ENOTDIR.
        if self.metadata(path)?.kind != Kind::Directory {
            return Err(Error::NotADirectory);
        }
        let path = self.path(path)?;
        let _lwext4 = LWEXT4.lock();
        let mut error = 0;
        // Safety: as in `metadata`.
        let dir = unsafe { ffi::simplos_ext4_dir_open(path.as_ptr(), &mut error) };
        if dir.is_null() {
            return Err(check(error).err().unwrap_or(Error::Io));
        }
        let mut entries = Vec::new();
        let mut name = [0u8; 255];
        let mut kind = 0;
        loop {
            // Safety: `dir` is open; `name` holds the longest name ext4 has.
            let length = unsafe {
                ffi::simplos_ext4_dir_next(dir, name.as_mut_ptr().cast(), name.len(), &mut kind)
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
            entries.push(DirEntry { name: String::from_utf8_lossy(name).into_owned(), kind });
        }
        // Safety: opened above, closed once.
        unsafe { ffi::simplos_ext4_dir_close(dir) };
        Ok(entries)
    }

    /// Open the file at `path` for reading.
    pub fn open(&self, path: &str) -> Result<File<'_>, Error> {
        match self.metadata(path)?.kind {
            Kind::Directory => return Err(Error::IsADirectory),
            Kind::File => {}
            _ => return Err(Error::Unsupported),
        }
        let path = self.path(path)?;
        let _lwext4 = LWEXT4.lock();
        let mut error = 0;
        // Safety: as in `metadata`; the flags are NUL-terminated.
        let raw = unsafe { ffi::simplos_ext4_file_open(path.as_ptr(), c"rb".as_ptr(), &mut error) };
        if raw.is_null() {
            return Err(check(error).err().unwrap_or(Error::Io));
        }
        Ok(File { _fs: self, raw })
    }
}

impl Drop for Ext4 {
    fn drop(&mut self) {
        let mut slots = LWEXT4.lock();
        let mount_point = CString::new(self.mount_point.as_str()).unwrap();
        // Safety: undoes what `attach` did, and only that: lwext4 would
        // unmount a mount that failed, and crash on its half-made state.
        unsafe {
            if self.mounted {
                ffi::ext4_umount(mount_point.as_ptr());
            }
            if self.registered {
                ffi::ext4_device_unregister(self.device_name.as_ptr());
            }
            ffi::simplos_ext4_device_free(self.device);
        }
        slots[self.slot] = false;
    }
}

/// An open file. It borrows its file system, which stays mounted meanwhile.
pub struct File<'fs> {
    _fs: &'fs Ext4,
    raw: *mut ffi::File,
}

impl File<'_> {
    pub fn size(&self) -> u64 {
        let _lwext4 = LWEXT4.lock();
        // Safety: open until `drop`.
        unsafe { ffi::ext4_fsize(self.raw) }
    }

    /// Read from where the last read stopped, into `buf`: how many bytes,
    /// 0 at the end.
    pub fn read(&mut self, buf: &mut [u8]) -> Result<usize, Error> {
        let _lwext4 = LWEXT4.lock();
        let mut read = 0;
        // Safety: open until `drop`; `buf` is writable for its length.
        check(unsafe { ffi::ext4_fread(self.raw, buf.as_mut_ptr().cast(), buf.len(), &mut read) })?;
        Ok(read)
    }

    /// Make the next read start at `offset` from the start of the file.
    pub fn seek(&mut self, offset: u64) -> Result<(), Error> {
        const SEEK_SET: u32 = 0;
        let offset = i64::try_from(offset).map_err(|_| Error::Other(22))?;
        let _lwext4 = LWEXT4.lock();
        // Safety: open until `drop`.
        check(unsafe { ffi::ext4_fseek(self.raw, offset, SEEK_SET) })
    }
}

impl Drop for File<'_> {
    fn drop(&mut self) {
        let _lwext4 = LWEXT4.lock();
        // Safety: opened in `Ext4::open`, closed once.
        unsafe { ffi::simplos_ext4_file_close(self.raw) };
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

    unsafe extern "C" {
        pub fn simplos_ext4_device_new(device: *mut c_void, sectors: u64) -> *mut BlockDev;
        pub fn simplos_ext4_device_free(device: *mut BlockDev);
        pub fn simplos_ext4_needs_recovery(mount_point: *const c_char) -> c_int;
        pub fn simplos_ext4_file_open(
            path: *const c_char,
            flags: *const c_char,
            error: *mut c_int,
        ) -> *mut File;
        pub fn simplos_ext4_file_close(file: *mut File) -> c_int;
        pub fn simplos_ext4_dir_open(path: *const c_char, error: *mut c_int) -> *mut Dir;
        pub fn simplos_ext4_dir_close(dir: *mut Dir);
        pub fn simplos_ext4_dir_next(
            dir: *mut Dir,
            name: *mut c_char,
            capacity: usize,
            kind: *mut u8,
        ) -> c_int;

        pub fn ext4_device_register(device: *mut BlockDev, name: *const c_char) -> c_int;
        pub fn ext4_device_unregister(name: *const c_char) -> c_int;
        pub fn ext4_mount(
            device: *const c_char,
            mount_point: *const c_char,
            read_only: bool,
        ) -> c_int;
        pub fn ext4_umount(mount_point: *const c_char) -> c_int;
        pub fn ext4_mode_get(path: *const c_char, mode: *mut u32) -> c_int;
        pub fn ext4_fread(
            file: *mut File,
            buf: *mut c_void,
            size: usize,
            read: *mut usize,
        ) -> c_int;
        pub fn ext4_fseek(file: *mut File, offset: i64, origin: u32) -> c_int;
        pub fn ext4_fsize(file: *mut File) -> u64;
    }
}
