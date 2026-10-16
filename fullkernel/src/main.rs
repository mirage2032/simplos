//! Boots the disk images built by `build.rs` under QEMU.
//!
//! `cargo run` uses the UEFI image; `cargo run -- --bios` uses the BIOS one.
//! `--debug` makes QEMU wait for a debugger before the first instruction (the
//! gdb server is always on). Anything else is passed through to QEMU, so
//! `cargo run -- -fw_cfg name=opt/simplos/fb,string=1024x768` asks the kernel
//! for a smaller framebuffer without rebuilding it.
//!
//! An ext4 disk goes with it, `disk.img` here, made the first time and kept,
//! so what's written to it is there next time; delete it to start afresh. It
//! starts with `hello.txt`, user/probe as `/bin/probe`, the userland that
//! `scripts/userland.sh` builds, busybox, uutils, zsh and vim, GCC, which
//! `scripts/toolchain.sh` builds, and Rust's rustc and cargo, which
//! `scripts/rust.sh` unpacks, if they can be.

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
    // On an AHCI controller of its own, which the kernel reads by DMA: four
    // times as fast under emulation as an IDE disk's port I/O, which is what
    // starting gcc's 36 MB cc1 waits on.
    if let Some(disk) = disk() {
        cmd.args(["-device", "ich9-ahci,id=simplos-ahci"]);
        cmd.arg("-drive")
            .arg(format!("id=simplos-disk,if=none,format=raw,file={}", disk.display()));
        cmd.args(["-device", "ide-hd,drive=simplos-disk,bus=simplos-ahci.0"]);
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
    // Room to compile more than hello world with gcc and g++, which QEMU's
    // default 128 MiB is about enough for.
    if !given("-m") {
        cmd.arg("-m").arg("1G");
    }

    let mut child = cmd.spawn().unwrap();
    child.wait().unwrap();
}

/// The ext4 disk to attach: `disk.img`, made with e2fsprogs' mke2fs if it
/// isn't there yet. None, with a note, if it can't be. A release's
/// `disk.qcow2` is made from the same tree by `scripts/build-images.sh`:
/// change one, change the other.
fn disk() -> Option<PathBuf> {
    let here = Path::new(env!("CARGO_MANIFEST_DIR"));
    let image = here.join("disk.img");
    if image.exists() {
        return Some(image);
    }
    // Afresh: what an earlier one left would be copied over, and through
    // its links, as vim's xxd went through busybox's /usr/bin/xxd onto
    // busybox itself.
    let tree = here.join("target/disk-tree");
    let _ = std::fs::remove_dir_all(&tree);
    std::fs::create_dir_all(&tree).ok()?;
    std::fs::write(tree.join("hello.txt"), "hello from the ext4 disk\n").ok()?;
    std::fs::create_dir_all(tree.join("bin")).ok()?;
    std::fs::copy(env!("PROBE_PATH"), tree.join("bin/probe")).ok()?;
    // Built the first time: the userland in a few minutes, GCC in a quarter
    // of an hour on a fast machine. Links and modes kept, and what's there replaced rather
    // than written through, as GCC's binutils replace busybox's links.
    let repository = here.join("..");
    let trees =
        [("userland", "busybox, uutils, zsh or vim"), ("toolchain", "gcc"), ("rust", "cargo")];
    for (what, has) in trees {
        let built = Command::new(repository.join(format!("scripts/{what}.sh"))).status();
        let copied = built.is_ok_and(|status| status.success())
            && Command::new("cp")
                .args(["-a", "--remove-destination"])
                .arg(repository.join(format!("target/{what}/root/.")))
                .arg(&tree)
                .status()
                .is_ok_and(|status| status.success());
        if !copied {
            eprintln!("note: the disk has no {has}; delete {} to try again", image.display());
        }
    }
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
        .arg("2G")
        .stdout(Stdio::null())
        .status();
    if made.is_ok_and(|status| status.success()) {
        return Some(image);
    }
    eprintln!("note: no ext4 disk: couldn't make {} with mke2fs (e2fsprogs)", image.display());
    None
}
