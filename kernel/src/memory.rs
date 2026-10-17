use alloc::vec;
use alloc::vec::Vec;
use bootloader_api::info::{MemoryRegionKind, MemoryRegions};
use core::ops::Range;
use core::sync::atomic::{AtomicU64, Ordering};
use spin::{Mutex, Once};
use x86_64::registers::control::{Cr3, Cr3Flags};
use x86_64::{
    PhysAddr, VirtAddr,
    structures::paging::{
        FrameAllocator, FrameDeallocator, Mapper, OffsetPageTable, Page, PageTable, PageTableFlags,
        PhysFrame, Size4KiB, Translate,
        mapper::{MappedFrame, TranslateResult},
        page::PageRange,
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
    KERNEL_PML4.call_once(|| Cr3::read().0);
    unsafe {
        let level_4_table = active_level_4_table(physical_memory_offset);
        OffsetPageTable::new(level_4_table, physical_memory_offset)
    }
}

/// Where all of physical memory is mapped: the start of the upper half.
pub const PHYS_MAP_BASE: u64 = 0xFFFF_8000_0000_0000;

/// The end of the lower half, which belongs to user programs.
pub const USER_END: u64 = 0x0000_8000_0000_0000;

/// The page table the bootloader left active: the kernel's own. Kernel threads
/// run on it, and every address space shares its upper half.
static KERNEL_PML4: Once<PhysFrame> = Once::new();

/// Switch to the kernel's page table, if another is active. Kernel threads run
/// on it, so that freeing a program's page tables never pulls them out from
/// under whoever is running.
pub fn use_kernel_page_table() {
    let kernel = *KERNEL_PML4.get().expect("memory::init ran");
    if Cr3::read().0 != kernel {
        // Safety: the kernel's page table maps everything the kernel uses.
        unsafe { Cr3::write(kernel, Cr3Flags::empty()) };
    }
}

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
/// Take `PAGE_TABLE` first when holding both.
pub static FRAMES: Mutex<Option<BootInfoFrameAllocator>> = Mutex::new(None);

/// The page table `pre_init` mapped the heap with, kept for mapping kernel
/// memory later. Every future address space will share its upper half.
pub static PAGE_TABLE: Mutex<Option<OffsetPageTable<'static>>> = Mutex::new(None);

/// Where kernel thread stacks are mapped: a PML4 entry of its own, see the
/// layout in `lib.rs`.
pub const STACKS_START: u64 = 0xFFFF_D000_0000_0000;

/// Pages in a kernel thread's stack: 80 KiB, what the bootloader gives the
/// boot thread, so code that runs there runs on any thread.
const STACK_PAGES: u64 = 20;

/// `attempt`, and if memory ran out, `attempt` again once the page cache has
/// given back what it can: the frames it keeps are free memory, as far as
/// anything that wants one goes. Whatever `attempt` did before running out
/// must be done again, or kept, the second time.
fn reclaiming<T>(mut attempt: impl FnMut() -> Option<T>) -> Option<T> {
    attempt().or_else(|| {
        crate::cache::shrink();
        attempt()
    })
}

/// A kernel thread's stack, with an unmapped guard page below it so that an
/// overflow faults instead of running into whatever is mapped underneath.
/// Unmapped, and its frames freed, when dropped.
pub struct KernelStack {
    /// The lowest mapped page; the guard page is the one below.
    bottom: Page,
}

impl KernelStack {
    /// `None` when memory runs out.
    pub fn new() -> Option<KernelStack> {
        reclaiming(Self::try_new)
    }

    fn try_new() -> Option<KernelStack> {
        // shortcut: a slot's addresses are never reused, only its frames, so
        // after about 6 million stacks this returns None; keep a free list of
        // slots if anything ever spawns that many.
        const SLOT_BYTES: u64 = (STACK_PAGES + 1) * 4096;
        static NEXT_SLOT: AtomicU64 = AtomicU64::new(0);
        let slot = NEXT_SLOT.fetch_add(1, Ordering::Relaxed);
        // The region is one PML4 entry, 512 GiB.
        if slot >= (1 << 39) / SLOT_BYTES {
            return None;
        }
        let guard = STACKS_START + slot * SLOT_BYTES;
        let stack = KernelStack { bottom: Page::containing_address(VirtAddr::new(guard)) + 1 };

        // After `stack`, so on an early return these are released before its
        // `drop` takes them again to undo a partial mapping.
        let mut pages = PAGE_TABLE.lock();
        let mut frames = FRAMES.lock();
        let (pages, frames) = (pages.as_mut()?, frames.as_mut()?);
        let flags = PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        for page in stack.pages() {
            let frame = frames.allocate_frame()?;
            // Safety: the page is in this stack's slot, which nothing else uses.
            unsafe { pages.map_to(page, frame, flags, frames) }.ok()?.flush();
        }
        Some(stack)
    }

    /// One past the highest byte, which is where a stack starts.
    pub fn top(&self) -> VirtAddr {
        (self.bottom + STACK_PAGES).start_address()
    }

    fn pages(&self) -> impl Iterator<Item = Page> {
        Page::range(self.bottom, self.bottom + STACK_PAGES)
    }
}

impl Drop for KernelStack {
    fn drop(&mut self) {
        let mut pages = PAGE_TABLE.lock();
        let mut frames = FRAMES.lock();
        let (Some(pages), Some(frames)) = (pages.as_mut(), frames.as_mut()) else { return };
        for page in self.pages() {
            // Pages `new` never got to are simply not mapped.
            if let Ok((frame, flush)) = pages.unmap(page) {
                flush.flush();
                // Safety: unmapped just now, and nothing else ever mapped it.
                unsafe { frames.deallocate_frame(frame) };
            }
        }
    }
}

/// A zeroed 4 KiB frame for a device to DMA into, as its physical address and
/// a pointer the kernel can use. `None` once memory runs out or before boot.
/// Give it back through `FRAMES` with `deallocate_frame` when the device is done.
pub fn alloc_dma_frame() -> Option<(PhysAddr, *mut u8)> {
    let frame = reclaiming(|| FRAMES.lock().as_mut()?.allocate_frame())?;
    let ptr = phys_ptr(frame.start_address())?;
    // Safety: the frame is ours alone and mapped at the physical-memory offset.
    unsafe { ptr.write_bytes(0, 4096) };
    Some((frame.start_address(), ptr))
}

/// A page whose frame others may hold too: the page cache, and other
/// address spaces mapping the same page of a file. Never writable, so
/// never written through; copied to a frame of its own before it's made
/// writable (`map`, `protect`), let go of rather than freed (`unmap`), and
/// shared again rather than copied by `fork`. One of the bits the CPU
/// leaves to the OS.
pub const SHARED: PageTableFlags = PageTableFlags::BIT_9;

/// A `SHARED` page a program mapped with MAP_SHARED, whose writes would
/// have to reach the file: which nothing here does, so it's never made
/// writable (`mprotect` refuses), where a copy would quietly keep them from
/// it. Kept with `SHARED` as its protection changes, and by `fork`.
pub const FILE_SHARED: PageTableFlags = PageTableFlags::BIT_10;

/// A user program's memory: a PML4 of its own, whose upper half is the
/// kernel's and whose lower half starts empty. Pages are mapped with fresh,
/// zeroed frames, and dropping it frees every frame and page table in the
/// lower half.
pub struct AddressSpace {
    pml4: PhysFrame,
    /// How many pages the program has mapped, each with a frame.
    pages: u64,
    /// Where the stack grows to as it's used: a page here is mapped as the
    /// program reaches it, or the kernel does for it (`grow`).
    stack: Range<u64>,
}

impl AddressSpace {
    /// `None` when memory runs out, even once the page cache has given back
    /// what it can.
    pub fn new() -> Option<AddressSpace> {
        let pml4 = reclaiming(|| FRAMES.lock().as_mut()?.allocate_frame())?;
        let space = AddressSpace { pml4, pages: 0, stack: 0..0 };
        let table = table(pml4);
        table.zero();
        let kernel = PAGE_TABLE.lock();
        let kernel = kernel.as_ref()?.level_4_table();
        // Only the top level is copied: below it the kernel half is shared.
        // shortcut: a top-level kernel entry created after this isn't seen
        // here; all of them exist by the end of boot, so copy them again if
        // the kernel ever adds a region later.
        for i in 256..512 {
            table[i] = kernel[i].clone();
        }
        Some(space)
    }

    /// A copy, as `fork` makes for the child: the same mappings, each with a
    /// frame of its own holding what the original's holds, but for `SHARED`
    /// ones, which share theirs. `None` when memory runs out, even once the
    /// page cache has given back what it can.
    ///
    /// shortcut: every page is copied at once; share them copy-on-write when
    /// forking big programs gets slow.
    pub fn duplicate(&self) -> Option<AddressSpace> {
        reclaiming(|| self.try_duplicate())
    }

    fn try_duplicate(&self) -> Option<AddressSpace> {
        let mut copy = AddressSpace::new()?;
        copy.pages = self.pages;
        copy.stack = self.stack.clone();
        // Declared after `copy`, so released before a failed copy's `drop`
        // takes it again.
        let mut frames = FRAMES.lock();
        copy_tables(self.pml4, copy.pml4, 4, 256, frames.as_mut()?)?;
        drop(frames);
        Some(copy)
    }

    /// Let the stack grow down over `stack` as it's used.
    pub fn grow_stack(&mut self, stack: Range<u64>) {
        self.stack = stack;
    }

    /// Map the page at `addr`, zeroed, if it's where the stack grows and
    /// isn't yet: whether it's mapped now. For a page fault there, and for
    /// the kernel's own reads and writes of the program's memory, which
    /// Linux grows the stack for too.
    pub fn grow(&mut self, addr: u64) -> bool {
        if !self.stack.contains(&addr) {
            return false;
        }
        let page = Page::containing_address(VirtAddr::new(addr));
        let flags = PageTableFlags::WRITABLE | PageTableFlags::NO_EXECUTE;
        self.map(Page::range(page, page + 1), flags).is_some()
    }

    /// How many pages the program has mapped, each with a frame of its own.
    pub fn pages(&self) -> u64 {
        self.pages
    }

    /// What CR3 holds while the program runs.
    pub fn pml4(&self) -> PhysFrame {
        self.pml4
    }

    fn mapper(&mut self) -> OffsetPageTable<'_> {
        // Safety: the table is this address space's alone, and `&mut self`
        // keeps this the only reference to it.
        unsafe { OffsetPageTable::new(table(self.pml4), VirtAddr::new(PHYS_MAP_BASE)) }
    }

    /// Map fresh zeroed frames over `pages`, accessible to the program, with
    /// `flags` besides. A page already mapped keeps its frame and gets the
    /// union of its flags and these, but a `SHARED` one made writable,
    /// which gets a copy of its own. `None` when memory runs out, even once
    /// the page cache has given back what it can.
    pub fn map(&mut self, pages: PageRange, flags: PageTableFlags) -> Option<()> {
        // Again from the start: what's mapped already stays as it is.
        reclaiming(|| self.try_map(pages, flags))
    }

    fn try_map(&mut self, pages: PageRange, flags: PageTableFlags) -> Option<()> {
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        // The tables above must let ring 3 through too.
        let table_flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        // Counted as they're mapped, even if memory runs out part way.
        let mut mapped = 0;
        let done = (|| {
            let mut frames = FRAMES.lock();
            let frames = frames.as_mut()?;
            let mut mapper = self.mapper();
            for page in pages {
                if let TranslateResult::Mapped { flags: old, .. } =
                    mapper.translate(page.start_address())
                {
                    // Writable if either asks; no-execute only if both do.
                    let nx = PageTableFlags::NO_EXECUTE;
                    let union = ((old | flags) & !nx) | (old & flags & nx);
                    if old.contains(SHARED) && union.contains(PageTableFlags::WRITABLE) {
                        unshare(&mut mapper, page, union, frames)?;
                        continue;
                    }
                    // Safety: the program's own page, keeping its frame.
                    unsafe { mapper.update_flags(page, union) }.ok()?.flush();
                    continue;
                }
                let frame = frames.allocate_frame()?;
                // Zeroed: a frame comes back holding whatever was in it before.
                // Safety: just allocated, so ours alone.
                unsafe { phys_ptr(frame.start_address())?.write_bytes(0, 4096) };
                // Safety: a lower-half page of this address space, unmapped until now.
                let done = unsafe {
                    mapper.map_to_with_table_flags(page, frame, flags, table_flags, frames)
                };
                match done {
                    Ok(flush) => flush.flush(),
                    // No frame for a page table: this one goes back.
                    Err(_) => {
                        // Safety: allocated above, and mapped nowhere.
                        unsafe { frames.deallocate_frame(frame) };
                        return None;
                    }
                }
                mapped += 1;
            }
            Some(())
        })();
        self.pages += mapped;
        done
    }

    /// Map `frame`, a shared one, at `page`, as `SHARED`, and never
    /// writable, whatever `flags` say: the hold on it the caller had is this
    /// mapping's now. What `page` mapped before goes. `None` when memory for
    /// a page table runs out; then the hold is let go.
    pub fn map_shared(
        &mut self,
        page: Page,
        frame: PhysFrame,
        flags: PageTableFlags,
    ) -> Option<()> {
        self.unmap(Page::range(page, page + 1));
        let flags = (flags - PageTableFlags::WRITABLE)
            | SHARED
            | PageTableFlags::PRESENT
            | PageTableFlags::USER_ACCESSIBLE;
        let table_flags =
            PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
        // What can run out is memory for its page tables.
        let mapped = reclaiming(|| {
            let mut frames = FRAMES.lock();
            let frames = frames.as_mut()?;
            // Safety: a lower-half page of this address space, unmapped just
            // now; and a frame whose holders only read it.
            let mapping = unsafe {
                self.mapper().map_to_with_table_flags(page, frame, flags, table_flags, frames)
            };
            mapping.ok().map(|flush| flush.flush())
        });
        if mapped.is_none() {
            FRAMES.lock().as_mut()?.release(frame);
            return None;
        }
        self.pages += 1;
        Some(())
    }

    /// Unmap `pages` and free their frames, or let go of them if they're
    /// `SHARED`, and the page tables that leaves empty. What isn't mapped is
    /// skipped, a whole table's worth at a time where there's no table.
    pub fn unmap(&mut self, pages: PageRange) {
        let mut frames = FRAMES.lock();
        let Some(frames) = frames.as_mut() else { return };
        let range = pages.start.start_address().as_u64()..pages.end.start_address().as_u64();
        if range.is_empty() {
            return;
        }
        let mut emptied = false;
        self.pages -= unmap_tables(self.pml4, 4, 0, &range, frames, &mut emptied);
        if emptied {
            // The CPU may still hold what the tables freed said: before
            // anyone else has their frames, which `FRAMES` held keeps.
            x86_64::instructions::tlb::flush_all();
        }
    }

    /// Zero what's in the mapped pages among `pages`, which stay mapped:
    /// but for `SHARED` ones, which hold what a file does, and stay as they
    /// are.
    pub fn zero(&mut self, pages: PageRange) {
        let mapper = self.mapper();
        for page in pages {
            if let TranslateResult::Mapped { frame, flags, .. } =
                mapper.translate(page.start_address())
                && !flags.contains(SHARED)
                && let Some(at) = phys_ptr(frame.start_address())
            {
                // Safety: a frame of the program's, mapped, 4 KiB.
                unsafe { at.write_bytes(0, 4096) };
            }
        }
    }

    /// Give the mapped pages among `pages` these flags, besides being present
    /// and accessible to the program: a `SHARED` one made writable gets a
    /// copy of its own first. `None` when memory for one runs out, even
    /// once the page cache has given back what it can.
    pub fn protect(&mut self, pages: PageRange, flags: PageTableFlags) -> Option<()> {
        // Again from the start: what's done stays done.
        reclaiming(|| self.try_protect(pages, flags))
    }

    fn try_protect(&mut self, pages: PageRange, flags: PageTableFlags) -> Option<()> {
        let flags = flags | PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let mut frames = FRAMES.lock();
        let frames = frames.as_mut()?;
        let mut mapper = self.mapper();
        for page in pages {
            let TranslateResult::Mapped { flags: old, .. } = mapper.translate(page.start_address())
            else {
                continue;
            };
            if old.contains(SHARED) && flags.contains(PageTableFlags::WRITABLE) {
                unshare(&mut mapper, page, flags, frames)?;
                continue;
            }
            // Safety: the program's own pages, keeping their frames.
            let kept = old & (SHARED | FILE_SHARED);
            if let Ok(flush) = unsafe { mapper.update_flags(page, flags | kept) } {
                flush.flush();
            }
        }
        Some(())
    }

    /// Put the mapped pages among `pages` out of the program's reach, as
    /// PROT_NONE does, their frames and what's in them kept: until `protect`
    /// or `map` gives them back.
    pub fn hide(&mut self, pages: PageRange) {
        let mut mapper = self.mapper();
        for page in pages {
            let TranslateResult::Mapped { flags: old, .. } = mapper.translate(page.start_address())
            else {
                continue;
            };
            let kept = old & (SHARED | FILE_SHARED);
            let flags = PageTableFlags::PRESENT | PageTableFlags::NO_EXECUTE | kept;
            // Safety: the program's own pages, keeping their frames.
            if let Ok(flush) = unsafe { mapper.update_flags(page, flags) } {
                flush.flush();
            }
        }
    }

    /// The flags of `page`, if it's mapped.
    pub fn flags(&mut self, page: Page) -> Option<PageTableFlags> {
        match self.mapper().translate(page.start_address()) {
            TranslateResult::Mapped { flags, .. } => Some(flags),
            _ => None,
        }
    }

    /// Move the `count` pages from `from` on, every one of them mapped, to
    /// as many from `to` on, none of them mapped, as `mremap` does: each
    /// with its frame and flags, so what's in them goes along, copied by
    /// none. `None`, with nothing moved, when memory for a page table runs
    /// out.
    pub fn move_pages(&mut self, from: Page, to: Page, count: u64) -> Option<()> {
        let mut frames = FRAMES.lock();
        let frames = frames.as_mut()?;
        let mut mapper = self.mapper();
        for moved in 0..count {
            if !move_page(&mut mapper, from + moved, to + moved, frames) {
                // Back where they were, whose tables are all still there.
                for back in 0..moved {
                    move_page(&mut mapper, to + back, from + back, frames);
                }
                return None;
            }
        }
        Some(())
    }

    /// Copy `bytes` to `addr`, whatever the pages' protection, but never
    /// into a `SHARED` one: for the loader.
    pub fn load(&mut self, addr: u64, bytes: &[u8]) -> Option<()> {
        self.copy(addr, bytes.len(), PageTableFlags::PRESENT, SHARED, |at, range| {
            // Safety: `copy` hands out mapped frames, `range.len()` long.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[range.clone()].as_ptr(), at, range.len())
            }
        })
    }

    /// Copy `bytes` to the program's memory at `addr`, which must be mapped
    /// writable for it. `None` if any of it isn't: the system call's EFAULT.
    pub fn write(&mut self, addr: u64, bytes: &[u8]) -> Option<()> {
        let need =
            PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE | PageTableFlags::WRITABLE;
        self.copy(addr, bytes.len(), need, SHARED, |at, range| {
            // Safety: as in `load`.
            unsafe {
                core::ptr::copy_nonoverlapping(bytes[range.clone()].as_ptr(), at, range.len())
            }
        })
    }

    /// Fill `buf` from the program's memory at `addr`, which must be mapped
    /// for it. `None` if any of it isn't: the system call's EFAULT.
    pub fn read(&mut self, addr: u64, buf: &mut [u8]) -> Option<()> {
        let need = PageTableFlags::PRESENT | PageTableFlags::USER_ACCESSIBLE;
        let anything = PageTableFlags::empty();
        self.copy(addr, buf.len(), need, anything, |at, range| {
            // Safety: as in `load`.
            unsafe {
                core::ptr::copy_nonoverlapping(at, buf[range.clone()].as_mut_ptr(), range.len())
            }
        })
    }

    /// Walk `len` bytes from `addr` a page at a time, through the page tables
    /// rather than the addresses themselves, so it works from any address
    /// space and an unmapped page is an error instead of a page fault: as
    /// is one without all of `need`, or with any of `forbid`.
    fn copy(
        &mut self,
        addr: u64,
        len: usize,
        need: PageTableFlags,
        forbid: PageTableFlags,
        mut each: impl FnMut(*mut u8, Range<usize>),
    ) -> Option<()> {
        if addr.checked_add(len as u64)? > USER_END {
            return None;
        }
        let mut done = 0;
        while done < len {
            let at = addr + done as u64;
            let translated = self.mapper().translate(VirtAddr::new(at));
            let TranslateResult::Mapped { frame, offset, flags } = translated else {
                // Where the stack hasn't reached yet, it does now.
                if self.grow(at) {
                    continue;
                }
                return None;
            };
            if !flags.contains(need) || flags.intersects(forbid) {
                return None;
            }
            let chunk = (len - done).min(4096 - (at % 4096) as usize);
            each(phys_ptr(frame.start_address() + offset)?, done..done + chunk);
            done += chunk;
        }
        Some(())
    }
}

