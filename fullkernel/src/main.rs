//! Boots the disk images built by `build.rs` under QEMU.
//!
//! `cargo run` uses the UEFI image; `cargo run -- --bios` uses the BIOS one.
//! `--debug` makes QEMU wait for a debugger before the first instruction (the
//! gdb server is always on). Anything else is passed through to QEMU, so
//! `cargo run -- -fw_cfg name=opt/simplos/fb,string=1024x768` asks the kernel
//! for a smaller framebuffer without rebuilding it.

use std::path::Path;

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
