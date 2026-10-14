//! The command line the kernel boots into. It reads lines from a terminal,
//! the console's, whose line discipline has done the editing, and writes
//! what the commands say back to it.

use crate::io::interrupts::idt::TIMER_COUNTER;
use crate::io::interrupts::pit::TIMER_HZ;
use crate::io::{disk, hpet};
use crate::tty::Tty;
use crate::{fs, init, process, syscall};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::Lazy;

pub const PROMPT: &str = "> ";

const HELP: &str = "\
commands:
  help          this list
  clear         clear the screen
  echo TEXT     print TEXT
  boot          firmware we booted from
  cpu           processor brand and vendor
  time          RTC time (unix seconds)
  uptime        timer ticks and HPET time since boot
  disks         disks found at boot
  ls [PATH]     list a directory: the ext4 disk's, or /dev
  cat PATH      print a file on the ext4 disk
  info          all of the above
  run [MODE]    run the user program in the ramdisk: hello,
                sleep [MS], float, bad-calls, fault or exit;
                cat with no paths copies what's typed, until ^D;
                on the ext4 disk: cat PATH.., ls PATH, stat PATH,
                write PATH TEXT, append PATH TEXT
  run /PATH [ARGS]
                run the program at PATH on the ext4 disk";

pub struct Shell {
    tty: Arc<Tty>,
}

impl Shell {
    pub fn new(tty: Arc<Tty>) -> Shell {
        Shell { tty }
    }

    /// Prompt, read a line and run it, forever.
    pub fn run(&mut self) -> ! {
        self.tty.write(b"simplos shell. Type 'help' for commands.\n");
        loop {
            self.tty.write(PROMPT.as_bytes());
            let line = self.tty.read(4096);
            // ^D at the prompt: there's nothing to end, so just a new line.
            if line.is_empty() {
                self.tty.write(b"\n");
                continue;
            }
            let output = self.execute(&String::from_utf8_lossy(&line));
            self.tty.write(output.as_bytes());
            if !output.is_empty() && !output.ends_with('\n') {
                self.tty.write(b"\n");
            }
        }
    }

    /// Run one line: what it says. A program it runs writes to the terminal
    /// itself, as it goes.
    pub fn execute(&mut self, line: &str) -> String {
        let line = line.trim();
        let (cmd, args) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "" => String::new(),
            "help" => HELP.to_string(),
            "clear" => {
                // The whole screen, and the cursor to the top.
                self.tty.write(b"\x1b[2J\x1b[H");
                String::new()
            }
            "echo" => args.trim().to_string(),
            "boot" => boot(),
            "cpu" => cpu(),
            "time" => time(),
            "uptime" => uptime(),
            "disks" => disks(),
            "ls" => ls(args.trim()),
            "cat" => cat(args.trim()),
            "run" => self.run_program(args),
            "info" => format!("{}\n{}\n{}\n{}\n{}", boot(), cpu(), time(), uptime(), disks()),
            _ => format!("unknown command: {cmd} (try help)"),
        }
    }

    /// Run the program on the disk at the path that starts `args`, or else
    /// the one in the ramdisk, with the rest of `args`, on the terminal; and
    /// wait for it: its exit status, if that isn't 0.
    fn run_program(&mut self, args: &str) -> String {
        let mut argv: Vec<&str> = args.split_whitespace().collect();
        let files = vec![Some(Arc::new(fs::Handle::tty(self.tty.clone()))); 3];
        let env = &init::ENVIRONMENT;
        let started = match argv.first() {
            Some(path) if path.starts_with('/') => process::spawn_program(path, &argv, env, files)
                .map_err(|errno| format!("run: {path}: {}", syscall::strerror(errno))),
            _ => {
                let Some(elf) = crate::ramdisk() else {
                    return String::from("run: no program in the ramdisk");
                };
                argv.insert(0, "probe");
                process::spawn_with(elf, &argv, env, files).map_err(|error| format!("run: {error}"))
            }
        };
        let settings = self.tty.termios();
        let status = match started {
            Ok(process) => process.wait(),
            Err(message) => return message,
        };
        // As the program found it, whatever it changed.
        self.tty.set_termios(settings, false);
        match status {
            0 => String::new(),
            status => format!("[exit status {status}]"),
        }
    }
}

fn boot() -> String {
    format!("Boot: {}", crate::boot_mode().as_str())
}

// Read CPUID once. The answer cannot change, and the brand string lives in
// extended leaves (0x8000_0002-4) that not every emulator implements.
static CPU: Lazy<(String, String)> = Lazy::new(|| {
    let cpu_id = x86::cpuid::CpuId::new();
    let brand = cpu_id
        .get_processor_brand_string()
        .map_or_else(|| String::from("unknown"), |brand| brand.as_str().trim().to_string());
    let vendor = cpu_id
        .get_vendor_info()
        .map_or_else(|| String::from("unknown"), |vendor| vendor.as_str().to_string());
    (brand, vendor)
});

fn cpu() -> String {
    format!("CPU: {}\nVendor: {}", CPU.0, CPU.1)
}

fn time() -> String {
    format!("RTC: {}", x86_rtc::Rtc::new().get_unix_timestamp())
}