impl Drop for AddressSpace {
    fn drop(&mut self) {
        let mut frames = FRAMES.lock();
        let Some(frames) = frames.as_mut() else { return };
        free_tables(self.pml4, 4, 256, frames);
    }
}

/// Free every frame mapped below `frame`, a page table of `level`, through its
/// first `entries` entries, then the table itself. Only the lower half's
/// tables and frames are ever passed here: the upper half's are the kernel's.
fn free_tables(frame: PhysFrame, level: u8, entries: usize, frames: &mut BootInfoFrameAllocator) {
    for entry in table(frame).iter().take(entries) {
        if entry.flags().contains(PageTableFlags::PRESENT) {
            // Never made: `map` maps 4 KiB pages only.
            debug_assert!(!entry.flags().contains(PageTableFlags::HUGE_PAGE));
            let below = PhysFrame::containing_address(entry.addr());
            if level > 1 {
                free_tables(below, level - 1, 512, frames);
            } else if entry.flags().contains(SHARED) {
                frames.release(below);
            } else {
                // Safety: a page of the program's, which is gone.
                unsafe { frames.deallocate_frame(below) };
            }
        }
    }
    // Safety: a table only the dropped address space used.
    unsafe { frames.deallocate_frame(frame) };
}

/// Unmap what `frame`, a page table of `level` mapping from `base` on, maps
/// of `range`, freeing the frames or letting go of `SHARED` ones, and free
/// the tables below it that leaves empty, setting `emptied` if any: how many
/// pages it unmapped. Only lower-half ranges that reach into what `frame`
/// maps are passed here.
fn unmap_tables(
    frame: PhysFrame,
    level: u8,
    base: u64,
    range: &Range<u64>,
    frames: &mut BootInfoFrameAllocator,
    emptied: &mut bool,
) -> u64 {
    let span = 4096u64 << (9 * (level - 1));
    let first = range.start.saturating_sub(base) / span;
    let last = ((range.end - 1 - base) / span).min(511);
    let entries = table(frame);
    let mut unmapped = 0;
    for i in first..=last {
        let (entry, start) = (&mut entries[i as usize], base + i * span);
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let below = PhysFrame::containing_address(entry.addr());
        if level == 1 {
            let shared = entry.flags().contains(SHARED);
            entry.set_unused();
            x86_64::instructions::tlb::flush(VirtAddr::new(start));
            match shared {
                true => frames.release(below),
                // Safety: unmapped from the only address space that had it.
                false => unsafe { frames.deallocate_frame(below) },
            }
            unmapped += 1;
            continue;
        }
        unmapped += unmap_tables(below, level - 1, start, range, frames, emptied);
        if table(below).iter().all(|entry| entry.is_unused()) {
            entry.set_unused();
            // Safety: a table of this address space's alone, now unlinked.
            unsafe { frames.deallocate_frame(below) };
            *emptied = true;
        }
    }
    unmapped
}

