//! Boots the disk images built by `build.rs` under QEMU.
//!
//! `cargo run` uses the UEFI image; `cargo run -- --bios` uses the BIOS one.
//! `--debug` makes QEMU wait for a debugger before the first instruction (the
//! gdb server is always on). Anything else is passed through to QEMU, so
//! `cargo run -- -fw_cfg name=opt/simplos/fb,string=1024x768` asks the kernel
//! for a smaller framebuffer without rebuilding it.
//!
//! An ext4 disk goes with it, `disk.img` here, made the first time and kept,
//! so what's written to it is there next time; delete it to start afresh.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn main() {
    // Set by the build script, which creates both images.
    let uefi_path = env!("UEFI_PATH");
    let bios_path = env!("BIOS_PATH");

    let args: Vec<String> = std::env::args().skip(1).collect();
    let uefi = !args.iter().any(|arg| arg == "--bios");

    let mut cmd = std::process::Command::new("qemu-system-x86_64");
    if uefi {
        cmd.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
        cmd.arg("-drive").arg(format!("format=raw,file={uefi_path}"));
    } else {
        cmd.arg("-drive").arg(format!("format=raw,file={bios_path}"));
    }

    // Always enable gdb server
    cmd.arg("-s");

    // Serial output to terminal for debugging
    cmd.arg("-serial").arg("stdio");

    // Process command line args
    args.iter().for_each(|arg| {
        if arg == "--debug" {
            cmd.arg("-S"); // Wait for debugger
        } else if arg != "--bios" {
            cmd.arg(arg);
        }
    });

    // After any of the caller's, whose ext4 disk the kernel then finds first.
    if let Some(disk) = disk() {
        cmd.arg("-drive").arg(format!("format=raw,file={}", disk.display()));
    }

    // KVM when the host can give it to us, software emulation when it can't —
    // CI runners and containers have no `/dev/kvm`, and hardcoding `-accel kvm`
    // made `cargo run` fail outright there. `-cpu host` only means anything with
    // KVM; under TCG, `qemu64` is the same CPU the browser build emulates, so a
    // local run without KVM reproduces what visitors to the site get.
    //
    // Skipped if the caller passed their own, so both stay overridable.
    let kvm = Path::new("/dev/kvm").exists();
    let given = |flag: &str| args.iter().any(|arg| arg == flag);
    if !given("-accel") && !given("-machine") {
        if !kvm {
            eprintln!("note: /dev/kvm is unavailable, falling back to software emulation (slow)");
        }
        cmd.arg("-accel").arg(if kvm { "kvm" } else { "tcg" });
    }
    if !given("-cpu") {
        cmd.arg("-cpu").arg(if kvm { "host" } else { "qemu64" });
    }

    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}

/// The ext4 disk to attach: `disk.img`, made with e2fsprogs' mke2fs if it
/// isn't there yet. None, with a note, if it can't be.
fn disk() -> Option<PathBuf> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let image = here.join("disk.img");
    if image.exists() {
        return Some(image);
    }
    let tree = here.join("target/disk-tree");
    std::fs::create_dir_all(&tree).ok()?;
    std::fs::write(tree.join("hello.txt"), "hello from the ext4 disk\n").ok()?;
    // Where Linux and Homebrew put it: often not on PATH.
    let mke2fs = [
        "/usr/sbin/mke2fs",
        "/opt/homebrew/opt/e2fsprogs/sbin/mke2fs",
        "/usr/local/opt/e2fsprogs/sbin/mke2fs",
    ]
    .into_iter()
    .find(|path| Path::new(path).exists())
    .unwrap_or("mke2fs");
    let made = Command::new(mke2fs)
        .args(["-q", "-t", "ext4", "-d"])
        .arg(&tree)
        .arg(&image)
        .arg("64M")
        .stdout(Stdio::null())
        .status();
    if made.is_ok_and(|status| status.success()) {
        return Some(image);
    }
    eprintln!("note: no ext4 disk: couldn't make {} with mke2fs (e2fsprogs)", image.display());
    None
}
