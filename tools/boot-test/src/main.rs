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
//! derived from the value written. `simplos::drivers::qemu::exit_qemu` writes
//! it at the end of a passing run.
//!
//! A kernel whose file name starts with `shells` (`tests/shells.rs`) gets the
//! userland on its ext4 disk too, busybox, uutils and zsh, which `user/ports/userland.sh`
//! builds the first time; one whose name starts with `toolchain`
//! (`tests/toolchain.rs`), GCC as well, from `user/ports/toolchain.sh`, and the
//! time compiling takes; one whose name starts with `rust` (`tests/rust.rs`),
//! Rust's rustc and cargo too, from `user/ports/rust.sh`, and the memory and
//! the disk they take. The others' tests list that disk and expect only
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
/// The same, for a kernel whose tests compile with GCC under emulation, each
/// given 300 s.
const TOOLCHAIN_TIMEOUT: Duration = Duration::from_secs(900);
/// And with rustc and cargo, each of which reads its 230 MB library as it
/// starts.
const RUST_TIMEOUT: Duration = Duration::from_secs(1800);

/// What a kernel boots with, by its name: the trees its ext4 disk gets
/// besides its own, as `scripts/WHAT.sh` build them; how big that disk is;
/// whether it gets the programs `crafted` makes; its memory; and how long it
/// may take.
struct Setup {
    trees: Vec<PathBuf>,
    disk: &'static str,
    crafted: bool,
    memory: &'static str,
    timeout: Duration,
}

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
    let named = |prefix: &str| {
        kernel.file_name().is_some_and(|name| name.to_string_lossy().starts_with(prefix))
    };
    let rust = named("rust");
    let toolchain = named("toolchain") || rust;
    let userland = named("shells") || toolchain;
    let mut trees = Vec::new();
    for (what, wanted) in [("userland", userland), ("toolchain", toolchain), ("rust", rust)] {
        match wanted.then(|| build(what)).transpose() {
            Ok(tree) => trees.extend(tree),
            Err(why) => {
                eprintln!("boot-test: {}: the {what}: {why}", kernel.display());
                return ExitCode::FAILURE;
            }
        }
    }
    // Programs made from user/probe, for tests/processes.rs.
    let crafted = named("processes");
    let setup = Setup {
        trees,
        disk: match () {
            _ if rust => "2G",
            _ if toolchain => "1G",
            _ if userland => "128M",
            _ if crafted => "64M",
            _ => "16M",
        },
        crafted,
        // rustc maps its library whole, in memory, as it starts.
        memory: if rust { "1G" } else { "128M" },
        timeout: match () {
            _ if rust => RUST_TIMEOUT,
            _ if toolchain => TOOLCHAIN_TIMEOUT,
            _ => TIMEOUT,
        },
    };
    // At once, as each is a QEMU of its own; what each says is shown in turn,
    // the first's as it comes and the next's once the first is done.
    let (kernel, setup) = (&kernel, &setup);
    let passed = std::thread::scope(|scope| {
        let runs: Vec<_> = firmwares
            .into_iter()
            .map(|firmware| {
                let (output, said) = mpsc::channel();
                let run = move || run(kernel, setup, firmware, output);
                (firmware, said, scope.spawn(run))
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

/// The tree `scripts/WHAT.sh` builds, the userland's, the toolchain's or
/// Rust's, built if it isn't yet.
fn build(what: &str) -> Result<PathBuf, String> {
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let status = Command::new(repository.join(format!("user/ports/{what}.sh")))
        .status()
        .map_err(|error| format!("couldn't run user/ports/{what}.sh: {error}"))?;
    match status.success() {
        true => Ok(repository.join(format!("target/{what}/root"))),
        false => Err(format!("user/ports/{what}.sh failed ({status})")),
    }
}

/// One boot of `kernel` under `firmware`, as `setup` says, what it writes to
/// its serial port sent to `output` as it comes: whether the guest reported
/// a pass, and if not, why.
fn run(
    kernel: &Path,
    setup: &Setup,
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
    make_ext4(&ext4_scratch, setup)?;

    let mut qemu = Command::new("qemu-system-x86_64");
    if let Firmware::Uefi = firmware {
        // QEMU's default firmware is SeaBIOS; UEFI is OVMF, from the same
        // crate `tools/boot` uses, so nothing has to be installed.
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
        .args(["-accel", "tcg", "-cpu", "qemu64", "-m", setup.memory])
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
    let timeout = setup.timeout;
    let status = loop {
        if let Some(status) = child.try_wait()? {
            break status;
        }
        if started.elapsed() > timeout {
            child.kill()?;
            child.wait()?;
            return Err(format!("did not finish within {}s", timeout.as_secs()).into());
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

/// An ext4 file system, as Linux makes them, as big as `setup` says, holding
/// `EXT4_TREE`; in /bin, user/probe for programs to run, and a script whose
/// interpreter isn't there; and `setup`'s trees: the userland's, with
/// /bin/sh, GCC's, Rust's. With `setup.crafted`, in /bin what `crafted` makes
/// too.
fn make_ext4(image: &Path, setup: &Setup) -> Result<(), Box<dyn Error + Send + Sync>> {
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
    if setup.crafted {
        let probe = std::fs::read(env!("CARGO_BIN_FILE_PROBE_probe"))?;
        for (name, program) in self::crafted(&probe) {
            let path = tree.join("bin").join(name);
            std::fs::write(&path, program)?;
            std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o755))?;
        }
    }
    // Links and modes as they are; and what's there replaced, not written
    // through, as the toolchain's binutils replace busybox's links.
    for from in &setup.trees {
        let copied = Command::new("cp")
            .args(["-a", "--remove-destination"])
            .arg(from.join("."))
            .arg(&tree)
            .status()?;
        if !copied.success() {
            return Err(format!("couldn't copy {} ({copied})", from.display()).into());
        }
    }
    // compinit's record of their completions, once they're all in.
    let repository = Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");
    let recorded =
        Command::new(repository.join("user/ports/completions.sh")).arg(&tree).status()?;
    if !recorded.success() {
        return Err(format!("user/ports/completions.sh failed ({recorded})").into());
    }
    // 4 KiB blocks, which the tests count in: for a disk this small, some
    // mke2fs.conf say 1 KiB. Inode tables zeroed now, as the images' are.
    let output = e2fsprogs("mke2fs")
        .args(["-q", "-F", "-t", "ext4", "-b", "4096", "-E", "lazy_itable_init=0", "-d"])
        .arg(&tree)
        .arg(image)
        .arg(setup.disk)
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

/// What tests/processes.rs runs that's made from user/probe, by name in
/// /bin: `big`, with a segment of `BIG_LEN` bytes more, each its offset in
/// it modulo 251, at `BIG_AT`, which its mode `peek` checks; and copies that
/// name an interpreter (PT_INTERP): probe, which then runs as theirs, by a
/// path from the root and from where they're run; one that isn't there; a
/// script too short to hold an ELF header, and one long enough that isn't
/// ELF, `long-script`; a file that can't be run; and a path without its
/// NUL.
fn crafted(probe: &[u8]) -> Vec<(&'static str, Vec<u8>)> {
    const PT_LOAD: u32 = 1;
    const PT_INTERP: u32 = 3;
    let big: Vec<u8> = (0..BIG_LEN).map(|i| (i % 251) as u8).collect();
    let naming =
        |path: &str| with_header(probe, PT_INTERP, &[path.as_bytes(), b"\0"].concat(), 0, 1);
    let long_script =
        "#!/bin/sh\n# Long enough to hold an ELF header, which it isn't.\necho a script\n";
    vec![
        ("big", with_header(probe, PT_LOAD, &big, BIG_AT, 4096)),
        ("interp-probe", naming("/bin/probe")),
        ("interp-relative", naming("probe")),
        ("interp-missing", naming("/lib/missing.so")),
        ("interp-script", naming("/bin/script")),
        ("interp-long-script", naming("/bin/long-script")),
        ("interp-text", naming("/hello.txt")),
        ("interp-unterminated", with_header(probe, PT_INTERP, b"/bin/probe", 0, 1)),
        ("long-script", long_script.as_bytes().to_vec()),
    ]
}

/// user/probe with its PT_GNU_STACK header, which simplos doesn't look at,
/// made one of `kind`, readable, for `contents`, put past the end of the
/// file, aligned to `align`, and in memory at `vaddr`.
fn with_header(probe: &[u8], kind: u32, contents: &[u8], vaddr: u64, align: usize) -> Vec<u8> {
    const PT_GNU_STACK: u32 = 0x6474_e551;
    const PF_R: u32 = 4;
    let at = |at: usize, len: usize| {
        u64::from_le_bytes(probe[at..at + 8].try_into().unwrap()) as usize & len
    };
    let (phoff, phnum) = (at(32, usize::MAX), at(56, 0xffff));
    let header = (0..phnum)
        .map(|i| phoff + i * 56)
        .find(|&header| at(header, 0xffff_ffff) as u32 == PT_GNU_STACK)
        .expect("user/probe has a PT_GNU_STACK header");
    let offset = probe.len().next_multiple_of(align);
    let mut program = probe.to_vec();
    program[header..header + 4].copy_from_slice(&kind.to_le_bytes());
    program[header + 4..header + 8].copy_from_slice(&PF_R.to_le_bytes());
    // Where in the file, where in memory twice, how long in each, and its
    // alignment.
    let len = contents.len() as u64;
    let fields = [offset as u64, vaddr, vaddr, len, len, align as u64];
    for (i, field) in fields.into_iter().enumerate() {
        program[header + 8 + i * 8..header + 16 + i * 8].copy_from_slice(&field.to_le_bytes());
    }
    program.resize(offset, 0);
    program.extend_from_slice(contents);
    program
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
