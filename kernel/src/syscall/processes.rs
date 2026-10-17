//! System calls on programs and their threads: copying one (`fork` and the
//! like) or starting a thread of it (`clone`), replacing one with another
//! (`execve`), waiting for copies to end (`wait4`), and the futexes its
//! threads wait on each other with.

use super::clocks;
use super::files::{self, Result, path_at, ret, string_at};
use super::{CLOCK_MONOTONIC, CLOCK_REALTIME, EFAULT, EINVAL, ENOMEM, ENOSYS, EPERM};
use super::{deadline, timespec_ms, timespec_ns};
use crate::futex::{self, Error};
use crate::memory::USER_END;
use crate::process::{self, Process, Registers, ThreadIds};
use crate::signal::{ERESTARTNOHAND, ERESTARTSYS};
use alloc::sync::Arc;
use alloc::vec::Vec;

const E2BIG: i64 = 7;
const ECHILD: i64 = 10;
const EAGAIN: i64 = 11;
const ETIMEDOUT: i64 = 110;

/// What a parent is sent as its child ends, as `fork` asks.
pub const SIGCHLD: u64 = 17;

/// `fork`, `vfork`, and `clone` as C libraries use it for them: a copy of the
/// program, or with CLONE_THREAD a new thread of it, which starts with
/// `child`, on `stack` if it isn't 0, and `fs` at `tls` with CLONE_SETTLS.
/// Its id is written at `tids`' first, in the parent's memory, with
/// CLONE_PARENT_SETTID; at the second, in the child's, with
/// CLONE_CHILD_SETTID; and 0 is, there, as it ends, with
/// CLONE_CHILD_CLEARTID, and a futex woken.
///
/// shortcut: CLONE_VM without CLONE_THREAD is taken as a copy, and
/// CLONE_VFORK doesn't hold the parent until the child execs or exits;
/// musl's `posix_spawn`, the one user, needs neither. And a thread shares
/// its program's descriptors and working directory, as every libc's do:
/// CLONE_FILES and CLONE_FS go with CLONE_THREAD, and only with it.
pub fn clone(
    process: &Arc<Process>,
    mut child: Registers,
    flags: u64,
    stack: u64,
    tids: [u64; 2],
    tls: u64,
) -> i64 {
    const CLONE_VM: u64 = 0x100;
    const CLONE_FS: u64 = 0x200;
    const CLONE_FILES: u64 = 0x400;
    const CLONE_SIGHAND: u64 = 0x800;
    const CLONE_VFORK: u64 = 0x4000;
    const CLONE_THREAD: u64 = 0x1_0000;
    const CLONE_SYSVSEM: u64 = 0x4_0000;
    const CLONE_SETTLS: u64 = 0x8_0000;
    const CLONE_PARENT_SETTID: u64 = 0x10_0000;
    const CLONE_CHILD_CLEARTID: u64 = 0x20_0000;
    const CLONE_DETACHED: u64 = 0x40_0000;
    const CLONE_CHILD_SETTID: u64 = 0x100_0000;
    // The low byte: what the parent is sent as the child ends.
    const SIGNAL: u64 = 0xFF;
    const SHARED: u64 = CLONE_FS | CLONE_FILES | CLONE_SIGHAND;
    let known = CLONE_VM
        | SHARED
        | CLONE_VFORK
        | CLONE_THREAD
        | CLONE_SYSVSEM
        | CLONE_SETTLS
        | CLONE_PARENT_SETTID
        | CLONE_CHILD_CLEARTID
        | CLONE_DETACHED
        | CLONE_CHILD_SETTID
        | SIGNAL;
    let has = |flag: u64| flags & flag != 0;
    let thread = has(CLONE_THREAD);
    // As Linux: a thread shares its handlers, and handlers are for memory.
    let linux = !(thread && !has(CLONE_SIGHAND) || has(CLONE_SIGHAND) && !has(CLONE_VM));
    let shared = if thread { flags & SHARED == SHARED } else { flags & SHARED == 0 };
    if flags & !known != 0 || !linux || !shared {
        return -EINVAL;
    }
    if stack != 0 {
        child.rsp = stack;
    }
    let ids = ThreadIds {
        parent: if has(CLONE_PARENT_SETTID) { tids[0] } else { 0 },
        child: if has(CLONE_CHILD_SETTID) { tids[1] } else { 0 },
        clear: if has(CLONE_CHILD_CLEARTID) { tids[1] } else { 0 },
    };
    // As `arch_prctl` checks it: `fs` at an address that isn't one would
    // panic the kernel as the thread is switched to.
    if has(CLONE_SETTLS) && tls >= USER_END {
        return -EPERM;
    }
    let tls = has(CLONE_SETTLS).then_some(tls);
    if thread {
        return ret(process::spawn_thread(process, child, tls, ids).map(|id| id as i64));
    }
    process::fork(process, child, tls, ids).map_or(-ENOMEM, |id| id as i64)
}

