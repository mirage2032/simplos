//! Terminals: what a program reads its keyboard from and writes its screen
//! through, with the line discipline Linux's have between them.
//!
//! What's typed goes through [`Tty::input`]: in canonical mode it's gathered
//! into lines, which can be edited until Enter hands them over (erase, kill,
//! word erase, end of file); otherwise each byte is ready at once. Either
//! way it's echoed, if asked. What's written, by programs and by the echo,
//! goes through [`Tty::write`] to the terminal's output: the screen, for the
//! console. A program sets all of this with the `termios` it reads and writes
//! with `ioctl`, laid out and defaulting as Linux's.
//!
//! A terminal is its session's, and one of the session's process groups is
//! in its foreground: ^C, ^\ and ^Z typed send it SIGINT, SIGQUIT and
//! SIGTSTP.

pub mod console;
pub mod terminal;

use crate::fs::pipe::ReadError;
use crate::process::signal::{self, Info};
use crate::thread::{self, WaitQueue};
use alloc::boxed::Box;
use alloc::collections::VecDeque;
use alloc::vec;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::Mutex;
use x86_64::instructions::interrupts;

/// Where a terminal's output goes.
pub type Output = Box<dyn FnMut(&[u8]) + Send>;

// The `termios` flags the line discipline acts on.
const ICRNL: u32 = 0o400;
const IGNCR: u32 = 0o200;
const INLCR: u32 = 0o100;
const IXON: u32 = 0o2000;
const OPOST: u32 = 0o1;
const ONLCR: u32 = 0o4;
const ISIG: u32 = 0o1;
const NOFLSH: u32 = 0o200;
const ICANON: u32 = 0o2;
const ECHO: u32 = 0o10;
const ECHOE: u32 = 0o20;
const ECHOK: u32 = 0o40;
const ECHONL: u32 = 0o100;
const ECHOCTL: u32 = 0o1000;
const ECHOKE: u32 = 0o4000;
const IEXTEN: u32 = 0o100000;

// Where each control character is in `cc`.
const VINTR: usize = 0;
const VQUIT: usize = 1;
const VERASE: usize = 2;
const VKILL: usize = 3;
const VEOF: usize = 4;
const VTIME: usize = 5;
const VMIN: usize = 6;
const VSUSP: usize = 10;
const VEOL: usize = 11;
const VWERASE: usize = 14;
const VEOL2: usize = 16;

/// The most a line can hold, as on Linux: the rest of what's typed into it
/// is dropped, but for the end of the line.
const MOST_LINE: usize = 4095;
/// The most typed ahead before more is dropped.
const MOST_READY: usize = 64 * 1024;
/// And in canonical mode, the most lines, each kept apart; an end of file
/// is an empty one.
const MOST_LINES: usize = 4096;

/// Linux's `struct termios`, as `TCGETS` and `TCSETS` pass it.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Termios {
    pub iflag: u32,
    pub oflag: u32,
    pub cflag: u32,
    pub lflag: u32,
    pub line: u8,
    pub cc: [u8; 19],
}

impl Termios {
    /// Its size, in a program's memory.
    pub const SIZE: usize = 36;

    /// As a terminal starts on Linux: canonical, echoing, with ^C, ^\,
    /// DEL, ^U, ^D, ^Z and ^W, at 38400 baud, 8 bits.
    pub const DEFAULT: Termios = Termios {
        iflag: ICRNL | IXON,
        oflag: OPOST | ONLCR,
        cflag: 0o17 | 0o60 | 0o200 | 0o2000,
        lflag: ISIG | ICANON | ECHO | ECHOE | ECHOK | ECHOCTL | ECHOKE | IEXTEN,
        line: 0,
        cc: [3, 0x1c, 0x7f, 0x15, 4, 0, 1, 0, 0x11, 0x13, 0x1a, 0, 0x12, 0xf, 0x17, 0x16, 0, 0, 0],
    };

    pub fn to_bytes(self) -> [u8; Termios::SIZE] {
        let mut bytes = [0; Termios::SIZE];
        for (at, flag) in [self.iflag, self.oflag, self.cflag, self.lflag].into_iter().enumerate() {
            bytes[at * 4..at * 4 + 4].copy_from_slice(&flag.to_le_bytes());
        }
        bytes[16] = self.line;
        bytes[17..].copy_from_slice(&self.cc);
        bytes
    }

