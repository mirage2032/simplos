//! The command line the kernel boots into: line editing, the commands, and the
//! lines that are on screen. Drawing them is `main.rs`'s job.

use crate::io::interrupts::idt::TIMER_COUNTER;
use crate::io::interrupts::pit::TIMER_HZ;
use crate::io::{disk, hpet};
use crate::{fs, process};
use alloc::format;
use alloc::string::{String, ToString};
use alloc::sync::Arc;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::Ordering;
use spin::{Lazy, Mutex};

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
                on the ext4 disk: cat PATH.., ls PATH, stat PATH,
                write PATH TEXT, append PATH TEXT";

/// What a key changed, so the caller repaints no more than it has to. Ordered
/// by how much, so several keys combine with `max`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Redraw {
    None,
    /// Only the input line.
    Line,
    /// Everything: history changed or scrolled.
    All,
}

pub struct Shell {
    /// What is being typed.
    pub line: String,
    /// Lines above the input line, oldest first, prompt lines included.
    pub history: Vec<String>,
    /// Text rows on screen; history keeps one fewer, the last is the input line.
    rows: usize,
    /// Text columns on screen, which caps the input line.
    cols: usize,
}

impl Shell {
    pub fn new(rows: usize, cols: usize) -> Shell {
        let mut shell = Shell { line: String::new(), history: Vec::new(), rows, cols };
        shell.print("simplos shell. Type 'help' for commands.");
        shell
    }

    /// Feed one typed character. Enter runs the line, backspace deletes.
    pub fn key(&mut self, c: char) -> Redraw {
        match c {
            '\n' => {
                let line = core::mem::take(&mut self.line);
                self.history.push(format!("{PROMPT}{line}"));
                let output = self.run(&line);
                self.print(&output);
                Redraw::All
            }
            '\u{8}' => match self.line.pop() {
                Some(_) => Redraw::Line,
                None => Redraw::None,
            },
            c if !c.is_control() && PROMPT.len() + self.line.len() + 1 < self.cols => {
                self.line.push(c);
                Redraw::Line
            }
            _ => Redraw::None,
        }
    }

    /// Append output, one history entry per line, dropping what scrolls off.
    fn print(&mut self, text: &str) {
        self.history.extend(text.lines().map(String::from));
        let keep = self.rows.saturating_sub(1);
        if self.history.len() > keep {
            self.history.drain(..self.history.len() - keep);
        }
    }

    fn run(&mut self, line: &str) -> String {
        let line = line.trim();
        let (cmd, args) = line.split_once(' ').unwrap_or((line, ""));
        match cmd {
            "" => String::new(),
            "help" => HELP.to_string(),
            "clear" => {
                self.history.clear();
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
            "run" => run(args),
            "info" => format!("{}\n{}\n{}\n{}\n{}", boot(), cpu(), time(), uptime(), disks()),
            _ => format!("unknown command: {cmd} (try help)"),
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

/// Run the program in the ramdisk with `args` and wait for it: what it wrote,
/// and its exit status if that isn't 0.
fn run(args: &str) -> String {
    let Some(elf) = crate::ramdisk() else { return String::from("run: no program in the ramdisk") };
    let output = Arc::new(Mutex::new(Vec::new()));
    let sink = output.clone();
    let mut argv = vec!["probe"];
    argv.extend(args.split_whitespace());
    let status = match process::spawn(elf, &argv, move |bytes| sink.lock().extend_from_slice(bytes))
    {
        Ok(process) => process.wait(),
        Err(error) => return format!("run: {error}"),
    };
    let mut text = String::from_utf8_lossy(&output.lock()).into_owned();
    if status != 0 {
        text += &format!("[exit status {status}]");
    }
    text
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
fn typed(shell: &mut Shell, text: &str) {
    text.chars().for_each(|c| {
        shell.key(c);
    });
}

#[test_case]
fn echo_prints_its_argument() {
    let mut shell = Shell::new(10, 80);
    typed(&mut shell, "echo hi\n");
    assert_eq!(shell.history[shell.history.len() - 2..], ["> echo hi", "hi"]);
    assert!(shell.line.is_empty());
}

#[test_case]
fn backspace_deletes_and_stops_at_empty() {
    let mut shell = Shell::new(10, 80);
    typed(&mut shell, "abc\u{8}");
    assert_eq!(shell.line, "ab");
    assert_eq!(shell.key('\u{8}'), Redraw::Line);
    assert_eq!(shell.key('\u{8}'), Redraw::Line);
    assert_eq!(shell.key('\u{8}'), Redraw::None);
    assert!(shell.line.is_empty());
}

#[test_case]
fn unknown_command_says_so() {
    let mut shell = Shell::new(10, 80);
    typed(&mut shell, "frob\n");
    assert_eq!(shell.history.last().unwrap(), "unknown command: frob (try help)");
}

#[test_case]
fn clear_empties_and_history_scrolls() {
    let mut shell = Shell::new(4, 80);
    typed(&mut shell, "help\n");
    assert_eq!(shell.history.len(), 3, "history keeps rows - 1 lines");
    typed(&mut shell, "clear\n");
    assert!(shell.history.is_empty());
}

#[test_case]
fn input_stops_at_screen_width() {
    let mut shell = Shell::new(4, 6);
    typed(&mut shell, "abcdef");
    assert_eq!(shell.line, "abc", "prompt + line + cursor fit in 6 columns");
}

#[test_case]
fn run_starts_the_program_in_the_ramdisk() {
    let mut shell = Shell::new(10, 80);
    typed(&mut shell, "run hello\n");
    assert_eq!(shell.history.last().unwrap(), "hello from user space");
    typed(&mut shell, "run fault\n");
    assert_eq!(shell.history.last().unwrap(), "[exit status 139]");
    // Which runs itself again, from the ext4 disk tools/boot-test attaches.
    typed(&mut shell, "run spawn /bin/probe hello\n");
    assert_eq!(
        shell.history[shell.history.len() - 2..],
        ["hello from user space", "exit status: 0"]
    );
}

#[test_case]
fn ls_and_cat_show_the_ext4_disk() {
    // The disk tools/boot-test attaches: hello.txt and dir/nested/deep.txt.
    let mut shell = Shell::new(10, 80);
    typed(&mut shell, "ls\n");
    assert_eq!(
        shell.history[shell.history.len() - 5..],
        ["bin/", "dev/", "dir/", "hello.txt", "lost+found/"]
    );
    typed(&mut shell, "ls /dev\n");
    assert_eq!(shell.history[shell.history.len() - 3..], ["null", "tty", "zero"]);
    typed(&mut shell, "cat /dev/null\n");
    assert_eq!(shell.history.last().unwrap(), "cat: /dev/null: Unsupported");
    typed(&mut shell, "ls dir/nested\n");
    assert_eq!(shell.history.last().unwrap(), "deep.txt");
    typed(&mut shell, "cat hello.txt\n");
    assert_eq!(shell.history.last().unwrap(), "hello from ext4");
    typed(&mut shell, "cat dir\n");
    assert_eq!(shell.history.last().unwrap(), "cat: dir: IsADirectory");
    typed(&mut shell, "ls missing\n");
    assert_eq!(shell.history.last().unwrap(), "ls: missing: NotFound");
}