/// Copy what's mapped below `from`, a page table of `level`, through its
/// first `entries` entries, into `to`, an empty one of the same level: new
/// tables, and new frames holding the same bytes, but for `SHARED` pages,
/// whose frames are held once more instead. Each new table is linked into
/// `to` before anything goes into it, so a copy cut short by running out
/// of memory is still freed whole, by its address space's `drop`.
fn copy_tables(
    from: PhysFrame,
    to: PhysFrame,
    level: u8,
    entries: usize,
    frames: &mut BootInfoFrameAllocator,
) -> Option<()> {
    for (i, entry) in table(from).iter().enumerate().take(entries) {
        if !entry.flags().contains(PageTableFlags::PRESENT) {
            continue;
        }
        let below = PhysFrame::containing_address(entry.addr());
        if level == 1 && entry.flags().contains(SHARED) {
            frames.hold(below);
            table(to)[i].set_frame(below, entry.flags());
            continue;
        }
        let frame = frames.allocate_frame()?;
        if level > 1 {
            table(frame).zero();
            table(to)[i].set_frame(frame, entry.flags());
            copy_tables(below, frame, level - 1, 512, frames)?;
        } else {
            let (source, target) =
                (phys_ptr(below.start_address())?, phys_ptr(frame.start_address())?);
            // Safety: a page of the program's, and a frame just allocated.
            unsafe { core::ptr::copy_nonoverlapping(source, target, 4096) };
            table(to)[i].set_frame(frame, entry.flags());
        }
    }
    Some(())
}

