//! System calls on the machine and on who a program runs as: what the system
//! is called, which user and group, which CPUs, and its resource limits.

use super::files::{Result, ret};
use super::{EFAULT, EINVAL, EPERM};
use crate::process::{self, Process};
use alloc::sync::Arc;

const ESRCH: i64 = 3;

/// `uname`'s `struct utsname`: six strings of 65 bytes. Linux's system call
/// interface, so programs that ask which system they're on are told Linux,
/// with the version giving simplos away.
pub fn uname(process: &Process, buf: u64) -> i64 {
    let fields = [
        "Linux",
        "simplos",
        "6.1.0",
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

/// The process `pid` names: `None` for the caller.
fn other(pid: u64) -> Result<Option<Arc<Process>>> {
    match pid {
        0 => Ok(None),
        pid => process::find(pid).map(Some).ok_or(ESRCH),
    }
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
    limits[RLIMIT_STACK] = (process::STACK_SIZE, process::STACK_SIZE);
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
