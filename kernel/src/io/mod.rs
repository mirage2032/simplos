use crate::io;

pub mod video;
pub mod serial;
pub mod utils;
pub mod interrupts;
pub mod ps2;

pub fn init_io(fb: bootloader_api::info::FrameBuffer) {
    video::init_video(fb);
    interrupts::init_interrupts();
    ps2::ps2_controller_init();
    interrupts::pic::config_pics();
    x86_64::instructions::interrupts::enable();
}