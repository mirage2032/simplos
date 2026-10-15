//! Legacy IDE disks through programmed I/O: every byte goes through a port.
//!
//! Slow, but it needs no PCI, no DMA and no memory setup, and it is how QEMU's
//! default `pc` machine attaches every `-drive` (the boot disk included).

use super::{DiskError, DiskInfo, SECTOR, parse_identify};
use crate::serial_println;
use alloc::vec::Vec;
use x86_64::instructions::port::Port;

/// `(command block, control)` ports of the primary and secondary channels.
const CHANNELS: [(u16, u16); 2] = [(0x1F0, 0x3F6), (0x170, 0x376)];

// Command block registers, as offsets from its base port.
const DATA: u16 = 0;
const ERROR: u16 = 1;
const COUNT: u16 = 2;
const LBA_LOW: u16 = 3;
const LBA_MID: u16 = 4;
const LBA_HIGH: u16 = 5;
const DRIVE: u16 = 6;
const STATUS_COMMAND: u16 = 7;

const ERR: u8 = 1 << 0;
const DRQ: u8 = 1 << 3;
const DF: u8 = 1 << 5;
const BSY: u8 = 1 << 7;

/// Control register bit that keeps the drive from raising interrupts.
const NIEN: u8 = 1 << 1;

const IDENTIFY: u8 = 0xEC;
const READ_SECTORS_EXT: u8 = 0x24;
const WRITE_SECTORS_EXT: u8 = 0x34;
const FLUSH_CACHE_EXT: u8 = 0xEA;

/// LBA48 can count up to 65536 sectors per command; 256 keeps a failed
/// command cheap to retry and is still far more than PIO wants at once.
const MAX_SECTORS: usize = 256;

pub struct Drive {
    base: u16,
    control: u16,
    slave: bool,
}

impl Drive {
    fn read_reg(&self, reg: u16) -> u8 {
        // Safety: the drive's own command block; reads only report its state.
        unsafe { Port::<u8>::new(self.base + reg).read() }
    }

    fn write_reg(&self, reg: u16, value: u8) {
        // Safety: as `read_reg`; what a write does is up to the caller.
        unsafe { Port::<u8>::new(self.base + reg).write(value) }
    }

    /// Point the channel at this drive and give it the ~400 ns it needs before
    /// its status means anything (four reads of the alternate status register).
    fn select(&self) {
        self.write_reg(DRIVE, 0xE0 | u8::from(self.slave) << 4);
        for _ in 0..4 {
            // Safety: reading the alternate status has no side effects.
            unsafe { Port::<u8>::new(self.control).read() };
        }
    }

    /// Wait for BSY to clear and return the status, or the drive's error.
    fn wait_ready(&self) -> Result<u8, DiskError> {
        self.wait_for(0)
    }

    /// Wait until the drive has a sector for us, or wants one.
    fn wait_data(&self) -> Result<(), DiskError> {
        self.wait_for(DRQ).map(|_| ())
    }

    /// Wait for BSY to clear and any of `bits` (for none, just that) to be
    /// set, and return the status; or the drive's error.
    fn wait_for(&self, bits: u8) -> Result<u8, DiskError> {
        let mut status = 0;
        super::poll(|| {
            status = self.read_reg(STATUS_COMMAND);
            status & BSY == 0 && (bits == 0 || status & (bits | ERR | DF) != 0)
        })?;
        if status & (ERR | DF) != 0 {
            return Err(DiskError::Device(self.read_reg(ERROR)));
        }
        Ok(status)
    }

