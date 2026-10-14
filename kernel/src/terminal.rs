//! The screen as a terminal: a grid of characters with a cursor, which the
//! console's output moves through and fills as a VT100's would, and which
//! [`Terminal::draw`] puts on the display.
//!
//! shortcut: of the escape sequences, only moving the cursor (`ESC [ H`) and
//! erasing the screen and lines (`ESC [ J`, `ESC [ K`); the others are read
//! and ignored. The rest of a VT100 comes when programs need it.

use crate::io::video::Display;
use alloc::string::String;
use alloc::vec;
use alloc::vec::Vec;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::mono_font::ascii::FONT_7X13;
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Baseline, Text};

const BACKGROUND: Rgb888 = Rgb888::new(23, 114, 243);
const FOREGROUND: Rgb888 = Rgb888::new(94, 243, 23);
/// Around the text, in pixels.
const MARGIN: usize = 8;
/// A character cell, in pixels.
const CELL_HEIGHT: usize = 13;
const CELL_WIDTH: usize = 7;

pub struct Terminal {
    rows: usize,
    cols: usize,
    /// Row by row; non-ASCII characters are kept as `?`, which is all the
    /// font has for them.
    cells: Vec<u8>,
    row: usize,
    /// `cols` once a character has gone in the last column: the next one
    /// starts a new line, as on a VT100, but a `\r` or a `\n` first doesn't.
    col: usize,
    /// Rows changed since they were last drawn.
    dirty: Vec<bool>,
    /// Lines scrolled off since `draw` last ran.
    scrolled: usize,
    /// Where `draw` last put the cursor; none before it first ran.
    drawn_cursor: Option<(usize, usize)>,
    escape: Escape,
}

/// Where in an escape sequence the last byte left off.
enum Escape {
    None,
    /// After `ESC`.
    Started,
    /// After `ESC [`, with the parameters so far.
    Csi(Vec<u8>),
}

impl Terminal {
    /// An empty one of `rows` and `cols`, the cursor at the top left.
    pub fn new(rows: usize, cols: usize) -> Terminal {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Terminal {
            rows,
            cols,
            cells: vec![b' '; rows * cols],
            row: 0,
            col: 0,
            dirty: vec![true; rows],
            scrolled: 0,
            drawn_cursor: None,
            escape: Escape::None,
        }
    }

    /// As many rows and columns as fit on `display`.
    pub fn fitting(display: &Display) -> Terminal {
        let size = display.bounding_box().size;
        let fit = |pixels: u32, cell: usize| (pixels as usize).saturating_sub(2 * MARGIN) / cell;
        Terminal::new(fit(size.height, CELL_HEIGHT), fit(size.width, CELL_WIDTH))
    }

    pub fn size(&self) -> (usize, usize) {
        (self.rows, self.cols)
    }

    pub fn write(&mut self, bytes: &[u8]) {
        bytes.iter().for_each(|&byte| self.byte(byte));
    }

    fn byte(&mut self, byte: u8) {
        match core::mem::replace(&mut self.escape, Escape::None) {
            Escape::Started => {
                // shortcut: of the two-byte sequences, none do anything.
                if byte == b'[' {
                    self.escape = Escape::Csi(Vec::new());
                }
                return;
            }
            Escape::Csi(mut parameters) => {
                if (0x40..=0x7e).contains(&byte) {
                    self.csi(&parameters, byte);
                } else if parameters.len() < 32 {
                    parameters.push(byte);
                    self.escape = Escape::Csi(parameters);
                }
                return;
            }
            Escape::None => {}
        }
        match byte {
            0x1b => self.escape = Escape::Started,
            b'\r' => self.col = 0,
            b'\n' | 0x0b | 0x0c => self.line_feed(),
            0x08 => self.col = self.col.min(self.cols - 1).saturating_sub(1),
            b'\t' => self.col = (self.col / 8 * 8 + 8).min(self.cols - 1),
            0x20..=0x7e => self.put(byte),
            // The first byte of a UTF-8 character.
            0xc0..=0xff => self.put(b'?'),
            _ => {}
        }
    }

    fn put(&mut self, byte: u8) {
        if self.col == self.cols {
            self.col = 0;
            self.line_feed();
        }
        self.cells[self.row * self.cols + self.col] = byte;
        self.dirty[self.row] = true;
        self.col += 1;
    }

    /// Down a line, scrolling at the bottom.
    fn line_feed(&mut self) {
        if self.row + 1 < self.rows {
            self.row += 1;
            return;
        }
        self.cells.copy_within(self.cols.., 0);
        let last = (self.rows - 1) * self.cols;
        self.cells[last..].fill(b' ');
        self.dirty.remove(0);
        self.dirty.push(true);
        self.scrolled += 1;
    }

