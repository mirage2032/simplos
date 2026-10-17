//! System calls on time: what time it is by each clock, how much of the CPU
//! a program and its children have had, alarms, and timers.

use super::files::{Result, ret};
use super::{EFAULT, EINVAL};
use crate::process::{Process, Times};
use crate::signal;
use crate::thread;
use crate::time::{self, Alarm, MOST_TIMERS, NANOS, TICK, Timer};
use core::sync::atomic::Ordering::Relaxed;

pub(super) const CLOCK_REALTIME: u64 = 0;
pub(super) const CLOCK_MONOTONIC: u64 = 1;
const CLOCK_PROCESS_CPUTIME_ID: u64 = 2;
const CLOCK_THREAD_CPUTIME_ID: u64 = 3;
const CLOCK_MONOTONIC_RAW: u64 = 4;
const CLOCK_REALTIME_COARSE: u64 = 5;
const CLOCK_MONOTONIC_COARSE: u64 = 6;
pub(super) const CLOCK_BOOTTIME: u64 = 7;

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

/// `alarm` as a `struct itimerval`, in `micros`, or as a `struct
/// itimerspec`: how often it repeats, and how long until it's next due.
fn itimerval(alarm: Alarm, micros: bool) -> [u8; 32] {
    let left = match alarm.due {
        0 => 0,
        // Due, but not yet sent: the least there can be, as Linux says.
        due => due.saturating_sub(time::since_boot()).max(if micros { 1000 } else { 1 }),
    };
    let mut value = [0u8; 32];
    value[..16].copy_from_slice(&split(alarm.every, micros));
    value[16..].copy_from_slice(&split(left, micros));
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
            put(process, old, &itimerval(*alarm, true))?;
        }
        if new != 0 {
            // One too far off to count to never comes, as on Linux.
            *alarm = match after {
                0 => Alarm::default(),
                after => Alarm { due: time::since_boot().saturating_add(after), every },
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
    ret(put(process, value, &itimerval(alarm, true)).map(|()| 0))
}

/// SIGALRM in `seconds`, or none for 0: the seconds that were left of the
/// alarm before, to the nearest, but at least 1 if there were any, as Linux.
pub fn alarm(process: &Process, seconds: u64) -> i64 {
    let mut alarm = process.alarm.lock();
    let left = match alarm.due {
        0 => 0,
        due => (due.saturating_sub(time::since_boot()).saturating_add(NANOS / 2) / NANOS).max(1),
    };
    *alarm = match seconds {
        0 => Alarm::default(),
        seconds => {
            let due = time::since_boot().saturating_add(seconds.saturating_mul(NANOS));
            Alarm { due, every: 0 }
        }
    };
    left as i64
}

/// The `struct timespec` in `bytes`, in nanoseconds.
pub(super) fn timespec_nanos(bytes: &[u8]) -> Result<u64> {
    let seconds = i64::from_le_bytes(bytes[..8].try_into().unwrap());
    let nanos = i64::from_le_bytes(bytes[8..16].try_into().unwrap());
    if seconds < 0 || !(0..NANOS as i64).contains(&nanos) {
        return Err(EINVAL);
    }
    Ok((seconds as u64).saturating_mul(NANOS).saturating_add(nanos as u64))
}

/// `timer_create`: a timer for the program, by `clock`, which when it comes
/// sends what the `struct sigevent` at `event` says: a signal to the program
/// (SIGEV_SIGNAL), or to one of its threads (SIGEV_THREAD_ID, which musl's
/// SIGEV_THREAD comes to), or nothing (SIGEV_NONE); for none given, SIGALRM
/// to the program, with the timer's id as its value. Its id, written at
/// `id`, the lowest free. Not set to come until `timer_settime` sets it.
///
/// shortcut: by the clocks that count time, not those that count CPU time.
pub fn timer_create(process: &Process, clock: u64, event: u64, id: u64) -> i64 {
    const SIGEV_SIGNAL: i32 = 0;
    const SIGEV_NONE: i32 = 1;
    const SIGEV_THREAD_ID: i32 = 4;
    const EAGAIN: i64 = 11;
    ret((|| {
        if ![CLOCK_REALTIME, CLOCK_MONOTONIC, CLOCK_BOOTTIME].contains(&clock) {
            return Err(EINVAL);
        }
        // The signal, the thread it's for, and the value it carries.
        let sent = match event {
            0 => None,
            event => {
                let mut sigevent = [0u8; 64];
                process.memory.lock().space.read(event, &mut sigevent).ok_or(EFAULT)?;
                let int = |at: usize| i32::from_le_bytes(sigevent[at..at + 4].try_into().unwrap());
                // A `union sigval`: an int, or a pointer, all 8 bytes kept.
                let value = i64::from_le_bytes(sigevent[..8].try_into().unwrap());
                let (number, notify, tid) = (int(8), int(12), int(16));
                let number = || match usize::try_from(number) {
                    Ok(number @ 1..=signal::COUNT) => Ok(number),
                    _ => Err(EINVAL),
                };
                Some(match notify {
                    SIGEV_NONE => (0, None, value),
                    SIGEV_SIGNAL => (number()?, None, value),
                    SIGEV_THREAD_ID => {
                        let tid = u64::try_from(tid).map_err(|_| EINVAL)?;
                        if !process.threads().iter().any(|task| task.id() == tid) {
                            return Err(EINVAL);
                        }
                        (number()?, Some(tid), value)
                    }
                    _ => return Err(EINVAL),
                })
            }
        };
        let index = {
            let mut timers = process.timers.lock();
            let index = match timers.iter().position(Option::is_none) {
                Some(index) => index,
                None if timers.len() < MOST_TIMERS => {
                    timers.push(None);
                    timers.len() - 1
                }
                None => return Err(EAGAIN),
            };
            let (signal, thread, value) = sent.unwrap_or((signal::SIGALRM, None, index as i64));
            let alarm = Alarm::default();
            timers[index] = Some(Timer { alarm, clock, signal, thread, value });
            index
        };
        if put(process, id, &(index as i32).to_le_bytes()).is_err() {
            process.timers.lock()[index] = None;
            return Err(EFAULT);
        }
        Ok(0)
    })())
}

/// Run `f` on the program's timer `id`: EINVAL if it has none by that id.
fn with_timer<T>(process: &Process, id: u64, f: impl FnOnce(&mut Timer) -> T) -> Result<T> {
    let index = usize::try_from(id as i32).map_err(|_| EINVAL)?;
    let mut timers = process.timers.lock();
    timers.get_mut(index).and_then(Option::as_mut).map(f).ok_or(EINVAL)
}

/// `timer_settime`: set timer `id` to come as the `struct itimerspec` at
/// `new` says, when its clock says the time in it, with TIMER_ABSTIME, or
/// else after that long; and every so often after that, if asked; or off,
/// for 0. What it was before, at `old`, if that isn't 0.
pub fn timer_settime(process: &Process, id: u64, flags: u64, new: u64, old: u64) -> i64 {
    const TIMER_ABSTIME: u64 = 1;
    ret((|| {
        let mut value = [0u8; 32];
        process.memory.lock().space.read(new, &mut value).ok_or(EFAULT)?;
        let (every, at) = (timespec_nanos(&value[..16])?, timespec_nanos(&value[16..])?);
        let clock = with_timer(process, id, |timer| timer.clock)?;
        // How long until then, by the clock the alarm thread goes by.
        let after = match flags & TIMER_ABSTIME {
            0 => at,
            _ => at.saturating_sub(now(process, clock)?),
        };
        let alarm = match at {
            0 => Alarm::default(),
            // Its time already past, it comes at once: as soon as can be.
            _ => Alarm { due: time::since_boot().saturating_add(after).max(1), every },
        };
        let before = with_timer(process, id, |timer| core::mem::replace(&mut timer.alarm, alarm))?;
        if old != 0 {
            put(process, old, &itimerval(before, false))?;
        }
        Ok(0)
    })())
}

/// `timer_gettime`: how often timer `id` comes, and how long until it next
/// does, as a `struct itimerspec` at `value`; 0 for one that's off.
pub fn timer_gettime(process: &Process, id: u64, value: u64) -> i64 {
    ret(with_timer(process, id, |timer| timer.alarm)
        .and_then(|alarm| put(process, value, &itimerval(alarm, false)))
        .map(|()| 0))
}

/// `timer_getoverrun`: how many more times timer `id` came before its last
/// signal was taken. None, as each time sends its signal again (see the
/// alarm thread in `time.rs`).
pub fn timer_getoverrun(process: &Process, id: u64) -> i64 {
    ret(with_timer(process, id, |_| 0))
}

/// `timer_delete`: timer `id` goes, and its id is free again.
pub fn timer_delete(process: &Process, id: u64) -> i64 {
    ret((|| {
        let index = usize::try_from(id as i32).map_err(|_| EINVAL)?;
        let mut timers = process.timers.lock();
        timers.get_mut(index).and_then(Option::take).ok_or(EINVAL)?;
        Ok(0)
    })())
}
