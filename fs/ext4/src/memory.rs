//! lwext4's `malloc`, `calloc`, `realloc` and `free` (it is built with
//! `CONFIG_USE_USER_MALLOC`), on Rust's allocator: the kernel's heap in
//! simplos, the host's in tests. Each block carries its size in a 16-byte
//! header, since `free` isn't told it and Rust's allocator needs it.

use alloc::alloc::{Layout, alloc, alloc_zeroed, dealloc, realloc};
use core::ffi::c_void;
use core::ptr;

/// Also keeps the block 16-byte aligned, as malloc's are.
const HEADER: usize = 16;

fn layout(size: usize) -> Option<Layout> {
    Layout::from_size_align(size.checked_add(HEADER)?, HEADER).ok()
}

fn allocate(size: usize, zeroed: bool) -> *mut c_void {
    let Some(layout) = layout(size) else { return ptr::null_mut() };
    // Safety: the layout is never zero-sized, because of the header.
    let base = unsafe { if zeroed { alloc_zeroed(layout) } else { alloc(layout) } };
    if base.is_null() {
        return ptr::null_mut();
    }
    // Safety: the header is the first 16 bytes of the new block.
    unsafe {
        base.cast::<usize>().write(size);
        base.add(HEADER).cast()
    }
}

/// The start of the block `pointer` came from, and the size it was asked for.
///
/// # Safety
///
/// `pointer` came from one of these functions and hasn't been freed.
unsafe fn block(pointer: *mut c_void) -> (*mut u8, usize) {
    // Safety: the caller's promise.
    unsafe {
        let base = pointer.cast::<u8>().sub(HEADER);
        (base, base.cast::<usize>().read())
    }
}

#[unsafe(no_mangle)]
extern "C" fn ext4_user_malloc(size: usize) -> *mut c_void {
    allocate(size, false)
}

#[unsafe(no_mangle)]
extern "C" fn ext4_user_calloc(count: usize, size: usize) -> *mut c_void {
    match count.checked_mul(size) {
        Some(total) => allocate(total, true),
        None => ptr::null_mut(),
    }
}

#[unsafe(no_mangle)]
extern "C" fn ext4_user_realloc(pointer: *mut c_void, size: usize) -> *mut c_void {
    if pointer.is_null() {
        return allocate(size, false);
    }
    let Some(new) = layout(size) else { return ptr::null_mut() };
    // Safety: lwext4 only passes back what these functions gave it.
    unsafe {
        let (base, old) = block(pointer);
        let base = realloc(base, layout(old).unwrap(), new.size());
        if base.is_null() {
            return ptr::null_mut();
        }
        base.cast::<usize>().write(size);
        base.add(HEADER).cast()
    }
}

#[unsafe(no_mangle)]
extern "C" fn ext4_user_free(pointer: *mut c_void) {
    if pointer.is_null() {
        return;
    }
    // Safety: as in `ext4_user_realloc`.
    unsafe {
        let (base, size) = block(pointer);
        dealloc(base, layout(size).unwrap());
    }
}
