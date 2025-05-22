use alloc::sync::Weak;
use embedded_graphics::prelude::*;
use spin::Mutex;
use crate::io::video::framebuffer::Framebuffer;
use core::borrow::BorrowMut;
use embedded_graphics::mono_font::ascii::FONT_6X10;
use embedded_graphics::mono_font::MonoTextStyle;
use embedded_graphics::pixelcolor::{BinaryColor, Rgb888};
use embedded_graphics::primitives::Rectangle;
use embedded_graphics::text::{Alignment, Text};

pub struct Textbuffer{
    framebuffer: Weak<Mutex<Framebuffer>>,
    area: Rectangle,
}

impl Dimensions for Textbuffer {
    fn bounding_box(&self) -> Rectangle {
        self.area
    }
}

impl DrawTarget for Textbuffer {
    type Color = BinaryColor;
    type Error = core::convert::Infallible;

    fn draw_iter<I>(&mut self, pixels: I) -> Result<(), Self::Error>
    where
        I: IntoIterator<Item = Pixel<Self::Color>>,
    {
        for Pixel(coord, color) in pixels {
            let col = match color {
                BinaryColor::On => Rgb888::new(0, 0, 0),
                BinaryColor::Off => Rgb888::new(255, 255, 255),
            };
            self.framebuffer.upgrade().expect("No framebuffer for text buffer")
                .lock().borrow_mut().set_pixel(coord.y as usize, coord.x as usize, &col);
        }
        Ok(())
    }
}

impl Textbuffer {
    pub fn new(framebuffer: Weak<Mutex<Framebuffer>>, area: Rectangle) -> Self {
        Self { framebuffer, area }
    }
}