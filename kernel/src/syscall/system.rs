//! System calls on the machine and on who a program runs as: what the system
//! is called, how long it's been up and how much memory it has, which user
//! and groups, which CPUs, how nice, and its resource limits.

use super::files::{Result, ret, string_at};
use super::signals::other;
use super::{EFAULT, EINVAL, EPERM};
use crate::process::{self, Process, Task};
use crate::{memory, serial_println, time};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering::Relaxed;

const ESRCH: i64 = 3;

/// The Linux release simplos says it is, by its system calls.
pub const RELEASE: &str = "6.1.0";

/// `uname`'s `struct utsname`: six strings of 65 bytes. Linux's system call
/// interface, so programs that ask which system they're on are told Linux,
/// with the version giving simplos away.
pub fn uname(process: &Process, buf: u64) -> i64 {
    let fields = [
        "Linux",
        "simplos",
        RELEASE,
        concat!("simplos ", env!("CARGO_PKG_VERSION")),
        "x86_64",
        "(none)",
    ];
    let mut utsname = [0u8; 6 * 65];
    for (field, value) in utsname.chunks_mut(65).zip(fields) {
        field[..value.len()].copy_from_slice(value.as_bytes());
    }
    ret(process.memory.lock().space.write(buf, &utsname).ok_or(EFAULT).map(|()| 0))
}

/// shortcut: there are no users but root, so every id is 0 and can only be
/// set to 0; give processes ids of their own when there are others to be.
pub fn set_id(id: u64) -> i64 {
    if id == 0 { 0 } else { -EPERM }
}

/// `getgroups`: the groups besides its own a program is in, none, as root
/// with no others; and `setgroups`, which says it's done.
///
/// shortcut: what `setgroups` sets is forgotten, as nothing checks groups;
/// keep it once something does.
pub fn groups(count: u64) -> i64 {
    if (count as i32) < 0 { -EINVAL } else { 0 }
}

/// `getrandom`: `len` random bytes at `buf`, from `random::fill`, which
/// never has to wait, whatever the flags ask: how many, at most a mebibyte
/// at a time, as with `read`, where Linux gives up to 32.
pub fn getrandom(process: &Process, buf: u64, len: u64, flags: u64) -> i64 {
    const GRND_NONBLOCK: u64 = 1;
    const GRND_RANDOM: u64 = 2;
    const GRND_INSECURE: u64 = 4;
    let both = GRND_RANDOM | GRND_INSECURE;
    if flags & !(GRND_NONBLOCK | both) != 0 || flags & both == both {
        return -EINVAL;
    }
    let mut bytes = vec![0; len.min(1 << 20) as usize];
    crate::random::fill(&mut bytes);
    match process.memory.lock().space.write(buf, &bytes) {
        Some(()) => bytes.len() as i64,
        None => -EFAULT,
    }
}

/// `sysinfo`'s `struct sysinfo`: seconds since boot, the memory there is and
/// is free, and how many processes, in bytes. Load averages and swap are 0.
pub fn sysinfo(process: &Process, info: u64) -> i64 {
    let (total, free) = memory::FRAMES.lock().as_ref().map_or((0, 0), |frames| frames.memory());
    let mut sysinfo = [0u8; 112];
    sysinfo[..8].copy_from_slice(&(time::since_boot() / time::NANOS).to_le_bytes());
    sysinfo[32..40].copy_from_slice(&total.to_le_bytes());
    sysinfo[40..48].copy_from_slice(&free.to_le_bytes());
    sysinfo[80..82].copy_from_slice(&(process::all().len() as u16).to_le_bytes());
    // mem_unit: the sizes are in bytes.
    sysinfo[104..108].copy_from_slice(&1u32.to_le_bytes());
    ret(process.memory.lock().space.write(info, &sysinfo).ok_or(EFAULT).map(|()| 0))
}

/// The processes `which` and `who` name for `getpriority` and
/// `setpriority`: process `who`, those in group `who`, or those of user
/// `who`, all of them; each the caller's own for 0. ESRCH for none.
fn prioritized(process: &Process, which: u64, who: u64) -> Result<Vec<Arc<Process>>> {
    const PRIO_PROCESS: u64 = 0;
    const PRIO_PGRP: u64 = 1;
    const PRIO_USER: u64 = 2;
    let who = who as u32 as u64;
    let named: Vec<_> = match which {
        PRIO_PROCESS => {
            let id = if who == 0 { process.id } else { who };
            process::all().into_iter().filter(|other| other.id == id).collect()
        }
        PRIO_PGRP => process::group(if who == 0 { process.pgid() } else { who }),
        // Root's are all there are.
        PRIO_USER if who == 0 => process::all(),
        PRIO_USER => Vec::new(),
        _ => return Err(EINVAL),
    };
    if named.is_empty() { Err(ESRCH) } else { Ok(named) }
}

/// How nice the least nice of the processes named is, as 20 less it, as
/// Linux's system call says it, so as never to be negative.
pub fn getpriority(process: &Process, which: u64, who: u64) -> i64 {
    ret(prioritized(process, which, who).map(|named| {
        let least = named.iter().map(|other| other.nice.load(Relaxed)).min().unwrap_or(0);
        i64::from(20 - least)
    }))
}

