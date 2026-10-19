//! The screen as a terminal: a grid of characters, each with its colours and
//! attributes, and a cursor, which the console's output moves through and
//! fills as the Linux console does, and which [`Terminal::draw`] puts on the
//! display.
//!
//! It understands what the `linux` terminfo entry says the console does,
//! which is what programs told `TERM=linux` send, and the few sequences of a
//! VT100's or an xterm's that programs send regardless: moving the cursor,
//! erasing, inserting and deleting characters and lines, scrolling a region,
//! colours and attributes, saving the cursor, the alternate screen, and
//! telling a program where the cursor is.
//!
//! shortcut: no line-drawing characters, no tab stops but every eighth
//! column, no insert mode, and the cursor keys don't change with modes; the
//! `linux` entry asks for none of these.

use crate::drivers::video::Display;
use alloc::format;
use alloc::vec;
use alloc::vec::Vec;
use embedded_graphics::mono_font::MonoTextStyleBuilder;
use embedded_graphics::mono_font::ascii::{FONT_7X13, FONT_7X13_BOLD};
use embedded_graphics::pixelcolor::Rgb888;
use embedded_graphics::prelude::*;
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Baseline, Text};

/// The screen's own colours, which characters have unless they ask for
/// others: the Linux console's, light grey on black.
const BACKGROUND: Rgb888 = PALETTE[0];
const FOREGROUND: Rgb888 = PALETTE[7];
/// What colours 0 to 15 are, as the Linux console has them: the eight that
/// `ESC [ 30-37 m` and `40-47` set, then their bright ones.
const PALETTE: [Rgb888; 16] = [
    Rgb888::new(0, 0, 0),
    Rgb888::new(170, 0, 0),
    Rgb888::new(0, 170, 0),
    Rgb888::new(170, 85, 0),
    Rgb888::new(0, 0, 170),
    Rgb888::new(170, 0, 170),
    Rgb888::new(0, 170, 170),
    Rgb888::new(170, 170, 170),
    Rgb888::new(85, 85, 85),
    Rgb888::new(255, 85, 85),
    Rgb888::new(85, 255, 85),
    Rgb888::new(255, 255, 85),
    Rgb888::new(85, 85, 255),
    Rgb888::new(255, 85, 255),
    Rgb888::new(85, 255, 255),
    Rgb888::new(255, 255, 255),
];
/// Around the text, in pixels.
const MARGIN: usize = 8;
/// A character cell, in pixels.
const CELL_HEIGHT: usize = 13;
const CELL_WIDTH: usize = 7;

/// How a character looks: its colours, `None` for the screen's own, and
/// its attributes.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Style {
    fg: Option<Rgb888>,
    bg: Option<Rgb888>,
    bold: bool,
    underline: bool,
    reverse: bool,
}

impl Style {
    const PLAIN: Style =
        Style { fg: None, bg: None, bold: false, underline: false, reverse: false };