/// `futex`: on the 32-bit word at `addr`, `op` says what, of what
/// `futex.rs` does: wait while it's `value`, with `timeout` the `struct
/// timespec` there, if it isn't 0, for how long, or with FUTEX_WAIT_BITSET
/// until when, by CLOCK_MONOTONIC or with FUTEX_CLOCK_REALTIME by that;
/// wake up to `value` waiters; or wake them and move up to `timeout`'s
/// value more to `addr2`, if the word is `value3` for FUTEX_CMP_REQUEUE.
/// The bitset ops' is `value3`. FUTEX_PRIVATE_FLAG is taken, as every
/// futex here is private.
///
/// shortcut: a wait for a while that a signal ends starts again from the
/// beginning, where Linux waits for what's left; and no priority-inheriting
/// futexes, nor FUTEX_WAKE_OP, which musl and Rust don't use.
pub fn futex(
    process: &Process,
    addr: u64,
    op: u64,
    value: u64,
    timeout: u64,
    addr2: u64,
    value3: u64,
) -> i64 {
    const FUTEX_WAIT: u64 = 0;
    const FUTEX_WAKE: u64 = 1;
    const FUTEX_REQUEUE: u64 = 3;
    const FUTEX_CMP_REQUEUE: u64 = 4;
    const FUTEX_WAIT_BITSET: u64 = 9;
    const FUTEX_WAKE_BITSET: u64 = 10;
    const FUTEX_PRIVATE_FLAG: u64 = 128;
    const FUTEX_CLOCK_REALTIME: u64 = 256;
    let command = op & !(FUTEX_PRIVATE_FLAG | FUTEX_CLOCK_REALTIME);
    let realtime = op & FUTEX_CLOCK_REALTIME != 0;
    let waits = command == FUTEX_WAIT || command == FUTEX_WAIT_BITSET;
    let known =
        [FUTEX_WAKE, FUTEX_REQUEUE, FUTEX_CMP_REQUEUE, FUTEX_WAKE_BITSET].contains(&command);
    if !waits && !known || realtime && !waits {
        return -ENOSYS;
    }
    if !addr.is_multiple_of(4) {
        return -EINVAL;
    }
    let (value, value3) = (value as u32, value3 as u32);
    let bitset = match command {
        FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET if value3 == 0 => return -EINVAL,
        FUTEX_WAIT_BITSET | FUTEX_WAKE_BITSET => value3,
        _ => futex::ANY,
    };
    let errno = |error: Error| match error {
        Error::Again => EAGAIN,
        Error::Fault => EFAULT,
        Error::TimedOut => ETIMEDOUT,
        Error::Interrupted if timeout != 0 => ERESTARTNOHAND,
        Error::Interrupted => ERESTARTSYS,
    };
    match command {
        FUTEX_WAIT | FUTEX_WAIT_BITSET => {
            let ms = match (timeout, command) {
                (0, _) => None,
                (at, FUTEX_WAIT) => Some(timespec_ms(process, at)),
                (at, _) => {
                    let clock = if realtime { CLOCK_REALTIME } else { CLOCK_MONOTONIC };
                    Some(timespec_ns(process, at).and_then(|until| {
                        let now = clocks::now(process, clock)?;
                        Ok(until.saturating_sub(now).div_ceil(1_000_000))
                    }))
                }
            };
            let ms = match ms.transpose() {
                Ok(ms) => ms,
                Err(errno) => return -errno,
            };
            match futex::wait(process, addr, value, deadline(ms), bitset) {
                Ok(()) => 0,
                Err(error) => -errno(error),
            }
        }
        // As Linux does, it wakes one if asked for none.
        FUTEX_WAKE | FUTEX_WAKE_BITSET => {
            futex::wake(process.id, addr, (value as i32).max(1) as usize, bitset) as i64
        }
        _ => {
            // The second count comes where a wait's timeout does.
            let (count, moved) = (value as i32, timeout as i32);
            if count < 0 || moved < 0 || !addr2.is_multiple_of(4) {
                return -EINVAL;
            }
            let expected = (command == FUTEX_CMP_REQUEUE).then_some(value3);
            let requeued =
                futex::requeue(process, addr, count as usize, addr2, moved as usize, expected);
            requeued.map_or_else(|error| -errno(error), |count| count as i64)
        }
    }
}

