// pub mod vga_buffer;

use alloc::sync::{Arc, Weak};
use crate::io::video::framebuffer::Framebuffer;
use core::ptr::null_mut;
use embedded_graphics::geometry::{Point, Size};
use embedded_graphics::primitives::Rectangle;
use spin::{Lazy, Mutex};

pub mod framebuffer;
pub mod textbuffer;
pub mod types;

pub struct VideoBuffers{
    pub framebuffer: Arc<Mutex<Framebuffer>>,
    pub textbuffer: Arc<Mutex<textbuffer::Textbuffer>>,
}

pub struct Video {
    pub buffers: Option<VideoBuffers>,
}

impl Default for Video {
    fn default() -> Self {
        Self {
            buffers: None,
        }
    }
}

impl Video{
    pub fn init(&mut self,fb: bootloader_api::info::FrameBuffer) {
        let fb_info = fb.info();
        let framebuffer = Arc::new(Mutex::new(Framebuffer::new(fb)));
        let textbuffer = Arc::new(Mutex::new(textbuffer::Textbuffer::new(
            Arc::downgrade(&framebuffer),
            Rectangle::new(
                Point::new(0, 0),
                Size::new(fb_info.width as u32, fb_info.height as u32),
            ),
        )));
        self.buffers = Some(VideoBuffers {
            framebuffer: framebuffer.clone(),
            textbuffer,
        });
    }
}

pub static VIDEO: Lazy<Mutex<Video>> = Lazy::new(|| unsafe {
    Mutex::new(Video::default())
});

pub fn init_video(fb: bootloader_api::info::FrameBuffer) {
    VIDEO.lock().init(fb);
}