    pub fn from_bytes(bytes: &[u8; Termios::SIZE]) -> Termios {
        let flag = |at: usize| u32::from_le_bytes(bytes[at * 4..at * 4 + 4].try_into().unwrap());
        Termios {
            iflag: flag(0),
            oflag: flag(1),
            cflag: flag(2),
            lflag: flag(3),
            line: bytes[16],
            cc: bytes[17..].try_into().unwrap(),
        }
    }
}

pub struct Tty {
    /// Taken only with interrupts off, as `read` and `poll` check it from
    /// `WaitQueue::wait_until`.
    state: Mutex<State>,
    /// Woken as input becomes ready to read.
    input_ready: WaitQueue,
    /// Taken only with interrupts on: writing to a screen takes a while.
    output: Mutex<Output>,
    /// The session it's the terminal of, and the process group in its
    /// foreground: 0 for none.
    session: AtomicU64,
    foreground: AtomicU64,
}

struct State {
    termios: Termios,
    /// Rows and columns, as `TIOCGWINSZ` tells them.
    size: (u16, u16),
    /// What `read` hands out, in pieces: in canonical mode a line each, or
    /// what came before an end of file, which may be nothing; otherwise as
    /// it was typed.
    ready: VecDeque<Vec<u8>>,
    /// The line being typed, in canonical mode.
    line: Vec<u8>,
    /// Nothing more will be typed: once `ready` is empty, reads find the end.
    closed: bool,
}

impl Tty {
    /// A terminal of `rows` and `cols`, as Linux starts one, writing to
    /// `output`.
    pub fn new(output: Output, rows: u16, cols: u16) -> Tty {
        Tty::with(output, Termios::DEFAULT, (rows, cols), false)
    }

    /// A terminal no one types on, whose output is kept as it's written, for
    /// the programs the kernel runs to see what they print: reading it finds
    /// the end, and newlines go out as they are.
    pub fn capture(output: Output) -> Tty {
        let termios = Termios { oflag: Termios::DEFAULT.oflag & !OPOST, ..Termios::DEFAULT };
        Tty::with(output, termios, (24, 80), true)
    }

    fn with(output: Output, termios: Termios, size: (u16, u16), closed: bool) -> Tty {
        let state = State { termios, size, ready: VecDeque::new(), line: Vec::new(), closed };
        Tty {
            state: Mutex::new(state),
            input_ready: WaitQueue::new(),
            output: Mutex::new(output),
            session: AtomicU64::new(0),
            foreground: AtomicU64::new(0),
        }
    }

    fn state<R>(&self, f: impl FnOnce(&mut State) -> R) -> R {
        interrupts::without_interrupts(|| f(&mut self.state.lock()))
    }

    /// What was typed: through the line discipline, then echoed; and the
    /// signals it typed sent to the foreground.
    pub fn input(&self, bytes: &[u8]) {
        let (mut echo, mut signals) = (Vec::new(), Vec::new());
        self.state(|state| {
            bytes.iter().for_each(|&byte| state.receive(byte, &mut echo, &mut signals));
        });
        self.input_ready.wake_all();
        thread::wake_pollers();
        if !echo.is_empty() {
            self.write(&echo);
        }
        let foreground = self.foreground();
        for signal in signals.into_iter().filter(|_| foreground != 0) {
            crate::process::signal_group(foreground, signal, Info::kernel());
        }
    }

    /// Up to `most` bytes of what's been typed, waiting until there are
    /// some: in canonical mode no more than a line; none at the end of file.
    /// For the kernel, whose threads have no signals to end it early.
    pub fn read(&self, most: usize) -> Vec<u8> {
        self.read_or_wait(most, true).expect("it waited")
    }

