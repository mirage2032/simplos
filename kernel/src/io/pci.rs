//! PCI configuration space, through the legacy `0xCF8`/`0xCFC` port pair.
//!
//! Enough to find a controller by its class code, read its BARs and let it do
//! DMA. Every PC chipset QEMU emulates (and every real one since the 90s)
//! answers on these ports.

use x86_64::instructions::port::Port;

const ADDRESS_PORT: u16 = 0xCF8;
const DATA_PORT: u16 = 0xCFC;

/// A function on the bus: `(bus, device, function)`.
pub type Location = (u8, u8, u8);

fn select((bus, device, function): Location, offset: u8) {
    let address = 1 << 31
        | u32::from(bus) << 16
        | u32::from(device) << 11
        | u32::from(function) << 8
        | u32::from(offset & 0xFC);
    // Safety: the address port only chooses which config register the data
    // port exposes.
    unsafe { Port::<u32>::new(ADDRESS_PORT).write(address) };
}

pub fn read32(location: Location, offset: u8) -> u32 {
    select(location, offset);
    // Safety: reading config space has no side effects.
    unsafe { Port::<u32>::new(DATA_PORT).read() }
}

pub fn write32(location: Location, offset: u8, value: u32) {
    select(location, offset);
    // Safety: callers write only registers they mean to change (command, BARs).
    unsafe { Port::<u32>::new(DATA_PORT).write(value) };
}

/// Every function whose class, subclass and programming interface match.
pub fn find(class: u8, subclass: u8, prog_if: u8) -> impl Iterator<Item = Location> {
    let wanted = u32::from(class) << 24 | u32::from(subclass) << 16 | u32::from(prog_if) << 8;
    (0..=255u8)
        .flat_map(|bus| (0..32u8).map(move |device| (bus, device)))
        .flat_map(|(bus, device)| {
            // No function 0, no device. Functions 1-7 only exist on a device
            // that says it has them.
            let functions = if read32((bus, device, 0), 0) & 0xFFFF == 0xFFFF {
                0
            } else if read32((bus, device, 0), 0x0C) & (0x80 << 16) != 0 {
                8
            } else {
                1
            };
            (0..functions).map(move |function| (bus, device, function))
        })
        .filter(|&location| read32(location, 0) & 0xFFFF != 0xFFFF)
        .filter(move |&location| read32(location, 0x08) & 0xFFFF_FF00 == wanted)
}

/// Let the device decode its memory BARs and master the bus (DMA).
pub fn enable_bus_master_and_memory(location: Location) {
    // The upper half is the status register, whose bits clear when written as
    // 1, so it gets zeros.
    let command = read32(location, 0x04) & 0xFFFF;
    write32(location, 0x04, command | 0b110);
}