    /// The colours it's drawn in. Bold is bright too, as on the Linux
    /// console: one of the 8 colours is its bright one.
    fn colors(self) -> (Rgb888, Rgb888) {
        let (mut fg, bg) = (self.fg.unwrap_or(FOREGROUND), self.bg.unwrap_or(BACKGROUND));
        if self.bold
            && let Some(at) = PALETTE[..8].iter().position(|&color| color == fg)
        {
            fg = PALETTE[at + 8];
        }
        if self.reverse { (bg, fg) } else { (fg, bg) }
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
struct Cell {
    /// ASCII; anything else is kept as `?`, which is all the font has for it.
    byte: u8,
    style: Style,
}

const BLANK: Cell = Cell { byte: b' ', style: Style::PLAIN };

/// Where the cursor is and what it writes with: what `ESC 7` saves.
#[derive(Clone, Copy)]
struct Cursor {
    row: usize,
    col: usize,
    style: Style,
}

pub struct Terminal {
    rows: usize,
    cols: usize,
    /// Row by row.
    cells: Vec<Cell>,
    /// The main screen's cells while the alternate screen is shown.
    main_screen: Option<Vec<Cell>>,
    row: usize,
    /// `cols` once a character has gone in the last column: the next one
    /// starts a new line, as on a VT100, but anything that moves the cursor
    /// first stops it.
    col: usize,
    /// What characters are written with.
    style: Style,
    saved: Cursor,
    /// The rows that scrolling moves, `top..bottom`: all of them, unless a
    /// program sets fewer (`ESC [ r`).
    top: usize,
    bottom: usize,
    cursor_visible: bool,
    /// A character past the last column starts a new line; without, it
    /// replaces the last one.
    autowrap: bool,
    /// Rows changed since they were last drawn.
    dirty: Vec<bool>,
    /// Lines the whole screen scrolled since `draw` last ran, which it moves
    /// on the display rather than drawing again.
    scrolled: usize,
    /// Where `draw` last put the cursor, if it showed it.
    drawn_cursor: Option<(usize, usize)>,
    /// Nothing's been drawn yet: whatever's on the display goes first.
    fresh: bool,
    escape: Escape,
    /// What the terminal answers programs with, to be typed into it.
    replies: Vec<u8>,
}

/// Where in an escape sequence the last byte left off.
enum Escape {
    None,
    /// After `ESC`.
    Started,
    /// After `ESC [`, with the parameters so far.
    Csi(Vec<u8>),
    /// After `ESC (` and the like, which choose character sets: one more
    /// byte says which, and it's ignored.
    Designate,
    /// After `ESC ]`, until BEL or `ESC \`: a title, say, which goes nowhere.
    /// Whether the last byte was `ESC`.
    Osc(bool),
}

impl Terminal {
    /// An empty one of `rows` and `cols`, the cursor at the top left.
    pub fn new(rows: usize, cols: usize) -> Terminal {
        let (rows, cols) = (rows.max(1), cols.max(1));
        Terminal {
            rows,
            cols,
            cells: vec![BLANK; rows * cols],
            main_screen: None,
            row: 0,
            col: 0,
            style: Style::PLAIN,
            saved: Cursor { row: 0, col: 0, style: Style::PLAIN },
            top: 0,
            bottom: rows,
            cursor_visible: true,
            autowrap: true,
            dirty: vec![true; rows],
            scrolled: 0,
            drawn_cursor: None,
            fresh: true,
            escape: Escape::None,
            replies: Vec::new(),
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

    /// What the terminal has answered since this was last asked, for whoever
    /// feeds its input: where the cursor is, for one.
    pub fn take_replies(&mut self) -> Vec<u8> {
        core::mem::take(&mut self.replies)
    }

    fn byte(&mut self, byte: u8) {
        match core::mem::replace(&mut self.escape, Escape::None) {
            Escape::None => {}
            Escape::Started => return self.esc(byte),
            Escape::Csi(mut parameters) => {
                match byte {
                    0x40..=0x7e => self.csi(&parameters, byte),
                    0x1b => self.escape = Escape::Started,
                    // CAN and SUB: abandoned.
                    0x18 | 0x1a => {}
                    _ if parameters.len() < 64 => {
                        parameters.push(byte);
                        self.escape = Escape::Csi(parameters);
                    }
                    // Far longer than any real one: dropped.
                    _ => {}
                }
                return;
            }
            Escape::Designate => return,
            Escape::Osc(escaped) => {
                if byte != 0x07 && !(escaped && byte == b'\\') {
                    self.escape = Escape::Osc(byte == 0x1b);
                }
                return;
            }
        }
        match byte {
            0x1b => self.escape = Escape::Started,
            b'\r' => self.col = 0,
            b'\n' | 0x0b | 0x0c => self.index(),
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
            if self.autowrap {
                self.col = 0;
                self.index();
            } else {
                self.col = self.cols - 1;
            }
        }
        self.cells[self.row * self.cols + self.col] = Cell { byte, style: self.style };
        self.dirty[self.row] = true;
        self.col += 1;
    }

    /// `ESC` and a byte.
    fn esc(&mut self, byte: u8) {
        match byte {
            b'[' => self.escape = Escape::Csi(Vec::new()),
            b']' => self.escape = Escape::Osc(false),
            b'(' | b')' | b'*' | b'+' | b'#' | b'%' => self.escape = Escape::Designate,
            b'7' => self.save(),
            b'8' => self.restore(),
            b'D' => self.index(),
            b'E' => {
                self.col = 0;
                self.index();
            }
            b'M' => self.reverse_index(),
            b'c' => *self = Terminal::new(self.rows, self.cols),
            _ => {}
        }
    }

    /// `ESC [ parameters last`.
    fn csi(&mut self, parameters: &[u8], last: u8) {
        let (private, parameters) = match parameters.first() {
            Some(b'?' | b'>' | b'=' | b'<') => (true, &parameters[1..]),
            _ => (false, parameters),
        };
        // Each parameter, 0 for one that's missing; none bigger than a
        // screen could need, so moving by one can't overflow.
        let text = core::str::from_utf8(parameters).unwrap_or("");
        let number = |n: &str| n.parse().unwrap_or(0).min(usize::from(u16::MAX));
        let numbers: Vec<usize> = text.split(';').map(number).collect();
        // A count or a position, which 0 or nothing make 1.
        let count = |at: usize| numbers.get(at).copied().filter(|&n| n != 0).unwrap_or(1);
        if private {
            if matches!(last, b'h' | b'l') {
                numbers.iter().for_each(|&mode| self.set_mode(mode, last == b'h'));
            }
            return;
        }
        let col = self.col.min(self.cols - 1);
        let at = self.row * self.cols + col;
        let line = self.row * self.cols..(self.row + 1) * self.cols;
        match last {
            b'A' => self.up(count(0)),
            b'B' | b'e' => self.down(count(0)),
            b'C' | b'a' => self.col = (col + count(0)).min(self.cols - 1),
            b'D' => self.col = col.saturating_sub(count(0)),
            b'E' => {
                self.down(count(0));
                self.col = 0;
            }
            b'F' => {
                self.up(count(0));
                self.col = 0;
            }
            b'G' | b'`' => self.col = (count(0) - 1).min(self.cols - 1),
            b'H' | b'f' => self.move_to(count(0) - 1, count(1) - 1),
            b'd' => self.move_to(count(0) - 1, col),
            b'J' => match numbers[0] {
                0 => self.erase(at..self.cells.len()),
                1 => self.erase(0..at + 1),
                2 | 3 => self.erase(0..self.cells.len()),
                _ => {}
            },
            b'K' => match numbers[0] {
                0 => self.erase(at..line.end),
                1 => self.erase(line.start..at + 1),
                2 => self.erase(line),
                _ => {}
            },
            b'X' => self.erase(at..(at + count(0)).min(line.end)),
            b'@' => {
                let n = count(0).min(line.end - at);
                self.cells.copy_within(at..line.end - n, at + n);
                self.erase(at..at + n);
            }
            b'P' => {
                let n = count(0).min(line.end - at);
                self.cells.copy_within(at + n..line.end, at);
                self.erase(line.end - n..line.end);
            }
            b'L' | b'M' if (self.top..self.bottom).contains(&self.row) => {
                match last {
                    b'L' => self.scroll_down(self.row, count(0)),
                    _ => self.scroll_up(self.row, count(0)),
                }
                self.col = 0;
            }
            b'S' => self.scroll_up(self.top, count(0)),
            b'T' => self.scroll_down(self.top, count(0)),
            b'm' => self.sgr(&numbers),
            b'r' => {
                let (top, bottom) = (count(0) - 1, numbers.get(1).copied().unwrap_or(0));
                let bottom = if bottom == 0 { self.rows } else { bottom.min(self.rows) };
                if top + 1 < bottom {
                    (self.top, self.bottom) = (top, bottom);
                    self.move_to(0, 0);
                }
            }
            b's' => self.save(),
            b'u' => self.restore(),
            // Device status: fine, and where the cursor is.
            b'n' if numbers[0] == 5 => self.replies.extend(b"\x1b[0n"),
            b'n' if numbers[0] == 6 => {
                let report = format!("\x1b[{};{}R", self.row + 1, col + 1);
                self.replies.extend(report.as_bytes());
            }
            // What the terminal is: what the Linux console says.
            b'c' if numbers[0] == 0 => self.replies.extend(b"\x1b[?6c"),
            _ => {}
        }
    }

    fn set_mode(&mut self, mode: usize, on: bool) {
        match mode {
            7 => self.autowrap = on,
            25 => self.cursor_visible = on,
            47 | 1047 => self.alternate(on),
            1049 => {
                if on {
                    self.save();
                }
                self.alternate(on);
                if !on {
                    self.restore();
                }
            }
            _ => {}
        }
    }

    /// Show the alternate screen, an empty one, or the main one again.
    fn alternate(&mut self, on: bool) {
        match (on, self.main_screen.is_some()) {
            (true, false) => {
                let alternate = vec![BLANK; self.cells.len()];
                self.main_screen = Some(core::mem::replace(&mut self.cells, alternate));
            }
            (false, true) => self.cells = self.main_screen.take().expect("it's there"),
            _ => return,
        }
        self.dirty.fill(true);
    }

    /// `ESC [ ... m`: colours and attributes.
    fn sgr(&mut self, numbers: &[usize]) {
        let mut at = 0;
        while at < numbers.len() {
            match numbers[at] {
                0 => self.style = Style::PLAIN,
                1 => self.style.bold = true,
                4 => self.style.underline = true,
                7 => self.style.reverse = true,
                21 | 22 => self.style.bold = false,
                24 => self.style.underline = false,
                27 => self.style.reverse = false,
                n @ 30..=37 => self.style.fg = Some(PALETTE[n - 30]),
                n @ 90..=97 => self.style.fg = Some(PALETTE[n - 90 + 8]),
                39 => self.style.fg = None,
                n @ 40..=47 => self.style.bg = Some(PALETTE[n - 40]),
                n @ 100..=107 => self.style.bg = Some(PALETTE[n - 100 + 8]),
                49 => self.style.bg = None,
                // One of 256 colours, or any.
                which @ (38 | 48) => {
                    let color = match numbers.get(at + 1) {
                        Some(5) => numbers.get(at + 2).map(|&index| color256(index)),
                        Some(2) => numbers.get(at + 2..at + 5).map(|rgb| {
                            let [r, g, b] = [rgb[0], rgb[1], rgb[2]].map(|n| n.min(255) as u8);
                            Rgb888::new(r, g, b)
                        }),
                        _ => None,
                    };
                    at += match numbers.get(at + 1) {
                        Some(5) => 2,
                        Some(2) => 4,
                        _ => 0,
                    };
                    match which {
                        38 => self.style.fg = color.or(self.style.fg),
                        _ => self.style.bg = color.or(self.style.bg),
                    }
                }
                // Blinking, other fonts and the like.
                _ => {}
            }
            at += 1;
        }
    }

    /// Down a line, scrolling at the bottom of the scrolling region.
    fn index(&mut self) {
        self.col = self.col.min(self.cols - 1);
        if self.row + 1 == self.bottom {
            self.scroll_up(self.top, 1);
        } else if self.row + 1 < self.rows {
            self.row += 1;
        }
    }

    /// Up a line, scrolling at the top of the scrolling region.
    fn reverse_index(&mut self) {
        self.col = self.col.min(self.cols - 1);
        if self.row == self.top {
            self.scroll_down(self.top, 1);
        } else if self.row > 0 {
            self.row -= 1;
        }
    }

    /// Up `count` rows, but not out of the scrolling region from inside it.
    fn up(&mut self, count: usize) {
        let limit = if self.row >= self.top { self.top } else { 0 };
        self.move_to(self.row.saturating_sub(count).max(limit), self.col);
    }

    fn down(&mut self, count: usize) {
        let limit = if self.row < self.bottom { self.bottom - 1 } else { self.rows - 1 };
        self.move_to((self.row + count).min(limit), self.col);
    }

    fn move_to(&mut self, row: usize, col: usize) {
        self.row = row.min(self.rows - 1);
        self.col = col.min(self.cols - 1);
    }

    fn save(&mut self) {
        self.saved = Cursor { row: self.row, col: self.col, style: self.style };
    }

    fn restore(&mut self) {
        let Cursor { row, col, style } = self.saved;
        (self.row, self.col, self.style) = (row.min(self.rows - 1), col.min(self.cols), style);
    }

    /// What erasing leaves: a space in the colour the background is being
    /// written in, as on the Linux console.
    fn blank(&self) -> Cell {
        Cell { byte: b' ', style: Style { bg: self.style.bg, ..Style::PLAIN } }
    }

    fn erase(&mut self, cells: core::ops::Range<usize>) {
        if cells.is_empty() {
            return;
        }
        let rows = cells.start / self.cols..=(cells.end - 1) / self.cols;
        let blank = self.blank();
        self.cells[cells].fill(blank);
        self.dirty[rows].fill(true);
    }

    /// Move the rows from `top` to the bottom of the scrolling region up
    /// `count`, blank ones coming in below.
    fn scroll_up(&mut self, top: usize, count: usize) {
        let (bottom, width) = (self.bottom, self.cols);
        let count = count.min(bottom - top);
        self.cells.copy_within((top + count) * width..bottom * width, top * width);
        let blank = self.blank();
        self.cells[(bottom - count) * width..bottom * width].fill(blank);
        if top == 0 && bottom == self.rows {
            // The whole screen: `draw` moves it on the display, and draws
            // just what came in.
            self.dirty.drain(..count);
            self.dirty.resize(self.rows, true);
            self.scrolled += count;
        } else {
            self.dirty[top..bottom].fill(true);
        }
    }

    /// Move the rows from `top` to the bottom of the scrolling region down
    /// `count`, blank ones coming in above.
    fn scroll_down(&mut self, top: usize, count: usize) {
        let (bottom, width) = (self.bottom, self.cols);
        let count = count.min(bottom - top);
        self.cells.copy_within(top * width..(bottom - count) * width, (top + count) * width);
        let blank = self.blank();
        self.cells[top * width..(top + count) * width].fill(blank);
        self.dirty[top..bottom].fill(true);
    }

    /// Put what's changed on `display`, and show it.
    pub fn draw(&mut self, display: &mut Display) {
        if self.fresh {
            display.clear(BACKGROUND).expect("the display clears");
            (self.fresh, self.scrolled, self.drawn_cursor) = (false, 0, None);
            self.dirty.fill(true);
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
        let cursor = self.cursor_visible.then_some((self.row, self.col.min(self.cols - 1)));
        if self.drawn_cursor != cursor {
            for (row, _) in [self.drawn_cursor, cursor].into_iter().flatten() {
                self.dirty[row] = true;
            }
        }
        let width = display.bounding_box().size.width;
        for row in (0..self.rows).filter(|&row| self.dirty[row]) {
            let y = (MARGIN + row * CELL_HEIGHT) as i32;
            let area = Rectangle::new(Point::new(0, y), Size::new(width, CELL_HEIGHT as u32));
            display.fill_rect(&area, BACKGROUND);
            let line = &self.cells[row * self.cols..(row + 1) * self.cols];
            // As each cell is drawn: the cursor's reversed.
            let look = |col: usize| match cursor == Some((row, col)) {
                true => Style { reverse: !line[col].style.reverse, ..line[col].style },
                false => line[col].style,
            };
            // Runs of cells that look alike, each drawn in one go.
            let mut start = 0;
            while start < self.cols {
                let style = look(start);
                let end =
                    (start + 1..self.cols).find(|&col| look(col) != style).unwrap_or(self.cols);
                let (fg, bg) = style.colors();
                let blank = line[start..end].iter().all(|cell| cell.byte == b' ');
                if !(blank && bg == BACKGROUND && !style.underline) {
                    let text: Vec<u8> = line[start..end].iter().map(|cell| cell.byte).collect();
                    let font = if style.bold { &FONT_7X13_BOLD } else { &FONT_7X13 };
                    let mut look = MonoTextStyleBuilder::new().font(font).text_color(fg);
                    look = look.background_color(bg);
                    if style.underline {
                        look = look.underline();
                    }
                    let at = Point::new((MARGIN + start * CELL_WIDTH) as i32, y);
                    let text = core::str::from_utf8(&text).unwrap_or("?");
                    Text::with_baseline(text, at, look.build(), Baseline::Top).draw(display).ok();
                }
                start = end;
            }
        }
        self.dirty.fill(false);
        self.drawn_cursor = cursor;
        display.present();
    }

    /// What's on row `row`, without the spaces at its end.
    #[cfg(test)]
    fn line(&self, row: usize) -> alloc::string::String {
        let cells = &self.cells[row * self.cols..(row + 1) * self.cols];
        let text: Vec<u8> = cells.iter().map(|cell| cell.byte).collect();
        alloc::string::String::from_utf8(text).unwrap().trim_end().into()
    }

    #[cfg(test)]
    fn lines(&self) -> Vec<alloc::string::String> {
        (0..self.rows).map(|row| self.line(row)).collect()
    }
}

/// One of the 256 colours `ESC [ 38 ; 5 ; n m` names: the 16, then a 6×6×6
/// cube of colours, then 24 greys.
fn color256(index: usize) -> Rgb888 {
    let level = |n: usize| if n == 0 { 0 } else { (55 + n * 40) as u8 };
    match index {
        0..=15 => PALETTE[index],
        16..=231 => {
            let n = index - 16;
            Rgb888::new(level(n / 36), level(n / 6 % 6), level(n % 6))
        }
        _ => {
            let grey = (8 + (index.min(255) - 232) * 10) as u8;
            Rgb888::new(grey, grey, grey)
        }
    }
}

#[test_case]
fn text_goes_where_the_cursor_is() {
    let mut terminal = Terminal::new(3, 10);
    terminal.write(b"one\r\ntwo\tx\rT");
    assert_eq!(terminal.lines()[..2], ["one", "Two     x"]);
    // A newline alone goes down, not back.
    terminal.write(b"\nz\x08Z");
    assert_eq!(terminal.line(2), " Z");
}

#[test_case]
fn a_full_line_wraps_only_when_more_comes() {
    let mut terminal = Terminal::new(3, 4);
    terminal.write(b"abcd\r\nef");
    assert_eq!(terminal.lines()[..2], ["abcd", "ef"]);
    terminal.write(b"ghi");
    assert_eq!(terminal.lines()[1..], ["efgh", "i"]);
    // Without autowrap, the last column is written over instead.
    terminal.write(b"\x1b[?7l\x1b[3;3Hxyz");
    assert_eq!(terminal.line(2), "i xz");
}

#[test_case]
fn the_bottom_line_scrolls_the_rest_up() {
    let mut terminal = Terminal::new(2, 10);
    terminal.write(b"1\r\n2\r\n3\r\n4");
    assert_eq!(terminal.lines(), ["3", "4"]);
    assert_eq!(terminal.scrolled, 2);
}

#[test_case]
fn the_cursor_moves_as_asked_but_stays_on_the_screen() {
    let mut terminal = Terminal::new(4, 10);
    // To row 2, column 2; then up, right, down and left; then to column 9,
    // then to row 1; then far past every edge.
    terminal.write(b"\x1b[2;2Ha\x1b[Ab\x1b[2Cc\x1b[2Bd\x1b[3De\x1b[9Gf\x1b[1dg");
    assert_eq!(terminal.lines(), ["  b  c   g", " a", "    e d f", ""]);
    terminal.write(b"\x1b[99;99Hh\x1b[99A\x1b[99Di");
    assert_eq!(terminal.line(0), "i b  c   g");
    assert_eq!(terminal.line(3), "         h");
}

#[test_case]
fn numbers_too_big_for_any_screen_stop_at_its_edges() {
    // The biggest a number can be, which added to a column or a row would
    // overflow: right, erasing and down, each as far as there is.
    let mut terminal = Terminal::new(3, 10);
    let most = "18446744073709551615";
    let moves = format!("ab\x1b[{most}Cc\r\n123\x1b[2;2H\x1b[{most}X\x1b[{most}Bd");
    terminal.write(moves.as_bytes());
    assert_eq!(terminal.lines(), ["ab       c", "1", " d"]);
}

#[test_case]
fn erasing_inserting_and_deleting() {
    let mut terminal = Terminal::new(4, 10);
    terminal.write(b"abcdefgh\r\n12345678\r\nABCDEFGH\r\nxyz");
    // In row 1: the rest of the line from column 5, then the line up to
    // column 2.
    terminal.write(b"\x1b[2;5H\x1b[K\x1b[2;2H\x1b[1K");
    assert_eq!(terminal.line(1), "  34");
    // In row 0: two spaces in at column 2, then three characters out at
    // column 6, then two erased at column 1.
    terminal.write(b"\x1b[1;3H\x1b[2@\x1b[1;7H\x1b[3P\x1b[1;2H\x1b[2X");
    assert_eq!(terminal.line(0), "a   cdh");
    // A line in at row 1, pushing the rest down and the last off; then
    // row 0 deleted, pulling them up.
    terminal.write(b"\x1b[2;4H\x1b[L\x1b[1;1H\x1b[M");
    assert_eq!(terminal.lines(), ["", "  34", "ABCDEFGH", ""]);
    // Everything below the cursor, at row 2 column 3, then all of it.
    terminal.write(b"\x1b[3;3H\x1b[J");
    assert_eq!(terminal.lines(), ["", "  34", "AB", ""]);
    terminal.write(b"\x1b[2J");
    assert!(terminal.lines().iter().all(|line| line.is_empty()));
}

#[test_case]
fn colours_and_attributes_go_with_what_is_written() {
    let mut terminal = Terminal::new(2, 10);
    terminal.write(b"\x1b[1;31ma\x1b[44;4mb\x1b[0mc\x1b[38;5;196;48;2;1;2;3md\x1b[7;39;49me");
    let style = |col: usize| terminal.cells[col].style;
    let red = Some(PALETTE[1]);
    let plain = Style::PLAIN;
    assert_eq!(style(0), Style { fg: red, bold: true, ..plain });
    assert_eq!(
        style(1),
        Style { fg: red, bg: Some(PALETTE[4]), bold: true, underline: true, ..plain }
    );
    assert_eq!(style(2), plain);
    let (fg, bg) = (Some(Rgb888::new(255, 0, 0)), Some(Rgb888::new(1, 2, 3)));
    assert_eq!(style(3), Style { fg, bg, ..plain });
    assert_eq!(style(4), Style { reverse: true, ..plain });
    // Erasing leaves the background being written in.
    terminal.write(b"\x1b[0;42m\x1b[2K");
    assert_eq!(terminal.cells[0].style.bg, Some(PALETTE[2]));
}

#[test_case]
fn characters_are_drawn_light_grey_on_black_and_bold_is_bright() {
    let colors = |style: Style| style.colors();
    let plain = Style::PLAIN;
    assert_eq!(colors(plain), (PALETTE[7], PALETTE[0]));
    // As the Linux console has it: bold white, bold blue's bright blue.
    assert_eq!(colors(Style { bold: true, ..plain }), (PALETTE[15], PALETTE[0]));
    let blue = Style { fg: Some(PALETTE[4]), bold: true, ..plain };
    assert_eq!(colors(blue), (PALETTE[12], PALETTE[0]));
    // A colour of its own stays as it is.
    let own = Style { fg: Some(Rgb888::new(1, 2, 3)), bold: true, ..plain };
    assert_eq!(colors(own), (Rgb888::new(1, 2, 3), PALETTE[0]));
    assert_eq!(colors(Style { bold: true, reverse: true, ..plain }), (PALETTE[0], PALETTE[15]));
}

#[test_case]
fn a_scrolling_region_scrolls_alone() {
    let mut terminal = Terminal::new(4, 10);
    terminal.write(b"top\r\none\r\ntwo\r\nend");
    // Rows 1 and 2 scroll; the cursor goes home.
    terminal.write(b"\x1b[2;3r\x1b[3;1H\nthree");
    assert_eq!(terminal.lines(), ["top", "two", "three", "end"]);
    // At the region's top, a reverse index scrolls it down.
    terminal.write(b"\x1b[2;1H\x1bMzero");
    assert_eq!(terminal.lines(), ["top", "zero", "two", "end"]);
    assert_eq!(terminal.scrolled, 0);
}

#[test_case]
fn the_alternate_screen_keeps_the_main_one_and_its_cursor() {
    let mut terminal = Terminal::new(2, 10);
    terminal.write(b"main\x1b[?1049h");
    assert_eq!(terminal.lines(), ["", ""]);
    terminal.write(b"\x1b[2;1Halternate\x1b[?1049l!");
    assert_eq!(terminal.lines(), ["main!", ""]);
}

#[test_case]
fn the_cursor_and_its_style_are_saved_and_put_back() {
    let mut terminal = Terminal::new(2, 10);
    terminal.write(b"\x1b[1;3H\x1b[31m\x1b7\x1b[2;1H\x1b[0mx\x1b8y");
    assert_eq!(terminal.lines(), ["  y", "x"]);
    assert_eq!(terminal.cells[2].style.fg, Some(PALETTE[1]));
}

#[test_case]
fn it_answers_where_the_cursor_is_and_what_it_is() {
    let mut terminal = Terminal::new(5, 10);
    terminal.write(b"\x1b[3;5H\x1b[6n\x1b[5n\x1b[c");
    assert_eq!(terminal.take_replies(), b"\x1b[3;5R\x1b[0n\x1b[?6c");
    assert!(terminal.take_replies().is_empty());
}

#[test_case]
fn what_is_not_known_is_read_and_ignored() {
    let mut terminal = Terminal::new(2, 20);
    // A title, a character set, a mode, a blink and an unknown sequence.
    terminal.write(b"\x1b]0;title\x07\x1b(B\x1b[?2004h\x1b[5m\x1b[9za\x1b]2;x\x1b\\b");
    assert_eq!(terminal.line(0), "ab");
}
