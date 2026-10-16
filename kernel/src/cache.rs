//! The page cache: what's in files, a page at a time as it's read, kept in
//! frames of its own, so that what's read again comes from memory rather
//! than the disk, and programs that map the same page of a file share one
//! frame (`memory::SHARED`), as programs' code is mapped as they start.
//! What's written goes to the file as it's written, and into whatever pages
//! of it are kept: ext4 tells of every change to a file, whoever makes it
//! (`changed`).
//!
//! Pages no program maps are given back as free memory runs low, and past
//! `MOST` of them, those used longest ago first; and all of them when
//! memory runs out (`shrink`).

use crate::memory::{self, FRAMES};
use alloc::collections::BTreeMap;
use alloc::vec::Vec;
use ext4::{Change, Error, File};
use spin::Mutex;
use x86_64::structures::paging::PhysFrame;

/// The most pages kept, but for what programs map: 256 MiB, room for
/// rustc's 230 MB librustc_driver. Each costs the kernel's heap, which is a
/// fixed 20 MiB, about 70 bytes.
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
    /// By file, its inode's number, and page, from the file's start.
    pages: BTreeMap<(u32, u64), Page>,
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
    let key = (file.inode(), index);
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
    let mut cache = CACHE.lock();
    let gone_from = match change {
        Change::Written { offset, bytes: written } => {
            let end = offset + written.len() as u64;
            let pages = (inode, offset / PAGE)..(inode, end.div_ceil(PAGE));
            for (&(_, index), page) in cache.pages.range(pages) {
                let start = (index * PAGE).max(offset);
                let stop = ((index + 1) * PAGE).min(end);
                let into = (start - index * PAGE) as usize..(stop - index * PAGE) as usize;
                let from = (start - offset) as usize..(stop - offset) as usize;
                // Safety: a frame of the cache's, written only here, with
                // the cache held; others only read it.
                let page = unsafe { &mut *bytes(page.frame) };
                page[into].copy_from_slice(&written[from]);
            }
            return;
        }
        // What's past the end goes; what's left of the last page past it
        // is zeros.
        Change::Resized { size } => {
            if let Some(page) = cache.pages.get(&(inode, size / PAGE)) {
                // Safety: as above.
                let page = unsafe { &mut *bytes(page.frame) };
                page[(size % PAGE) as usize..].fill(0);
            }
            size.div_ceil(PAGE)
        }
        Change::Gone => 0,
    };
    let gone: Vec<_> =
        cache.pages.range((inode, gone_from)..=(inode, u64::MAX)).map(|(k, _)| *k).collect();
    let mut frames = FRAMES.lock();
    let frames = frames.as_mut().expect("memory is set up");
    for key in gone {
        let page = cache.pages.remove(&key).expect("just found");
        frames.release(page.frame);
    }
}

/// Give back pages no program maps if free memory is down to a sixteenth
/// of all of it, or `MOST` are kept: an eighth of them. Looked into once an
/// eighth as many pages as are kept have been read in since the last time,
/// as looking goes through them all: while programs map most of them, as
/// rustc maps its 230 MB library, there may be nothing to give back for a
/// long while.
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
    if free < total / 16 || kept >= MOST {
        give_back(kept / 8 + 1);
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
    give_back(usize::MAX);
}

/// Give back up to `count` of the pages no program maps, those used
/// longest ago first.
fn give_back(count: usize) {
    let mut cache = CACHE.lock();
    let mut frames = FRAMES.lock();
    let Some(frames) = frames.as_mut() else { return };
    let mut idle: Vec<(u64, (u32, u64))> = cache
        .pages
        .iter()
        .filter(|(_, page)| frames.holders(page.frame) == 1)
        .map(|(key, page)| (page.used, *key))
        .collect();
    idle.sort_unstable();
    for (_, key) in idle.into_iter().take(count) {
        let page = cache.pages.remove(&key).expect("just found");
        frames.release(page.frame);
    }
}
