//! Boots the kernel's integration tests under QEMU.
//!
//! The tests are bare-metal ELF binaries: no `main`, no libtest, and no way to
//! report a result except to the machine they are running on. So cargo cannot
//! run them directly, and this stands in as its `runner` (wired up in
//! `kernel/.cargo/config.toml`). It wraps the ELF in a bootable disk image,
//! starts QEMU, and turns how the guest shut itself down back into an exit code
//! cargo understands.
//!
//! The guest's half of that conversation is `isa-debug-exit`: a device whose
//! only function is that writing to its port makes QEMU exit with a status
//! derived from the value written. `simplos::io::utils::qemu::exit_qemu` writes
//! it at the end of a passing run.

use std::error::Error;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::time::{Duration, Instant};

/// Long enough for a slow interpreted boot on a loaded CI runner, short enough
/// that a hung test fails the job rather than sitting until the job's own limit.
const TIMEOUT: Duration = Duration::from_secs(180);

/// QEMU reports a debug-exit of `value` as `(value << 1) | 1`, so the kernel's
/// `QemuExitCode::Success` (0x10) and `Failed` (0x11) arrive as these.
const GUEST_SUCCESS: i32 = (0x10 << 1) | 1;
const GUEST_FAILED: i32 = (0x11 << 1) | 1;

fn main() -> ExitCode {
    // Cargo passes the test binary first; with a libtest-style harness it may
    // append its own flags, which mean nothing to a kernel and are dropped.
    let Some(kernel) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: boot-test <kernel-elf>");
        return ExitCode::from(2);
    };

    match run(&kernel) {
        Ok(code) => code,
        Err(error) => {
            eprintln!("boot-test: {}: {error}", kernel.display());
            ExitCode::FAILURE
        }
    }
}

fn run(kernel: &Path) -> Result<ExitCode, Box<dyn Error>> {
    // Beside the ELF, inside cargo's target directory: writable, cleaned up by
    // `cargo clean`, and left in place afterwards so a failing test can be
    // booted by hand with the exact image that failed.
    let image = kernel.with_extension("img");
    bootloader::BiosBoot::new(kernel).create_disk_image(&image)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    qemu.arg("-drive")
        .arg(format!("format=raw,file={}", image.display()))
        // How the guest reports its result. Without it a finished test would
        // just sit in `hlt_loop` until the timeout.
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        // Test output is `serial_println!`, and cargo shows it for a test that
        // fails.
        .args(["-serial", "stdio"])
        .args(["-display", "none"])
        // Never KVM: these run on CI, and a test that only passes with hardware
        // virtualisation is worse than no test.
        .args(["-accel", "tcg", "-cpu", "qemu64", "-m", "128M"])
        // A triple fault should end the run, not restart it into a loop.
        .arg("-no-reboot");

    let mut child = qemu.spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("couldn't start qemu-system-x86_64 ({error}); is QEMU installed?"),
        )
    })?;

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            child.kill()?;
            child.wait()?;
            eprintln!(
                "boot-test: {} did not finish within {}s",
                kernel.display(),
                TIMEOUT.as_secs()
            );
            return Ok(ExitCode::FAILURE);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(match status.code() {
        Some(GUEST_SUCCESS) => ExitCode::SUCCESS,
        Some(GUEST_FAILED) => {
            eprintln!("boot-test: {} reported a failure", kernel.display());
            ExitCode::FAILURE
        }
        // Anything else is the guest dying on its own terms — a triple fault, a
        // QEMU error, or a kill signal — none of which is a pass.
        other => {
            eprintln!(
                "boot-test: {} exited unexpectedly ({})",
                kernel.display(),
                match other {
                    Some(code) => format!("status {code}"),
                    None => "killed by a signal".to_owned(),
                }
            );
            ExitCode::FAILURE
        }
    })
}
