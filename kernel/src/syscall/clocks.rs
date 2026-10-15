//! System calls on time: what time it is by each clock, how much of the CPU
//! a program and its children have had, and alarms.

use super::files::{Result, ret};
use super::{EFAULT, EINVAL};
use crate::process::{Process, Times};
use crate::thread;
use crate::time::{self, Alarm, NANOS, TICK};
use core::sync::atomic::Ordering::Relaxed;

const CLOCK_REALTIME: u64 = 0;
const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
const CLOCK_BOOTTIME: u64 = 7;

/// What `clock` says, in nanoseconds: the time of day, the time since boot
/// (which simplos never stops counting, so `CLOCK_BOOTTIME` is the same), or
/// the CPU time the program has had (one thread to each, so the thread's).
pub(super) fn now(process: &Process, clock: u64) -> Result<u64> {
    match clock {
        CLOCK_REALTIME | CLOCK_REALTIME_COARSE => Ok(time::now()),
        CLOCK_MONOTONIC | CLOCK_MONOTONIC_RAW | CLOCK_MONOTONIC_COARSE | CLOCK_BOOTTIME => {
            Ok(time::since_boot())
        }
        CLOCK_PROCESS_CPUTIME_ID | CLOCK_THREAD_CPUTIME_ID => {
            let times = &process.times;
            Ok((times.user.load(Relaxed) + times.system.load(Relaxed)) * TICK)
        }
        _ => Err(EINVAL),
    }
}

/// `nanos` as a `struct timespec`, or with `micros` a `struct timeval`.
fn split(nanos: u64, micros: bool) -> [u8; 16] {
    let fraction = match micros {
        true => nanos % NANOS / 1000,
        false => nanos % NANOS,
    };
    let mut time = [0u8; 16];
    time[..8].copy_from_slice(&(nanos / NANOS).to_le_bytes());
    time[8..].copy_from_slice(&fraction.to_le_bytes());
    time
}

fn put(process: &Process, addr: u64, bytes: &[u8]) -> Result<()> {
    process.memory.lock().space.write(addr, bytes).ok_or(EFAULT)
}

pub fn clock_gettime(process: &Process, clock: u64, timespec: u64) -> i64 {
    ret(now(process, clock)
        .and_then(|nanos| put(process, timespec, &split(nanos, false)))
        .map(|()| 0))
}

/// How finely `clock` counts: to the nanosecond, near enough, by the HPET; to
/// the tick for CPU time, the coarse clocks, and all of them without one.
pub fn clock_getres(process: &Process, clock: u64, timespec: u64) -> i64 {
    ret((|| {
        now(process, clock)?;
        let fine = !matches!(
            clock,
            CLOCK_PROCESS_CPUTIME_ID
                | CLOCK_THREAD_CPUTIME_ID
                | CLOCK_REALTIME_COARSE
                | CLOCK_MONOTONIC_COARSE
        );
        let resolution = if fine && crate::io::hpet::elapsed_nanos().is_some() { 1 } else { TICK };
        if timespec != 0 {
            put(process, timespec, &split(resolution, false))?;
        }
        Ok(0)
    })())
}

/// The time of day as a `struct timeval`, and a `struct timezone` of UTC.
pub fn gettimeofday(process: &Process, timeval: u64, timezone: u64) -> i64 {
    ret((|| {
        if timeval != 0 {
            put(process, timeval, &split(time::now(), true))?;
        }
        if timezone != 0 {
            put(process, timezone, &[0; 8])?;
        }
        Ok(0)
    })())
}

/// The time of day in seconds, also at `at` if that isn't 0.
pub fn time(process: &Process, at: u64) -> i64 {
    let seconds = time::now() / NANOS;
    if at != 0 && put(process, at, &seconds.to_le_bytes()).is_err() {
        return -EFAULT;
    }
    seconds as i64
}

/// `ticks` of CPU time as a `struct timeval`.
fn timeval(ticks: u64) -> [u8; 16] {
    split(ticks * TICK, true)
}

/// What `wait4` and `getrusage` tell of CPU time: a `struct rusage`, with its
/// user and system time; its other counts, of page faults and the like, are 0.
pub(super) fn rusage(user: u64, system: u64) -> [u8; 144] {
    let mut usage = [0u8; 144];
    usage[..16].copy_from_slice(&timeval(user));
    usage[16..32].copy_from_slice(&timeval(system));
    usage
}

/// A process's own CPU time, and its children's, as `wait4` adds them up.
pub(super) fn total(times: &Times) -> (u64, u64) {
    (
        times.user.load(Relaxed) + times.children_user.load(Relaxed),
        times.system.load(Relaxed) + times.children_system.load(Relaxed),
    )
}

