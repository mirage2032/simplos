//! The first program, as Linux runs it: once the kernel is up, the first of
//! `/sbin/init`, `/etc/init`, `/bin/init` and `/bin/sh` there is on the disk
//! runs on the console, with the environment Linux gives it. The kernel's
//! own shell is what's left when there's none, and once it ends.
//!
//! Which program can be chosen, with its arguments, as Linux's `init=` does,
//! through fw_cfg: `-fw_cfg name=opt/simplos/init,string=/bin/sh` for QEMU,
//! or `none` for the kernel's shell.

use crate::fs::Handle;
use crate::io::fwcfg;
use crate::process::{self, Process};
use crate::tty::{Termios, Tty};
use crate::{serial_println, syscall};
use alloc::format;
use alloc::string::String;
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;

/// Where Linux looks for it, in order.
pub const PROGRAMS: [&str; 4] = ["/sbin/init", "/etc/init", "/bin/init", "/bin/sh"];

/// Its environment, as Linux gives it; the kernel's shell gives the programs
/// it runs the same.
pub const ENVIRONMENT: [&str; 2] = ["HOME=/", "TERM=linux"];

/// The fw_cfg file that chooses it.
const CHOICE: &str = "opt/simplos/init";

/// Run the first program on `tty`, and wait for it to end.
pub fn run(tty: &Arc<Tty>) {
    let mut chosen = [0u8; 256];
    let chosen = fwcfg::read_file(CHOICE, &mut chosen)
        .map(|len| String::from_utf8_lossy(&chosen[..len.min(chosen.len())]).into_owned());
    let words: Vec<&str> = chosen.as_deref().unwrap_or("").split_whitespace().collect();
    let started = match words.split_first() {
        Some((&"none", _)) => return,
        Some((&path, args)) => start(tty, &[path], args),
        None => start(tty, &PROGRAMS, &[]),
    };
    let Some((path, first)) = started else { return };
    let status = first.wait();
    // Linux would stop here, with nothing left to run. This has its shell,
    // which wants the terminal as it began, whatever the program left.
    tty.set_termios(Termios::DEFAULT, false);
    tty.write(format!("\n{path} ended, with status {status}.\n").as_bytes());
}

/// Start the first of `programs` there is on the disk on `tty`, with `args`
/// after its name: which it was, and the process.
pub fn start(tty: &Arc<Tty>, programs: &[&str], args: &[&str]) -> Option<(String, Arc<Process>)> {
    for &path in programs {
        let mut argv = vec![path];
        argv.extend(args);
        let console = Arc::new(Handle::tty(tty.clone()));
        match process::spawn_program(path, &argv, &ENVIRONMENT, vec![Some(console); 3]) {
            Ok(first) => {
                serial_println!("simplos: running {}", path);
                return Some((path.into(), first));
            }
            // ENOENT: not there, so on to the next.
            Err(2) => {}
            // There, but it can't be run: said so, as Linux does, and on.
            Err(errno) => {
                let why =
                    format!("{path} is there but couldn't be run: {}", syscall::strerror(errno));
                serial_println!("simplos: {}", why);
                tty.write(format!("{why}\n").as_bytes());
            }
        }
    }
    None
}
