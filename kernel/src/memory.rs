use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::{Mutex, Once};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{FrameAllocator, OffsetPageTable, PageTable, PhysFrame, Size4KiB},
};

/// Initialize a new OffsetPageTable.
///
/// # Safety
///
/// The caller must guarantee that the complete physical memory is mapped to
/// virtual memory at the passed `physical_memory_offset`. Also, this function
/// must be only called once to avoid aliasing `&mut` references (which is
/// undefined behavior).
pub unsafe fn init(physical_memory_offset: VirtAddr) -> OffsetPageTable<'static> {
    PHYS_OFFSET.call_once(|| physical_memory_offset);
    unsafe {
        let level_4_table = active_level_4_table(physical_memory_offset);
        OffsetPageTable::new(level_4_table, physical_memory_offset)
    }
}

/// Where all of physical memory is mapped: the start of the upper half.
pub const PHYS_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// Unmap the lower half, which belongs to user programs. All the bootloader
/// leaves there are identity mappings of its own GDT and of the trampoline that
/// jumped to the kernel, both dead once the kernel's GDT is loaded.
pub fn clear_lower_half(mapper: &mut OffsetPageTable) {
    for entry in mapper.level_4_table_mut().iter_mut().take(256) {
        entry.set_unused();
    }
    x86_64::instructions::tlb::flush_all();
}

/// Returns a mutable reference to the active level 4 table.
///
/// This function is unsafe because the caller must guarantee that the
/// complete physical memory is mapped to virtual memory at the passed
/// `physical_memory_offset`. Also, this function must be only called once
/// to avoid aliasing `&mut` references (which is undefined behavior).
unsafe fn active_level_4_table(physical_memory_offset: VirtAddr) -> &'static mut PageTable {
    use x86_64::registers::control::Cr3;

    let (level_4_table_frame, _) = Cr3::read();

    let phys = level_4_table_frame.start_address();
    let virt = physical_memory_offset + phys.as_u64();
    let page_table_ptr: *mut PageTable = virt.as_mut_ptr();

    unsafe { &mut *page_table_ptr }
}

/// Where all of physical memory is mapped, set once by [`init`].
static PHYS_OFFSET: Once<VirtAddr> = Once::new();

/// Where physical address `addr` is mapped. Only the first 4 GiB is
/// guaranteed to be (the bootloader maps up to the end of RAM, and at least that).
pub fn phys_ptr(addr: PhysAddr) -> Option<*mut u8> {
    Some((*PHYS_OFFSET.get()? + addr.as_u64()).as_mut_ptr())
}

/// The frame allocator `pre_init` used for the heap, kept for drivers that
/// need memory with a known physical address.
pub static FRAMES: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// A zeroed 4 KiB frame for a device to DMA into, as its physical address and
/// a pointer the kernel can use. `None` once memory runs out or before boot.
///
/// shortcut: frames are never freed, fine for a few per device at boot; add a
/// free list if a driver starts allocating per request.
pub fn alloc_dma_frame() -> Option<(PhysAddr, *mut u8)> {
    let frame = FRAMES.lock().as_mut()?.allocate_frame()?;
    let ptr = phys_ptr(frame.start_address())?;
    // Safety: the frame is ours alone and mapped at the physical-memory offset.
    unsafe { ptr.write_bytes(0, 4096) };
    Some((frame.start_address(), ptr))
}

/// A FrameAllocator that returns usable frames from the bootloader's memory map.
pub struct BootInfoFrameAllocator {
    memory_map: &'static MemoryRegions,
    /// Index of the region frames are currently taken from.
    region: usize,
    /// Next address to hand out in that region; 0 means its start.
    next: u64,
}

impl BootInfoFrameAllocator {
    /// Create a FrameAllocator from the passed memory map.
    ///
    /// # Safety
    ///
    /// The caller must guarantee that the passed memory map is valid. The main
    /// requirement is that all frames that are marked as `USABLE` in it are
    /// really unused.
    pub unsafe fn init(memory_map: &'static MemoryRegions) -> Self {
        BootInfoFrameAllocator { memory_map, region: 0, next: 0 }
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    /// Picks up where the last call stopped, so handing out n frames costs
    /// O(n) rather than walking the whole map from the start every time.
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        while let Some(region) = self.memory_map.get(self.region) {
            let start = self.next.max(region.start.next_multiple_of(4096));
            if region.kind == MemoryRegionKind::Usable && start + 4096 <= region.end {
                self.next = start + 4096;
                return Some(PhysFrame::containing_address(PhysAddr::new(start)));
            }
            self.region += 1;
            self.next = 0;
        }
        None
    }
}
