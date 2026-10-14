//! System calls on signals, and on the process groups and sessions they're
//! sent to: what each signal does, which are blocked, sending them, and
//! waiting for one.

use super::files::{Result, ret};
use super::{EFAULT, EINVAL, EPERM};
use crate::process::{self, Process};
use crate::signal::{self, Action, AltStack, COUNT, ERESTARTNOHAND, Info, SI_TKILL};
use alloc::sync::Arc;
use alloc::vec::Vec;

const ESRCH: i64 = 3;
const ENOMEM: i64 = 12;

/// The size of the signal sets programs pass: 64 bits, as Linux's.
const SET_SIZE: u64 = 8;

/// A signal number a program passed: 1 to 64.
fn signal_number(signal: u64) -> Result<usize> {
    match signal as usize {
        signal @ 1..=COUNT => Ok(signal),
        _ => Err(EINVAL),
    }
}

fn read_set(process: &Process, addr: u64) -> Result<u64> {
    let mut set = [0u8; 8];
    process.memory.lock().space.read(addr, &mut set).ok_or(EFAULT)?;
    Ok(u64::from_le_bytes(set))
}

fn write_set(process: &Process, addr: u64, set: u64) -> Result<()> {
    process.memory.lock().space.write(addr, &set.to_le_bytes()).ok_or(EFAULT)
}

/// What `signal` does, and what it did before.
pub fn rt_sigaction(process: &Process, signal: u64, new: u64, old: u64, size: u64) -> i64 {
    ret((|| {
        let signal = signal_number(signal)?;
        if size != SET_SIZE {
            return Err(EINVAL);
        }
        let unchangeable = signal == signal::SIGKILL || signal == signal::SIGSTOP;
        if new != 0 && unchangeable {
            return Err(EINVAL);
        }
        let mut action = [0u8; Action::SIZE];
        if new != 0 {
            process.memory.lock().space.read(new, &mut action).ok_or(EFAULT)?;
        }
        if old != 0 {
            let bytes = process.signals.action(signal).to_bytes();
            process.memory.lock().space.write(old, &bytes).ok_or(EFAULT)?;
        }
        if new != 0 {
            process.signals.set_action(signal, Action::from_bytes(&action));
        }
        Ok(0)
    })())
}

/// Block more signals, fewer, or just these; and say which were.
pub fn rt_sigprocmask(process: &Process, how: u64, new: u64, old: u64, size: u64) -> i64 {
    const SIG_BLOCK: u64 = 0;
    const SIG_UNBLOCK: u64 = 1;
    const SIG_SETMASK: u64 = 2;
    ret((|| {
        if size != SET_SIZE {
            return Err(EINVAL);
        }
        let blocked = process.signals.blocked();
        let mask = match new {
            0 => None,
            new => Some(read_set(process, new)?),
        };
        let blocked_now = match (how, mask) {
            (_, None) => blocked,
            (SIG_BLOCK, Some(mask)) => blocked | mask,
            (SIG_UNBLOCK, Some(mask)) => blocked & !mask,
            (SIG_SETMASK, Some(mask)) => mask,
            _ => return Err(EINVAL),
        };
        if old != 0 {
            write_set(process, old, blocked)?;
        }
        process.signals.set_blocked(blocked_now);
        Ok(0)
    })())
}

/// The signals sent and not yet taken: those blocked, as any other would
/// have been taken as the call that unblocked it returned.
pub fn rt_sigpending(process: &Process, set: u64, size: u64) -> i64 {
    if size != SET_SIZE {
        return -EINVAL;
    }
    ret(write_set(process, set, process.signals.pending()).map(|()| 0))
}

/// Block just `mask` and wait for a signal; once its handler has run, what
/// was blocked before is again. Returns EINTR, then.
pub fn rt_sigsuspend(process: &Process, mask: u64, size: u64) -> i64 {
    if size != SET_SIZE {
        return -EINVAL;
    }
    let mask = match read_set(process, mask) {
        Ok(mask) => mask,
        Err(errno) => return -errno,
    };
    process.signals.suspend_with(mask);
    pause(process)
}

/// Wait for a signal: one whose handler runs ends it, with EINTR.
pub fn pause(process: &Process) -> i64 {
    let _ = process.signals.arrived.wait_until_interruptible(|| false);
    -ERESTARTNOHAND
}

/// `stack_t`'s flags.
const SS_ONSTACK: u64 = 1;
const SS_DISABLE: u64 = 2;
const SS_AUTODISARM: u64 = 1 << 31;