    /// `ESC [ parameters final`.
    fn csi(&mut self, parameters: &[u8], last: u8) {
        let text = core::str::from_utf8(parameters).unwrap_or("");
        let numbers: Vec<usize> = text.split(';').map(|n| n.parse().unwrap_or(0)).collect();
        // A parameter, 0 or missing meaning `default`.
        let number = |at: usize, default: usize| {
            numbers.get(at).copied().filter(|&n| n != 0).unwrap_or(default)
        };
        let cursor = self.row * self.cols + self.col.min(self.cols - 1);
        let (line, end) = (self.row * self.cols, self.cells.len());
        match last {
            b'H' | b'f' => {
                self.row = (number(0, 1) - 1).min(self.rows - 1);
                self.col = (number(1, 1) - 1).min(self.cols - 1);
            }
            b'J' => match numbers[0] {
                0 => self.erase(cursor..end),
                1 => self.erase(0..cursor + 1),
                2 | 3 => self.erase(0..end),
                _ => {}
            },
            b'K' => match numbers[0] {
                0 => self.erase(cursor..line + self.cols),
                1 => self.erase(line..cursor + 1),
                2 => self.erase(line..line + self.cols),
                _ => {}
            },
            _ => {}
        }
    }

    fn erase(&mut self, cells: core::ops::Range<usize>) {
        if cells.is_empty() {
            return;
        }
        let rows = cells.start / self.cols..=(cells.end - 1) / self.cols;
        self.cells[cells].fill(b' ');
        self.dirty[rows].fill(true);
    }

    /// Put what's changed on `display`, and show it.
    pub fn draw(&mut self, display: &mut Display) {
        if self.drawn_cursor.is_none() {
            display.clear(BACKGROUND).expect("the display clears");
        }
        if self.scrolled > 0 {
            if self.scrolled < self.rows {
                let (top, height) = (MARGIN as u32, (self.rows * CELL_HEIGHT) as u32);
                let by = (self.scrolled * CELL_HEIGHT) as u32;
                display.scroll_up(top, height, by, BACKGROUND);
            } else {
                self.dirty.fill(true);
            }
            // It went up with the rest.
            let up = |(row, col): (usize, usize)| row.checked_sub(self.scrolled).map(|r| (r, col));
            self.drawn_cursor = self.drawn_cursor.and_then(up);
            self.scrolled = 0;
        }
        let cursor = (self.row, self.col.min(self.cols - 1));
        if self.drawn_cursor != Some(cursor) {
            if let Some((row, _)) = self.drawn_cursor {
                self.dirty[row] = true;
            }
            self.dirty[cursor.0] = true;
        }
        let style = MonoTextStyle::new(&FONT_7X13, FOREGROUND);
        let width = display.bounding_box().size.width;
        for row in (0..self.rows).filter(|&row| self.dirty[row]) {
            let y = (MARGIN + row * CELL_HEIGHT) as i32;
            let line = Rectangle::new(Point::new(0, y), Size::new(width, CELL_HEIGHT as u32));
            display.fill_rect(&line, BACKGROUND);
            let cells = &self.cells[row * self.cols..(row + 1) * self.cols];
            let text = String::from_utf8_lossy(cells);
            let mut draw = |text: &str, col: usize| {
                let at = Point::new((MARGIN + col * CELL_WIDTH) as i32, y);
                Text::with_baseline(text, at, style, Baseline::Top).draw(display).ok();
            };
            draw(text.trim_end(), 0);
            if row == cursor.0 {
                draw("_", cursor.1);
            }
        }
        self.dirty.fill(false);
        self.drawn_cursor = Some(cursor);
        display.present();
    }

    /// What's on row `row`, without the spaces at its end.
    #[cfg(test)]
    fn line(&self, row: usize) -> String {
        let cells = &self.cells[row * self.cols..(row + 1) * self.cols];
        String::from_utf8_lossy(cells).trim_end().into()
    }
}

#[test_case]
fn text_goes_where_the_cursor_is() {
    let mut terminal = Terminal::new(3, 10);
    terminal.write(b"one\r\ntwo\tx\rT");
    assert_eq!([terminal.line(0), terminal.line(1)], ["one", "Two     x"]);
    // A newline alone goes down, not back.
    terminal.write(b"\nz\x08Z");
    assert_eq!(terminal.line(2), " Z");
}

#[test_case]
fn a_full_line_wraps_only_when_more_comes() {
    let mut terminal = Terminal::new(3, 4);
    terminal.write(b"abcd\r\nef");
    assert_eq!([terminal.line(0), terminal.line(1)], ["abcd", "ef"]);
    terminal.write(b"ghi");
    assert_eq!([terminal.line(1), terminal.line(2)], ["efgh", "i"]);
}

#[test_case]
fn the_bottom_line_scrolls_the_rest_up() {
    let mut terminal = Terminal::new(2, 10);
    terminal.write(b"1\r\n2\r\n3\r\n4");
    assert_eq!([terminal.line(0), terminal.line(1)], ["3", "4"]);
    assert_eq!(terminal.scrolled, 2);
}

#[test_case]
fn escape_sequences_move_the_cursor_and_erase() {
    let mut terminal = Terminal::new(3, 10);
    terminal.write(b"abc\r\ndef\r\nghi");
    // To row 2, column 2, then the rest of the line goes.
    terminal.write(b"\x1b[2;2H\x1b[K");
    assert_eq!(terminal.line(1), "d");
    // Then the whole screen, and back to the top left; what isn't known is
    // read and ignored.
    terminal.write(b"\x1b[2J\x1b[H\x1b[?25l\x1b[1mX");
    assert_eq!([terminal.line(0), terminal.line(1), terminal.line(2)], ["X", "", ""]);
}
