//! Boots the kernel's integration tests under QEMU.
//!
//! The tests are bare-metal ELF binaries: no `main`, no libtest, and no way to
//! report a result except to the machine they are running on. So cargo cannot
//! run them directly, and this stands in as its `runner` (wired up in
//! `kernel/.cargo/config.toml`). It wraps the ELF in a BIOS and a UEFI disk
//! image, boots both at once under QEMU, and turns how the guest shut itself
//! down back into an exit code cargo understands: a pass only if both runs
//! passed. `SIMPLOS_FIRMWARE=bios` (or `uefi`) boots just the one.
//!
//! The guest's half of that conversation is `isa-debug-exit`: a device whose
//! only function is that writing to its port makes QEMU exit with a status
//! derived from the value written. `simplos::io::utils::qemu::exit_qemu` writes
//! it at the end of a passing run.
//!
//! A kernel whose file name starts with `shells` (`tests/shells.rs`) gets the
//! userland on its ext4 disk too, busybox and zsh, which `scripts/userland.sh`
//! builds the first time; the others' tests list that disk and expect only
//! what's theirs.

use std::error::Error;
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode, Stdio};
use std::sync::mpsc::{self, Sender};
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
    // under is half of the diagnosis. Unless asked for just one, as when
    // checking that a change to the kernel fails a test.
    let firmwares = match std::env::var("SIMPLOS_FIRMWARE").map(|name| name.to_lowercase()) {
        Err(_) => vec![Firmware::Bios, Firmware::Uefi],
        Ok(name) if name == "bios" => vec![Firmware::Bios],
        Ok(name) if name == "uefi" => vec![Firmware::Uefi],
        Ok(name) => {
            eprintln!("boot-test: SIMPLOS_FIRMWARE is bios or uefi, not {name}");
            return ExitCode::from(2);
        }
    };
    let userland =
        kernel.file_name().is_some_and(|name| name.to_string_lossy().starts_with("shells"));
    let userland = match userland.then(build_userland).transpose() {
        Ok(userland) => userland,
        Err(why) => {
            eprintln!("boot-test: {}: the userland: {why}", kernel.display());
            return ExitCode::FAILURE;
        }
    };
    // At once, as each is a QEMU of its own; what each says is shown in turn,
    // the first's as it comes and the next's once the first is done.
    let (kernel, userland) = (&kernel, userland.as_deref());
    let passed = std::thread::scope(|scope| {
        let runs: Vec<_> = firmwares
            .into_iter()
            .map(|firmware| {
                let (output, said) = mpsc::channel();
                (firmware, said, scope.spawn(move || run(kernel, userland, firmware, output)))
            })
            .collect();
        let mut passed = true;
        for (firmware, said, run) in runs {
            println!("boot-test: {} under {}", kernel.display(), firmware.name());
            for bytes in said {
                let mut stdout = io::stdout().lock();
                let _ = stdout.write_all(&bytes).and_then(|()| stdout.flush());
            }
            if let Err(why) = run.join().expect("a run doesn't panic") {
                eprintln!("boot-test: {} ({}): {why}", kernel.display(), firmware.name());
                passed = false;
            }
        }
        passed
    });
    if passed { ExitCode::SUCCESS } else { ExitCode::FAILURE }
}

/// The tree `scripts/userland.sh` builds, built if it isn't yet.
fn build_userland() -> Result<PathBuf, String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new(repository.join("scripts/userland.sh"))
        .status()
        .map_err(|error| format!("couldn't run scripts/userland.sh: {error}"))?;
    match status.success() {
        true => Ok(repository.join("target/userland/root")),
        false => Err(format!("scripts/userland.sh failed ({status})")),
    }
}

