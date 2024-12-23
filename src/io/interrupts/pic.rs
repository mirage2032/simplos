use pic8259::ChainedPics;
use spin::{Lazy, Mutex};

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

static PICS: Lazy<Mutex<ChainedPics>> = Lazy::new(|| unsafe {
    Mutex::new( ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) )
});

pub fn init_pics() {
    unsafe {
        PICS.lock().initialize();
    }
}