/// Where handlers that ask to run on another stack run, and where they did.
pub fn sigaltstack(process: &Process, new: u64, old: u64, rsp: u64) -> i64 {
    const MINSIGSTKSZ: u64 = 2048;
    ret((|| {
        let current = process.signals.altstack();
        let on_it = current.holds(rsp);
        let mut stack = [0u8; 24];
        if new != 0 {
            process.memory.lock().space.read(new, &mut stack).ok_or(EFAULT)?;
        }
        if old != 0 {
            let flags = match (on_it, current.enabled) {
                (true, _) => SS_ONSTACK,
                (false, false) => SS_DISABLE,
                (false, true) => 0,
            };
            let mut stack = [0u8; 24];
            stack[..8].copy_from_slice(&current.sp.to_le_bytes());
            stack[8..16].copy_from_slice(&flags.to_le_bytes());
            stack[16..].copy_from_slice(&current.size.to_le_bytes());
            process.memory.lock().space.write(old, &stack).ok_or(EFAULT)?;
        }
        if new != 0 {
            if on_it {
                return Err(EPERM);
            }
            let word = |at: usize| u64::from_le_bytes(stack[at..at + 8].try_into().unwrap());
            let (sp, flags, size) = (word(0), word(8) & 0xffff_ffff, word(16));
            let altstack = match flags & !SS_AUTODISARM {
                SS_DISABLE => AltStack { sp: 0, size: 0, enabled: false },
                0 if size < MINSIGSTKSZ => return Err(ENOMEM),
                0 => AltStack { sp, size, enabled: true },
                _ => return Err(EINVAL),
            };
            process.signals.set_altstack(altstack);
        }
        Ok(0)
    })())
}

/// Send `signal` to process `pid`; to the caller's group, for 0; to every
/// process but the first and the caller, for -1; or to group `-pid`. Signal
/// 0 just asks whether there's anyone to send it to.
pub fn kill(process: &Process, pid: u64, signal: u64) -> i64 {
    let pid = pid as i32 as i64;
    let targets: Vec<Arc<Process>> = match pid {
        1.. => process::find(pid as u64).into_iter().collect(),
        0 => process::group(process.pgid()),
        -1 => process::all().into_iter().filter(|p| p.id != 1 && p.id != process.id).collect(),
        _ => process::group(pid.unsigned_abs()),
    };
    ret(send_to(&targets, signal, Info::user(process.id)))
}

/// `tkill` and `tgkill`: to one thread, which with a thread to each program
/// is to the program, `tid`; `tgkill`'s `tgid` must be its.
pub fn tkill(process: &Process, tgid: Option<u64>, tid: u64, signal: u64) -> i64 {
    let target = process::find(tid).filter(|target| tgid.is_none_or(|tgid| tgid == target.id));
    let info = Info { code: SI_TKILL, pid: process.id, status: 0 };
    ret(send_to(&target.into_iter().collect::<Vec<_>>(), signal, info))
}

fn send_to(targets: &[Arc<Process>], signal: u64, info: Info) -> Result<i64> {
    let signal = match signal {
        0 => None,
        signal => Some(signal_number(signal)?),
    };
    if targets.is_empty() {
        return Err(ESRCH);
    }
    if let Some(signal) = signal {
        targets.iter().for_each(|target| signal::send(target, signal, info));
    }
    Ok(0)
}

/// The process numbered `pid`; `None` for 0, which means the caller.
fn other(pid: u64) -> Result<Option<Arc<Process>>> {
    match pid {
        0 => Ok(None),
        pid => process::find(pid).map(Some).ok_or(ESRCH),
    }
}

pub fn getpgid(process: &Process, pid: u64) -> i64 {
    ret(other(pid).map(|found| found.as_deref().unwrap_or(process).pgid() as i64))
}

pub fn getsid(process: &Process, pid: u64) -> i64 {
    ret(other(pid).map(|found| found.as_deref().unwrap_or(process).sid() as i64))
}

/// Put the caller, or a child of its, in group `pgid`: a new one, numbered
/// as the process, or one already in its session.
pub fn setpgid(process: &Process, pid: u64, pgid: u64) -> i64 {
    ret((|| {
        let found = other(pid)?;
        let target = found.as_deref().unwrap_or(process);
        if target.id != process.id && target.parent_id() != process.id {
            return Err(ESRCH);
        }
        let pgid = match pgid as i32 {
            0 => target.id,
            1.. => pgid,
            _ => return Err(EINVAL),
        };
        // A session's leader leads it; and a group must be in the session.
        let in_session =
            |pgid: u64| process::group(pgid).iter().any(|member| member.sid() == process.sid());
        if target.sid() != process.sid()
            || target.sid() == target.id
            || (pgid != target.id && !in_session(pgid))
        {
            return Err(EPERM);
        }
        target.set_pgid(pgid);
        Ok(0)
    })())
}

/// Lead a new session, in a group of its own, without a terminal: unless
/// some group is already numbered as the caller.
pub fn setsid(process: &Process) -> i64 {
    if !process::group(process.id).is_empty() {
        return -EPERM;
    }
    process.lead_session();
    process.id as i64
}