/// One boot of `kernel` under `firmware`, with `userland` on its ext4 disk
/// if given, what it writes to its serial port sent to `output` as it comes:
/// whether the guest reported a pass, and if not, why.
fn run(
    kernel: &Path,
    userland: Option<&Path>,
    firmware: Firmware,
    output: Sender<Vec<u8>>,
) -> Result<(), Box<dyn Error + Send + Sync>> {
    let name = firmware.name();
    // Beside the ELF, inside cargo's target directory: writable, cleaned up by
    // `cargo clean`, and left in place afterwards so a failing test can be
    // booted by hand with the exact image that failed. Each firmware's own, as
    // both run at once.
    let file = |what: &str| kernel.with_extension(format!("{}.{what}", name.to_lowercase()));
    let image = file("img");
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
    let ata_scratch = file("ata.img");
    let ahci_scratch = file("ahci.img");
    for (path, size) in [(&ata_scratch, 1 << 20), (&ahci_scratch, 2 << 20)] {
        std::fs::File::create(path)?.set_len(size)?;
    }
    // And an ext4 one, made by Linux's own mke2fs and checked afterwards by
    // its e2fsck, for what the kernel's file system code does to it.
    let ext4_scratch = file("ext4.img");
    // A program bigger than the kernel's heap, for tests/processes.rs.
    let big =
        kernel.file_name().is_some_and(|name| name.to_string_lossy().starts_with("processes"));
    make_ext4(&ext4_scratch, userland, big)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    if let Firmware::Uefi = firmware {
        // QEMU's default firmware is SeaBIOS; UEFI is OVMF, from the same
        // crate `fullkernel`'s runner uses, so nothing has to be installed.
        qemu.arg("-bios").arg(ovmf_prebuilt::ovmf_pure_efi());
    }
    // The scratch disks' flushes are taken as done (`cache=unsafe`) rather than
    // each made an fsync of its image on the host: tests/files.rs alone makes
    // 2,000 of them, and how long those take is the host's business, not the
    // kernel's. The images still hold everything written, for e2fsck.
    qemu.arg("-drive")
        .arg(format!("format=raw,file={}", image.display()))
        .arg("-drive")
        .arg(format!("format=raw,file={},if=ide,index=1,cache=unsafe", ata_scratch.display()))
        .arg("-drive")
        .arg(format!("format=raw,file={},if=ide,index=2,cache=unsafe", ext4_scratch.display()))
        .args(["-device", "ich9-ahci,id=ahci", "-device", "ide-hd,drive=scratch,bus=ahci.0"])
        .arg("-drive")
        .arg(format!("format=raw,file={},if=none,id=scratch,cache=unsafe", ahci_scratch.display()))
        // How the guest reports its result. Without it a finished test would
        // just sit in `hlt_loop` until the timeout.
        .args(["-device", "isa-debug-exit,iobase=0xf4,iosize=0x04"])
        // Which firmware this run is, so the guest can check it booted the
        // one we meant rather than quietly testing the same path twice.
        .arg("-fw_cfg")
        .arg(format!("name=opt/simplos/firmware,string={name}"))
        // Test output is `serial_println!`, passed on as it comes.
        .args(["-serial", "stdio"])
        .args(["-display", "none"])
        // Never KVM: these run on CI, and a test that only passes with hardware
        // virtualisation is worse than no test.
        .args(["-accel", "tcg", "-cpu", "qemu64", "-m", "128M"])
        // A triple fault should end the run, not restart it into a loop.
        .arg("-no-reboot")
        // No network card: the kernel has no driver for one, and both
        // firmwares spend seconds starting the boot ROM QEMU gives it.
        .args(["-nic", "none"]);

    // Nothing to read from stdin: two at once mustn't both take the terminal.
    let mut child = qemu.stdin(Stdio::null()).stdout(Stdio::piped()).spawn().map_err(|error| {
        io::Error::new(
            error.kind(),
            format!("couldn't start qemu-system-x86_64 ({error}); is QEMU installed?"),
        )
    })?;
    let mut serial = child.stdout.take().expect("piped");
    std::thread::spawn(move || {
        let mut chunk = [0; 4096];
        while let Ok(read @ 1..) = serial.read(&mut chunk) {
            let _ = output.send(chunk[..read].to_vec());
        }
    });

    let started = Instant::now();
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > TIMEOUT {
            child.kill()?;
            child.wait()?;
            return Err(format!("did not finish within {}s", TIMEOUT.as_secs()).into());
        }
        std::thread::sleep(Duration::from_millis(50));
    };

    match status.code() {
        Some(GUEST_SUCCESS) => check_ext4(&ext4_scratch)
            .map_err(|problems| format!("its ext4 disk:\n{problems}").into()),
        Some(GUEST_FAILED) => Err("reported a failure".into()),
        // Anything else is the guest dying on its own terms — a triple fault, a
        // QEMU error, or a kill signal — none of which is a pass.
        Some(code) => Err(format!("exited unexpectedly (status {code})").into()),
        None => Err("exited unexpectedly (killed by a signal)".into()),
    }
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
/// /bin, user/probe for programs to run, and a script whose interpreter isn't
/// there. With `userland`'s tree too, 128 MiB, and /bin/sh is; with `big`,
/// 64 MiB, and /bin/big, from `big_program`.
fn make_ext4(
    image: &Path,
    userland: Option<&Path>,
    big: bool,
) -> Result<(), Box<dyn Error + Send + Sync>> {
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
    if big {
        let probe = std::fs::read(env!("CARGO_BIN_FILE_PROBE_probe"))?;
        std::fs::write(tree.join("bin/big"), big_program(&probe))?;
        std::fs::set_permissions(tree.join("bin/big"), std::fs::Permissions::from_mode(0o755))?;
    }
    if let Some(userland) = userland {
        // Links and modes as they are.
        let copied = Command::new("cp").arg("-a").arg(userland.join(".")).arg(&tree).status()?;
        if !copied.success() {
            return Err(format!("couldn't copy {} ({copied})", userland.display()).into());
        }
    }
    // 4 KiB blocks, which the tests count in: for a disk this small, some
    // mke2fs.conf say 1 KiB.
    let output = e2fsprogs("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-b", "4096", "-d"])
        .arg(&tree)
        .arg(image)
        .arg(match (userland.is_some(), big) {
            (true, _) => "128M",
            (false, true) => "64M",
            (false, false) => "16M",
        })
        .output()
        .map_err(|error| format!("couldn't run mke2fs ({error}); is e2fsprogs installed?"))?;
    if !output.status.success() {
        return Err(format!("mke2fs failed: {}", String::from_utf8_lossy(&output.stderr)).into());
    }
    Ok(())
}

