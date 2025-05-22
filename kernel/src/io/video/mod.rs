// pub mod vga_buffer;

use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::PixelFormat;
use embedded_graphics::draw_target::DrawTarget;
use embedded_graphics::geometry::{Dimensions, Point, Size};
use embedded_graphics::pixelcolor::{Rgb888, RgbColor};
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::Pixel;
use spin::{Lazy};
use crate::utils::imutex::IMutex;

pub struct VideoBuffer {
    width:u32,
    height:u32,
    buffer: Vec<u8>,
    bytes_per_pixel: u32,
    pixel_format: PixelFormat,
}

impl VideoBuffer {
    pub fn new(
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
        pixel_format: PixelFormat,
    ) -> VideoBuffer{
        VideoBuffer {
            width,
            height,
            buffer: vec![0; (width * height * bytes_per_pixel) as usize],
            bytes_per_pixel,
            pixel_format,
        }
    }
    pub fn init(
        &mut self,
        width: u32,
        height: u32,
        bytes_per_pixel: u32,
        pixel_format: PixelFormat,
    ){
        self.width = width;
        self.height = height;
        self.buffer = vec![0; (width * height * bytes_per_pixel) as usize];
        self.bytes_per_pixel = bytes_per_pixel;
        self.pixel_format = pixel_format;
    }
    fn set_bgr(&mut self,mut index:u32,color:&Rgb888) {
        index = index * self.bytes_per_pixel;
        self.buffer[index as usize] = color.b();
        self.buffer[index as usize + 1] = color.g();
        self.buffer[index as usize + 2] = color.r();
    }
    fn set_rgb(&mut self,mut index:u32,color:&Rgb888) {
        index = index * self.bytes_per_pixel;
        self.buffer[index as usize] = color.r();
        self.buffer[index as usize + 1] = color.g();
        self.buffer[index as usize + 2] = color.b();
    }

    fn set_u8(&mut self,mut index:u32,color:&Rgb888) {
        index = index * self.bytes_per_pixel;
        let r = color.r() >> 5;
        let g = color.g() >> 5;
        let b = color.b() >> 6;
        self.buffer[index as usize] = (r << 5) | (g << 2) | b;
    }

    pub fn get_buffer(&self) -> &Vec<u8> {
        &self.buffer
    }
}

impl Default for VideoBuffer {
    fn default() -> Self {
        VideoBuffer::new(0, 0, 0, PixelFormat::Unknown {
            red_position: 0,
            green_position: 0,
            blue_position: 0,
        })
    }
}

impl Dimensions for VideoBuffer {
    fn bounding_box(&self) -> Rectangle {
        Rectangle::new(Point::new(0, 0), Size::new(self.width, self.height))
    }
}

impl DrawTarget for VideoBuffer {
    type Color = Rgb888;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            let index = (coord.y as u32 * self.width + coord.x as u32) * self.bytes_per_pixel;
            match self.pixel_format{
                PixelFormat::Bgr => {
                    self.set_bgr(index, &color);
                }
                PixelFormat::Rgb => {
                    self.set_rgb(index, &color);
                }
                PixelFormat::U8 => {
                    self.set_u8(index, &color);
                }
                _ => {
                    panic!("Unsupported pixel format");
                }
            }
        }
        Ok(())
    }
}

pub static VIDEO: Lazy<IMutex<VideoBuffer>> = Lazy::new(|| IMutex::<VideoBuffer>::new(VideoBuffer::default()));

pub fn init_video(fb: bootloader_api::info::FrameBuffer) {
    let fb_info = fb.info();
    VIDEO.lock().init(
        fb_info.width as u32,
        fb_info.height as u32,
        fb_info.bytes_per_pixel  as u32,
        fb_info.pixel_format,
    );
}
