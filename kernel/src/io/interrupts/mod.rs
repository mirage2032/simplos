pub mod gdt;
pub mod idt;
pub mod pic;
pub fn init_interrupts() {
    gdt::init_gdt();
    idt::init_idt();
    pic::init_pics();
}
