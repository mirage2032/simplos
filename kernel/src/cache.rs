//! The page cache: what's in files, a page at a time as it's read, kept in
//! frames of its own, so that what's read again comes from memory rather
//! than the disk, and programs that map the same page of a file share one
//! frame (`memory::SHARED`), as programs' code is mapped as they start.
//! What's written goes to the file as it's written, and into whatever pages
//! of it are kept: ext4 tells of every change to a file, whoever makes it
//! (`changed`).
//!
//! Pages no program maps are given back as free memory runs low, those
//! used longest ago first, and all of them when memory runs out (`shrink`).
//! Past `MOST` pages, the oldest go whether programs map them or not: those
//! that do keep their frames, which the cache no longer knows of. So the
//! cache's index, which the kernel's heap holds, stays the size `MOST`
//! makes it, and nothing it does takes more of the heap besides.

use crate::memory::{self, FRAMES};
use alloc::collections::BTreeMap;
use ext4::{Change, Error, File};
use spin::Mutex;
use x86_64::structures::paging::PhysFrame;

/// The most pages kept: 256 MiB, room for rustc's 230 MB librustc_driver.
/// Each costs the kernel's heap, which is a fixed 20 MiB, about 35 bytes in
/// the index.
const MOST: usize = 65536;

/// Bytes in a page.
const PAGE: u64 = 4096;

struct Page {
    /// A shared frame, which the cache holds once.
    frame: PhysFrame,
    /// When it was last used, by `Cache::clock`.
    used: u64,
}

struct Cache {
    /// By file, its inode's number, and page, from the file's start: ext4
    /// files have fewer than 2^32 pages.
    pages: BTreeMap<(u32, u32), Page>,
    /// Counts each use of a page.
    clock: u64,
    /// Pages read in since `make_room` last looked for some to give back.
    read_in: usize,
}

static CACHE: Mutex<Cache> = Mutex::new(Cache { pages: BTreeMap::new(), clock: 0, read_in: 0 });

/// How many pages are kept, for /proc/meminfo.
pub fn pages() -> usize {
    CACHE.lock().pages.len()
}

/// Read into `buf` from `file`, from `offset` on, through the cache: how
/// many bytes, 0 at or past the end. The tree (`fs::lock`) is held.
pub fn read(file: &File, offset: u64, buf: &mut [u8]) -> Result<usize, Error> {
    let len = buf.len().min(file.size()?.saturating_sub(offset) as usize);
    let mut done = 0;
    while done < len {
        let at = offset + done as u64;
        let within = (at % PAGE) as usize;
        let chunk = (len - done).min(PAGE as usize - within);
        with_frame(file, at / PAGE, |frame| {
            // Safety: a frame of the cache's, which only `changed` writes,
            // and the tree it takes is held.
            let page = unsafe { &*bytes(frame) };
            buf[done..done + chunk].copy_from_slice(&page[within..within + chunk]);
        })?;
        done += chunk;
    }
    Ok(len)
}

/// Page `index` of `file`, in a frame held for the caller, who maps it
/// (`AddressSpace::map_shared`) or lets go of it (`release`). The tree is
/// held.
pub fn page(file: &File, index: u64) -> Result<PhysFrame, Error> {
    with_frame(file, index, |frame| {
        FRAMES.lock().as_mut().expect("memory is set up").hold(frame);
        frame
    })
}

/// Run `f` on the frame of page `index` of `file`, read in first if it
/// isn't kept: with the cache held, so it's kept until `f` is done.
fn with_frame<R>(file: &File, index: u64, f: impl FnOnce(PhysFrame) -> R) -> Result<R, Error> {
    let key = (file.inode(), u32::try_from(index).map_err(|_| Error::InvalidArgument)?);
    {
        let mut cache = CACHE.lock();
        cache.clock += 1;
        let now = cache.clock;
        if let Some(page) = cache.pages.get_mut(&key) {
            page.used = now;
            return Ok(f(page.frame));
        }
    }
    // Read without the cache held: it's the disk, which waits.
    let frame = fill(file, index)?;
    let mut cache = CACHE.lock();
    cache.clock += 1;
    let used = cache.clock;
    let kept = cache.pages.entry(key).or_insert(Page { frame, used }).frame;
    // Read in meanwhile by someone else, whose it keeps.
    if kept != frame {
        release(frame);
    }
    Ok(f(kept))
}

