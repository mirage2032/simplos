//! System calls on programs: copying one (`fork` and the like), replacing
//! one with another (`execve`), and waiting for copies to end (`wait4`).

use super::files::{self, AT_FDCWD, Result, lookup, path_at, ret, string_at};
use super::{EFAULT, EINVAL, ENOMEM};
use crate::fs;
use crate::process::{self, Process, Registers};
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

/// Wait for a child to end: the one numbered `pid`, or with any other `pid`
/// any of them. Its number, with how it ended in `wstatus`; or with WNOHANG
/// and none ended yet, 0.
///
/// shortcut: no process groups, so 0 and below -1 mean any child too; and
/// nothing stops, so WUNTRACED and WCONTINUED change nothing.
pub fn wait4(process: &Process, pid: u64, wstatus: u64, options: u64, rusage: u64) -> i64 {
    const WNOHANG: u64 = 1;
    let pid = pid as i32;
    let wanted = |child: &Arc<Process>| pid <= 0 || child.id == pid as u64;
    let (mut any, mut ended) = (false, None);
    process.child_exited.wait_until(|| {
        let mut children = process.children.lock();
        any = children.iter().any(wanted);
        let at = children.iter().position(|child| wanted(child) && child.ended().is_some());
        // Taken out, but dropped only once interrupts are back on: it may be
        // the last of it, and freeing its memory takes locks taken with
        // them on.
        ended = at.map(|at| children.remove(at));
        !any || ended.is_some() || options & WNOHANG != 0
    });
    let Some(child) = ended else { return if any { 0 } else { -ECHILD } };
    let status = child.ended().expect("it ended");
    ret((|| {
        let mut memory = process.memory.lock();
        if wstatus != 0 {
            memory.space.write(wstatus, &status.to_le_bytes()).ok_or(EFAULT)?;
        }
        // `struct rusage`: nothing is counted yet.
        if rusage != 0 {
            memory.space.write(rusage, &[0; 144]).ok_or(EFAULT)?;
        }
        Ok(child.id as i64)
    })())
}
