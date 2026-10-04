//! Bochs VBE extensions — changing the display mode after boot.
//!
//! The firmware picks the mode before the kernel exists: UEFI through the GOP,
//! BIOS through a VESA call in the bootloader's real-mode stage 2. Neither can
//! be told at runtime, so a kernel that wants a different resolution has to ask
//! the graphics device itself. QEMU's `stdvga` (and `bochs-display`, and VirtualBox,
//! and real Bochs) expose a small index/data register pair for exactly that.
//!
//! The registers are 16-bit: write which one you want to `0x1CE`, then its value
//! to `0x1CF`. A mode takes effect when `ENABLE` is written last.

use x86_64::instructions::port::Port;

const INDEX_PORT: u16 = 0x01CE;
const DATA_PORT: u16 = 0x01CF;

const INDEX_ID: u16 = 0;
const INDEX_XRES: u16 = 1;
const INDEX_YRES: u16 = 2;
const INDEX_BPP: u16 = 3;
const INDEX_ENABLE: u16 = 4;
const INDEX_VIRT_WIDTH: u16 = 6;
const INDEX_X_OFFSET: u16 = 8;
const INDEX_Y_OFFSET: u16 = 9;

const DISABLED: u16 = 0x00;
const ENABLED: u16 = 0x01;
const LFB_ENABLED: u16 = 0x40;

/// Interface revisions, `ID0` through `ID5`. Writing an ID the device doesn't
/// implement makes it report the newest one it does, so the readback is both a
/// presence test and a version query.
const ID_NEWEST: u16 = 0xB0C5;
const ID_OLDEST: u16 = 0xB0C0;

/// The mode the device reports after a change, read back rather than assumed.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Mode {
    pub width: u32,
    pub height: u32,
    /// Pixels per scanline, which the device derives from the width but is
    /// free to round up.
    pub stride: u32,
    pub bytes_per_pixel: u32,
}

fn write(index: u16, value: u16) {
    // Safety: the dispi index and data ports are a fixed pair belonging to the
    // VGA device. Writing them reconfigures the display and nothing else; the
    // caller is responsible for the mode being one the framebuffer can hold.
    unsafe {
        Port::<u16>::new(INDEX_PORT).write(index);
        Port::<u16>::new(DATA_PORT).write(value);
    }
}

fn read(index: u16) -> u16 {
    // Safety: as `write`, and reading the data port has no side effects.
    unsafe {
        Port::<u16>::new(INDEX_PORT).write(index);
        Port::<u16>::new(DATA_PORT).read()
    }
}

/// Whether a device with these extensions is present.
///
/// Absent hardware leaves the ports floating, so they read back as `0xFFFF`
/// rather than an ID — which is the case on real machines, and the reason a
/// failed probe has to be an ordinary "keep the mode we were given" rather than
/// an error.
pub fn present() -> bool {
    write(INDEX_ID, ID_NEWEST);
    (ID_OLDEST..=ID_NEWEST).contains(&read(INDEX_ID))
}

/// Switch to `width` x `height`, keeping the bit depth the firmware chose.
///
/// Returns what the device actually ended up in, which is not necessarily what
/// was asked for — a device may round the stride up, or clamp a dimension it
/// cannot do. Callers must treat the result as authoritative, and must have
/// already checked that the new mode fits in the memory the bootloader mapped:
/// this cannot grow the framebuffer, only re-describe it.
pub fn set_mode(width: u32, height: u32, bytes_per_pixel: u32) -> Option<Mode> {
    if !present() {
        return None;
    }

    // Mode registers are only safe to change while the extension is off, and
    // `ENABLE` is what commits them.
    write(INDEX_ENABLE, DISABLED);
    write(INDEX_XRES, u16::try_from(width).ok()?);
    write(INDEX_YRES, u16::try_from(height).ok()?);
    write(INDEX_BPP, u16::try_from(bytes_per_pixel * 8).ok()?);
    write(INDEX_VIRT_WIDTH, u16::try_from(width).ok()?);
    write(INDEX_X_OFFSET, 0);
    write(INDEX_Y_OFFSET, 0);
    write(INDEX_ENABLE, ENABLED | LFB_ENABLED);

    let bits = u32::from(read(INDEX_BPP));
    let stride = u32::from(read(INDEX_VIRT_WIDTH));
    Some(Mode {
        width: u32::from(read(INDEX_XRES)),
        height: u32::from(read(INDEX_YRES)),
        // A device that reports no virtual width is using the visible one.
        stride: if stride == 0 { width } else { stride },
        bytes_per_pixel: bits.div_ceil(8),
    })
}
