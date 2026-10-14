use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use spin::{Mutex, Once};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, FrameDeallocator, OffsetPageTable, PageTable, PhysFrame, Size4KiB,
    },
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

/// The frame allocator `pre_init` used for the heap, kept for everything after
/// that needs physical memory: drivers, and soon page tables and processes.
pub static FRAMES: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// A zeroed 4 KiB frame for a device to DMA into, as its physical address and
/// a pointer the kernel can use. `None` once memory runs out or before boot.
/// Give it back through `FRAMES` with `deallocate_frame` when the device is done.
pub fn alloc_dma_frame() -> Option<(PhysAddr, *mut u8)> {
    let frame = FRAMES.lock().as_mut()?.allocate_frame()?;
    let ptr = phys_ptr(frame.start_address())?;
    // Safety: the frame is ours alone and mapped at the physical-memory offset.
    unsafe { ptr.write_bytes(0, 4096) };
    Some((frame.start_address(), ptr))
}

/// Hands out the usable frames in the bootloader's memory map, and takes them
/// back. A frame comes back holding whatever was in it: zero it before anyone
/// else, a user program above all, can read it.
pub struct BootInfoFrameAllocator {
    memory_map: &'static MemoryRegions,
    /// Index of the region frames are currently taken from.
    region: usize,
    /// Next address to hand out in that region; 0 means its start.
    next: u64,
    /// Frames given back, as a stack threaded through the frames themselves:
    /// each one's first 8 bytes hold the address of the one below it, so the
    /// list costs no memory of its own.
    ///
    /// shortcut: no contiguous runs and no double-free check; add a bitmap when
    /// something needs either (multi-frame DMA buffers, huge pages).
    freed: Option<PhysFrame>,
}

/// Ends the `freed` stack. Not 0, because frame 0 can be usable memory.
const NO_FRAME: u64 = u64::MAX;

/// The first 8 bytes of `frame`, where a freed frame keeps its link.
fn link(frame: PhysFrame) -> *mut u64 {
    // Frames only exist after `init` has set the physical-memory offset.
    phys_ptr(frame.start_address()).expect("physical memory is mapped").cast()
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
        BootInfoFrameAllocator { memory_map, region: 0, next: 0, freed: None }
    }
}

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    /// Reuses the most recently freed frame if there is one. Otherwise picks up
    /// where the last call stopped, so handing out n frames costs O(n) rather
    /// than walking the whole map from the start every time.
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        if let Some(frame) = self.freed {
            // Safety: a freed frame is ours until handed out, which is now.
            let below = unsafe { link(frame).read() };
            self.freed =
                (below != NO_FRAME).then(|| PhysFrame::containing_address(PhysAddr::new(below)));
            return Some(frame);
        }
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

impl FrameDeallocator<Size4KiB> for BootInfoFrameAllocator {
    /// # Safety
    ///
    /// `frame` must have come from this allocator, and nothing may use it any
    /// more: not a mapping, not a device, not a second `deallocate_frame`.
    unsafe fn deallocate_frame(&mut self, frame: PhysFrame) {
        let below = self.freed.map_or(NO_FRAME, |top| top.start_address().as_u64());
        // Safety: the caller has given the frame up, so it is ours to write.
        unsafe { link(frame).write(below) };
        self.freed = Some(frame);
    }
}
