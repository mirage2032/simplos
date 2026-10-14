pub mod gdt;
pub mod idt;
pub mod pic;
pub mod pit;
pub fn init_interrupts() {
    gdt::init_gdt();
    idt::init_idt();
    pic::init_pics();
    pit::init_pit();
}
