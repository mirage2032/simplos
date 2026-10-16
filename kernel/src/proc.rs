//! /proc, as Linux's procfs shows it, made as it's read: what the kernel
//! knows of the machine, and of each process in a directory named by its
//! id. Enough for busybox's `free`, `ps`, `top`, `df` and `mount`, and for
//! what reads /proc/self.
//!
//! shortcut: no /proc/PID/maps, environ or the like; add them as programs
//! ask for them.

use crate::io::disk;
use crate::process::{self, CPU_TIME, Process};
use crate::time::{self, NANOS, TICK};
use crate::{memory, thread};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;
use ext4::{DirEntry, Ext4, Kind, Metadata};

/// What's at a path in /proc.
pub enum Node {
    Dir(Vec<DirEntry>),
    File(Vec<u8>),
    Link(Vec<u8>),
}

impl Node {
    /// What `stat` tells of it, at `path`, a path from the root. A file's
    /// size is 0, as on Linux: it's only known as it's read.
    pub fn metadata(&self, path: &str) -> Metadata {
        let now = (time::now() / NANOS) as u32;
        let (kind, permissions, links, size) = match self {
            Node::Dir(_) => (Kind::Directory, 0o555, 2, 0),
            Node::File(_) => (Kind::File, 0o444, 1, 0),
            Node::Link(target) => (Kind::Symlink, 0o777, 1, target.len() as u64),
        };
        Metadata {
            inode: inode(path),
            kind,
            permissions,
            links,
            uid: 0,
            gid: 0,
            size,
            blocks: 0,
            accessed: now,
            modified: now,
            changed: now,
        }
    }
}

/// An inode number for `path`, a path from the root, that no file on a disk
/// has: ext4's go up to its inode count, far below these.
pub fn inode(path: &str) -> u32 {
    let hash = path
        .bytes()
        .fold(0x811c_9dc5u32, |hash, byte| (hash ^ u32::from(byte)).wrapping_mul(0x0100_0193));
    0xF000_0000 | hash >> 4
}

/// What's at `path` within /proc, "" for /proc itself; `disk`, the root's,
/// for /proc/mounts.
pub fn node(path: &str, disk: Option<&Ext4>) -> Option<Node> {
    let (first, rest) = path.split_once('/').unwrap_or((path, ""));
    let file = |text: String| Some(Node::File(text.into_bytes()));
    match (first, rest) {
        ("", "") => Some(Node::Dir(root())),
        ("self", "") => thread::current_process().map(|me| Node::Link(me.id.to_string().into())),
        ("meminfo", "") => file(meminfo()),
        ("uptime", "") => file(uptime()),
        ("loadavg", "") => file(loadavg()),
        ("stat", "") => file(stat()),
        ("mounts", "") => file(mounts(disk)),
        ("diskstats", "") => file(diskstats()),
        ("version", "") => file(format!("Linux version {}\n", crate::syscall::RELEASE)),
        (id, rest) => {
            let process = process::find(id.parse().ok()?)?;
            match rest {
                "" => {
                    let mut entries: Vec<DirEntry> = PROCESS_FILES
                        .map(|name| entry(name, Kind::File, &format!("{id}/{name}")))
                        .into();
                    entries.push(entry("fd", Kind::Directory, &format!("{id}/fd")));
                    Some(Node::Dir(entries))
                }
                "fd" => Some(Node::Dir(descriptors(&process, &format!("proc/{id}/fd")))),
                "stat" => file(process_stat(&process)),
                "status" => file(status(&process)),
                "cmdline" => Some(Node::File(process.command.lock().args.clone())),
                "comm" => file(format!("{}\n", process.command.lock().name)),
                _ => None,
            }
        }
    }
}

/// What's in each process's directory, but for `fd`.
const PROCESS_FILES: [&str; 4] = ["cmdline", "comm", "stat", "status"];

/// The entries of a directory of `process`'s open descriptors, at `path`,
/// a path from the root: /dev/fd or /proc/PID/fd.
pub fn descriptors(process: &Process, path: &str) -> Vec<DirEntry> {
    let files = process.files.lock();
    let open = files.iter().enumerate().filter(|(_, file)| file.is_some());
    let entry = |fd: usize| DirEntry {
        name: fd.to_string(),
        kind: Kind::Other,
        inode: inode(&format!("{path}/{fd}")),
    };
    open.map(|(fd, _)| entry(fd)).collect()
}

