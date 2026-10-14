use core::cell::UnsafeCell;
use spin::Lazy;
use x86_64::VirtAddr;
use x86_64::instructions::segmentation::{CS, SS, Segment};
use x86_64::instructions::tables::load_tss;
use x86_64::structures::gdt::{Descriptor, GlobalDescriptorTable, SegmentSelector};
use x86_64::structures::tss::TaskStateSegment;

pub const DOUBLE_FAULT_IST_INDEX: u16 = 0;

/// The TSS, writable after it is loaded: [`set_kernel_stack`] changes the stack
/// the CPU switches to when an interrupt or exception arrives from ring 3.
struct Tss(UnsafeCell<TaskStateSegment>);

// Safety: one CPU. The double-fault stack is written once, before the TSS is
// loaded; `rsp0` only by the scheduler, with interrupts off.
unsafe impl Sync for Tss {}

static TSS: Lazy<Tss> = Lazy::new(|| {
    let mut tss = TaskStateSegment::new();
    tss.interrupt_stack_table[DOUBLE_FAULT_IST_INDEX as usize] = {
        const STACK_SIZE: usize = 4096 * 10;
        static mut STACK: [u8; STACK_SIZE] = [0; STACK_SIZE];

        let stack_start = VirtAddr::from_ptr(&raw const STACK);
        stack_start + STACK_SIZE as u64
    };
    Tss(UnsafeCell::new(tss))
});

/// Order matters to `sysret`, which takes ring 3's selectors from fixed offsets:
/// user data must come right before user code.
static GDT: Lazy<(GlobalDescriptorTable, Selectors)> = Lazy::new(|| {
    let mut gdt = GlobalDescriptorTable::new();
    let code_selector = gdt.append(Descriptor::kernel_code_segment());
    let data_selector = gdt.append(Descriptor::kernel_data_segment());
    // Safety: the descriptor only records the TSS's address.
    let tss_selector = gdt.append(Descriptor::tss_segment(unsafe { &*TSS.0.get() }));
    let user_data_selector = gdt.append(Descriptor::user_data_segment());
    let user_code_selector = gdt.append(Descriptor::user_code_segment());
    let selectors = Selectors {
        code_selector,
        data_selector,
        tss_selector,
        user_code_selector,
        user_data_selector,
    };
    (gdt, selectors)
});

pub struct Selectors {
    pub code_selector: SegmentSelector,
    pub data_selector: SegmentSelector,
    tss_selector: SegmentSelector,
    pub user_code_selector: SegmentSelector,
    pub user_data_selector: SegmentSelector,
}

pub fn selectors() -> &'static Selectors {
    &GDT.1
}

pub fn init_gdt() {
    GDT.0.load();
    unsafe {
        CS::set_reg(GDT.1.code_selector);
        SS::set_reg(GDT.1.data_selector);
        load_tss(GDT.1.tss_selector);
    }
}

/// The stack the CPU switches to when an interrupt or exception arrives while
/// ring 3 is running: the kernel stack of the thread that runs it.
pub fn set_kernel_stack(top: VirtAddr) {
    // Safety: see `Tss`; the CPU reads `rsp0` only when entering ring 0 from
    // ring 3, which can't happen while the kernel is writing it.
    unsafe { (*TSS.0.get()).privilege_stack_table[0] = top };
}
