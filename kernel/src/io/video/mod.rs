// pub mod vga_buffer;

use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::{FrameBuffer, PixelFormat};
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{Dimensions, Point, Size};
use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::Pixel;
use spin::{Lazy, Mutex};

/// Unified display driver that owns the hardware framebuffer and a back buffer
pub struct Display {
    width: u32,
    height: u32,
    /// Pixels per hardware scanline, which is not always `width`.
    stride: u32,
    bytes_per_pixel: u32,
    pixel_format: PixelFormat,
    back_buffer: Vec<u8>,
    framebuffer: Option<FrameBuffer>,
    /// Inclusive range of back-buffer rows changed since the last `present`.
    dirty: Option<(u32, u32)>,
}

impl Display {
    /// Create a new uninitialized display
    pub fn new() -> Display {
        Display {
            width: 0,
            height: 0,
            stride: 0,
            bytes_per_pixel: 0,
            pixel_format: PixelFormat::Unknown {
                red_position: 0,
                green_position: 0,
                blue_position: 0,
            },
            back_buffer: Vec::new(),
            framebuffer: None,
            dirty: None,
        }
    }

    /// Initialize the display with a framebuffer from the bootloader
    pub fn init(&mut self, framebuffer: FrameBuffer) {
        let info = framebuffer.info();
        self.width = info.width as u32;
        self.height = info.height as u32;
        self.stride = info.stride as u32;
        self.bytes_per_pixel = info.bytes_per_pixel as u32;
        self.pixel_format = info.pixel_format;
        
        let bufsize = (self.width * self.height * self.bytes_per_pixel) as usize;
        self.back_buffer = vec![0; bufsize];
        self.framebuffer = Some(framebuffer);
        self.dirty = Some((0, self.height.saturating_sub(1)));
    }

    /// Check if the display is initialized
    pub fn is_initialized(&self) -> bool {
        self.framebuffer.is_some()
    }

    /// Copy the rows changed since the last call to the hardware framebuffer.
    ///
    /// Blitting the whole buffer every frame is by far the most expensive thing
    /// this kernel does — 8 MB per frame at 1080p — and it dominates everything
    /// else once the CPU is emulated rather than native. The status block only
    /// ever touches a few hundred rows.
    pub fn present(&mut self) {
        let Some((top, bottom)) = self.dirty.take() else {
            return;
        };
        let (width, bpp) = (self.width as usize, self.bytes_per_pixel as usize);
        let src_pitch = width * bpp;
        let dst_pitch = self.stride as usize * bpp;
        let Some(fb) = self.framebuffer.as_mut() else {
            return;
        };
        let buffer = fb.buffer_mut();
        for y in top as usize..=bottom as usize {
            let (src, dst) = (y * src_pitch, y * dst_pitch);
            if dst + src_pitch > buffer.len() || src + src_pitch > self.back_buffer.len() {
                break;
            }
            buffer[dst..dst + src_pitch].copy_from_slice(&self.back_buffer[src..src + src_pitch]);
        }
    }

    /// Clear the back buffer with a color
    pub fn clear(&mut self, color: Rgb888) -> Result<(), &str> {
        let area = self.bounding_box();
        self.fill_rect(&area, color);
        Ok(())
    }

    /// Fill a rectangle, clipped to the screen. Used both for the one-off full
    /// clear and for erasing the previous frame's text before redrawing it.
    pub fn fill_rect(&mut self, area: &Rectangle, color: Rgb888) {
        let Some((left, top, right, bottom)) = self.clip(area) else {
            return;
        };
        let (width, bpp) = (self.width as usize, self.bytes_per_pixel as usize);
        for y in top..=bottom {
            let row = y as usize * width * bpp;
            for x in left..=right {
                self.set_pixel(row + x as usize * bpp, &color);
            }
        }
        self.mark_dirty(top, bottom);
    }

    /// `area` intersected with the screen, as inclusive `(left, top, right, bottom)`.
    fn clip(&self, area: &Rectangle) -> Option<(u32, u32, u32, u32)> {
        if self.width == 0 || self.height == 0 || area.size.width == 0 || area.size.height == 0 {
            return None;
        }
        let left = area.top_left.x.max(0) as u32;
        let top = area.top_left.y.max(0) as u32;
        let right = (area.top_left.x + area.size.width as i32 - 1).max(0) as u32;
        let bottom = (area.top_left.y + area.size.height as i32 - 1).max(0) as u32;
        if left >= self.width || top >= self.height {
            return None;
        }
        Some((left, top, right.min(self.width - 1), bottom.min(self.height - 1)))
    }

    fn mark_dirty(&mut self, top: u32, bottom: u32) {
        self.dirty = Some(match self.dirty {
            Some((old_top, old_bottom)) => (old_top.min(top), old_bottom.max(bottom)),
            None => (top, bottom),
        });
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
            if coord.x < 0 || coord.y < 0 || coord.x as u32 >= self.width {
                continue;
            }
            let index = ((coord.y as u32 * self.width + coord.x as u32) * self.bytes_per_pixel) as usize;
            if index < max_index {
                self.set_pixel(index, &color);
                let row = coord.y as u32;
                self.mark_dirty(row, row);
            }
        }
        Ok(())
    }

    fn fill_solid(&mut self, area: &Rectangle, color: Self::Color) -> Result<(), Self::Error> {
        self.fill_rect(area, color);
        Ok(())
    }
}

/// Global display instance
pub static DISPLAY: Lazy<Mutex<Display>> = Lazy::new(|| Mutex::new(Display::new()));

/// Initialize the display with a framebuffer from the bootloader
pub fn init_display(framebuffer: FrameBuffer) {
    DISPLAY.lock().init(framebuffer);
}