/// Give `page`, mapped `SHARED`, a frame of its own holding what the shared
/// one does, mapped with `flags`, letting go of the shared one: as copy on
/// write does, done before it's written. `None` when memory runs out; then
/// it's as it was.
fn unshare(
    mapper: &mut OffsetPageTable,
    page: Page,
    flags: PageTableFlags,
    frames: &mut BootInfoFrameAllocator,
) -> Option<()> {
    let table_flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let TranslateResult::Mapped { frame: MappedFrame::Size4KiB(shared), .. } =
        mapper.translate(page.start_address())
    else {
        return None;
    };
    let own = frames.allocate_frame()?;
    let (source, target) = (phys_ptr(shared.start_address())?, phys_ptr(own.start_address())?);
    // Safety: a frame the page maps, and one just allocated.
    unsafe { core::ptr::copy_nonoverlapping(source, target, 4096) };
    let (_, flush) = mapper.unmap(page).ok()?;
    flush.flush();
    frames.release(shared);
    // Safety: the page just unmapped, whose tables are still there, now with
    // a frame no one else has.
    let flags = flags - SHARED - FILE_SHARED;
    unsafe { mapper.map_to_with_table_flags(page, own, flags, table_flags, frames) }
        .expect("a page's tables stay as it's unmapped")
        .flush();
    Some(())
}

