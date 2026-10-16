use core::sync::atomic::{AtomicUsize, Ordering};
use fixed_size_block::FixedSizeBlockAllocator;
use x86_64::{
    VirtAddr,
    structures::paging::{
        FrameAllocator, Mapper, Page, PageTableFlags, Size4KiB, mapper::MapToError,
    },
};

pub mod fixed_size_block;

/// Upper half, with a PML4 entry to itself; see the layout in `lib.rs`.
pub const HEAP_START: usize = 0xFFFF_C000_0000_0000;

/// How big the heap is: a sixteenth of the machine's memory, at least
/// 20 MiB and at most 256 MiB, mapped whole as it's set up. What a bigger
/// machine runs takes more of it: the page cache's index; lwext4's
/// journal, which keeps a copy of each block a transaction writes until
/// it's committed; and each program's reads and writes, 64 KiB at a time.
/// With `screen` bytes more for the display's copy of the screen, which is
/// on the heap too, 8 MiB at 1920x1080: but never less than 20 MiB, which a
/// small machine's programs can't spare more than, the screen in it.
pub fn heap_size(memory: u64, screen: usize) -> usize {
    const SMALLEST: usize = 20 << 20;
    const LARGEST: usize = 256 << 20;
    (memory as usize / 16 + screen).clamp(SMALLEST, LARGEST + screen).next_multiple_of(4096)
}

/// What `init_heap` made it.
static SIZE: AtomicUsize = AtomicUsize::new(0);

/// How big the heap is.
pub fn size() -> usize {
    SIZE.load(Ordering::Relaxed)
}

/// What the heap has free in its fallback, where everything bigger than a
/// block comes from: not counting the few blocks kept for their size.
pub fn free() -> usize {
    x86_64::instructions::interrupts::without_interrupts(|| ALLOCATOR.lock().free())
}

#[global_allocator]
static ALLOCATOR: Locked<FixedSizeBlockAllocator> = Locked::new(FixedSizeBlockAllocator::new());

/// Map `size` bytes of heap, and hand them to the allocator.
pub fn init_heap(
    mapper: &mut impl Mapper<Size4KiB>,
    frame_allocator: &mut impl FrameAllocator<Size4KiB>,
    size: usize,
) -> Result<(), MapToError<Size4KiB>> {
    let page_range = {
        let heap_start = VirtAddr::new(HEAP_START as u64);
        let heap_end = heap_start + size as u64 - 1u64;
        let heap_start_page = Page::containing_address(heap_start);
        let heap_end_page = Page::containing_address(heap_end);
        Page::range_inclusive(heap_start_page, heap_end_page)
    };

    for page in page_range {
        let frame = frame_allocator.allocate_frame().ok_or(MapToError::FrameAllocationFailed)?;
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE;
        unsafe { mapper.map_to(page, frame, flags, frame_allocator)?.flush() };
    }

    unsafe {
        ALLOCATOR.lock().init(HEAP_START, size);
    }
    SIZE.store(size, Ordering::Relaxed);

    Ok(())
}

/// A wrapper around spin::Mutex to permit trait implementations.
pub struct Locked<A> {
    inner: spin::Mutex<A>,
}

impl<A> Locked<A> {
    pub const fn new(inner: A) -> Self {
        Locked { inner: spin::Mutex::new(inner) }
    }

    pub fn lock(&self) -> spin::MutexGuard<'_, A> {
        self.inner.lock()
    }
}
