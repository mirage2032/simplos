use x86_64::instructions::port::Port;

pub mod gdt;
pub mod pic;
pub mod idt;
pub fn init_interrupts() {
    gdt::init_gdt();
    idt::init_idt();
    pic::init_pics();
    pic::enable_interrupts();
}