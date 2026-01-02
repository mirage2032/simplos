// pub mod vga_buffer;

use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::{FrameBuffer, PixelFormat};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{Dimensions, Point, Size};
use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::primitives::{PrimitiveStyleBuilder, Rectangle};
use embedded_graphics::{Drawable, Pixel};
use embedded_graphics::prelude::Primitive;
use spin::{Lazy, Mutex};

/// Unified display driver that owns the hardware framebuffer and a back buffer
pub struct Display {
    width: u32,
    height: u32,
    bytes_per_pixel: u32,
    pixel_format: PixelFormat,
    back_buffer: Vec<u8>,
    framebuffer: Option<FrameBuffer>,
}

impl Display {
    /// Create a new uninitialized display
    pub fn new() -> Display {
        Display {
            width: 0,
            height: 0,
            bytes_per_pixel: 0,
            pixel_format: PixelFormat::Unknown {
                red_position: 0,
                green_position: 0,
                blue_position: 0,
            },
            back_buffer: Vec::new(),
            framebuffer: None,
        }
    }

    /// Initialize the display with a framebuffer from the bootloader
    pub fn init(&mut self, framebuffer: FrameBuffer) {
        let info = framebuffer.info();
        self.width = info.width as u32;
        self.height = info.height as u32;
        self.bytes_per_pixel = info.bytes_per_pixel as u32;
        self.pixel_format = info.pixel_format;
        
        let bufsize = (self.width * self.height * self.bytes_per_pixel) as usize;
        self.back_buffer = vec![0; bufsize];
        self.framebuffer = Some(framebuffer);
    }

    /// Check if the display is initialized
    pub fn is_initialized(&self) -> bool {
        self.framebuffer.is_some()
    }

    /// Present the back buffer to the screen (copy to hardware framebuffer)
    pub fn present(&mut self) {
        if let Some(fb) = self.framebuffer.as_mut() {
            fb.buffer_mut().copy_from_slice(&self.back_buffer);
        }
    }

    /// Clear the back buffer with a color
    pub fn clear(&mut self, color: Rgb888) -> Result<(), &str> {
        let clear_style = PrimitiveStyleBuilder::new().fill_color(color).build();
        self.bounding_box()
            .into_styled(clear_style)
            .draw(self)
            .map_err(|_| "Failed to clear screen")
    }

    fn set_pixel(&mut self, index: usize, color: &Rgb888) {
        match self.pixel_format {
            PixelFormat::Bgr => {
                self.back_buffer[index] = color.b();
                self.back_buffer[index + 1] = color.g();
                self.back_buffer[index + 2] = color.r();
            }
            PixelFormat::Rgb => {
                self.back_buffer[index] = color.r();
                self.back_buffer[index + 1] = color.g();
                self.back_buffer[index + 2] = color.b();
            }
            PixelFormat::U8 => {
                let r = color.r() >> 5;
                let g = color.g() >> 5;
                let b = color.b() >> 6;
                self.back_buffer[index] = (r << 5) | (g << 2) | b;
            }
            _ => {
                panic!("Unsupported pixel format");
            }
        }
    }
}

impl Default for Display {
    fn default() -> Self {
        Display::new()
    }
}

impl Dimensions for Display {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(Point::new(0, 0), Size::new(self.width, self.height))
    }
}

impl DrawTarget for Display {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        let max_index = (self.width * self.height * self.bytes_per_pixel) as usize;
        
        for Pixel(coord, color) in pixels {
            let index = ((coord.y as u32 * self.width + coord.x as u32) * self.bytes_per_pixel) as usize;
            if index < max_index {
                self.set_pixel(index, &color);
            }
        }
        Ok(())
    }
}

/// Global display instance
pub static DISPLAY: Lazy<Mutex<Display>> = Lazy::new(|| Mutex::new(Display::new()));

/// Initialize the display with a framebuffer from the bootloader
pub fn init_display(framebuffer: FrameBuffer) {
    DISPLAY.lock().init(framebuffer);
}
