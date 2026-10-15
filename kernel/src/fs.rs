//! File systems, as one tree: at the root, the first ext4 disk found, mounted
//! to read and write by [`init`], or with no disk an empty directory that
//! can't be written; and at /dev, the devices.
//!
//! Every call into them, through a path or through an open file, holds
//! [`ROOT`]'s lock ([`lock`]), and whoever waits for it sleeps.

use crate::io::disk;
use crate::proc;
use crate::serial_println;
use crate::thread::{self, SleepLock, SleepLockGuard};
use crate::tty::Tty;
use alloc::boxed::Box;
use alloc::collections::VecDeque;
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
    ext4::set_clock(|| (crate::time::now() / crate::time::NANOS) as u32);
    // Small writes wait in memory to be made as one (see `ext4`): a second
    // at most, so the power going loses no more. If they can't be made, the
    // program's next fsync is told. Once, however often the disk is mounted
    // again.
    static WRITER: spin::Once = spin::Once::new();
    WRITER.call_once(|| {
        thread::spawn(|| {
            loop {
                thread::sleep(1000);
                lock().flush();
            }
        })
        .expect("memory for the disk's writer");
    });
    for (index, info) in disk::list().into_iter().enumerate() {
        match Ext4::mount_writable(Box::new(Disk { index, sectors: info.sectors })) {
            Ok(fs) => {
                serial_println!("simplos: ext4 on disk {}", index);
                *ROOT.lock() = Some(fs);
                return;
            }
            // Said so, or it looks as if there were no ext4 disk at all.
            Err(error) if is_ext4(index) => {
                serial_println!("simplos: disk {}: ext4 that won't mount: {:?}", index, error);
            }
            Err(_) => {}
        }
    }
}

