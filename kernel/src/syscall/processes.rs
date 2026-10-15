//! System calls on programs: copying one (`fork` and the like), replacing
//! one with another (`execve`), and waiting for copies to end (`wait4`).

use super::clocks;
use super::files::{self, AT_FDCWD, Result, lookup, path_at, ret, string_at};
use super::{EFAULT, EINVAL, ENOMEM};
use crate::fs;
use crate::process::{self, Process, Registers};
use crate::signal::ERESTARTSYS;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec::Vec;

const ENOENT: i64 = 2;
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
    const ELOOP: i64 = 40;
    // What the strings and their pointers may take of the new stack.
    let mut room = 128 * 1024;
    let mut path = path_at(process, path)?;
    let mut args = strings_at(process, argv, &mut room)?;
    let env = strings_at(process, envp, &mut room)?;
    let mut elf = fs::read_program(&lookup(process, AT_FDCWD, &path)?)?;
    // A script: what its first line names runs instead, given the line's
    // argument if it has one and the script's path, in place of the first
    // of the script's arguments. Its interpreter can be a script too, up to
    // where Linux gives up.
    for depth in 0.. {
        let Some((interpreter, argument)) = interpreter(&elf) else { break };
        if depth == 5 {
            return Err(ELOOP);
        }
        let interpreter = String::from_utf8(interpreter.to_vec()).map_err(|_| ENOENT)?;
        let mut first = Vec::from([interpreter.clone().into_bytes()]);
        first.extend(argument.map(<[u8]>::to_vec));
        first.push(path.into_bytes());
        let more: usize = first.iter().map(|string| string.len() + 9).sum();
        room = room.checked_sub(more).ok_or(E2BIG)?;
        args.splice(..args.len().min(1), first);
        elf = fs::read_program(&lookup(process, AT_FDCWD, &interpreter)?)?;
        path = interpreter;
    }
    let args: Vec<&[u8]> = args.iter().map(Vec::as_slice).collect();
    let env: Vec<&[u8]> = env.iter().map(Vec::as_slice).collect();
    let start = process::exec(process, &elf, &args, &env).map_err(process::load_errno)?;
    files::close_on_exec(process);
    Ok(start)
}

/// What runs the script `program`, as Linux reads its first line: `#!`, then
/// the interpreter's path, then what's left of the line as one argument if
/// anything is, past spaces and tabs. Within the first 256 bytes, where a
/// line that goes on past them must at least have the path whole. `None` if
/// it isn't a script, or names nothing.
fn interpreter(program: &[u8]) -> Option<(&[u8], Option<&[u8]>)> {
    const LONGEST: usize = 256;
    let blank = |byte: &u8| matches!(byte, b' ' | b'\t');
    let head = program.strip_prefix(b"#!")?;
    let head = &head[..head.len().min(LONGEST - 2)];
    let line = match head.iter().position(|&byte| byte == b'\n') {
        Some(end) => &head[..end],
        None if program.len() < LONGEST => head,
        None => {
            let line = &head[..head.len() - 1];
            let path = line.iter().position(|byte| !blank(byte))?;
            line[path..].iter().position(|byte| blank(byte) || *byte == 0)?;
            line
        }
    };
    let line = line.split(|&byte| byte == 0).next()?;
    let start = line.iter().position(|byte| !blank(byte))?;
    let end = line.iter().rposition(|byte| !blank(byte))? + 1;
    let line = &line[start..end];
    let (path, rest) = line.split_at(line.iter().position(blank).unwrap_or(line.len()));
    let argument = rest.iter().position(|byte| !blank(byte)).map(|at| &rest[at..]);
    Some((path, argument))
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

#[cfg(test)]
mod tests {
    use super::interpreter;
    use alloc::vec::Vec;

    /// The interpreter `line` names, and its argument, as text.
    fn named(line: &[u8]) -> Option<(&str, Option<&str>)> {
        let text = |bytes| core::str::from_utf8(bytes).unwrap();
        interpreter(line).map(|(path, argument)| (text(path), argument.map(text)))
    }

    #[test_case]
    fn a_script_s_first_line_names_what_runs_it() {
        assert_eq!(named(b"#!/bin/sh\necho"), Some(("/bin/sh", None)));
        // One argument, however many words, without the blanks around it.
        assert_eq!(named(b"#! \t/bin/sh  -e -x \t\nmore"), Some(("/bin/sh", Some("-e -x"))));
        // The end of the file ends the line too.
        assert_eq!(named(b"#!/bin/sh"), Some(("/bin/sh", None)));
        assert_eq!(named(b"#!\n/bin/sh"), None);
        assert_eq!(named(b"#! \t \n"), None);
        assert_eq!(named(b"\x7fELF"), None);
        // Too long a line: its argument is cut short, but not its path.
        let mut long = b"#!/bin/sh ".to_vec();
        long.resize(300, b'x');
        let cut = core::str::from_utf8(&long[10..255]).unwrap();
        assert_eq!(named(&long), Some(("/bin/sh", Some(cut))));
        let mut path: Vec<u8> = b"#!/".to_vec();
        path.resize(300, b'x');
        assert_eq!(named(&path), None);
    }
}
