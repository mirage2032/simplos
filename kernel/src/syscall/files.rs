//! System calls on files: file descriptors, paths, and what's behind them,
//! the ext4 file system or the console.

use super::{EBADF, EFAULT, EINVAL};
use crate::fs::{self, Handle, ROOT};
use crate::pipe;
use crate::process::Process;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use ext4::{Kind, Metadata};

const ENOENT: i64 = 2;
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
const MOST_FILES: usize = 1024;

/// An errno, positive.
type Result<T> = core::result::Result<T, i64>;

/// What a system call returns: the value, or minus the errno.
fn ret(result: Result<i64>) -> i64 {
    result.unwrap_or_else(|errno| -errno)
}

fn handle(process: &Process, fd: u64) -> Result<Arc<Handle>> {
    let files = process.files.lock();
    files.get(fd as usize).cloned().flatten().ok_or(EBADF)
}

/// Give `handle` the lowest descriptor that's free.
fn install(process: &Process, handle: Handle) -> Result<i64> {
    install_from(process, Arc::new(handle), 0)
}

/// Give `handle` the lowest descriptor from `lowest` on that's free.
fn install_from(process: &Process, handle: Arc<Handle>, lowest: usize) -> Result<i64> {
    let mut files = process.files.lock();
    let free = files.iter().enumerate().skip(lowest).find(|(_, file)| file.is_none());
    let fd = free.map_or(files.len().max(lowest), |(fd, _)| fd);
    if fd >= MOST_FILES {
        return Err(EMFILE);
    }
    if fd >= files.len() {
        files.resize(fd + 1, None);
    }
    files[fd] = Some(handle);
    Ok(fd as i64)
}

/// The NUL-terminated string at `addr` in the program's memory.
fn path_at(process: &Process, addr: u64) -> Result<String> {
    const PATH_MAX: usize = 4096;
    let mut path = Vec::new();
    let mut memory = process.memory.lock();
    let mut at = addr;
    loop {
        // No further than the end of the page: the next may not be mapped.
        let mut chunk = vec![0; 4096 - (at % 4096) as usize];
        memory.space.read(at, &mut chunk).ok_or(EFAULT)?;
        let end = chunk.iter().position(|&byte| byte == 0);
        path.extend_from_slice(&chunk[..end.unwrap_or(chunk.len())]);
        if path.len() >= PATH_MAX {
            return Err(ENAMETOOLONG);
        }
        if end.is_some() {
            return String::from_utf8(path).map_err(|_| EINVAL);
        }
        at += chunk.len() as u64;
    }
}

/// `path`, from the root, as `dirfd` says where it starts.
fn lookup(process: &Process, dirfd: u64, path: &str) -> Result<String> {
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
        if let Some(device) = fs::device(&path) {
            return install(process, device);
        }
        let root = ROOT.lock();
        let fs = root.as_ref().ok_or(ENOENT)?;
        let found = fs.metadata(&path);
        let handle = match found {
            Ok(Metadata { kind: Kind::Directory, .. }) => {
                if access != O_RDONLY || flags & O_CREAT != 0 {
                    return Err(EISDIR);
                }
                let dir = fs.open_dir(&path).map_err(fs::errno)?;
                Handle::Dir { dir, path, next: 0.into() }
            }
            Ok(_) if flags & O_DIRECTORY != 0 => return Err(ENOTDIR),
            Ok(_) if flags & O_CREAT != 0 && flags & O_EXCL != 0 => return Err(EEXIST),
            Err(ext4::Error::NotFound) if flags & O_CREAT != 0 => {
                let file = fs.create(&path).map_err(fs::errno)?;
                fs.set_permissions(&path, permissions(process, mode)).map_err(fs::errno)?;
                file_handle(file, access, flags)
            }
            Err(error) => return Err(fs::errno(error)),
            Ok(_) => {
                let file = match access {
                    O_RDONLY => fs.open(&path),
                    _ if flags & O_TRUNC != 0 => fs.create(&path),
                    _ => fs.open_writable(&path),
                };
                file_handle(file.map_err(fs::errno)?, access, flags)
            }
        };
        install(process, handle)
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
    let _root = ROOT.lock();
    let handle = process.files.lock().get_mut(fd as usize).and_then(Option::take);
    match handle {
        Some(handle) => {
            drop(handle);
            0
        }
        None => -EBADF,
    }
}

/// Close every descriptor, as a program ends.
pub fn close_all(process: &Process) {
    let _root = ROOT.lock();
    let files = core::mem::take(&mut *process.files.lock());
    drop(files);
}