    /// Select the drive and send an LBA48 command.
    fn command(&self, command: u8, lba: u64, count: u16) -> Result<(), DiskError> {
        self.select();
        self.wait_ready()?;
        // Each register holds two bytes in turn: the high halves go first.
        let [c0, c1] = count.to_le_bytes();
        let [l0, l1, l2, l3, l4, l5, ..] = lba.to_le_bytes();
        for (reg, high, low) in
            [(COUNT, c1, c0), (LBA_LOW, l3, l0), (LBA_MID, l4, l1), (LBA_HIGH, l5, l2)]
        {
            self.write_reg(reg, high);
            self.write_reg(reg, low);
        }
        self.write_reg(STATUS_COMMAND, command);
        Ok(())
    }

    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DiskError> {
        let mut data = Port::<u16>::new(self.base + DATA);
        for (i, chunk) in buf.chunks_mut(MAX_SECTORS * SECTOR).enumerate() {
            let start = lba + (i * MAX_SECTORS) as u64;
            self.command(READ_SECTORS_EXT, start, (chunk.len() / SECTOR) as u16)?;
            for sector in chunk.as_chunks_mut::<SECTOR>().0 {
                self.wait_data()?;
                for pair in sector.as_chunks_mut::<2>().0 {
                    // Safety: DRQ is set, so the data port holds this sector.
                    *pair = unsafe { data.read() }.to_le_bytes();
                }
            }
        }
        Ok(())
    }

    pub fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), DiskError> {
        let mut data = Port::<u16>::new(self.base + DATA);
        for (i, chunk) in buf.chunks(MAX_SECTORS * SECTOR).enumerate() {
            let start = lba + (i * MAX_SECTORS) as u64;
            self.command(WRITE_SECTORS_EXT, start, (chunk.len() / SECTOR) as u16)?;
            for sector in chunk.as_chunks::<SECTOR>().0 {
                self.wait_data()?;
                for pair in sector.as_chunks::<2>().0 {
                    // Safety: DRQ is set, so the drive is waiting for this sector.
                    unsafe { data.write(u16::from_le_bytes(*pair)) };
                }
            }
            self.wait_ready()?;
        }
        self.command(FLUSH_CACHE_EXT, 0, 0)?;
        self.wait_ready()?;
        Ok(())
    }

    /// The drive's IDENTIFY data, or `None` if there is no ATA disk here.
    fn identify(&self) -> Option<[u16; 256]> {
        self.select();
        for reg in [COUNT, LBA_LOW, LBA_MID, LBA_HIGH] {
            self.write_reg(reg, 0);
        }
        self.write_reg(STATUS_COMMAND, IDENTIFY);
        if self.read_reg(STATUS_COMMAND) == 0 {
            return None; // nothing attached
        }
        // Only an ATA disk answers here; ATAPI and SATA devices abort the
        // command and leave their signature in the LBA registers instead.
        let mut status = 0;
        super::poll(|| {
            status = self.read_reg(STATUS_COMMAND);
            status & BSY == 0
        })
        .ok()?;
        if status & ERR != 0 || self.read_reg(LBA_MID) != 0 || self.read_reg(LBA_HIGH) != 0 {
            return None;
        }
        self.wait_data().ok()?;
        let mut data = Port::<u16>::new(self.base + DATA);
        // Safety: DRQ is set, so the data port holds the IDENTIFY sector.
        Some(core::array::from_fn(|_| unsafe { data.read() }))
    }
}

pub fn probe() -> Vec<(DiskInfo, Drive)> {
    let mut found = Vec::new();
    for (base, control) in CHANNELS {
        // A channel with nothing on it floats high.
        // Safety: reading a status register has no side effects.
        if unsafe { Port::<u8>::new(base + STATUS_COMMAND).read() } == 0xFF {
            continue;
        }
        // Safety: the control register only takes reset and interrupt-enable bits.
        unsafe { Port::<u8>::new(control).write(NIEN) };
        for slave in [false, true] {
            let drive = Drive { base, control, slave };
            let Some(id) = drive.identify() else { continue };
            match parse_identify(&id) {
                Some((model, sectors)) => found
                    .push((DiskInfo { kind: "ata", model, sectors, ..DiskInfo::default() }, drive)),
                None => {
                    serial_println!("simplos: skipping ATA disk without 512-byte sectors");
                }
            }
        }
    }
    found
}