/// Whether disk `index` has ext4's (or ext3's, or ext2's) magic number,
/// 56 bytes into the superblock, which is 1 KiB in.
fn is_ext4(index: usize) -> bool {
    let mut sector = [0; disk::SECTOR];
    disk::read(index, 2, &mut sector).is_ok() && sector[56..58] == 0xef53u16.to_le_bytes()
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
    /// A file in /proc, as it was when it was opened.
    Proc {
        /// From the root.
        path: String,
        data: Vec<u8>,
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

/// /dev's links to /dev/fd's, as Linux has them to /proc/self/fd's.
fn dev_link(name: &str) -> Option<&'static str> {
    match name {
        "stdin" => Some("fd/0"),
        "stdout" => Some("fd/1"),
        "stderr" => Some("fd/2"),
        _ => None,
    }
}

/// What the descriptor at `path` is open on, if it's open: /dev/fd/N, the
/// calling program's descriptor N, or /proc/PID/fd/N, process PID's.
/// Opening it is `dup`'s, as FreeBSD's fdescfs has it, where Linux opens the
/// file anew, with a place of its own in it.
pub fn descriptor(path: &str) -> Option<Arc<Handle>> {
    let (process, fd) = match on(path) {
        On::Devices(name) => (crate::thread::current_process()?, name.strip_prefix("fd/")?),
        On::Proc(within) => {
            let (id, fd) = within.split_once("/fd/")?;
            (crate::process::find(id.parse().ok()?)?, fd)
        }
        On::Disk => return None,
    };
    let fd: usize = fd.parse().ok()?;
    let files = process.files.lock();
    files.get(fd)?.as_ref().map(|fd| fd.handle.clone())
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
        Handle::File { .. } | Handle::Dir { .. } | Handle::Proc { .. } => None,
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

/// Which file system a path from the root is on: the devices or /proc,
/// with the path within (`""` for /dev or /proc itself), or else the disk.
enum On<'p> {
    Devices(&'p str),
    Proc(&'p str),
    Disk,
}

fn on(path: &str) -> On<'_> {
    let within = |name: &str| match path.strip_prefix(name)? {
        "" => Some(""),
        rest => rest.strip_prefix('/'),
    };
    match (within("dev"), within("proc")) {
        (Some(rest), _) => On::Devices(rest),
        (_, Some(rest)) => On::Proc(rest),
        _ => On::Disk,
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
    /// Write what waits in memory: once this returns, every change is on
    /// the disk; an error if any that waited couldn't be written, since the
    /// last time this said.
    pub fn sync(&self) -> Result<(), Error> {
        self.disk.as_ref().map_or(Ok(()), Ext4::sync)
    }

    /// Write what waits in memory, as `sync` does, leaving telling whether
    /// it could be to it.
    pub fn flush(&self) {
        if let Some(disk) = self.disk.as_ref() {
            disk.flush();
        }
    }

    /// What's at `path`, a path from the root.
    pub fn metadata(&self, path: &str) -> Result<Metadata, Error> {
        if let Some(handle) = descriptor(path) {
            return self.handle_metadata(&handle);
        }
        match on(path) {
            On::Devices("") => Ok(directory(DEV_INODE, 0o755)),
            On::Devices("fd") => Ok(directory(proc::inode(path), 0o555)),
            On::Devices(name) if let Some(target) = dev_link(name) => {
                Ok(proc::Node::Link(target.into()).metadata(path))
            }
            On::Devices(_) => device(path).map(device_metadata).ok_or(Error::NotFound),
            On::Proc(within) => {
                let node = proc::node(within, self.disk.as_ref()).ok_or(Error::NotFound)?;
                Ok(node.metadata(path))
            }
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
                let mut entries: Vec<DirEntry> = DEVICES.into_iter().map(entry).collect();
                let named = |name: &str, kind| DirEntry {
                    name: name.into(),
                    kind,
                    inode: proc::inode(&alloc::format!("dev/{name}")),
                };
                entries.push(named("fd", Kind::Directory));
                for name in ["stdin", "stdout", "stderr"] {
                    entries.push(named(name, Kind::Symlink));
                }
                (None, entries)
            }
            On::Devices("fd") => match crate::thread::current_process() {
                Some(process) => (None, proc::descriptors(&process, path)),
                None => (None, Vec::new()),
            },
            On::Devices(_) => {
                return Err(self.metadata(path).err().unwrap_or(Error::NotADirectory));
            }
            On::Proc(within) => match proc::node(within, self.disk.as_ref()) {
                Some(proc::Node::Dir(entries)) => (None, entries),
                Some(_) => return Err(Error::NotADirectory),
                None => return Err(Error::NotFound),
            },
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
        if path.is_empty() {
            entries.retain(|entry| entry.name != "dev" && entry.name != "proc");
            entries.push(DirEntry { name: "dev".into(), kind: Kind::Directory, inode: DEV_INODE });
            let inode = proc::inode("proc");
            entries.push(DirEntry { name: "proc".into(), kind: Kind::Directory, inode });
        }
        Ok((dir, entries))
    }

    /// What `fstat` tells of the file `handle` is open on.
    pub fn handle_metadata(&self, handle: &Handle) -> Result<Metadata, Error> {
        match handle {
            Handle::File { file, .. } => file.metadata(),
            Handle::Dir { dir: Some(dir), .. } => dir.metadata(),
            Handle::Dir { path, .. } => self.metadata(path),
            Handle::Proc { path, .. } => Ok(proc::Node::File(Vec::new()).metadata(path)),
            special => Ok(self::special(special).expect("not on the disk")),
        }
    }

    /// Where the symbolic link at `path` points, as it was given.
    pub fn read_link(&self, path: &str) -> Result<Vec<u8>, Error> {
        match on(path) {
            On::Devices(name) => dev_link(name)
                .map(|target| target.as_bytes().to_vec())
                .ok_or(Error::InvalidArgument),
            On::Proc(within) => match proc::node(within, self.disk.as_ref()) {
                Some(proc::Node::Link(target)) => Ok(target),
                Some(_) => Err(Error::InvalidArgument),
                None => Err(Error::NotFound),
            },
            On::Disk => self.disk(path)?.read_link(path),
        }
    }

    /// What `statfs` tells of the file system `path` is on: its type, by
    /// Linux's number for it, and how big it is and how much is free: none
    /// of it, for those that are only in memory.
    pub fn statfs(&self, path: &str) -> Result<(u64, ext4::Usage), Error> {
        const EXT4_SUPER_MAGIC: u64 = 0xEF53;
        const PROC_SUPER_MAGIC: u64 = 0x9FA0;
        const TMPFS_MAGIC: u64 = 0x0102_1994;
        const RAMFS_MAGIC: u64 = 0x8584_58F6;
        let none =
            ext4::Usage { block_size: 4096, blocks: 0, free_blocks: 0, inodes: 0, free_inodes: 0 };
        match on(path) {
            On::Devices(_) => Ok((TMPFS_MAGIC, none)),
            On::Proc(_) => Ok((PROC_SUPER_MAGIC, none)),
            On::Disk => match self.disk.as_ref() {
                Some(disk) => Ok((EXT4_SUPER_MAGIC, disk.usage()?)),
                None => Ok((RAMFS_MAGIC, none)),
            },
        }
    }

    /// The file in /proc at `path`, as it is now: none if it isn't one.
    pub fn proc_file(&self, path: &str) -> Option<Vec<u8>> {
        match on(path) {
            On::Proc(within) => match proc::node(within, self.disk.as_ref())? {
                proc::Node::File(data) => Some(data),
                _ => None,
            },
            _ => None,
        }
    }

    /// The disk, for what only it can do: making, changing and removing
    /// files. Neither /dev nor the root without a disk can be written.
    pub fn disk(&self, path: &str) -> Result<&Ext4, Error> {
        match on(path) {
            On::Devices(_) | On::Proc(_) => Err(Error::ReadOnly),
            On::Disk => self.disk.as_ref().ok_or(Error::ReadOnly),
        }
    }
}

/// Where `path`, from the directory `base` (a path from the root, as
/// `resolve` makes them), really leads, as a path from the root: with each
/// symbolic link on the way followed, and the last one too if `follow_last`
/// or the path ends in `/`, as on Linux. Every part before the last has to be
/// a directory once followed: ENOENT or ENOTDIR if one isn't. ELOOP after 40
/// links, where Linux gives up too.
pub fn lookup(base: &str, path: &str, follow_last: bool) -> Result<String, i64> {
    const ENOTDIR: i64 = 20;
    const ELOOP: i64 = 40;
    let tree = lock();
    let mut here: Vec<String> = match path.starts_with('/') {
        true => Vec::new(),
        false => base.split('/').filter(|part| !part.is_empty()).map(String::from).collect(),
    };
    let follow_last = follow_last || path.ends_with('/');
    let mut rest: VecDeque<String> = path.split('/').map(String::from).collect();
    let mut links = 0;
    while let Some(part) = rest.pop_front() {
        match part.as_str() {
            "" | "." => continue,
            ".." => {
                here.pop();
                continue;
            }
            _ => here.push(part),
        }
        let last = rest.iter().all(|part| part.is_empty() || part == ".");
        if last && !follow_last {
            break;
        }
        let at = here.join("/");
        match tree.metadata(&at) {
            // What isn't there yet can be made, at the end of a path.
            Err(_) if last => break,
            Err(error) => return Err(errno(error)),
            Ok(Metadata { kind: Kind::Symlink, .. }) => {
                links += 1;
                if links > 40 {
                    return Err(ELOOP);
                }
                let target = tree.read_link(&at).map_err(errno)?;
                here.pop();
                if target.starts_with(b"/") {
                    here.clear();
                }
                let target = String::from_utf8_lossy(&target);
                target.rsplit('/').for_each(|part| rest.push_front(part.into()));
            }
            Ok(Metadata { kind: Kind::Directory, .. }) => {}
            Ok(_) if last => {}
            Ok(_) => return Err(ENOTDIR),
        }
    }
    Ok(here.join("/"))
}

/// `path` made absolute against the directory `base`, and with `.`, `..`
/// and repeated slashes taken out: a path from the root, without the leading
/// slash, as `Ext4` takes it. By the names alone, which is where a path
/// leads if nothing on the way is a symbolic link: `lookup` follows them.
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

/// All of the program file at `path`, a path from the root, for `execve` and
/// the kernel to start: or the errno saying why not.
pub fn read_program(path: &str) -> Result<Vec<u8>, i64> {
    const ENOMEM: i64 = 12;
    const EACCES: i64 = 13;
    // shortcut: read whole into the kernel's heap, so no bigger than this;
    // load segments straight from the file when programs get bigger.
    const MOST: u64 = 4 << 20;
    let tree = lock();
    let metadata = tree.metadata(path).map_err(errno)?;
    // shortcut: anyone's execute bit will do, as there's one user.
    if metadata.kind != Kind::File || metadata.permissions & 0o111 == 0 {
        return Err(EACCES);
    }
    if metadata.size > MOST {
        return Err(ENOMEM);
    }
    let file = tree.disk(path).map_err(errno)?.open(path).map_err(errno)?;
    // As much as it may be, the heap may not have it to spare: then the
    // program can't start, rather than the kernel stopping.
    let mut elf = Vec::new();
    elf.try_reserve_exact(metadata.size as usize).map_err(|_| ENOMEM)?;
    elf.resize(metadata.size as usize, 0);
    let mut done = 0;
    while done < elf.len() {
        match file.read_at(done as u64, &mut elf[done..]).map_err(errno)? {
            0 => break,
            read => done += read,
        }
    }
    elf.truncate(done);
    Ok(elf)
}

/// Linux's errno for `error`.
pub fn errno(error: ext4::Error) -> i64 {
    use ext4::Error::*;
    match error {
        NotPermitted => 1,
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