pub fn read(process: &Process, fd: u64, buf: u64, count: u64) -> i64 {
    ret(read_at(process, fd, buf, count, None))
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

/// Read from `at`, or else from the descriptor's place, and move it on.
fn read_at(process: &Process, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<i64> {
    let count = (count as usize).min(MOST);
    match &*handle(process, fd)? {
        Handle::Console | Handle::Pipe(_) if at.is_some() => Err(ESPIPE),
        // shortcut: programs get no input yet: their standard input is at
        // its end.
        Handle::Console => Ok(0),
        Handle::Null => Ok(0),
        Handle::Zero => {
            process.memory.lock().space.write(buf, &vec![0; count]).ok_or(EFAULT)?;
            Ok(count as i64)
        }
        Handle::Pipe(end) if end.write => Err(EBADF),
        // Not even for nothing: with no data, it would wait.
        Handle::Pipe(_) if count == 0 => Ok(0),
        Handle::Pipe(end) => {
            let data = end.read(count);
            process.memory.lock().space.write(buf, &data).ok_or(EFAULT)?;
            Ok(data.len() as i64)
        }
        Handle::Dir { .. } => Err(EISDIR),
        Handle::File { read: false, .. } => Err(EBADF),
        Handle::File { file, offset, .. } => {
            let _root = ROOT.lock();
            let from = at.unwrap_or_else(|| offset.load(Relaxed));
            let mut data = vec![0; count];
            let read = file.read_at(from, &mut data).map_err(fs::errno)?;
            process.memory.lock().space.write(buf, &data[..read]).ok_or(EFAULT)?;
            if at.is_none() {
                offset.store(from + read as u64, Relaxed);
            }
            Ok(read as i64)
        }
    }
}

/// Write at `at`, or else at the descriptor's place, and move it on.
fn write_at(process: &Process, fd: u64, buf: u64, count: u64, at: Option<u64>) -> Result<i64> {
    let handle = handle(process, fd)?;
    let mut data = vec![0; (count as usize).min(MOST)];
    process.memory.lock().space.read(buf, &mut data).ok_or(EFAULT)?;
    match &*handle {
        Handle::Console | Handle::Pipe(_) if at.is_some() => Err(ESPIPE),
        Handle::Console => {
            (process.output.lock())(&data);
            Ok(data.len() as i64)
        }
        Handle::Null | Handle::Zero => Ok(data.len() as i64),
        Handle::Pipe(end) if !end.write => Err(EBADF),
        // shortcut: no SIGPIPE, as there are no signals: just the error,
        // which is what a Rust program, ignoring SIGPIPE, gets anyway.
        Handle::Pipe(end) => end.write(&data).map(|written| written as i64).ok_or(EPIPE),
        Handle::Dir { .. } | Handle::File { write: false, .. } => Err(EBADF),
        Handle::File { file, append, offset, .. } => {
            let _root = ROOT.lock();
            // At the end for a file opened to append, even with `pwrite`, as
            // on Linux.
            let to = match (append, at) {
                (true, _) => file.size().map_err(fs::errno)?,
                (false, Some(at)) => at,
                (false, None) => offset.load(Relaxed),
            };
            let written = file.write_at(to, &data).map_err(fs::errno)?;
            if at.is_none() {
                offset.store(to + written as u64, Relaxed);
            }
            Ok(written as i64)
        }
    }
}

pub fn lseek(process: &Process, fd: u64, offset: u64, whence: u64) -> i64 {
    const SEEK_SET: u64 = 0;
    const SEEK_CUR: u64 = 1;
    const SEEK_END: u64 = 2;
    ret((|| {
        let offset = offset as i64;
        match &*handle(process, fd)? {
            Handle::Console | Handle::Pipe(_) => Err(ESPIPE),
            Handle::Null | Handle::Zero => Ok(0),
            Handle::File { file, offset: place, .. } => {
                let _root = ROOT.lock();
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
    // What isn't on the disk has its type in `permissions`.
    let special = |mode| Metadata {
        inode: 0,
        kind: Kind::Other,
        permissions: mode,
        links: 1,
        uid: 0,
        gid: 0,
        size: 0,
        blocks: 0,
        accessed: 0,
        modified: 0,
        changed: 0,
    };
    let _root = ROOT.lock();
    match handle {
        // Character devices, as a terminal is.
        Handle::Console => Ok(special(0o020620)),
        Handle::Null | Handle::Zero => Ok(special(0o020666)),
        // A FIFO.
        Handle::Pipe(_) => Ok(special(0o010600)),
        Handle::File { file, .. } => file.metadata().map_err(fs::errno),
        Handle::Dir { dir, .. } => dir.metadata().map_err(fs::errno),
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
        if let Some(device) = fs::device(&path) {
            return put_stat(process, &metadata(&device)?, buf);
        }
        let metadata = {
            let root = ROOT.lock();
            let fs = root.as_ref().ok_or(ENOENT)?;
            fs.metadata(&path).map_err(fs::errno)?
        };
        put_stat(process, &metadata, buf)
    })())
}

/// Hand out the directory's next entries, as many as fit in `count` bytes,
/// as `struct linux_dirent64`s.
pub fn getdents64(process: &Process, fd: u64, buf: u64, count: u64) -> i64 {
    ret((|| {
        let handle = handle(process, fd)?;
        let Handle::Dir { dir, next, .. } = &*handle else { return Err(ENOTDIR) };
        let mut out = Vec::new();
        let mut at = next.load(Relaxed);
        for entry in dir.entries().iter().skip(at) {
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
        if out.is_empty() && at < dir.entries().len() {
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
/// shortcut: close-on-exec is taken and forgotten: until `execve`, it means
/// nothing. Nor can the flags `open` set be changed.
pub fn fcntl(process: &Process, fd: u64, command: u64, arg: u64) -> i64 {
    const F_DUPFD: u64 = 0;
    const F_GETFD: u64 = 1;
    const F_SETFD: u64 = 2;
    const F_GETFL: u64 = 3;
    const F_SETFL: u64 = 4;
    const F_DUPFD_CLOEXEC: u64 = 1030;
    ret((|| {
        let handle = handle(process, fd)?;
        match command {
            F_DUPFD | F_DUPFD_CLOEXEC => install_from(process, handle, arg as usize),
            F_GETFD | F_SETFD | F_SETFL => Ok(0),
            F_GETFL => Ok(match &*handle {
                Handle::Console | Handle::Null | Handle::Zero => 2,
                Handle::Pipe(end) => end.write as i64,
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

pub fn mkdirat(process: &Process, dirfd: u64, path: u64, mode: u64) -> i64 {
    ret((|| {
        let path = lookup(process, dirfd, &path_at(process, path)?)?;
        let root = ROOT.lock();
        let fs = root.as_ref().ok_or(ENOENT)?;
        fs.create_dir(&path).map_err(fs::errno)?;
        fs.set_permissions(&path, permissions(process, mode)).map_err(fs::errno)?;
        Ok(0)
    })())
}

/// `unlink`, or with `AT_REMOVEDIR`, `rmdir`.
pub fn unlinkat(process: &Process, dirfd: u64, path: u64, flags: u64) -> i64 {
    ret((|| {
        let name = path_at(process, path)?;
        let path = lookup(process, dirfd, &name)?;
        let root = ROOT.lock();
        let fs = root.as_ref().ok_or(ENOENT)?;
        let removed = match flags & AT_REMOVEDIR {
            0 => fs.remove_file(&path),
            // `lookup` takes these out, which would remove the wrong
            // directory; Linux refuses them.
            _ => match name.trim_end_matches('/').rsplit('/').next() {
                Some(".") => return Err(EINVAL),
                Some("..") => return Err(ENOTEMPTY),
                _ => fs.remove_dir(&path),
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
        let root = ROOT.lock();
        let fs = root.as_ref().ok_or(ENOENT)?;
        if flags & RENAME_NOREPLACE != 0 && fs.metadata(&to).is_ok() {
            return Err(EEXIST);
        }
        fs.rename(&from, &to).map_err(fs::errno)?;
        Ok(0)
    })())
}

pub fn chdir(process: &Process, path: u64) -> i64 {
    ret((|| {
        let path = lookup(process, AT_FDCWD, &path_at(process, path)?)?;
        let root = ROOT.lock();
        let fs = root.as_ref().ok_or(ENOENT)?;
        if fs.metadata(&path).map_err(fs::errno)?.kind != Kind::Directory {
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
///
/// shortcut: no O_NONBLOCK, which nothing has needed yet; close-on-exec is
/// taken and forgotten, as with `fcntl`.
pub fn pipe2(process: &Process, fds: u64, flags: u64) -> i64 {
    ret((|| {
        if flags & !O_CLOEXEC != 0 {
            return Err(EINVAL);
        }
        let (read, write) = pipe::new();
        let read = install(process, Handle::Pipe(read))?;
        let installed = install(process, Handle::Pipe(write)).and_then(|write| {
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
    ret(handle(process, fd).and_then(|handle| install_from(process, handle, 0)))
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
        let _root = ROOT.lock();
        let replaced = {
            let mut files = process.files.lock();
            if to >= files.len() {
                files.resize(to + 1, None);
            }
            files[to].replace(handle)
        };
        drop(replaced);
        Ok(to as i64)
    })())
}

/// Every change is on the disk by the time the call that made it returns.
pub fn fsync(process: &Process, fd: u64) -> i64 {
    ret(handle(process, fd).map(|_| 0))
}

/// Whether `fd` is open, for `poll`.
pub fn is_open(process: &Process, fd: i32) -> bool {
    handle(process, fd as u64).is_ok()
}
