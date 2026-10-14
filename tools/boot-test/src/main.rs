//! Boots the kernel's integration tests under QEMU.
//!
//! The tests are bare-metal ELF binaries: no `main`, no libtest, and no way to
//! report a result except to the machine they are running on. So cargo cannot
//! run them directly, and this stands in as its `runner` (wired up in
//! `kernel/.cargo/config.toml`). It wraps the ELF in a BIOS and a UEFI disk
//! image, boots each under QEMU, and turns how the guest shut itself down back
//! into an exit code cargo understands: a pass only if both runs passed.
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

/// The two ways the kernel can be started. They share nothing before the
/// kernel's entry point (a real-mode stage chain on one side, a UEFI
/// application on the other) and hand over different memory maps and
/// framebuffers, so a test that passes under one says nothing about the other.
#[derive(Clone, Copy)]
enum Firmware {
    Bios,
    Uefi,
}

impl Firmware {
    /// Spelled as `simplos::BootMode::as_str` spells it, because the guest
    /// compares the two (see `tests/basic_boot.rs`).
    fn name(self) -> &'static str {
        match self {
            Firmware::Bios => "BIOS",
            Firmware::Uefi => "UEFI",
        }
    }
}

fn main() -> ExitCode {
    // Cargo passes the test binary first; with a libtest-style harness it may
    // append its own flags, which mean nothing to a kernel and are dropped.
    let Some(kernel) = std::env::args_os().nth(1).map(PathBuf::from) else {
        eprintln!("usage: boot-test <kernel-elf>");
        return ExitCode::from(2);
    };

    // Both, even when the first fails: which firmware a failure shows up
    // under is half of the diagnosis.
    let mut passed = true;
    for firmware in [Firmware::Bios, Firmware::Uefi] {
        println!("boot-test: {} under {}", kernel.display(), firmware.name());
        passed &= run(&kernel, firmware).unwrap_or_else(|error| {
            eprintln!("boot-test: {} ({}): {error}", kernel.display(), firmware.name());
            false
        });
    }
    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// One boot of `kernel` under `firmware`: whether the guest reported a pass.
fn run(kernel: &Path, firmware: Firmware) -> Result<bool, Box<dyn Error>> {
    let name = firmware.name();
    // Beside the ELF, inside cargo's target directory: writable, cleaned up by
    // `cargo clean`, and left in place afterwards so a failing test can be
    // booted by hand with the exact image that failed.
    let image = kernel.with_extension(format!("{}.img", name.to_lowercase()));
    let probe = PathBuf::from(env!("CARGO_BIN_FILE_PROBE_probe"));
    match firmware {
        Firmware::Bios => {
            bootloader::BiosBoot::new(kernel).set_ramdisk(&probe).create_disk_image(&image)?
        }
        Firmware::Uefi => {
            bootloader::UefiBoot::new(kernel).set_ramdisk(&probe).create_disk_image(&image)?
        }
    }
    // Blank scratch disks for tests/disk.rs to write to, one per controller
    // kind, sized differently so the test can tell which is which. Recreated
    // each run, so nothing a test wrote survives into the next.
    let ata_scratch = kernel.with_extension("ata.img");
    let ahci_scratch = kernel.with_extension("ahci.img");
    for (path, size) in [(&ata_scratch, 1 << 20), (&ahci_scratch, 2 << 20)] {
        std::fs::File::create(path)?.set_len(size)?;
    }
    // And an ext4 one, made by Linux's own mke2fs and checked afterwards by
    // its e2fsck, for what the kernel's file system code does to it.
    let ext4_scratch = kernel.with_extension("ext4.img");
    make_ext4(&ext4_scratch)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    if let Firmware::Uefi = firmware {
        // QEMU's default firmware is SeaBIOS; UEFI is OVMF, from the same
        // crate `fullkernel`'s runner uses, so nothing has to be installed.
        qemu.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
    }
    qemu.arg("-drive")
        .arg(format!("format=raw,file={}", image.display()))
        .arg("-drive")
        .arg(format!("format=raw,file={},if=ide,index=1", ata_scratch.display()))
        .arg("-drive")
        .arg(format!("format=raw,file={},if=ide,index=2", ext4_scratch.display()))
        .args(["-device", "ich9-ahci,id=ahci", "-device", "ide-hd,drive=scratch,bus=ahci.0"])
        .arg("-drive")
        .arg(format!("format=raw,file={},if=none,id=scratch", ahci_scratch.display()))
        // How the guest reports its result. Without it a finished test would
        // just sit in `hlt_loop` until the timeout.
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        // Which firmware this run is, so the guest can check it booted the
        // one we meant rather than quietly testing the same path twice.
        .arg("-fw_cfg")
        .arg(format!("name=opt/simplos/firmware,string={name}"))
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
                "boot-test: {} ({name}) did not finish within {}s",
                kernel.display(),
                TIMEOUT.as_secs()
            );
            return Ok(false);
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    Ok(match status.code() {
        Some(GUEST_SUCCESS) => check_ext4(&ext4_scratch)
            .map_err(|problems| {
                eprintln!("boot-test: {} ({name}), its ext4 disk:\n{problems}", kernel.display())
            })
            .is_ok(),
        Some(GUEST_FAILED) => {
            eprintln!("boot-test: {} ({name}) reported a failure", kernel.display());
            false
        }
        // Anything else is the guest dying on its own terms — a triple fault, a
        // QEMU error, or a kill signal — none of which is a pass.
        other => {
            eprintln!(
                "boot-test: {} ({name}) exited unexpectedly ({})",
                kernel.display(),
                match other {
                    Some(code) => format!("status {code}"),
                    None => "killed by a signal".to_owned(),
                }
            );
            false
        }
    })
}

