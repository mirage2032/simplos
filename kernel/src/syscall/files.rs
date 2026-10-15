//! System calls on files: file descriptors, paths, and what's behind them,
//! the ext4 file system or the console.

use super::{EBADF, EFAULT, EINVAL, ENOTTY, EPERM};
use crate::fs::{self, Device, Handle};
use crate::pipe::{self, ReadError, WriteError};
use crate::process;
use crate::process::{Fd, Process};
use crate::signal::{self, ERESTARTSYS, Info};
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use ext4::{Kind, Metadata};

const ENOENT: i64 = 2;
const ENXIO: i64 = 6;
const EAGAIN: i64 = 11;
const EEXIST: i64 = 17;
const ENOTDIR: i64 = 20;
const EISDIR: i64 = 21;
const EMFILE: i64 = 24;
const ESPIPE: i64 = 29;
const ERANGE: i64 = 34;
const ENAMETOOLONG: i64 = 36;
const ENOTEMPTY: i64 = 39;
const EPIPE: i64 = 32;

const O_ACCMODE: u64 = 3;
const O_RDONLY: u64 = 0;
const O_WRONLY: u64 = 1;
const O_CREAT: u64 = 0o100;
const O_EXCL: u64 = 0o200;
const O_TRUNC: u64 = 0o1000;
const O_APPEND: u64 = 0o2000;
const O_NONBLOCK: u64 = 0o4000;
const O_DIRECTORY: u64 = 0o200000;
const O_CLOEXEC: u64 = 0o2000000;

/// A `dirfd` that means the working directory.
pub const AT_FDCWD: u64 = -100i64 as u64;
const AT_EMPTY_PATH: u64 = 0x1000;
/// For `unlinkat`: remove a directory, as `rmdir` does.
pub const AT_REMOVEDIR: u64 = 0x200;

/// The most one read or write moves: a program asking for more gets less,
/// as it may, and can't empty the kernel's heap.
const MOST: usize = 1 << 20;

/// Descriptors a program can have open at once.
pub(super) const MOST_FILES: usize = 1024;

/// An errno, positive.
pub(super) type Result<T> = core::result::Result<T, i64>;

/// What a system call returns: the value, or minus the errno.
pub(super) fn ret(result: Result<i64>) -> i64 {
    result.unwrap_or_else(|errno| -errno)
}

/// Set `handle`'s O_NONBLOCK as `flags` have it, if it has one.
fn set_nonblocking(handle: &Handle, flags: u64) {
    if let Some(nonblocking) = handle.nonblocking() {
        nonblocking.store(flags & O_NONBLOCK != 0, Relaxed);
    }
}

fn handle(process: &Process, fd: u64) -> Result<Arc<Handle>> {
    let files = process.files.lock();
    files.get(fd as usize).and_then(Option::as_ref).map(|fd| fd.handle.clone()).ok_or(EBADF)
}

/// Give `handle` the lowest descriptor that's free.
fn install(process: &Process, handle: Handle, cloexec: bool) -> Result<i64> {
    install_from(process, Arc::new(handle), 0, cloexec)
}

/// Give `handle` the lowest descriptor from `lowest` on that's free.
fn install_from(
    process: &Process,
    handle: Arc<Handle>,
    lowest: usize,
    cloexec: bool,
) -> Result<i64> {
    let mut files = process.files.lock();
    let free = files.iter().enumerate().skip(lowest).find(|(_, file)| file.is_none());
    let fd = free.map_or(files.len().max(lowest), |(fd, _)| fd);
    if fd >= MOST_FILES {
        return Err(EMFILE);
    }
    if fd >= files.len() {
        files.resize(fd + 1, None);
    }
    files[fd] = Some(Fd { handle, cloexec });
    Ok(fd as i64)
}

/// The NUL-terminated string at `addr` in the program's memory, without its
/// NUL: `too_long` if it's `most` bytes or more.
pub(super) fn string_at(
    process: &Process,
    addr: u64,
    most: usize,
    too_long: i64,
) -> Result<Vec<u8>> {
    let mut string = Vec::new();
    let mut at = addr;
    loop {
        // No further than the end of the page: the next may not be mapped.
        let mut chunk = vec![0; 4096 - (at % 4096) as usize];
        process.memory.lock().space.read(at, &mut chunk).ok_or(EFAULT)?;
        let end = chunk.iter().position(|&byte| byte == 0);
        string.extend_from_slice(&chunk[..end.unwrap_or(chunk.len())]);
        if string.len() >= most {
            return Err(too_long);
        }
        if end.is_some() {
            return Ok(string);
        }
        at += chunk.len() as u64;
    }
}