fn entry(name: &str, kind: Kind, path: &str) -> DirEntry {
    DirEntry { name: name.into(), kind, inode: inode(&format!("proc/{path}")) }
}

/// /proc itself: the machine's files, and a directory for each process.
fn root() -> Vec<DirEntry> {
    let mut entries = Vec::from([entry("self", Kind::Symlink, "self")]);
    for name in ["diskstats", "loadavg", "meminfo", "mounts", "stat", "uptime", "version"] {
        entries.push(entry(name, Kind::File, name));
    }
    for process in process::all() {
        let id = process.id.to_string();
        entries.push(entry(&id, Kind::Directory, &id));
    }
    entries
}

/// Memory, in kB: there's no swap, and nothing is kept to give back, so all
/// that's free is available.
fn meminfo() -> String {
    let (total, free) = memory::FRAMES.lock().as_ref().map_or((0, 0), |frames| frames.memory());
    let lines = [
        ("MemTotal", total),
        ("MemFree", free),
        ("MemAvailable", free),
        ("Buffers", 0),
        ("Cached", 0),
        ("SwapCached", 0),
        ("Shmem", 0),
        ("SReclaimable", 0),
        ("SwapTotal", 0),
        ("SwapFree", 0),
    ];
    lines
        .map(|(name, bytes)| format!("{:<16}{:>8} kB\n", format!("{name}:"), bytes / 1024))
        .concat()
}

/// Seconds since boot, and of them, seconds idle, to the hundredth.
fn uptime() -> String {
    let up = time::since_boot() / (NANOS / 100);
    let idle = CPU_TIME.idle.load(Relaxed) * TICK / (NANOS / 100);
    format!("{}.{:02} {}.{:02}\n", up / 100, up % 100, idle / 100, idle % 100)
}

/// No load averages are kept; how many processes run, of how many, and the
/// last one's id.
fn loadavg() -> String {
    let all = process::all();
    let last = all.iter().map(|process| process.id).max().unwrap_or(0);
    format!("0.00 0.00 0.00 1/{} {last}\n", all.len())
}

/// The CPU's time, the one there is and all of them, in ticks; when it was
/// booted; and how many processes there have been.
fn stat() -> String {
    let [user, system, idle] =
        [&CPU_TIME.user, &CPU_TIME.system, &CPU_TIME.idle].map(|count| count.load(Relaxed));
    let cpu = format!("{user} 0 {system} {idle} 0 0 0 0 0 0");
    let boot = (time::now() - time::since_boot()) / NANOS;
    let processes = process::all().iter().map(|process| process.id).max().unwrap_or(0);
    format!(
        "cpu  {cpu}\ncpu0 {cpu}\nintr 0\nctxt 0\nbtime {boot}\nprocesses {processes}\n\
         procs_running 1\nprocs_blocked 0\n"
    )
}

/// What's mounted where: the disk, if there is one, at the root, as Linux
/// names the one it mounts there itself; the devices; and this.
fn mounts(disk: Option<&Ext4>) -> String {
    let root = match disk {
        Some(_) => "/dev/root / ext4 rw,relatime 0 0\n",
        None => "rootfs / rootfs ro 0 0\n",
    };
    format!("{root}devtmpfs /dev devtmpfs rw 0 0\nproc /proc proc rw 0 0\n")
}

/// Each disk's reads and writes since boot, and their sectors, by the
/// names Linux gives disks, sda and on, in the order they were found. No
/// times: none are kept.
fn diskstats() -> String {
    let line = |(index, disk): (usize, &disk::DiskInfo)| {
        let name = char::from(b'a' + index as u8);
        let (reads, read, writes, written) =
            (disk.reads, disk.sectors_read, disk.writes, disk.sectors_written);
        format!(
            "{:>4} {:>7} sd{name} {reads} 0 {read} 0 {writes} 0 {written} 0 0 0 0 0 0 0 0 0 0\n",
            8,
            index * 16
        )
    };
    disk::list().iter().enumerate().map(line).collect()
}

/// Its state, as `ps` shows it: running, sleeping, stopped or ended, and
/// the word for it.
fn state(process: &Process) -> (char, &'static str) {
    let current = thread::current_process().is_some_and(|me| me.id == process.id);
    match () {
        _ if process.ended().is_some() => ('Z', "zombie"),
        _ if process.signals.stopped() => ('T', "stopped"),
        _ if current => ('R', "running"),
        _ => ('S', "sleeping"),
    }
}

