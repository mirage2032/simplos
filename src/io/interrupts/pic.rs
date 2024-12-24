use core::ops::Deref;
use pic8259::ChainedPics;
use spin::{Lazy, Mutex};
use x86_64::instructions::port::Port;
use crate::println;

pub const PIC_1_OFFSET: u8 = 32;
pub const PIC_2_OFFSET: u8 = PIC_1_OFFSET + 8;

#[derive(Debug, Clone, Copy)]
#[repr(u8)]
pub enum InterruptIndex {
    Timer = PIC_1_OFFSET,
    Keyboard,
    PicSlave,
    Com2,
    Com1,
    Parallel23,
    Floppy,
    Parallel1,
    CmosRtc,
    Acpi,
    Free1,
    Free2,
    Ps2Mouse,
    Fpu,
    PrimaryAta,
    SecondaryAta,
}

impl InterruptIndex {
    pub fn as_u8(self) -> u8 {
        self as u8
    }
    pub fn as_usize(self) -> usize {
        self as usize
    }
}

static PICS: Lazy<Mutex<ChainedPics>> = Lazy::new(|| unsafe {
    Mutex::new( ChainedPics::new(PIC_1_OFFSET, PIC_2_OFFSET) )
});

pub fn init_pics() {
    unsafe {
        // Step 1: Initialize the PICs and unmask IRQ2 (cascade) and IRQ12 (mouse)
        PICS.lock().initialize();

        // Read the current interrupt masks
        let mut masks = PICS.lock().read_masks();

        // Unmask IRQ2 on PIC1 (cascade line) and IRQ12 on PIC2 (mouse interrupt)
        let pic2_bit = !(1 << 2);  // Bit 2 corresponds to IRQ2
        let mouse_bit = !(1 << (12 % 8)); // Bit 4 corresponds to IRQ12
        masks[0] &= pic2_bit;      // Clear the mask for IRQ2 on PIC1
        masks[1] &= mouse_bit;     // Clear the mask for IRQ12 on PIC2

        // Write the updated masks back to the PICs
        PICS.lock().write_masks(masks[0], masks[1]);

        let mut ps2_data: Port<u8> = Port::new(0x60);
        let mut ps2_status: Port<u8> = Port::new(0x64);


        //get mouseid
        let mut get_mouse_id = {
            let mut ps2_status = ps2_status.clone();
            let mut ps2_data = ps2_data.clone();
            move || -> Result<u8, u8> {
                ps2_status.write(0xD4);
                ps2_data.write(0xF2);
                let ack = ps2_data.read();
                if ack == 0xFA {
                    Ok(ps2_data.read())
                } else {
                    Err(ack)
                }
            }
        };
        
        println!("Mouseid: {}",get_mouse_id().unwrap());
        
        ps2_status.write(0xAD); // Disable first PS/2 port
        ps2_status.write(0xA7); // Disable second PS/2 port
        ps2_data.read();        // Flush the output buffer
        
        //set configuration byte
        ps2_status.write(0x20);
        let mut config_byte = ps2_data.read();
        println!("0 PS/2 controller configuration byte:{:#b}", config_byte);
        config_byte &= 0b10001100; // Clear the IRQ12 and IRQ1 bits
        config_byte |= 2; // Clear the translation bit
        println!("1 PS/2 controller configuration byte:{:#b}", config_byte);
        
        ps2_status.write(0x60);
        ps2_data.write(config_byte);
        
        //test controller
        ps2_status.write(0xAA); // Test the controller
        let test_result = ps2_data.read();
        if test_result == 0x55 {
            println!("PS/2 controller test passed");
        } else {
            println!("PS/2 controller test failed");
        }

        // //check dual channel
        // ps2_status.write(0xAB);
        
        //test the first PS/2 port
        ps2_status.write(0xAB); // Enable first PS/2 port
        let test_result = ps2_data.read();
        if test_result == 0x00 {
            println!("PS/2 port 1 test passed");
        } else {
            println!("PS/2 port 1 test failed");
        }
        
        //test the second PS/2 port
        ps2_status.write(0xA9); // Enable second PS/2 port
        let test_result = ps2_data.read();
        if test_result == 0x00 {
            println!("PS/2 port 2 test passed");
        } else {
            println!("PS/2 port 2 test failed");
        }

        // default mouse settings
        ps2_status.write(0xD4);
        ps2_data.write(0xF6);
        ps2_data.read();

        let mut set_mouse_rate = |sample_rate: u8| {
            ps2_status.write(0xD4);
            ps2_data.write(0xF3);
            ps2_data.read();
            ps2_status.write(0xD4);
            ps2_data.write(sample_rate);
            ps2_data.read()
        };
        
        //enable Zaxis
        set_mouse_rate(200);
        set_mouse_rate(100);
        set_mouse_rate(80);
        if get_mouse_id().unwrap() == 3 {
            set_mouse_rate(200);
            set_mouse_rate(200);
            set_mouse_rate(80);
        }
        
        set_mouse_rate(200);
        

        // enable zaxis
        
        //enable mouse data reporting
        ps2_status.write(0xD4);
        ps2_data.write(0xF4);
        ps2_data.read();
            
        //set configuration byte
        ps2_status.write(0x20);
        let mut config_byte = ps2_data.read();
        println!("3 PS/2 controller configuration byte:{:#b}", config_byte);
        config_byte |= 0b01110011; // Clear the IRQ12 and IRQ1 bits
        ps2_status.write(0x60);
        ps2_data.write(config_byte);
        println!("4 PS/2 controller configuration byte set:{:#b}", config_byte);
        
        //enable interrupts
        ps2_status.write(0xAE); // Enable first PS/2 port
        ps2_status.write(0xA8); // Enable second PS/2 port
        
        println!("Mouseid after:{}",get_mouse_id().unwrap());
    }
}

pub fn enable_interrupts() {
    x86_64::instructions::interrupts::enable();
}

pub fn disable_interrupts() {
    x86_64::instructions::interrupts::disable();
}

pub fn notify_end_of_interrupt(interrupt_index: &InterruptIndex) {
    unsafe {
        PICS.lock().notify_end_of_interrupt(interrupt_index.as_u8());
    }
}