/// The path at `addr` in the program's memory.
pub(super) fn path_at(process: &Process, addr: u64) -> Result<String> {
    const PATH_MAX: usize = 4096;
    String::from_utf8(string_at(process, addr, PATH_MAX, ENAMETOOLONG)?).map_err(|_| EINVAL)
}

/// `path`, from the root, as `dirfd` says where it starts.
pub(super) fn lookup(process: &Process, dirfd: u64, path: &str) -> Result<String> {
    if path.is_empty() {
        return Err(ENOENT);
    }
    if path.starts_with('/') {
        return Ok(fs::resolve("", path));
    }
    if dirfd == AT_FDCWD {
        return Ok(fs::resolve(&process.cwd.lock(), path));
    }
    match &*handle(process, dirfd)? {
        Handle::Dir { path: base, .. } => Ok(fs::resolve(base, path)),
        _ => Err(ENOTDIR),
    }
}

pub fn open(process: &Process, dirfd: u64, path: u64, flags: u64, mode: u64) -> i64 {
    ret((|| {
        let access = flags & O_ACCMODE;
        if access == O_ACCMODE {
            return Err(EINVAL);
        }
        let path = lookup(process, dirfd, &path_at(process, path)?)?;
        let cloexec = flags & O_CLOEXEC != 0;
        if let Some(device) = fs::device(&path) {
            let handle = match device {
                Device::Null => Handle::Null,
                Device::Zero => Handle::Zero,
                Device::Tty => Handle::tty(process.tty.lock().clone().ok_or(ENXIO)?),
            };
            set_nonblocking(&handle, flags);
            return install(process, handle, cloexec);
        }
        let tree = fs::lock();
        let handle = match tree.metadata(&path) {
            Ok(Metadata { kind: Kind::Directory, .. }) => {
                if access != O_RDONLY || flags & O_CREAT != 0 {
                    return Err(EISDIR);
                }
                let (dir, entries) = tree.open_dir(&path).map_err(fs::errno)?;
                Handle::Dir { dir, entries, path, next: 0.into() }
            }
            Ok(_) if flags & O_DIRECTORY != 0 => return Err(ENOTDIR),
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
            Err(ext4::Error::NotFound) if flags & O_CREAT != 0 => {
                let disk = tree.disk(&path).map_err(fs::errno)?;
                let file = disk.create(&path).map_err(fs::errno)?;
                disk.set_permissions(&path, permissions(process, mode)).map_err(fs::errno)?;
                file_handle(file, access, flags)
            }
            Err(error) => return Err(fs::errno(error)),
            Ok(_) => {
                let disk = tree.disk(&path).map_err(fs::errno)?;
                let file = match access {
                    O_RDONLY => disk.open(&path),
                    _ if flags & O_TRUNC != 0 => disk.create(&path),
                    _ => disk.open_writable(&path),
                };
                file_handle(file.map_err(fs::errno)?, access, flags)
            }
        };
        install(process, handle, cloexec)
    })())
}

/// What a new file asked to have `mode` gets: less what the umask takes off.
///
/// shortcut: set just after it's made, so a crash in between leaves it with
/// lwext4's 0666 or 0777.
fn permissions(process: &Process, mode: u64) -> u32 {
    mode as u32 & 0o7777 & !process.umask.load(Relaxed)
}

fn file_handle(file: ext4::File, access: u64, flags: u64) -> Handle {
    Handle::File {
        file,
        read: access != O_WRONLY,
        write: access != O_RDONLY,
        append: flags & O_APPEND != 0,
        offset: 0.into(),
    }
}

pub fn close(process: &Process, fd: u64) -> i64 {
    // Closing a file is a call into the file system.
    let _tree = fs::lock();
    let handle = process.files.lock().get_mut(fd as usize).and_then(Option::take);
    match handle {
        Some(handle) => {
            drop(handle);
            0
        }
        None => -EBADF,
    }
}

/// Close the descriptors marked close-on-exec, as `execve` succeeds.
pub fn close_on_exec(process: &Process) {
    let _tree = fs::lock();
    let mut files = process.files.lock();
    let closed: Vec<Fd> = files.iter_mut().filter_map(|fd| fd.take_if(|fd| fd.cloexec)).collect();
    drop(files);
    drop(closed);
}

