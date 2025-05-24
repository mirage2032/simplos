use crate::io;

pub mod video;
pub mod serial;
pub mod utils;
pub mod interrupts;
pub mod ps2;

pub fn init_io() {
    interrupts::init_interrupts();
    ps2::ps2_controller_init();
    interrupts::pic::config_pics();
}