/// Move the page at `from`, which is mapped, to `to`, which isn't, with its
/// frame and flags: whether it went, which only a page table for `to` that
/// couldn't be had stops. Then it's still at `from`.
fn move_page(
    mapper: &mut OffsetPageTable,
    from: Page,
    to: Page,
    frames: &mut BootInfoFrameAllocator,
) -> bool {
    let table_flags =
        PageTableFlags::PRESENT | PageTableFlags::WRITABLE | PageTableFlags::USER_ACCESSIBLE;
    let TranslateResult::Mapped { flags, .. } = mapper.translate(from.start_address()) else {
        return false;
    };
    let Ok((frame, flush)) = mapper.unmap(from) else { return false };
    flush.flush();
    // Safety: the program's own frame, which `from` no longer maps; and
    // `to` is a lower-half page of the same address space.
    if let Ok(flush) =
        unsafe { mapper.map_to_with_table_flags(to, frame, flags, table_flags, frames) }
    {
        flush.flush();
        return true;
    }
    // Safety: as above, back where it was, whose tables unmapping kept.
    unsafe { mapper.map_to_with_table_flags(from, frame, flags, table_flags, frames) }
        .expect("a page's tables stay as it's unmapped")
        .flush();
    false
}

/// The page table in `frame`, through the physical-memory map.
fn table(frame: PhysFrame) -> &'static mut PageTable {
    let ptr = phys_ptr(frame.start_address()).expect("physical memory is mapped");
    // Safety: page tables are frames of their own; callers hold the only use.
    unsafe { &mut *ptr.cast::<PageTable>() }
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
    /// How many frames are handed out now.
    used: u64,
    /// How many hold each frame handed out to be held by more than one
    /// holder (`SHARED` pages), by the frame's number; 0 for the rest. The
    /// last to let go of one frees it. Made by `count_holders` as big as the
    /// highest usable frame needs, once and in one piece, so that counting
    /// takes no more of the heap as more is shared: 1 MiB for 1 GiB. Four
    /// bytes each, as a program can map one page of a file as often as it
    /// likes, each time holding it once more: two would run out at 65536.
    ///
    /// shortcut: on a machine with more than about 64 GiB, these take more
    /// than a quarter of the kernel's heap; count only those shared, by
    /// number, if one turns up.
    shared: Vec<u32>,
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
        BootInfoFrameAllocator {
            memory_map,
            region: 0,
            next: 0,
            freed: None,
            used: 0,
            shared: Vec::new(),
        }
    }

    /// Make room to count the holders of every frame there is, now that
    /// the kernel's heap is there to keep the counts in: before the first
    /// `allocate_shared`.
    pub fn count_holders(&mut self) {
        let usable =
            self.memory_map.iter().filter(|region| region.kind == MemoryRegionKind::Usable);
        let end = usable.map(|region| region.end).max().unwrap_or(0);
        self.shared = vec![0; (end / 4096) as usize];
    }

    /// The count of `frame`'s holders.
    fn holders_of(&mut self, frame: PhysFrame) -> &mut u32 {
        &mut self.shared[(frame.start_address().as_u64() / 4096) as usize]
    }

    /// The bytes of memory there are to hand out, and of those the bytes
    /// not handed out now.
    pub fn memory(&self) -> (u64, u64) {
        let usable =
            self.memory_map.iter().filter(|region| region.kind == MemoryRegionKind::Usable);
        let frames: u64 = usable
            .map(|region| (region.end / 4096).saturating_sub(region.start.div_ceil(4096)))
            .sum();
        (frames * 4096, (frames - self.used) * 4096)
    }

    /// A frame for more than one holder to hold, held once, by whoever
    /// asked: zero it before anyone else reads it.
    pub fn allocate_shared(&mut self) -> Option<PhysFrame> {
        let frame = self.allocate_frame()?;
        *self.holders_of(frame) = 1;
        Some(frame)
    }

    /// Hold `frame`, a shared one, once more.
    pub fn hold(&mut self, frame: PhysFrame) {
        let holders = self.holders_of(frame);
        assert!(*holders > 0, "a shared frame");
        // Each holder is a page-table entry, or the cache: never 2^32.
        *holders = holders.checked_add(1).expect("fewer holders than page-table entries");
    }

    /// Let go of `frame`, a shared one: the last holder to frees it.
    pub fn release(&mut self, frame: PhysFrame) {
        let holders = self.holders_of(frame);
        assert!(*holders > 0, "a shared frame");
        *holders -= 1;
        if *holders == 0 {
            // Safety: no one holds it any more.
            unsafe { self.deallocate_frame(frame) };
        }
    }

    /// How many hold `frame`, a shared one: 0 for one that isn't.
    pub fn holders(&self, frame: PhysFrame) -> u32 {
        let index = (frame.start_address().as_u64() / 4096) as usize;
        self.shared.get(index).copied().unwrap_or(0)
    }

    /// The most recently freed frame, or else the next never handed out.
    fn take(&mut self) -> Option<PhysFrame> {
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

unsafe impl FrameAllocator<Size4KiB> for BootInfoFrameAllocator {
    /// Reuses the most recently freed frame if there is one. Otherwise picks up
    /// where the last call stopped, so handing out n frames costs O(n) rather
    /// than walking the whole map from the start every time.
    fn allocate_frame(&mut self) -> Option<PhysFrame> {
        let frame = self.take();
        self.used += u64::from(frame.is_some());
        frame
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
        self.used -= 1;
    }
}

#[test_case]
fn kernel_stacks_sit_above_a_guard_page() {
    use x86_64::structures::paging::Translate;
    let mapped = |addr: VirtAddr| {
        PAGE_TABLE
            .lock()
            .as_ref()
            .expect("pre_init keeps the page table")
            .translate_addr(addr)
            .is_some()
    };
    let stack = KernelStack::new().expect("memory for a stack");
    let (top, bottom) = (stack.top() - 8u64, stack.bottom.start_address());
    // Safety: both ends of the stack just mapped.
    unsafe {
        top.as_mut_ptr::<u64>().write(1);
        bottom.as_mut_ptr::<u64>().write(1);
    }
    assert!(mapped(top) && mapped(bottom));
    assert!(!mapped(bottom - 1u64), "the guard page below the stack is mapped");
    drop(stack);
    assert!(!mapped(top) && !mapped(bottom), "a dropped stack is still mapped");
}

#[test_case]
fn a_shared_page_made_writable_is_copied_first() {
    // A frame of the kind the page cache shares, holding 0x11s, mapped
    // read-only; then made writable, by `protect` and by `map`: each time
    // the page gets a frame of its own holding the same, and the shared one
    // is let go of, as it was.
    let frame = FRAMES.lock().as_mut().unwrap().allocate_shared().unwrap();
    let bytes = || phys_ptr(frame.start_address()).unwrap();
    // Safety: the frame just allocated, 4 KiB.
    unsafe { bytes().write_bytes(0x11, 4096) };
    let holders = || FRAMES.lock().as_ref().unwrap().holders(frame);
    let page = Page::containing_address(VirtAddr::new(0x1000_0000));
    let writable = PageTableFlags::WRITABLE;
    let ways: [fn(&mut AddressSpace, PageRange) -> Option<()>; 2] = [
        |space, pages| space.protect(pages, PageTableFlags::WRITABLE),
        |space, pages| space.map(pages, PageTableFlags::WRITABLE),
    ];
    for made_writable in ways {
        let mut space = AddressSpace::new().unwrap();
        FRAMES.lock().as_mut().unwrap().hold(frame);
        space.map_shared(page, frame, PageTableFlags::empty()).unwrap();
        assert_eq!(space.flags(page).map(|flags| flags.contains(SHARED)), Some(true));
        assert_eq!(holders(), 2);
        made_writable(&mut space, Page::range(page, page + 1)).unwrap();
        let flags = space.flags(page).unwrap();
        assert!(flags.contains(writable) && !flags.contains(SHARED), "{flags:?}");
        assert_eq!(holders(), 1, "the shared frame is still held");
        let mut read = [0; 2];
        space.write(0x1000_0000, &[0x22]).unwrap();
        space.read(0x1000_0000, &mut read).unwrap();
        assert_eq!(read, [0x22, 0x11]);
        // Safety: as above.
        assert_eq!(unsafe { bytes().read() }, 0x11, "written through the shared frame");
    }
    FRAMES.lock().as_mut().unwrap().release(frame);
    assert_eq!(holders(), 0);
}

#[test_case]
fn unmapping_frees_the_tables_it_empties_and_skips_those_not_there() {
    let free = || FRAMES.lock().as_ref().unwrap().memory().1;
    let mut space = AddressSpace::new().unwrap();
    let before = free();
    // Pages 2 MiB apart, each in a table of its own.
    let at = |i: u64| Page::containing_address(VirtAddr::new(0x1000_0000_0000 + (i << 21)));
    for i in 0..64 {
        space.map(Page::range(at(i), at(i) + 1), PageTableFlags::WRITABLE).unwrap();
    }
    assert!(free() < before);
    // All of the lower half: a page at a time, hours.
    let end = Page::containing_address(VirtAddr::new(USER_END - 4096));
    space.unmap(Page::range(Page::containing_address(VirtAddr::new(0x1000)), end));
    assert_eq!(space.pages(), 0);
    assert_eq!(free(), before, "the tables are still there");
}

#[test_case]
fn a_frame_takes_more_holders_than_a_u16_counts() {
    // As one page of a file mapped 70000 times over by one program: each
    // mapping holds the cache's frame once more.
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().unwrap();
    let frame = frames.allocate_shared().unwrap();
    for _ in 0..70_000 {
        frames.hold(frame);
    }
    assert_eq!(frames.holders(frame), 70_001);
    for _ in 0..70_000 {
        frames.release(frame);
    }
    assert_eq!(frames.holders(frame), 1);
    frames.release(frame);
    assert_eq!(frames.holders(frame), 0);
}