pub fn read(process: &Process, fd: u64, buf: u64, count: u64) -> i64 {
    ret(read_at(process, fd, buf, count, None))
}

/// `read` into each of the `count` buffers the `struct iovec`s at `iov`
/// name, in turn: one read as long as all of them, as Linux's is, so that a
/// terminal's line or a pipe's bytes come as they would to `read`.
pub fn readv(process: &Process, fd: u64, iov: u64, count: u64) -> i64 {
    ret((|| {
        let buffers = iovecs(process, iov, count)?;
        let total = buffers.iter().map(|&(_, len)| len).sum::<u64>();
        let data = read_data(process, fd, total, None)?;
        let mut memory = process.memory.lock();
        let mut rest = &data[..];
        for (base, len) in buffers {
            let (now, later) = rest.split_at((len as usize).min(rest.len()));
            memory.space.write(base, now).ok_or(EFAULT)?;
            rest = later;
        }
        Ok(data.len() as i64)
    })())
}

/// `write` what the `struct iovec`s at `iov` name, as one write: so a pipe
/// takes it whole or not at all, as for `write`.
pub fn writev(process: &Process, fd: u64, iov: u64, count: u64) -> i64 {
    ret((|| {
        let mut data = Vec::new();
        for (base, len) in iovecs(process, iov, count)? {
            let len = (len as usize).min(MOST - data.len());
            let start = data.len();
            data.resize(start + len, 0);
            process.memory.lock().space.read(base, &mut data[start..]).ok_or(EFAULT)?;
        }
        write_data(process, fd, &data, None)
    })())
}

/// The `count` buffers, as base and length, the `struct iovec`s at `iov` name.
fn iovecs(process: &Process, iov: u64, count: u64) -> Result<Vec<(u64, u64)>> {
    const IOV_MAX: u64 = 1024;
    if count > IOV_MAX {
        return Err(EINVAL);
    }
    let mut table = vec![0u8; count as usize * 16];
    process.memory.lock().space.read(iov, &mut table).ok_or(EFAULT)?;
    let word = |bytes: &[u8]| u64::from_le_bytes(bytes.try_into().unwrap());
    let buffers: Vec<_> = table
        .as_chunks::<16>()
        .0
        .iter()
        .map(|entry| (word(&entry[..8]), word(&entry[8..])))
        .collect();
    // Their total, as a `ssize_t`, has to be one.
    match buffers.iter().try_fold(0u64, |total, &(_, len)| total.checked_add(len)) {
        Some(total) if total as i64 >= 0 => Ok(buffers),
        _ => Err(EINVAL),
    }
}

pub fn pread(process: &Process, fd: u64, buf: u64, count: u64, offset: u64) -> i64 {
    ret(at(offset).and_then(|offset| read_at(process, fd, buf, count, Some(offset))))
}

pub fn write(process: &Process, fd: u64, buf: u64, count: u64) -> i64 {
    ret(write_at(process, fd, buf, count, None))
}

pub fn pwrite(process: &Process, fd: u64, buf: u64, count: u64, offset: u64) -> i64 {
    ret(at(offset).and_then(|offset| write_at(process, fd, buf, count, Some(offset))))
}

/// An offset a program gave, which can't be negative.
fn at(offset: u64) -> Result<u64> {
    if (offset as i64) < 0 { Err(EINVAL) } else { Ok(offset) }
}

/// The errno for a read that took nothing.
fn read_errno(error: ReadError) -> i64 {
    match error {
        ReadError::Empty => EAGAIN,
        ReadError::Interrupted => ERESTARTSYS,
    }
}

