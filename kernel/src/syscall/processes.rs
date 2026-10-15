//! System calls on programs: copying one (`fork` and the like), replacing
//! one with another (`execve`), and waiting for copies to end (`wait4`).

use super::clocks;
use super::files::{self, AT_FDCWD, Result, lookup, path_at, ret, string_at};
use super::{EFAULT, EINVAL, ENOMEM};
use crate::fs;
use crate::process::{self, Process, Registers};
use crate::signal::ERESTARTSYS;
use alloc::sync::Arc;
use alloc::vec::Vec;

const E2BIG: i64 = 7;
const ECHILD: i64 = 10;

/// What a parent is sent as its child ends, as `fork` asks.
pub const SIGCHLD: u64 = 17;

/// `fork`, `vfork`, and `clone` as C libraries use it for them: a copy of the
/// program, which starts with `child`, on `stack` if it isn't 0.
///
/// shortcut: CLONE_VM is taken as a copy too, and CLONE_VFORK doesn't hold
/// the parent until the child execs or exits; musl's `posix_spawn`, the one
/// user, needs neither. Threads, which really share memory, come later.
pub fn clone(process: &Arc<Process>, mut child: Registers, flags: u64, stack: u64) -> i64 {
    const CLONE_VM: u64 = 0x100;
    const CLONE_VFORK: u64 = 0x4000;
    // The low byte: what the parent is sent as the child ends.
    const SIGNAL: u64 = 0xFF;
    if flags & !(CLONE_VM | CLONE_VFORK | SIGNAL) != 0 {
        return -EINVAL;
    }
    if stack != 0 {
        child.rsp = stack;
    }
    process::fork(process, child).map_or(-ENOMEM, |id| id as i64)
}

/// Replace the program with the one at `path`, given the `argv` and `envp`
/// arrays: the registers to enter it with. Its descriptors stay open, but
/// for those marked close-on-exec.
///
/// shortcut: ELF programs only; a script's `#!` line isn't followed, and
/// shells run scripts themselves when told ENOEXEC.
pub fn execve(process: &Process, path: u64, argv: u64, envp: u64) -> Result<Registers> {
    // What the strings and their pointers may take of the new stack.
    let mut room = 128 * 1024;
    let path = lookup(process, AT_FDCWD, &path_at(process, path)?)?;
    let args = strings_at(process, argv, &mut room)?;
    let env = strings_at(process, envp, &mut room)?;
    let elf = fs::read_program(&path)?;
    let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
    let env: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();
    let start = process::exec(process, &elf, &args, &env).map_err(process::load_errno)?;
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
