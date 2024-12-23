use core::fmt;
use spin::{Lazy, Mutex};
use volatile::Volatile;

#[allow(dead_code)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum Color {
    Black = 0,
    Blue = 1,
    Green = 2,
    Cyan = 3,
    Red = 4,
    Magenta = 5,
    Brown = 6,
    LightGray = 7,
    DarkGray = 8,
    LightBlue = 9,
    LightGreen = 10,
    LightCyan = 11,
    LightRed = 12,
    Pink = 13,
    Yellow = 14,
    White = 15,
}

impl From<u8> for Color {
    fn from(value: u8) -> Self {
        match value {
            0 => Color::Black,
            1 => Color::Blue,
            2 => Color::Green,
            3 => Color::Cyan,
            4 => Color::Red,
            5 => Color::Magenta,
            6 => Color::Brown,
            7 => Color::LightGray,
            8 => Color::DarkGray,
            9 => Color::LightBlue,
            10 => Color::LightGreen,
            11 => Color::LightCyan,
            12 => Color::LightRed,
            13 => Color::Pink,
            14 => Color::Yellow,
            15 => Color::White,
            _ => Color::White,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(transparent)]
pub struct ColorCode(u8);

impl ColorCode {
    pub fn new(foreground: Color, background: Color) -> ColorCode {
        ColorCode((background as u8) << 4 | (foreground as u8))
    }

    pub fn get_foreground(&self) -> Color {
        let foreground = self.0 & 0x0F;
        foreground.into()
    }

    pub fn get_background(&self) -> Color {
        let background = self.0 >> 4;
        background.into()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[repr(C)]
struct ScreenChar {
    ascii_character: u8,
    color_code: ColorCode,
}

const BUFFER_HEIGHT: usize = 25;
const BUFFER_WIDTH: usize = 80;

struct Buffer {
    chars: [[Volatile<ScreenChar>; BUFFER_WIDTH]; BUFFER_HEIGHT],
}
pub struct Writer {
    column_position: usize,
    default_color_code: ColorCode,
    buffer: &'static mut Buffer,
    width: usize,
    height: usize,
}
impl Writer {
    pub fn new(default_color_code: ColorCode) -> Writer {
        let mut writer = Writer {
            column_position:0,
            default_color_code,
            buffer: unsafe { &mut *(0xb8000 as *mut Buffer) },
            height: BUFFER_HEIGHT,
            width: BUFFER_WIDTH,
        };
        writer.clear();
        writer
    }

    pub fn size(&self) -> (usize, usize) {
        (self.width, self.height)
    }

    pub fn set_color(&mut self, color_code: ColorCode) {
        self.default_color_code = color_code;
    }
    
    pub fn get_color(&self) -> ColorCode {
        self.default_color_code
    }

    pub fn write_byte(&mut self, byte: u8) {
        self.write_byte_color(byte, self.default_color_code)
    }
    
    pub fn write_byte_at(&mut self, byte: u8, row: usize, col: usize) {
        self.write_byte_color_at(byte, row, col, self.default_color_code)
    }
    
    pub fn write_byte_color_at(&mut self, byte: u8, row: usize, col: usize, color_code: ColorCode) {
        self.buffer.chars[row][col].write(ScreenChar {
            ascii_character: byte,
            color_code,
        });
    }
    pub fn write_byte_color(&mut self, byte: u8, color_code: ColorCode) {
        if byte == b'\n' {
            self.new_line_color(color_code.get_background());
            return;
        }
        if self.column_position >= self.width {
            self.new_line_color(color_code.get_background());
        }
        let row = self.height - 1;
        let col = self.column_position;
        self.buffer.chars[row][col].write(ScreenChar {
            ascii_character: byte,
            color_code,
        });
        self.column_position += 1;
    }

    pub fn write_string(&mut self, s: &str) {
        self.write_string_color(s, self.default_color_code)
    }
    pub fn write_string_color(&mut self, s: &str, color_code: ColorCode) {
        for byte in s.bytes() {
            match byte {
                // printable ASCII byte or newline
                0x20..=0x7e | b'\n' => self.write_byte_color(byte, color_code),
                // not part of printable ASCII range
                _ => self.write_byte(0xfe),
            }
        }
    }
    pub fn new_line(&mut self) {
        self.new_line_color(self.default_color_code.get_background())
    }
    pub fn new_line_color(&mut self, background_color: Color) {
        for row in 1..self.height {
            for col in 0..self.width {
                let character = self.buffer.chars[row][col].read();
                self.buffer.chars[row - 1][col].write(character);
            }
        }
        self.clear_row_color(self.height - 1, background_color);
        self.column_position = 0;
    }
    pub fn clear_row(&mut self, row: usize) {
        self.clear_row_color(row, self.default_color_code.get_background());
    }
    pub fn clear_row_color(&mut self, row: usize, background_color: Color) {
        let blank = ScreenChar {
            ascii_character: b' ',
            color_code: ColorCode::new(background_color, background_color),
        };
        for col in 0..self.width {
            self.buffer.chars[row][col].write(blank);
        }
    }

    pub fn clear(&mut self) {
        for row in 0..self.height {
            self.clear_row(row);
        }
    }

    pub fn clear_color(&mut self, background_color: Color) {
        for row in 0..self.height {
            self.clear_row_color(row, background_color);
        }
    }
}

impl fmt::Write for Writer {
    fn write_str(&mut self, s: &str) -> fmt::Result {
        self.write_string(s);
        Ok(())
    }
}

#[macro_export]
macro_rules! print {
    ($($arg:tt)*) => ($crate::vga_buffer::_print(format_args!($($arg)*)));
}

#[macro_export]
macro_rules! println {
    () => ($crate::print!("\n"));
    ($($arg:tt)*) => ($crate::print!("{}\n", format_args!($($arg)*)));
}

#[doc(hidden)]
pub fn _print(args: fmt::Arguments) {
    use core::fmt::Write;
    WRITER.lock().write_fmt(args).unwrap();
}
pub static WRITER: Lazy<Mutex<Writer>> =
    Lazy::new(|| Mutex::new(Writer::new(ColorCode::new(Color::Yellow, Color::Blue))));

#[test_case]
fn test_println_many() {
    for _ in 0..200 {
        println!("test_println_many output");
    }
}

#[test_case]
fn test_println_output() {
    let s = "Some test string that fits on a single line";
    println!("{}", s);
    for (i, c) in s.chars().enumerate() {
        let screen_char = WRITER.lock().buffer.chars[BUFFER_HEIGHT - 2][i].read();
        assert_eq!(char::from(screen_char.ascii_character), c);
    }
}