    /// `read`, but if `wait` is false and there's nothing to read yet, or a
    /// signal comes first, nothing.
    ///
    /// shortcut: not waiting, a read in raw mode takes nothing until there
    /// are VMIN bytes, where Linux's takes what's there.
    pub fn read_or_wait(&self, most: usize, wait: bool) -> Result<Vec<u8>, ReadError> {
        let mut taken = Vec::new();
        let mut done = true;
        if most > 0 {
            let waited = self.input_ready.wait_until_interruptible(|| {
                done = self.state.lock().take(most, &mut taken);
                done || !wait
            });
            waited.map_err(|_| ReadError::Interrupted)?;
        }
        if done { Ok(taken) } else { Err(ReadError::Empty) }
    }

    /// Make it session `session`'s terminal, with group `foreground` in its
    /// foreground; 0 and 0 for no one's.
    pub fn control(&self, session: u64, foreground: u64) {
        self.session.store(session, Ordering::Relaxed);
        self.foreground.store(foreground, Ordering::Relaxed);
    }

    pub fn session(&self) -> u64 {
        self.session.load(Ordering::Relaxed)
    }

    pub fn foreground(&self) -> u64 {
        self.foreground.load(Ordering::Relaxed)
    }

    pub fn set_foreground(&self, group: u64) {
        self.foreground.store(group, Ordering::Relaxed);
    }

    /// Whether a `read` would find something, or the end, without waiting,
    /// as `poll` asks.
    pub fn readable(&self) -> bool {
        self.state(|state| match state.termios.lflag & ICANON {
            0 => {
                let (min, time) = (state.termios.cc[VMIN], state.termios.cc[VTIME]);
                let wanted = if time == 0 && min > 0 { usize::from(min) } else { 1 };
                state.available() >= wanted || state.closed
            }
            _ => !state.ready.is_empty() || state.closed,
        })
    }

    /// How many bytes a `read` could take now, for `FIONREAD`.
    pub fn available(&self) -> usize {
        self.state(|state| state.available())
    }

    /// Write `data` out, with newlines as `\r\n` if the settings say so.
    pub fn write(&self, data: &[u8]) {
        let oflag = self.state(|state| state.termios.oflag);
        let mut output = self.output.lock();
        if oflag & (OPOST | ONLCR) == OPOST | ONLCR && data.contains(&b'\n') {
            let mut translated = Vec::with_capacity(data.len() * 2);
            for &byte in data {
                if byte == b'\n' {
                    translated.push(b'\r');
                }
                translated.push(byte);
            }
            output(&translated);
        } else {
            output(data);
        }
    }

    pub fn termios(&self) -> Termios {
        self.state(|state| state.termios)
    }

    /// Change the settings, throwing away what's been typed and not read if
    /// `flush`. A line being typed is handed over as it is once canonical
    /// mode ends, as Linux does.
    pub fn set_termios(&self, termios: Termios, flush: bool) {
        self.state(|state| {
            if flush {
                state.flush();
            }
            if termios.lflag & ICANON == 0 && !state.line.is_empty() {
                let line = core::mem::take(&mut state.line);
                state.ready.push_back(line);
            }
            state.termios = termios;
        });
        self.input_ready.wake_all();
        thread::wake_pollers();
    }

    /// Throw away what's been typed and not read.
    pub fn flush_input(&self) {
        self.state(State::flush);
    }

    pub fn size(&self) -> (u16, u16) {
        self.state(|state| state.size)
    }

    pub fn set_size(&self, rows: u16, cols: u16) {
        self.state(|state| state.size = (rows, cols));
    }
}

impl State {
    fn available(&self) -> usize {
        self.ready.iter().map(Vec::len).sum()
    }

    fn flush(&mut self) {
        self.ready.clear();
        self.line.clear();
    }

    /// A line to be read, unless as much as is kept is waiting already: a
    /// program that never reads could fill the heap, with the terminal's
    /// own answers to it.
    fn finish(&mut self, line: Vec<u8>) {
        if self.ready.len() < MOST_LINES && self.available() < MOST_READY {
            self.ready.push_back(line);
        }
    }