/// The CPU time of the caller (`RUSAGE_SELF`, or the same `RUSAGE_THREAD`) or
/// of the children it has waited for (`RUSAGE_CHILDREN`).
pub fn getrusage(process: &Process, who: u64, usage: u64) -> i64 {
    const RUSAGE_SELF: i64 = 0;
    const RUSAGE_CHILDREN: i64 = -1;
    const RUSAGE_THREAD: i64 = 1;
    let times = &process.times;
    let (user, system) = match who as i64 {
        RUSAGE_SELF | RUSAGE_THREAD => (times.user.load(Relaxed), times.system.load(Relaxed)),
        RUSAGE_CHILDREN => (times.children_user.load(Relaxed), times.children_system.load(Relaxed)),
        _ => return -EINVAL,
    };
    ret(put(process, usage, &rusage(user, system)).map(|()| 0))
}

/// The caller's CPU time and its children's, in clock ticks of 1/100 s, the
/// timer's ticks; and the ticks since boot.
pub fn times(process: &Process, tms: u64) -> i64 {
    const _: () = assert!(TICK == NANOS / 100, "times() counts in hundredths");
    let times = &process.times;
    let counts = [&times.user, &times.system, &times.children_user, &times.children_system];
    if tms != 0 {
        let mut buf = [0u8; 32];
        for (field, count) in buf.chunks_mut(8).zip(counts) {
            field.copy_from_slice(&count.load(Relaxed).to_le_bytes());
        }
        if put(process, tms, &buf).is_err() {
            return -EFAULT;
        }
    }
    thread::ticks() as i64
}

const ITIMER_REAL: u64 = 0;

/// The `struct timeval` in `bytes`, in nanoseconds.
fn timeval_nanos(bytes: &[u8]) -> Result<u64> {
    let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let micros = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if seconds < 0 || !(0..1_000_000).contains(&micros) {
        return Err(EINVAL);
    }
    Ok((seconds as u64).saturating_mul(NANOS).saturating_add(micros as u64 * 1000))
}

/// The caller's alarm as a `struct itimerval`: how often it repeats, and how
/// long until it's next due.
fn itimerval(alarm: Alarm) -> [u8; 32] {
    let left = match alarm.due {
        0 => 0,
        // Due, but not yet sent: a microsecond, then, as Linux would say.
        due => due.saturating_sub(time::since_boot()).max(1000),
    };
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&split(alarm.every, true));
    value[16..].copy_from_slice(&split(left, true));
    value
}

/// Set the caller's alarm from the `struct itimerval` at `new`, saying what
/// it was at `old`: SIGALRM when its time comes, and every so often after if
/// asked. A copy starts without one; `execve` keeps it.
///
/// shortcut: the real-time alarm only; ITIMER_VIRTUAL and ITIMER_PROF, which
/// count CPU time, are EINVAL.
pub fn setitimer(process: &Process, which: u64, new: u64, old: u64) -> i64 {
    ret((|| {
        if which != ITIMER_REAL {
            return Err(EINVAL);
        }
        let mut value = [0u8; 32];
        if new != 0 {
            process.memory.lock().space.read(new, &mut value).ok_or(EFAULT)?;
        }
        let (every, after) = (timeval_nanos(&value[..16])?, timeval_nanos(&value[16..])?);
        let mut alarm = process.alarm.lock();
        if old != 0 {
            put(process, old, &itimerval(*alarm))?;
        }
        if new != 0 {
            *alarm = match after {
                0 => Alarm::default(),
                after => Alarm { due: time::since_boot() + after, every },
            };
        }
        Ok(0)
    })())
}

pub fn getitimer(process: &Process, which: u64, value: u64) -> i64 {
    if which != ITIMER_REAL {
        return -EINVAL;
    }
    let alarm = *process.alarm.lock();
    ret(put(process, value, &itimerval(alarm)).map(|()| 0))
}

/// SIGALRM in `seconds`, or none for 0: the seconds that were left of the
/// alarm before, to the nearest, but at least 1 if there were any, as Linux.
pub fn alarm(process: &Process, seconds: u64) -> i64 {
    let mut alarm = process.alarm.lock();
    let left = match alarm.due {
        0 => 0,
        due => ((due.saturating_sub(time::since_boot()) + NANOS / 2) / NANOS).max(1),
    };
    *alarm = match seconds {
        0 => Alarm::default(),
        seconds => Alarm { due: time::since_boot() + seconds.saturating_mul(NANOS), every: 0 },
    };
    left as i64
}