/// e2fsprogs lives in /sbin, which isn't on every user's PATH.
fn e2fsprogs(tool: &str) -> Command {
    let sbin = Path::new("/usr/sbin").join(tool);
    Command::new(if sbin.exists() { sbin } else { PathBuf::from(tool) })
}

/// The files the ext4 scratch disk starts with, which `tests/ext4.rs` reads.
const EXT4_TREE: &[(&str, &str)] =
    &[("hello.txt", "hello from ext4\n"), ("dir/nested/deep.txt", "deep\n")];

/// A 16 MiB ext4 file system holding `EXT4_TREE`, as Linux makes them, and in
/// /bin, user/probe for programs to run, and a file marked executable that
/// isn't a program.
fn make_ext4(image: &Path) -> Result<(), Box<dyn Error>> {
    use std::os::unix::fs::PermissionsExt;
    let tree = image.with_extension("tree");
    let _ = std::fs::remove_dir_all(&tree);
    for (path, contents) in EXT4_TREE {
        let path = tree.join(path);
        std::fs::create_dir_all(path.parent().expect("a file in a directory"))?;
        std::fs::write(path, contents)?;
    }
    std::fs::create_dir(tree.join("bin"))?;
    std::fs::copy(env!("CARGO_BIN_FILE_PROBE_probe"), tree.join("bin/probe"))?;
    let script = tree.join("bin/script");
    std::fs::write(&script, "#!/bin/sh\necho a script\n")?;
    std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755))?;
    let output = e2fsprogs("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-d"])
        .arg(&tree)
        .arg(image)
        .arg("16M")
        .output()
        .map_err(|error| format!("couldn't run mke2fs ({error}); is e2fsprogs installed?"))?;
    if !output.status.success() {
        return Err(format!("mke2fs failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

/// What's wrong with the ext4 disk after a run: anything e2fsck finds,
/// changing nothing; or, since a passing run unmounts it, its still being
/// marked as mounted, which e2fsck doesn't count as wrong.
fn check_ext4(image: &Path) -> Result<(), String> {
    let output = e2fsprogs("e2fsck").arg("-fn").arg(image).output().map_err(|e| e.to_string())?;
    if !output.status.success() {
        return Err(String::from_utf8_lossy(&output.stdout).into_owned());
    }
    // The superblock, 1 KiB in: `s_state` says clean (bit 0), and
    // `s_feature_incompat` doesn't say the journal needs replaying (bit 2).
    let disk = std::fs::read(image).map_err(|e| e.to_string())?;
    let superblock = &disk[1024..2048];
    let state = u16::from_le_bytes([superblock[0x3a], superblock[0x3b]]);
    let incompat = u32::from_le_bytes(superblock[0x60..0x64].try_into().unwrap());
    match state & 1 != 0 && incompat & 4 == 0 {
        true => Ok(()),
        false => Err(String::from("it's still mounted: unmount it before a passing exit")),
    }
}