    /// Take up to `most` bytes into `taken`, if there's enough to read:
    /// whether the read is done.
    fn take(&mut self, most: usize, taken: &mut Vec<u8>) -> bool {
        if self.termios.lflag & ICANON != 0 {
            // A line, or what of it is left; or an empty piece, the end of file.
            return match self.ready.pop_front() {
                Some(mut piece) => {
                    if piece.len() > most {
                        let rest = piece.split_off(most);
                        self.ready.push_front(rest);
                    }
                    *taken = piece;
                    true
                }
                None => self.closed,
            };
        }
        // shortcut: VTIME's timer isn't kept: with VMIN, a read waits for
        // that many bytes however long they take, and without, it doesn't
        // wait at all.
        let min = usize::from(self.termios.cc[VMIN]).min(most);
        if self.available() < min && !self.closed {
            return false;
        }
        while taken.len() < most {
            let Some(mut piece) = self.ready.pop_front() else { break };
            let room = most - taken.len();
            if piece.len() > room {
                self.ready.push_front(piece.split_off(room));
            }
            taken.append(&mut piece);
        }
        true
    }

    /// One byte typed: into the line, or ready at once; what to echo, and
    /// the signal it is, if it's one.
    fn receive(&mut self, mut byte: u8, echo: &mut Vec<u8>, signals: &mut Vec<usize>) {
        let Termios { iflag, lflag, cc, .. } = self.termios;
        let echoing = lflag & ECHO != 0;
        match byte {
            b'\r' if iflag & IGNCR != 0 => return,
            b'\r' if iflag & ICRNL != 0 => byte = b'\n',
            b'\n' if iflag & INLCR != 0 => byte = b'\r',
            _ => {}
        }
        let is = |index: usize| cc[index] != 0 && byte == cc[index];
        if lflag & ISIG != 0 && (is(VINTR) || is(VQUIT) || is(VSUSP)) {
            signals.push(match () {
                _ if is(VINTR) => signal::SIGINT,
                _ if is(VQUIT) => signal::SIGQUIT,
                _ => signal::SIGTSTP,
            });
            // What's typed and not read goes, as the program it was for is
            // interrupted.
            if lflag & NOFLSH == 0 {
                self.flush();
            }
            if echoing {
                self.echo(byte, echo);
            }
            return;
        }
        if lflag & ICANON == 0 {
            if self.available() < MOST_READY {
                // Onto what's ready, which a read takes across pieces: a
                // piece each, counting them for each byte took long.
                match self.ready.back_mut() {
                    Some(last) if !last.is_empty() => last.push(byte),
                    _ => self.ready.push_back(vec![byte]),
                }
            }
            if echoing {
                self.echo(byte, echo);
            }
            return;
        }
        if is(VERASE) {
            self.erase(1, echo);
        } else if is(VWERASE) && lflag & IEXTEN != 0 {
            let spaces = self.line.iter().rev().take_while(|byte| byte.is_ascii_whitespace());
            let spaces = spaces.count();
            let word = self.line[..self.line.len() - spaces].iter().rev();
            let word = word.take_while(|byte| !byte.is_ascii_whitespace()).count();
            self.erase(spaces + word, echo);
        } else if is(VKILL) {
            if lflag & ECHOKE != 0 && lflag & ECHOE != 0 {
                self.erase(self.line.len(), echo);
            } else {
                self.line.clear();
                if echoing {
                    self.echo(byte, echo);
                    if lflag & ECHOK != 0 {
                        echo.push(b'\n');
                    }
                }
            }
        } else if is(VEOF) {
            // Not echoed, and not part of what's read.
            let line = core::mem::take(&mut self.line);
            self.finish(line);
        } else if byte == b'\n' || is(VEOL) || is(VEOL2) {
            self.line.push(byte);
            let line = core::mem::take(&mut self.line);
            self.finish(line);
            if echoing || (byte == b'\n' && lflag & ECHONL != 0) {
                echo.push(byte);
            }
        } else if self.line.len() < MOST_LINE {
            self.line.push(byte);
            if echoing {
                self.echo(byte, echo);
            }
        }
    }

    /// Echo `byte`: a control character as `^` and a letter, if asked.
    fn echo(&self, byte: u8, echo: &mut Vec<u8>) {
        if self.termios.lflag & ECHOCTL != 0 && is_caret(byte) {
            echo.extend([b'^', byte ^ 0x40]);
        } else {
            echo.push(byte);
        }
    }