/// /proc/PID/stat's 52 fields, as proc(5) lists them. It reads as never
/// faulting, as its pages are all there from the start.
fn process_stat(process: &Process) -> String {
    let tty = process.tty.lock().clone();
    // tty1's device number, 4 and 1, for any terminal.
    let (tty_number, foreground) = match tty {
        Some(tty) => (4 << 8 | 1, tty.foreground() as i64),
        None => (0, -1),
    };
    let times = &process.times;
    let nice = i64::from(process.nice.load(Relaxed));
    let pages = process.memory.lock().space.pages();
    let fields: [i64; 49] = [
        process.parent_id() as i64,
        process.pgid() as i64,
        process.sid() as i64,
        tty_number,
        foreground,
        // flags, and faults: its own and its children's, minor and major.
        0,
        0,
        0,
        0,
        0,
        times.user.load(Relaxed) as i64,
        times.system.load(Relaxed) as i64,
        times.children_user.load(Relaxed) as i64,
        times.children_system.load(Relaxed) as i64,
        20 + nice,
        nice,
        // Its threads; no interval timer; when it started.
        threads(process) as i64,
        0,
        (process.started / TICK) as i64,
        // Its memory, in bytes and in pages, all of it there; no limit.
        (pages * 4096) as i64,
        pages as i64,
        -1,
        // Where its code, stack and registers are.
        0,
        0,
        0,
        0,
        0,
        // Its signals, pending, blocked, ignored and caught.
        pending(process) as i64,
        blocked(process) as i64,
        0,
        0,
        // What it waits in, swap twice, and what its parent is sent as it
        // ends: SIGCHLD.
        0,
        0,
        0,
        17,
        // The CPU, real-time priority and policy, and the rest.
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
        0,
    ];
    let (state, _) = state(process);
    let fields: Vec<String> = fields
        .iter()
        // The limit, as an unsigned number.
        .map(|&field| if field == -1 { u64::MAX.to_string() } else { field.to_string() })
        .collect();
    format!("{} ({}) {state} {}\n", process.id, process.command.lock().name, fields.join(" "))
}

/// /proc/PID/status: as it starts on Linux.
fn status(process: &Process) -> String {
    let (state, word) = state(process);
    format!(
        "Name:\t{}\nUmask:\t{:04o}\nState:\t{state} ({word})\nTgid:\t{id}\nPid:\t{id}\n\
         PPid:\t{}\nUid:\t0\t0\t0\t0\nGid:\t0\t0\t0\t0\nThreads:\t{}\nSigPnd:\t{:016x}\n\
         SigBlk:\t{:016x}\n",
        process.command.lock().name,
        process.umask.load(Relaxed),
        process.parent_id(),
        threads(process),
        pending(process),
        blocked(process),
        id = process.id,
    )
}

/// How many threads it has: one still once it has ended, as Linux keeps the
/// one it started with until it's waited for.
fn threads(process: &Process) -> usize {
    process.threads().len().max(1)
}

/// The signals sent to it, and to its first thread, and not yet taken.
fn pending(process: &Process) -> u64 {
    let first = process.threads().first().map_or(0, |task| task.signals.pending());
    process.signals.pending() | first
}

/// The signals its first thread blocks.
fn blocked(process: &Process) -> u64 {
    process.threads().first().map_or(0, |task| task.signals.blocked())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn the_machine_s_files_read_as_linux_s() {
        let text = |path| match node(path, None) {
            Some(Node::File(text)) => String::from_utf8(text).unwrap(),
            _ => panic!("{path} isn't a file"),
        };
        assert!(text("meminfo").starts_with("MemTotal:  "), "{}", text("meminfo"));
        assert!(text("meminfo").contains("\nMemAvailable:   "));
        assert_eq!(text("mounts").lines().next(), Some("rootfs / rootfs ro 0 0"));
        assert!(text("stat").starts_with("cpu  "));
        // The disk booted from, at least: read from since, looking for ext4.
        let diskstats = text("diskstats");
        assert!(diskstats.starts_with("   8       0 sda "), "{diskstats}");
        let reads: u64 = diskstats.split_whitespace().nth(3).unwrap().parse().unwrap();
        assert!(reads > 0, "{diskstats}");
        assert_eq!(text("loadavg").split(' ').count(), 5);
        // Nothing runs this as a program: there's no self.
        assert!(node("self", None).is_none());
        assert!(node("missing", None).is_none());
        let Some(Node::Dir(entries)) = node("", None) else { panic!("/proc is a directory") };
        assert!(entries.iter().any(|entry| entry.name == "meminfo"));
    }
}