/// A frame holding page `index` of `file`, as much of it as there is, the
/// rest zeros, held once: the cache's hold, once it's kept.
fn fill(file: &File, index: u64) -> Result<PhysFrame, Error> {
    make_room();
    let allocate = || FRAMES.lock().as_mut().and_then(|frames| frames.allocate_shared());
    let frame = allocate()
        .or_else(|| {
            shrink();
            allocate()
        })
        .ok_or(Error::OutOfMemory)?;
    // Safety: just allocated, so the cache's alone.
    let page = unsafe { &mut *bytes(frame) };
    page.fill(0);
    let mut done = 0;
    while done < page.len() {
        match file.read_at(index * PAGE + done as u64, &mut page[done..]) {
            Ok(0) => break,
            Ok(read) => done += read,
            Err(error) => {
                release(frame);
                return Err(error);
            }
        }
    }
    Ok(frame)
}

/// The 4 KiB in `frame`, through the physical-memory map.
fn bytes(frame: PhysFrame) -> *mut [u8; PAGE as usize] {
    memory::phys_ptr(frame.start_address()).expect("physical memory is mapped").cast()
}

/// Let go of `frame`, a shared one.
pub fn release(frame: PhysFrame) {
    FRAMES.lock().as_mut().expect("memory is set up").release(frame);
}

/// What ext4 tells of a change to what's in a file (`Ext4::watch`): the
/// pages of it kept made to hold what it does now, or given back. Programs
/// that map them see the change, as on Linux; those given back stay with
/// the programs that map them, as they were.
pub fn changed(inode: u32, change: Change<'_>) {
    // A page's number, as the index has it: ext4 files have fewer pages.
    let index = |page: u64| u32::try_from(page).unwrap_or(u32::MAX);
    let mut cache = CACHE.lock();
    let gone_from = match change {
        Change::Written { offset, bytes: written } if !written.is_empty() => {
            let end = offset + written.len() as u64;
            let pages = (inode, index(offset / PAGE))..=(inode, index((end - 1) / PAGE));
            for (&(_, number), page) in cache.pages.range(pages) {
                let at = u64::from(number) * PAGE;
                let (start, stop) = (at.max(offset), (at + PAGE).min(end));
                let into = (start - at) as usize..(stop - at) as usize;
                let from = (start - offset) as usize..(stop - offset) as usize;
                // Safety: a frame of the cache's, written only here, with
                // the cache held; others only read it.
                let page = unsafe { &mut *bytes(page.frame) };
                page[into].copy_from_slice(&written[from]);
            }
            return;
        }
        Change::Written { .. } => return,
        // What's past the end goes; what's left of the last page past it
        // is zeros.
        Change::Resized { size } => {
            if let Some(page) = cache.pages.get(&(inode, index(size / PAGE))) {
                // Safety: as above.
                let page = unsafe { &mut *bytes(page.frame) };
                page[(size % PAGE) as usize..].fill(0);
            }
            index(size.div_ceil(PAGE))
        }
        Change::Gone => 0,
    };
    // One at a time, rather than all of a big file's numbers gathered first.
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().expect("memory is set up");
    while let Some((&key, _)) = cache.pages.range((inode, gone_from)..=(inode, u32::MAX)).next() {
        let page = cache.pages.remove(&key).expect("just found");
        frames.release(page.frame);
    }
}

/// Give back pages no program maps if free memory is down to a sixteenth
/// of all of it: an eighth of them. And if more than `MOST` are kept, the
/// oldest, mapped or not, down to seven eighths of that. Looked into once
/// an eighth as many pages as are kept have been read in since the last
/// time, as looking goes through them all: while programs map most of
/// them, as rustc maps its 230 MB library, there may be nothing to give
/// back for a long while.
fn make_room() {
    let kept = {
        let mut cache = CACHE.lock();
        cache.read_in += 1;
        if cache.read_in < (cache.pages.len() / 8).max(64) {
            return;
        }
        cache.read_in = 0;
        cache.pages.len()
    };
    let (total, free) = FRAMES.lock().as_ref().map_or((0, 0), |frames| frames.memory());
    if free < total / 16 {
        forget_oldest(kept / 8 + 1, true);
    }
    let kept = CACHE.lock().pages.len();
    if kept > MOST {
        forget_oldest(kept - MOST / 8 * 7, false);
    }
}

