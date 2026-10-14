//! The command line the kernel boots into: line editing, the commands, and the
//! lines that are on screen. Drawing them is `main.rs`'s job.

use crate::io::interrupts::idt::TIMER_COUNTER;
use crate::io::{disk, hpet};
use alloc::format;
use alloc::string::{String, ToString};
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
  info          all of the above";

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
    match hpet::elapsed_millis() {
        Some(ms) => format!("Timer: {ticks}\nHPET: {}.{:03}s", ms / 1000, ms % 1000),
        None => format!("Timer: {ticks}\nHPET: unavailable"),
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