/// Where /bin/big's segment of its own is, past where user/probe's are, and
/// how long: bigger than the kernel's heap, which programs were once read
/// into whole.
const BIG_AT: u64 = 0x1000_0000;
const BIG_LEN: usize = 24 << 20;

/// user/probe, with a segment more: its PT_GNU_STACK header, which simplos
/// doesn't look at, made a PT_LOAD of `BIG_LEN` bytes, each its offset in it
/// modulo 251, read-only, at `BIG_AT`, from past the end of the file as it
/// was. Its mode `peek` tells whether they're there.
fn big_program(probe: &[u8]) -> Vec<u8> {
    const PT_GNU_STACK: u32 = 0x6474_e551;
    let at = |at: usize, len: usize| {
        u64::from_le_bytes(probe[at..at + 8].try_into().unwrap()) as usize & len
    };
    let (phoff, phnum) = (at(32, usize::MAX), at(56, 0xffff));
    let header = (0..phnum)
        .map(|i| phoff + i * 56)
        .find(|&header| at(header, 0xffff_ffff) as u32 == PT_GNU_STACK)
        .expect("user/probe has a PT_GNU_STACK header");
    let offset = probe.len().next_multiple_of(4096);
    let mut big = probe.to_vec();
    // PT_LOAD, PF_R; then where in the file, where in memory twice, how
    // long in each, and its alignment.
    big[header..header + 8].copy_from_slice(&[1, 0, 0, 0, 4, 0, 0, 0]);
    let fields = [offset as u64, BIG_AT, BIG_AT, BIG_LEN as u64, BIG_LEN as u64, 4096];
    for (i, field) in fields.into_iter().enumerate() {
        big[header + 8 + i * 8..header + 16 + i * 8].copy_from_slice(&field.to_le_bytes());
    }
    big.resize(offset, 0);
    big.extend((0..BIG_LEN).map(|i| (i % 251) as u8));
    big
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
