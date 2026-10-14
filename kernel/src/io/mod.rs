pub mod console;
pub mod disk;
pub mod fwcfg;
pub mod hpet;
pub mod interrupts;
pub mod pci;
pub mod ps2;
pub mod serial;
pub mod utils;
pub mod video;

pub fn init_io() {
    interrupts::init_interrupts();
    ps2::ps2_controller_init();
    interrupts::pic::config_pics();
}
