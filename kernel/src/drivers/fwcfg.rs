//! QEMU's firmware configuration device.
//!
//! The only thing we ask it for is `opt/simplos/fb`, the framebuffer mode the
//! host wants (see [`crate::io::video::resolve_mode`]). It is the one channel
//! that can tell a booted kernel something without rebuilding it: QEMU exposes
//! anything passed as `-fw_cfg name=opt/...,string=...` as a named file here.
//!
//! The protocol is two ports and a byte stream. Writing a 16-bit key to the
//! selector port rewinds that item to offset zero; every read of the data port
//! then returns the next byte. Everything the device itself defines is
//! big-endian, which is why the integers below are assembled by hand.

use x86_64::instructions::port::{Port, PortWriteOnly};

const SELECTOR_PORT: u16 = 0x510;
const DATA_PORT: u16 = 0x511;

/// Reads back "QEMU" when the device is really there.
const KEY_SIGNATURE: u16 = 0x0000;
/// The directory of named files, which is how `opt/...` entries are found.
const KEY_FILE_DIR: u16 = 0x0019;

/// A directory entry: 32-bit size, 16-bit selector, 16-bit reserved, 56-byte
/// NUL-padded name.
const ENTRY_LEN: usize = 64;
/// The directory is read as a stream, so a bogus count would be an unbounded
/// loop over a dead port. Real QEMU exposes a few dozen entries.
const MAX_ENTRIES: u32 = 256;

/// Point the device at `key`, which also rewinds it to the start of that item.
fn select(key: u16) {
    // Safety: the fw_cfg selector is a fixed, write-only 16-bit port. Writing a
    // key has no effect beyond choosing what the data port will return.
    unsafe { PortWriteOnly::<u16>::new(SELECTOR_PORT).write(key) };
}

/// Read the next `buf.len()` bytes of the selected item.
fn read_bytes(buf: &mut [u8]) {
    let mut port = Port::<u8>::new(DATA_PORT);
    for byte in buf.iter_mut() {
        // Safety: reading the fw_cfg data port is side-effect free apart from
        // advancing the device's own offset into the selected item.
        *byte = unsafe { port.read() };
    }
}

/// Whether a fw_cfg device is present. On hardware without one the ports float
/// and read back as `0xFF`, so the signature is the only honest test.
pub fn present() -> bool {
    let mut signature = [0u8; 4];
    select(KEY_SIGNATURE);
    read_bytes(&mut signature);
    &signature == b"QEMU"
}

/// Read the named fw_cfg file into `out`, returning how many bytes it holds.
///
/// A file longer than `out` is truncated; `None` means no such file, or no
/// device at all.
pub fn read_file(name: &str, out: &mut [u8]) -> Option<usize> {
    if !present() {
        return None;
    }

    select(KEY_FILE_DIR);
    let mut count = [0u8; 4];
    read_bytes(&mut count);
    let count = u32::from_be_bytes(count).min(MAX_ENTRIES);

    // Entries have to be walked in order: there is no seeking, only the
    // sequential stream that selecting the directory started.
    let mut found = None;
    for _ in 0..count {
        let mut entry = [0u8; ENTRY_LEN];
        read_bytes(&mut entry);
        let size = u32::from_be_bytes([entry[0], entry[1], entry[2], entry[3]]) as usize;
        let key = u16::from_be_bytes([entry[4], entry[5]]);
        let raw = &entry[8..];
        let len = raw.iter().position(|&c| c == 0).unwrap_or(raw.len());
        if &raw[..len] == name.as_bytes() {
            found = Some((key, size));
            break;
        }
    }

    let (key, size) = found?;
    let len = size.min(out.len());
    select(key);
    read_bytes(&mut out[..len]);
    Some(len)
}
