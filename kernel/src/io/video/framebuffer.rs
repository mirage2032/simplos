use embedded_graphics::pixelcolor::{Rgb888, RgbColor};

pub struct Framebuffer {
    pub framebuffer: bootloader_api::info::FrameBuffer,
}

impl Framebuffer{
    pub fn new(framebuffer:bootloader_api::info::FrameBuffer) -> Framebuffer {
        Framebuffer {
            framebuffer
        }
    }
    
    pub fn clear(&mut self,clear_color: &Rgb888){
        let fb_info = self.framebuffer.info();
        for i in 0..fb_info.height {
            for j in 0..fb_info.width {
                self.set_pixel(i, j, &clear_color);
            }
        }
    }

    pub fn set_pixel(&mut self, y: usize, x: usize, color: &Rgb888) {
        let fb_info = self.framebuffer.info();
        let buffer = self.framebuffer.buffer_mut();
        match fb_info.pixel_format{
            bootloader_api::info::PixelFormat::Bgr => {
                let index = (y * fb_info.width + x) * fb_info.bytes_per_pixel;
                buffer[index] = color.b();
                buffer[index + 1] = color.g();
                buffer[index + 2] = color.r();
            },
            bootloader_api::info::PixelFormat::Rgb => {
                let index = (y * fb_info.width + x) * fb_info.bytes_per_pixel;
                buffer[index] = color.r();
                buffer[index + 1] = color.g();
                buffer[index + 2] = color.b();
            },
            _ => {
                panic!("Unsupported pixel format");
            }
        }
    }
}