/// `membarrier`: with one CPU, each of a program's threads has seen what the
/// others wrote by the time it runs again, so every barrier asked for is
/// already made. QUERY says which there are; musl registers for the private
/// expedited one as it starts its first thread.
pub fn membarrier(command: u64, flags: u64) -> i64 {
    const QUERY: u64 = 0;
    // GLOBAL, GLOBAL_EXPEDITED and its REGISTER, PRIVATE_EXPEDITED and its
    // REGISTER.
    const SUPPORTED: u64 = 0b1_1111;
    match command {
        _ if flags != 0 => -EINVAL,
        QUERY => SUPPORTED as i64,
        command if command & SUPPORTED == command && command.is_power_of_two() => 0,
        _ => -EINVAL,
    }
}

/// Replace the program with the one at `path`, given the `argv` and `envp`
/// arrays: the registers to enter it with. Its descriptors stay open, but
/// for those marked close-on-exec.
pub fn execve(process: &Process, path: u64, argv: u64, envp: u64) -> Result<Registers> {
    // What the strings and their pointers may take of the new stack.
    let mut room = 128 * 1024;
    let path = path_at(process, path)?;
    let mut args = strings_at(process, argv, &mut room)?;
    let env = strings_at(process, envp, &mut room)?;
    let cwd = process.cwd.lock().clone();
    let (elf, exe) = process::program_for(&cwd, &path, &mut args)?;
    let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
    let env: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();
    let start = process::exec(process, &elf, &args, &env)?;
    *process.command.lock() =
        process::Command { exe, ..process::Command::new(path.as_bytes(), &args) };
    files::close_on_exec(process);
    Ok(start)
}

/// The strings an array of pointers ending in a null one points to, as
/// `argv` and `envp` are, taking what they and their pointers hold off
/// `room`: E2BIG if it runs out.
fn strings_at(process: &Process, addr: u64, room: &mut usize) -> Result<Vec<Vec<u8>>> {
    let mut strings = Vec::new();
    // Linux takes a null array as an empty one.
    let mut at = addr;
    while at != 0 {
        let mut pointer = [0; 8];
        process.memory.lock().space.read(at, &mut pointer).ok_or(EFAULT)?;
        let pointer = u64::from_le_bytes(pointer);
        if pointer == 0 {
            break;
        }
        *room = room.checked_sub(8).ok_or(E2BIG)?;
        let string = string_at(process, pointer, *room, E2BIG)?;
        *room -= string.len() + 1;
        strings.push(string);
        // Read whole, so not past the end of the lower half.
        at += 8;
    }
    Ok(strings)
}

/// Wait for a child to end, or with WUNTRACED to stop, or with WCONTINUED
/// to continue: the one numbered `pid`; with -1 any; with 0 any in the
/// caller's group; and below -1 any in group `-pid`. Its number, with how it
/// ended, stopped or continued in `wstatus`; or with WNOHANG and nothing to
/// tell yet, 0. A signal ends the wait early.
pub fn wait4(process: &Process, pid: u64, wstatus: u64, options: u64, rusage: u64) -> i64 {
    const WNOHANG: u64 = 1;
    const WUNTRACED: u64 = 2;
    const WCONTINUED: u64 = 8;
    let (pid, pgid) = (pid as i32, process.pgid());
    let wanted = |child: &Arc<Process>| match pid {
        1.. => child.id == pid as u64,
        0 => child.pgid() == pgid,
        -1 => true,
        _ => child.pgid() == u64::from(pid.unsigned_abs()),
    };
    let (stops, continues) = (options & WUNTRACED != 0, options & WCONTINUED != 0);
    let (mut any, mut found) = (false, None);
    let waited = process.child_exited.wait_until_interruptible(|| {
        let mut children = process.children.lock();
        any = children.iter().any(wanted);
        for at in 0..children.len() {
            let child = &children[at];
            if !wanted(child) {
                continue;
            }
            if let Some(status) = child.ended() {
                // Taken out, but dropped only once interrupts are back on:
                // it may be the last of it, and freeing its memory takes
                // locks taken with them on.
                found = Some((children.remove(at), status));
                return true;
            }
            if let Some(status) = child.signals.take_report(stops, continues) {
                found = Some((child.clone(), status));
                return true;
            }
        }
        !any || options & WNOHANG != 0
    });
    if waited.is_err() {
        return -ERESTARTSYS;
    }
    let Some((child, status)) = found else { return if any { 0 } else { -ECHILD } };
    // Its CPU time is its parent's children's, once it's waited for.
    if child.ended().is_some() {
        process.times.add_child(&child.times);
    }
    ret((|| {
        let mut memory = process.memory.lock();
        if wstatus != 0 {
            memory.space.write(wstatus, &status.to_le_bytes()).ok_or(EFAULT)?;
        }
        if rusage != 0 {
            let (user, system) = clocks::total(&child.times);
            memory.space.write(rusage, &clocks::rusage(user, system)).ok_or(EFAULT)?;
        }
        Ok(child.id as i64)
    })())
}