/// Make the processes named `nice`, which is taken to be within -20 to 19:
/// root may make them less nice as well as more.
pub fn setpriority(process: &Process, which: u64, who: u64, nice: u64) -> i64 {
    let nice = (nice as i32).clamp(-20, 19);
    ret(prioritized(process, which, who).map(|named| {
        named.iter().for_each(|other| other.nice.store(nice, Relaxed));
        0
    }))
}

/// The CPUs `pid` (the caller, for 0) may run on: the one there is, as a
/// mask of `len` bytes, of which Linux fills 8.
pub fn sched_getaffinity(process: &Process, pid: u64, len: u64, mask: u64) -> i64 {
    ret((|| {
        if len < 8 || !len.is_multiple_of(8) {
            return Err(EINVAL);
        }
        other(pid)?;
        process.memory.lock().space.write(mask, &1u64.to_le_bytes()).ok_or(EFAULT)?;
        Ok(8)
    })())
}

/// How many resources `RLIMIT_*` numbers.
pub const LIMITS: usize = 16;

/// No limit, `RLIM_INFINITY`.
const UNLIMITED: u64 = u64::MAX;

/// What a program the kernel starts may use, soft and hard limits, as
/// Linux's defaults: no limit on most, no core files, the descriptors and
/// the stack there are.
///
/// shortcut: limits are kept and reported but not enforced, past those the
/// kernel has anyway.
pub const DEFAULT_LIMITS: [(u64, u64); LIMITS] = {
    const RLIMIT_STACK: usize = 3;
    const RLIMIT_CORE: usize = 4;
    const RLIMIT_NOFILE: usize = 7;
    const RLIMIT_NICE: usize = 13;
    const RLIMIT_RTPRIO: usize = 14;
    let mut limits = [(UNLIMITED, UNLIMITED); LIMITS];
    limits[RLIMIT_STACK] = (process::STACK_MOST, process::STACK_MOST);
    limits[RLIMIT_CORE] = (0, UNLIMITED);
    limits[RLIMIT_NOFILE] = (super::files::MOST_FILES as u64, super::files::MOST_FILES as u64);
    limits[RLIMIT_NICE] = (0, 0);
    limits[RLIMIT_RTPRIO] = (0, 0);
    limits
};

/// `prlimit64`: `pid`'s (the caller's, for 0) limit on `resource`, at `old`,
/// and then `new`'s if it's given; and `getrlimit` and `setrlimit` through
/// it. Root may raise a hard limit, though not past the descriptors there are.
pub fn prlimit(process: &Process, pid: u64, resource: u64, new: u64, old: u64) -> i64 {
    const RLIMIT_NOFILE: u64 = 7;
    ret((|| {
        let found = other(pid)?;
        let target = found.as_deref().unwrap_or(process);
        let resource = usize::try_from(resource).ok().filter(|&r| r < LIMITS).ok_or(EINVAL)?;
        let mut pair = [0u8; 16];
        let limit = if new != 0 {
            process.memory.lock().space.read(new, &mut pair).ok_or(EFAULT)?;
            let soft = u64::from_le_bytes(pair[..8].try_into().unwrap());
            let hard = u64::from_le_bytes(pair[8..].try_into().unwrap());
            if soft > hard {
                return Err(EINVAL);
            }
            if resource as u64 == RLIMIT_NOFILE && hard > super::files::MOST_FILES as u64 {
                return Err(EPERM);
            }
            Some((soft, hard))
        } else {
            None
        };
        let mut limits = target.limits.lock();
        if old != 0 {
            let (soft, hard) = limits[resource];
            pair[..8].copy_from_slice(&soft.to_le_bytes());
            pair[8..].copy_from_slice(&hard.to_le_bytes());
            process.memory.lock().space.write(old, &pair).ok_or(EFAULT)?;
        }
        if let Some(limit) = limit {
            limits[resource] = limit;
        }
        Ok(0)
    })())
}

/// `prctl`: of all it does, a thread's name, `task`'s, the current one's, as
/// PR_SET_NAME sets it, its first 15 bytes, and PR_GET_NAME tells it, with
/// its NUL: what /proc/PID/comm says of its program's first thread. The rest
/// is EINVAL, as Linux says of an option it doesn't know.
pub fn prctl(process: &Process, task: &Task, option: u64, arg: u64) -> i64 {
    const PR_SET_NAME: u64 = 15;
    const PR_GET_NAME: u64 = 16;
    ret((|| match option {
        PR_SET_NAME => {
            let given = string_at(process, arg, 4096, EINVAL)?;
            let mut name = [0u8; 16];
            let len = given.len().min(15);
            name[..len].copy_from_slice(&given[..len]);
            *task.name.lock() = Some(name);
            if task.id() == process.id {
                process.command.lock().name =
                    alloc::string::String::from_utf8_lossy(&name[..len]).into();
            }
            Ok(0)
        }
        PR_GET_NAME => {
            let own = *task.name.lock();
            let name = own.unwrap_or_else(|| {
                let mut name = [0u8; 16];
                let program = process.command.lock().name.clone();
                let len = program.len().min(15);
                name[..len].copy_from_slice(&program.as_bytes()[..len]);
                name
            });
            process.memory.lock().space.write(arg, &name).ok_or(EFAULT)?;
            Ok(0)
        }
        _ => {
            serial_println!("simplos: process {}: no prctl option {}", process.id, option);
            Err(EINVAL)
        }
    })())
}
