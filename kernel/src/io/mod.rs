pub mod video;
pub mod serial;
pub mod utils;
pub mod interrupts;
pub mod ps2;
pub mod hpet;
pub mod console;

pub fn init_io() {
    interrupts::init_interrupts();
    ps2::ps2_controller_init();
    interrupts::pic::config_pics();
}