/// Keep nothing: what's kept may not be what's on the disk, now another, or
/// the same one again, is mounted. Programs keep the pages they map.
pub fn clear() {
    let mut cache = CACHE.lock();
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().expect("memory is set up");
    for page in core::mem::take(&mut cache.pages).into_values() {
        frames.release(page.frame);
    }
}

/// Give back every page no program maps: for when memory has run out.
pub fn shrink() {
    forget_oldest(usize::MAX, true);
}

/// Give back up to `count` pages, those used longest ago first, by the
/// clock: of those no program maps, if `idle`; or of any, those programs
/// map staying with them. Without gathering and sorting them, which would
/// take the heap that the cache is to leave be: the oldest found in two
/// goes, the first counting them by when they were used, in 64 spans of
/// the clock, and the second giving back those older than the span that
/// makes `count`.
fn forget_oldest(count: usize, idle: bool) {
    const SPANS: usize = 64;
    let mut cache = CACHE.lock();
    let mut frames = FRAMES.lock();
    let Some(frames) = frames.as_mut() else { return };
    let Cache { pages, clock, .. } = &mut *cache;
    let wanted = |page: &Page, frames: &memory::BootInfoFrameAllocator| {
        !idle || frames.holders(page.frame) == 1
    };
    let Some(oldest) =
        pages.values().filter(|page| wanted(page, frames)).map(|page| page.used).min()
    else {
        return;
    };
    let span = (*clock - oldest) / SPANS as u64 + 1;
    let mut counts = [0; SPANS];
    for page in pages.values().filter(|page| wanted(page, frames)) {
        counts[((page.used - oldest) / span) as usize] += 1;
    }
    // Before the end of the first span by which `count` have been used.
    let mut seen = 0;
    let before = counts.iter().position(|&n| {
        seen += n;
        seen >= count
    });
    let before = before.map_or(u64::MAX, |at| oldest + (at as u64 + 1) * span);
    let mut gone = 0;
    pages.retain(|_, page| {
        if gone == count || page.used >= before || !wanted(page, frames) {
            return true;
        }
        frames.release(page.frame);
        gone += 1;
        false
    });
}

#[test_case]
fn gives_back_the_oldest_and_leaves_programs_their_pages() {
    use alloc::vec::Vec;
    // Thirty pages of a made-up file, used in turn: every third one also
    // held, as a program that maps it would.
    const INODE: u32 = u32::MAX;
    let mut held = Vec::new();
    {
        let mut cache = CACHE.lock();
        let mut frames = FRAMES.lock();
        let frames = frames.as_mut().unwrap();
        for number in 0..30 {
            let frame = frames.allocate_shared().unwrap();
            if number % 3 == 0 {
                frames.hold(frame);
                held.push(frame);
            }
            cache.clock += 1;
            let used = cache.clock;
            cache.pages.insert((INODE, number), Page { frame, used });
        }
    }
    let kept = || {
        let cache = CACHE.lock();
        cache.pages.range((INODE, 0)..=(INODE, u32::MAX)).map(|(key, _)| key.1).collect::<Vec<_>>()
    };
    let but = |gone: &[u32]| (0..30).filter(|n| !gone.contains(n)).collect::<Vec<_>>();
    // The five used longest ago of those no program maps.
    forget_oldest(5, true);
    assert_eq!(kept(), but(&[1, 2, 4, 5, 7]));
    // Then the four used longest ago of any, whose frames the programs keep.
    forget_oldest(4, false);
    assert_eq!(kept(), but(&[0, 1, 2, 3, 4, 5, 6, 7, 8]));
    let holders = |frame| FRAMES.lock().as_ref().unwrap().holders(frame);
    assert_eq!(held[..3].iter().map(|&frame| holders(frame)).collect::<Vec<_>>(), [1, 1, 1]);
    // Out of memory: everything no program maps.
    shrink();
    assert_eq!(kept(), [9, 12, 15, 18, 21, 24, 27]);
    // The file gone, nothing of it.
    changed(INODE, Change::Gone);
    assert!(kept().is_empty());
    for frame in held {
        assert_eq!(holders(frame), 1, "a program's page let go of");
        release(frame);
    }
}