fn uptime() -> String {
    let ticks = TIMER_COUNTER.load(Ordering::Relaxed);
    let tick_ms = ticks as u64 * 1000 / TIMER_HZ as u64;
    let timer =
        format!("Timer: {ticks} ticks, {}.{:03}s at {TIMER_HZ} Hz", tick_ms / 1000, tick_ms % 1000);
    match hpet::elapsed_millis() {
        Some(ms) => format!("{timer}\nHPET: {}.{:03}s", ms / 1000, ms % 1000),
        None => format!("{timer}\nHPET: unavailable"),
    }
}

fn disks() -> String {
    let disks = disk::list();
    let mut out = format!("Disks: {}", disks.len());
    for (i, d) in disks.iter().enumerate() {
        out += &format!("\n  {i}: {} \"{}\" {} sectors", d.kind, d.model, d.sectors);
    }
    out
}

/// The entries of the directory at `path`, sorted, with a `/` after each
/// directory's name.
fn ls(path: &str) -> String {
    match fs::lock().open_dir(&fs::resolve("", path)) {
        Ok((_, mut entries)) => {
            entries.sort_by(|a, b| a.name.cmp(&b.name));
            let names = entries.into_iter().map(|entry| match entry.kind {
                ext4::Kind::Directory => entry.name + "/",
                _ => entry.name,
            });
            names.collect::<Vec<_>>().join("\n")
        }
        Err(error) => format!("ls: {path}: {error:?}"),
    }
}

/// The file at `path`, as text: a file on the disk, as there's nothing to
/// read in the devices.
fn cat(path: &str) -> String {
    let tree = fs::lock();
    let at = fs::resolve("", path);
    let file = match tree.metadata(&at).map(|metadata| metadata.kind) {
        Ok(ext4::Kind::File) => tree.disk(&at).and_then(|disk| disk.open(&at)),
        Ok(ext4::Kind::Directory) => Err(ext4::Error::IsADirectory),
        Ok(_) => Err(ext4::Error::Unsupported),
        Err(error) => Err(error),
    };
    let (file, size) = match file.and_then(|file| Ok((file.size()?, file))) {
        Ok((size, file)) => (file, size),
        Err(error) => return format!("cat: {path}: {error:?}"),
    };
    let mut contents = vec![0; size as usize];
    let mut done = 0;
    while done < contents.len() {
        match file.read_at(done as u64, &mut contents[done..]) {
            Ok(0) => break,
            Ok(read) => done += read,
            Err(error) => return format!("cat: {path}: {error:?}"),
        }
    }
    String::from_utf8_lossy(&contents[..done]).into_owned()
}

#[cfg(test)]
fn shell() -> (Shell, Arc<spin::Mutex<Vec<u8>>>) {
    let screen = Arc::new(spin::Mutex::new(Vec::new()));
    let sink = screen.clone();
    let tty =
        Tty::capture(alloc::boxed::Box::new(move |bytes| sink.lock().extend_from_slice(bytes)));
    (Shell::new(Arc::new(tty)), screen)
}

#[test_case]
fn echo_prints_its_argument() {
    assert_eq!(shell().0.execute("echo  hi \n"), "hi");
}

#[test_case]
fn unknown_command_says_so() {
    assert_eq!(shell().0.execute("frob"), "unknown command: frob (try help)");
}

#[test_case]
fn run_starts_the_program_in_the_ramdisk_on_the_terminal() {
    let (mut shell, screen) = shell();
    assert_eq!(shell.execute("run hello"), "");
    assert_eq!(shell.execute("run fault"), "[exit status 139]");
    // Which runs itself again, from the ext4 disk tools/boot-test attaches;
    // or is run from there, with an environment, as a terminal's programs
    // are given.
    assert_eq!(shell.execute("run spawn /bin/probe hello"), "");
    assert_eq!(shell.execute("run /bin/probe env a"), "");
    let screen = String::from_utf8(screen.lock().clone()).unwrap();
    assert_eq!(
        screen,
        "hello from user space\nhello from user space\nexit status: 0\n\
         [\"env\", \"a\"]\nHOME=/\nTERM=linux\n"
    );
    // What isn't there, or isn't a program.
    assert_eq!(shell.execute("run /missing"), "run: /missing: No such file or directory");
    assert_eq!(shell.execute("run /hello.txt"), "run: /hello.txt: Permission denied");
}

#[test_case]
fn ls_and_cat_show_the_ext4_disk() {
    // The disk tools/boot-test attaches: hello.txt and dir/nested/deep.txt.
    let mut shell = shell().0;
    assert_eq!(shell.execute("ls"), "bin/\ndev/\ndir/\nhello.txt\nlost+found/");
    assert_eq!(shell.execute("ls /dev"), "null\ntty\nzero");
    assert_eq!(shell.execute("cat /dev/null"), "cat: /dev/null: Unsupported");
    assert_eq!(shell.execute("ls dir/nested"), "deep.txt");
    assert_eq!(shell.execute("cat hello.txt"), "hello from ext4\n");
    assert_eq!(shell.execute("cat dir"), "cat: dir: IsADirectory");
    assert_eq!(shell.execute("ls missing"), "ls: missing: NotFound");
}