/// Read from `at`, or else from the descriptor's place, and move it on.
fn read_at(process: &Process, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<i64> {
    let data = read_data(process, fd, count, at)?;
    process.memory.lock().space.write(buf, &data).ok_or(EFAULT)?;
    Ok(data.len() as i64)
}

/// What a read of `count` bytes takes, from `at` or the descriptor's place.
fn read_data(process: &Process, fd: u64, count: u64, at: Option<u64>) -> Result<Vec<u8>> {
    let count = (count as usize).min(MOST);
    match &*handle(process, fd)? {
        Handle::Tty { .. } | Handle::Pipe { .. } if at.is_some() => Err(ESPIPE),
        Handle::Tty { tty, nonblocking } => {
            tty.read_or_wait(count, !nonblocking.load(Relaxed)).map_err(read_errno)
        }
        Handle::Null => Ok(Vec::new()),
        Handle::Zero => Ok(vec![0; count]),
        Handle::Pipe { end, .. } if end.write => Err(EBADF),
        // Not even for nothing: with no data, it would wait.
        Handle::Pipe { .. } if count == 0 => Ok(Vec::new()),
        Handle::Pipe { end, nonblocking } => {
            end.read(count, !nonblocking.load(Relaxed)).map_err(read_errno)
        }
        Handle::Dir { .. } => Err(EISDIR),
        Handle::File { read: false, .. } => Err(EBADF),
        Handle::File { file, offset, .. } => {
            let _tree = fs::lock();
            let from = at.unwrap_or_else(|| offset.load(Relaxed));
            let mut data = vec![0; count];
            let read = file.read_at(from, &mut data).map_err(fs::errno)?;
            data.truncate(read);
            if at.is_none() {
                offset.store(from + read as u64, Relaxed);
            }
            Ok(data)
        }
    }
}

/// Write at `at`, or else at the descriptor's place, and move it on.
fn write_at(process: &Process, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<i64> {
    let mut data = vec![0; (count as usize).min(MOST)];
    process.memory.lock().space.read(buf, &mut data).ok_or(EFAULT)?;
    write_data(process, fd, &data, at)
}

/// Write `data` at `at`, or else at the descriptor's place, and move it on.
fn write_data(process: &Process, fd: u64, data: &[u8], at: Option<u64>) -> Result<i64> {
    let handle = handle(process, fd)?;
    match &*handle {
        Handle::Tty { .. } | Handle::Pipe { .. } if at.is_some() => Err(ESPIPE),
        Handle::Tty { tty, .. } => {
            tty.write(data);
            Ok(data.len() as i64)
        }
        Handle::Null | Handle::Zero => Ok(data.len() as i64),
        Handle::Pipe { end, .. } if !end.write => Err(EBADF),
        Handle::Pipe { end, nonblocking } => match end.write(data, !nonblocking.load(Relaxed)) {
            Ok(written) => Ok(written as i64),
            // And SIGPIPE, which ends a program that doesn't ignore it.
            Err(WriteError::Broken) => {
                signal::send(process, signal::SIGPIPE, Info::user(process.id));
                Err(EPIPE)
            }
            Err(WriteError::Full) => Err(EAGAIN),
            Err(WriteError::Interrupted) => Err(ERESTARTSYS),
        },
        Handle::Dir { .. } | Handle::File { write: false, .. } => Err(EBADF),
        Handle::File { file, append, offset, .. } => {
            let _tree = fs::lock();
            // At the end for a file opened to append, even with `pwrite`, as
            // on Linux.
            let to = match (append, at) {
                (true, _) => file.size().map_err(fs::errno)?,
                (false, Some(at)) => at,
                (false, None) => offset.load(Relaxed),
            };
            let written = file.write_at(to, data).map_err(fs::errno)?;
            if at.is_none() {
                offset.store(to + written as u64, Relaxed);
            }
            Ok(written as i64)
        }
    }
}

/// Where the symbolic link at `path` points, at most `size` bytes of it.
///
/// shortcut: there are no symbolic links yet, so anything there isn't one:
/// EINVAL, as Linux says for a file.
pub fn readlinkat(process: &Process, dirfd: u64, path: u64, size: u64) -> i64 {
    ret((|| {
        if size as i64 <= 0 {
            return Err(EINVAL);
        }
        let path = lookup(process, dirfd, &path_at(process, path)?)?;
        fs::lock().metadata(&path).map_err(fs::errno)?;
        Err(EINVAL)
    })())
}

pub fn lseek(process: &Process, fd: u64, offset: u64, whence: u64) -> i64 {
    const SEEK_SET: u64 = 0;
    const SEEK_CUR: u64 = 1;
    const SEEK_END: u64 = 2;
    ret((|| {
        let offset = offset as i64;
        match &*handle(process, fd)? {
            Handle::Tty { .. } | Handle::Pipe { .. } => Err(ESPIPE),
            Handle::Null | Handle::Zero => Ok(0),
            Handle::File { file, offset: place, .. } => {
                let _tree = fs::lock();
                let from = match whence {
                    SEEK_SET => 0,
                    SEEK_CUR => place.load(Relaxed),
                    SEEK_END => file.size().map_err(fs::errno)?,
                    _ => return Err(EINVAL),
                };
                let to = (from as i64).checked_add(offset).filter(|to| *to >= 0).ok_or(EINVAL)?;
                place.store(to as u64, Relaxed);
                Ok(to)
            }
            // Places in a directory are entry numbers, as getdents64 hands
            // them out: they can be gone back to, but not added to.
            Handle::Dir { next, .. } => match whence {
                SEEK_SET if offset >= 0 => {
                    next.store(offset as usize, Relaxed);
                    Ok(offset)
                }
                SEEK_CUR if offset == 0 => Ok(next.load(Relaxed) as i64),
                _ => Err(EINVAL),
            },
        }
    })())
}

/// The descriptor's file's metadata.
fn metadata(handle: &Handle) -> Result<Metadata> {
    let tree = fs::lock();
    match handle {
        Handle::File { file, .. } => file.metadata().map_err(fs::errno),
        Handle::Dir { dir: Some(dir), .. } => dir.metadata().map_err(fs::errno),
        Handle::Dir { path, .. } => tree.metadata(path).map_err(fs::errno),
        special => Ok(fs::special(special).expect("not on the disk")),
    }
}

/// Linux's `struct stat` for x86-64, from `metadata`.
fn stat_bytes(metadata: &Metadata) -> [u8; 144] {
    let kind = match metadata.kind {
        Kind::File => 0o100000,
        Kind::Directory => 0o040000,
        Kind::Symlink => 0o120000,
        // Devices and pipes, whose `permissions` carry their type.
        Kind::Other => 0,
    };
    let mut stat = [0; 144];
    let wide = [
        (8, u64::from(metadata.inode)),
        (16, u64::from(metadata.links)),
        (48, metadata.size),
        (56, 4096),
        (64, metadata.blocks),
        (72, u64::from(metadata.accessed)),
        (88, u64::from(metadata.modified)),
        (104, u64::from(metadata.changed)),
    ];
    for (at, value) in wide {
        stat[at..at + 8].copy_from_slice(&value.to_le_bytes());
    }
    for (at, value) in [(24, kind | metadata.permissions), (28, metadata.uid), (32, metadata.gid)] {
        stat[at..at + 4].copy_from_slice(&value.to_le_bytes());
    }
    stat
}

fn put_stat(process: &Process, metadata: &Metadata, buf: u64) -> Result<i64> {
    process.memory.lock().space.write(buf, &stat_bytes(metadata)).ok_or(EFAULT)?;
    Ok(0)
}

pub fn fstat(process: &Process, fd: u64, buf: u64) -> i64 {
    ret(handle(process, fd).and_then(|handle| put_stat(process, &metadata(&handle)?, buf)))
}

/// `stat`, `lstat` and `fstatat`, which are all the same: no symbolic link
/// is followed yet.
pub fn newfstatat(process: &Process, dirfd: u64, path: u64, buf: u64, flags: u64) -> i64 {
    ret((|| {
        let path = path_at(process, path)?;
        // The file `dirfd` is open on itself.
        if path.is_empty() && flags & AT_EMPTY_PATH != 0 && dirfd != AT_FDCWD {
            return put_stat(process, &metadata(&*handle(process, dirfd)?)?, buf);
        }
        let path = match path.is_empty() && flags & AT_EMPTY_PATH != 0 {
            true => String::new(),
            false => lookup(process, dirfd, &path)?,
        };
        let metadata = fs::lock().metadata(&path).map_err(fs::errno)?;
        put_stat(process, &metadata, buf)
    })())
}

/// Hand out the directory's next entries, as many as fit in `count` bytes,
/// as `struct linux_dirent64`s.
pub fn getdents64(process: &Process, fd: u64, buf: u64, count: u64) -> i64 {
    ret((|| {
        let handle = handle(process, fd)?;
        let Handle::Dir { entries, next, .. } = &*handle else { return Err(ENOTDIR) };
        let mut out = Vec::new();
        let mut at = next.load(Relaxed);
        for entry in entries.iter().skip(at) {
            let name = entry.name.as_bytes();
            // Inode, offset, length and type, then the name and its NUL,
            // padded so the next one starts 8-byte aligned.
            let length = (19 + name.len() + 1).next_multiple_of(8);
            if out.len() + length > count as usize {
                break;
            }
            let start = out.len();
            out.extend_from_slice(&u64::from(entry.inode).to_le_bytes());
            out.extend_from_slice(&(at as i64 + 1).to_le_bytes());
            out.extend_from_slice(&(length as u16).to_le_bytes());
            out.push(match entry.kind {
                Kind::File => 8,
                Kind::Directory => 4,
                Kind::Symlink => 10,
                Kind::Other => 0,
            });
            out.extend_from_slice(name);
            out.resize(start + length, 0);
            at += 1;
        }
        if out.is_empty() && at < entries.len() {
            // Not even one fits.
            return Err(EINVAL);
        }
        process.memory.lock().space.write(buf, &out).ok_or(EFAULT)?;
        next.store(at, Relaxed);
        Ok(out.len() as i64)
    })())
}

/// Copies of descriptors, and their flags.
///
/// shortcut: of the flags `open` set, only O_NONBLOCK can be changed.
pub fn fcntl(process: &Process, fd: u64, command: u64, arg: u64) -> i64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    const FD_CLOEXEC: u64 = 1;
    ret((|| {
        let handle = handle(process, fd)?;
        match command {
            F_DUPFD => install_from(process, handle, arg as usize, false),
            F_DUPFD_CLOEXEC => install_from(process, handle, arg as usize, true),
            F_GETFD | F_SETFD => {
                let mut files = process.files.lock();
                let fd = files[fd as usize].as_mut().ok_or(EBADF)?;
                if command == F_SETFD {
                    fd.cloexec = arg & FD_CLOEXEC != 0;
                    return Ok(0);
                }
                Ok(fd.cloexec as i64)
            }
            F_SETFL => {
                set_nonblocking(&handle, arg);
                Ok(0)
            }
            F_GETFL => Ok(match &*handle {
                Handle::Tty { nonblocking, .. } => 2 | nonblocking_flag(nonblocking),
                Handle::Null | Handle::Zero => 2,
                Handle::Pipe { end, nonblocking } => {
                    end.write as i64 | nonblocking_flag(nonblocking)
                }
                Handle::Dir { .. } => (O_RDONLY | O_DIRECTORY) as i64,
                Handle::File { read, write, append, .. } => {
                    let access = match (read, write) {
                        (true, false) => O_RDONLY,
                        (false, _) => O_WRONLY,
                        (true, true) => 2,
                    };
                    (access | if *append { O_APPEND } else { 0 }) as i64
                }
            }),
            _ => Err(EINVAL),
        }
    })())
}

fn nonblocking_flag(nonblocking: &core::sync::atomic::AtomicBool) -> i64 {
    if nonblocking.load(Relaxed) { O_NONBLOCK as i64 } else { 0 }
}

pub fn mkdirat(process: &Process, dirfd: u64, path: u64, mode: u64) -> i64 {
    ret((|| {
        let path = lookup(process, dirfd, &path_at(process, path)?)?;
        let tree = fs::lock();
        let disk = tree.disk(&path).map_err(fs::errno)?;
        disk.create_dir(&path).map_err(fs::errno)?;
        disk.set_permissions(&path, permissions(process, mode)).map_err(fs::errno)?;
        Ok(0)
    })())
}

/// `unlink`, or with `AT_REMOVEDIR`, `rmdir`.
pub fn unlinkat(process: &Process, dirfd: u64, path: u64, flags: u64) -> i64 {
    ret((|| {
        let name = path_at(process, path)?;
        let path = lookup(process, dirfd, &name)?;
        let tree = fs::lock();
        let disk = tree.disk(&path).map_err(fs::errno)?;
        let removed = match flags & AT_REMOVEDIR {
            0 => disk.remove_file(&path),
            // `lookup` takes these out, which would remove the wrong
            // directory; Linux refuses them.
            _ => match name.trim_end_matches('/').rsplit('/').next() {
                Some(".") => return Err(EINVAL),
                Some("..") => return Err(ENOTEMPTY),
                _ => disk.remove_dir(&path),
            },
        };
        removed.map_err(fs::errno)?;
        Ok(0)
    })())
}

/// `rename`, `renameat` and `renameat2`, which may be asked not to replace
/// anything.
pub fn renameat2(
    process: &Process,
    from_dirfd: u64,
    from: u64,
    to_dirfd: u64,
    to: u64,
    flags: u64,
) -> i64 {
    const RENAME_NOREPLACE: u64 = 1;
    ret((|| {
        // Not RENAME_EXCHANGE nor RENAME_WHITEOUT, yet.
        if flags & !RENAME_NOREPLACE != 0 {
            return Err(EINVAL);
        }
        let from = lookup(process, from_dirfd, &path_at(process, from)?)?;
        let to = lookup(process, to_dirfd, &path_at(process, to)?)?;
        let tree = fs::lock();
        if flags & RENAME_NOREPLACE != 0 && tree.metadata(&to).is_ok() {
            return Err(EEXIST);
        }
        // Both on the disk: nothing else can be written.
        tree.disk(&to).map_err(fs::errno)?;
        tree.disk(&from).map_err(fs::errno)?.rename(&from, &to).map_err(fs::errno)?;
        Ok(0)
    })())
}

pub fn chdir(process: &Process, path: u64) -> i64 {
    ret((|| {
        let path = lookup(process, AT_FDCWD, &path_at(process, path)?)?;
        if fs::lock().metadata(&path).map_err(fs::errno)?.kind != Kind::Directory {
            return Err(ENOTDIR);
        }
        *process.cwd.lock() = path;
        Ok(0)
    })())
}

/// The working directory, NUL-terminated: its length with the NUL.
pub fn getcwd(process: &Process, buf: u64, size: u64) -> i64 {
    ret((|| {
        let cwd = alloc::format!("/{}\0", process.cwd.lock());
        if cwd.len() as u64 > size {
            return Err(ERANGE);
        }
        process.memory.lock().space.write(buf, cwd.as_bytes()).ok_or(EFAULT)?;
        Ok(cwd.len() as i64)
    })())
}

/// Set the umask, and say what it was.
pub fn umask(process: &Process, mask: u64) -> i64 {
    process.umask.swap(mask as u32 & 0o777, Relaxed).into()
}

/// `pipe` and `pipe2`: a new pipe, whose reading and writing descriptors go
/// to `fds` as two ints.
pub fn pipe2(process: &Process, fds: u64, flags: u64) -> i64 {
    ret((|| {
        if flags & !(O_CLOEXEC | O_NONBLOCK) != 0 {
            return Err(EINVAL);
        }
        let cloexec = flags & O_CLOEXEC != 0;
        let (read, write) = pipe::new();
        let (read, write) = (Handle::pipe(read), Handle::pipe(write));
        set_nonblocking(&read, flags);
        set_nonblocking(&write, flags);
        let read = install(process, read, cloexec)?;
        let installed = install(process, write, cloexec).and_then(|write| {
            let mut ends = [0u8; 8];
            ends[..4].copy_from_slice(&(read as i32).to_le_bytes());
            ends[4..].copy_from_slice(&(write as i32).to_le_bytes());
            if process.memory.lock().space.write(fds, &ends).is_none() {
                let gone = process.files.lock()[write as usize].take();
                drop(gone);
                return Err(EFAULT);
            }
            Ok(0)
        });
        if installed.is_err() {
            let gone = process.files.lock()[read as usize].take();
            drop(gone);
        }
        installed
    })())
}

pub fn dup(process: &Process, fd: u64) -> i64 {
    ret(handle(process, fd).and_then(|handle| install_from(process, handle, 0, false)))
}

/// `dup2` and `dup3`: `fd` copied to `to`, closing what was there first.
/// `dup2` takes copying a descriptor to itself; `dup3` doesn't.
pub fn dup3(process: &Process, fd: u64, to: u64, flags: u64, dup2: bool) -> i64 {
    ret((|| {
        let handle = handle(process, fd)?;
        if fd == to {
            return if dup2 { Ok(to as i64) } else { Err(EINVAL) };
        }
        if flags & !O_CLOEXEC != 0 {
            return Err(EINVAL);
        }
        let to = to as usize;
        if to >= MOST_FILES {
            return Err(EBADF);
        }
        // Closing what was there is a call into the file system.
        let _tree = fs::lock();
        let replaced = {
            let mut files = process.files.lock();
            if to >= files.len() {
                files.resize(to + 1, None);
            }
            files[to].replace(Fd { handle, cloexec: flags & O_CLOEXEC != 0 })
        };
        drop(replaced);
        Ok(to as i64)
    })())
}

/// Every change is on the disk by the time the call that made it returns.
pub fn fsync(process: &Process, fd: u64) -> i64 {
    ret(handle(process, fd).map(|_| 0))
}

/// What `fd` refers to, for `poll`: `None` if it isn't open.
pub fn open_handle(process: &Process, fd: i32) -> Option<Arc<Handle>> {
    handle(process, fd as u64).ok()
}

/// The terminal calls: its settings, its size, what's waiting in it, and
/// whose it is: its session, and the group in its foreground; and for
/// anything, FIONBIO, which sets O_NONBLOCK.
pub fn ioctl(process: &Process, fd: u64, request: u64, arg: u64) -> i64 {
    use crate::tty::Termios;
    const TCGETS: u64 = 0x5401;
    const TCSETS: u64 = 0x5402;
    const TCSETSW: u64 = 0x5403;
    const TCSETSF: u64 = 0x5404;
    const TCFLSH: u64 = 0x540B;
    const TIOCSCTTY: u64 = 0x540E;
    const TIOCGPGRP: u64 = 0x540F;
    const TIOCSPGRP: u64 = 0x5410;
    const TIOCNOTTY: u64 = 0x5422;
    const TIOCGSID: u64 = 0x5429;
    const TIOCGWINSZ: u64 = 0x5413;
    const TIOCSWINSZ: u64 = 0x5414;
    const FIONREAD: u64 = 0x541B;
    const FIONBIO: u64 = 0x5421;
    ret((|| {
        let handle = handle(process, fd)?;
        let mut memory = process.memory.lock();
        if request == FIONBIO {
            let mut on = [0; 4];
            memory.space.read(arg, &mut on).ok_or(EFAULT)?;
            let on = i32::from_le_bytes(on) != 0;
            set_nonblocking(&handle, if on { O_NONBLOCK } else { 0 });
            return Ok(0);
        }
        let Handle::Tty { tty, .. } = &*handle else { return Err(ENOTTY) };
        // Whether it's the caller's session's terminal.
        let controlling = process.tty.lock().as_ref().is_some_and(|own| Arc::ptr_eq(own, tty));
        let mut put = |bytes: &[u8]| memory.space.write(arg, bytes).ok_or(EFAULT).map(|()| 0);
        match request {
            TIOCGPGRP | TIOCSPGRP | TIOCGSID | TIOCNOTTY if !controlling => Err(ENOTTY),
            TIOCGPGRP => put(&(tty.foreground() as i32).to_le_bytes()),
            TIOCGSID => put(&(tty.session() as i32).to_le_bytes()),
            TIOCSPGRP => {
                let mut group = [0u8; 4];
                memory.space.read(arg, &mut group).ok_or(EFAULT)?;
                let group = i32::from_le_bytes(group);
                if group < 0 {
                    return Err(EINVAL);
                }
                // One of its session's groups.
                let members = process::group(group as u64);
                if !members.iter().any(|member| member.sid() == process.sid()) {
                    return Err(EPERM);
                }
                tty.set_foreground(group as u64);
                Ok(0)
            }
            // A session's leader without a terminal takes this one; from
            // another session only if it asks to, with 1.
            TIOCSCTTY => {
                let taken = tty.session() != 0 && tty.session() != process.sid();
                if process.sid() != process.id
                    || process.tty.lock().is_some()
                    || (taken && arg != 1)
                {
                    return Err(EPERM);
                }
                tty.control(process.sid(), process.pgid());
                *process.tty.lock() = Some(tty.clone());
                Ok(0)
            }
            // shortcut: a session's leader giving its terminal up doesn't
            // send its foreground SIGHUP, as Linux's does.
            TIOCNOTTY => {
                *process.tty.lock() = None;
                if process.sid() == process.id {
                    tty.control(0, 0);
                }
                Ok(0)
            }
            TCGETS => put(&tty.termios().to_bytes()),
            TIOCGWINSZ => {
                let (rows, cols) = tty.size();
                let mut winsize = [0; 8];
                winsize[..2].copy_from_slice(&rows.to_le_bytes());
                winsize[2..4].copy_from_slice(&cols.to_le_bytes());
                put(&winsize)
            }
            FIONREAD => put(&(tty.available() as i32).to_le_bytes()),
            TCSETS | TCSETSW | TCSETSF => {
                // Output is written as it comes, so there's none to wait for.
                let mut termios = [0; Termios::SIZE];
                memory.space.read(arg, &mut termios).ok_or(EFAULT)?;
                tty.set_termios(Termios::from_bytes(&termios), request == TCSETSF);
                Ok(0)
            }
            TIOCSWINSZ => {
                let mut winsize = [0u8; 8];
                memory.space.read(arg, &mut winsize).ok_or(EFAULT)?;
                let half = |at: usize| u16::from_le_bytes([winsize[at], winsize[at + 1]]);
                tty.set_size(half(0), half(2));
                Ok(0)
            }
            // Input, or input and output, of which there's none waiting.
            TCFLSH if arg == 0 || arg == 2 => {
                tty.flush_input();
                Ok(0)
            }
            TCFLSH if arg == 1 => Ok(0),
            TCFLSH => Err(EINVAL),
            _ => Err(ENOTTY),
        }
    })())
}