    /// Take the last `count` bytes off the line, and off the screen too if
    /// the settings say so.
    fn erase(&mut self, count: usize, echo: &mut Vec<u8>) {
        let lflag = self.termios.lflag;
        for _ in 0..count {
            let Some(byte) = self.line.pop() else { return };
            if lflag & ECHO == 0 {
                continue;
            }
            if lflag & ECHOE == 0 {
                self.echo(self.termios.cc[VERASE], echo);
                continue;
            }
            // As wide as it was echoed.
            let width = if lflag & ECHOCTL != 0 && is_caret(byte) { 2 } else { 1 };
            for _ in 0..width {
                echo.extend(b"\x08 \x08");
            }
        }
    }
}

/// Whether ECHOCTL echoes `byte` as `^` and a letter: control characters
/// but for tab and newline, and DEL.
fn is_caret(byte: u8) -> bool {
    (byte < 0x20 && byte != b'\t' && byte != b'\n') || byte == 0x7f
}

#[cfg(test)]
fn typed(text: &[u8]) -> (Tty, alloc::sync::Arc<Mutex<Vec<u8>>>) {
    let screen = alloc::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = screen.clone();
    let tty = Tty::new(Box::new(move |bytes| sink.lock().extend_from_slice(bytes)), 24, 80);
    tty.input(text);
    (tty, screen)
}

#[test_case]
fn canonical_mode_hands_over_whole_lines() {
    let (tty, screen) = typed(b"ab\x7fc\rhalf");
    assert!(tty.readable());
    assert_eq!(tty.read(100), b"ac\n");
    // Not yet a line.
    assert!(!tty.readable());
    // Taken a few at a time, then the end of file once the line is gone.
    tty.input(b"\x04\x04");
    assert_eq!(tty.read(2), b"ha");
    assert_eq!(tty.read(100), b"lf");
    assert_eq!(tty.read(100), b"");
    assert_eq!(*screen.lock(), b"ab\x08 \x08c\r\nhalf");
}

#[test_case]
fn control_characters_echo_as_carets_and_erase_as_wide() {
    let (tty, screen) = typed(b"x\x01\x7f\x03y\r");
    // ^C threw away the line before it.
    assert_eq!(tty.read(100), b"y\n");
    assert_eq!(*screen.lock(), b"x^A\x08 \x08\x08 \x08^Cy\r\n");
}

#[test_case]
fn raw_mode_hands_over_bytes_as_they_come() {
    let (tty, screen) = typed(b"");
    let lflag = Termios::DEFAULT.lflag & !(ICANON | ECHO | ISIG);
    tty.set_termios(Termios { iflag: 0, lflag, ..Termios::DEFAULT }, false);
    tty.input(b"\r\x03\x7f");
    assert_eq!(tty.available(), 3);
    assert_eq!(tty.read(2), b"\r\x03");
    assert_eq!(tty.read(2), b"\x7f");
    assert!(screen.lock().is_empty());
    // Kept to a limit, as a program that never reads asks no more of.
    tty.input(&vec![b'x'; MOST_READY + 10]);
    assert_eq!(tty.available(), MOST_READY);
}

#[test_case]
fn lines_no_one_reads_are_kept_to_a_limit() {
    let (tty, _) = typed(&[0x04; MOST_LINES + 10]);
    let mut lines = 0;
    while tty.readable() {
        tty.read(1);
        lines += 1;
    }
    assert_eq!(lines, MOST_LINES);
}

#[test_case]
fn a_capture_terminal_reads_as_ended_and_writes_as_given() {
    let screen = alloc::sync::Arc::new(Mutex::new(Vec::new()));
    let sink = screen.clone();
    let tty = Tty::capture(Box::new(move |bytes| sink.lock().extend_from_slice(bytes)));
    assert!(tty.readable());
    assert_eq!(tty.read(10), b"");
    tty.write(b"a\nb");
    assert_eq!(*screen.lock(), b"a\nb");
}

#[test_case]
fn termios_goes_to_and_from_linux_s_layout() {
    let bytes = Termios::DEFAULT.to_bytes();
    assert_eq!(bytes[..4], (ICRNL | IXON).to_le_bytes());
    // VERASE, then VEOF.
    assert_eq!((bytes[17 + VERASE], bytes[17 + VEOF]), (0x7f, 4));
    assert_eq!(Termios::from_bytes(&bytes), Termios::DEFAULT);
}
