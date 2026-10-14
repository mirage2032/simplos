//! SATA disks behind an AHCI controller, one command at a time, polled.
//!
//! The controller does the transfer itself (DMA), so it needs memory it can
//! find by physical address: per port, a command list, a place to drop the
//! disk's replies (FIS = frame information structure, a SATA packet) and a
//! command table. All of that, plus a bounce buffer for the data, lives in two
//! frames from [`crate::memory::alloc_dma_frame`].

use super::{DiskError, DiskInfo, SECTOR, parse_identify, words};
use crate::io::pci;
use crate::memory::{alloc_dma_frame, phys_ptr};
use crate::serial_println;
use alloc::vec::Vec;
use core::sync::atomic::{Ordering, fence};
use x86_64::PhysAddr;

// Controller (HBA) registers.
const CAP: usize = 0x00;
const GHC: usize = 0x04;
const PI: usize = 0x0C;
const CAP_S64A: u32 = 1 << 31;
const GHC_AE: u32 = 1 << 31;

// Port registers, as offsets from the port's own block.
const PX_CLB: usize = 0x00;
const PX_CLBU: usize = 0x04;
const PX_FB: usize = 0x08;
const PX_FBU: usize = 0x0C;
const PX_IS: usize = 0x10;
const PX_IE: usize = 0x14;
const PX_CMD: usize = 0x18;
const PX_TFD: usize = 0x20;
const PX_SIG: usize = 0x24;
const PX_SSTS: usize = 0x28;
const PX_SERR: usize = 0x30;
const PX_CI: usize = 0x38;

const CMD_ST: u32 = 1 << 0;
const CMD_SUD: u32 = 1 << 1;
const CMD_POD: u32 = 1 << 2;
const CMD_FRE: u32 = 1 << 4;
const CMD_FR: u32 = 1 << 14;
const CMD_CR: u32 = 1 << 15;
const IS_TFES: u32 = 1 << 30;
const TFD_ERR: u32 = 1 << 0;
const TFD_DRQ: u32 = 1 << 3;
const TFD_BSY: u32 = 1 << 7;

/// What a plain SATA disk (not ATAPI, not a port multiplier) reports.
const SIG_ATA: u32 = 0x0000_0101;

const IDENTIFY: u8 = 0xEC;
const READ_DMA_EXT: u8 = 0x25;
const WRITE_DMA_EXT: u8 = 0x35;
const FLUSH_CACHE_EXT: u8 = 0xEA;

// Layout of the two frames. First: the command list (1 KiB-aligned) and the
// received-FIS area (256-aligned). Second: the command table (128-aligned)
// with its single PRDT entry (the data descriptor), then the bounce buffer.
const FIS_AREA: usize = 1024;
const PRDT: usize = 0x80;
const BOUNCE: usize = 2048;
const BOUNCE_SECTORS: usize = (4096 - BOUNCE) / SECTOR;

/// shortcut: spin-count timeouts, not wall-clock; use the HPET if real disks prove slower.
const SPINS: u32 = 1_000_000;

pub struct Port {
    /// The port's register block, mapped.
    regs: *mut u8,
    list: (PhysAddr, *mut u8),
    table: (PhysAddr, *mut u8),
}

// Safety: the pointers are to MMIO and DMA frames this `Port` alone uses, and
// it is only ever reached through the `DISKS` lock.
unsafe impl Send for Port {}

fn read(regs: *mut u8, offset: usize) -> u32 {
    // Safety: `regs` is a mapped AHCI register block and `offset` one of its registers.
    unsafe { regs.add(offset).cast::<u32>().read_volatile() }
}

fn write(regs: *mut u8, offset: usize, value: u32) {
    // Safety: as `read`.
    unsafe { regs.add(offset).cast::<u32>().write_volatile(value) }
}

fn spin_until(mut done: impl FnMut() -> bool) -> Result<(), DiskError> {
    (0..SPINS).find(|_| done()).map(|_| ()).ok_or(DiskError::Timeout)
}

impl Port {
    fn reg(&self, offset: usize) -> u32 {
        read(self.regs, offset)
    }

    fn set(&self, offset: usize, value: u32) {
        write(self.regs, offset, value)
    }

    /// Stop the port's command engine and FIS receiving, which must both be
    /// off before their memory can change.
    fn stop(&self) -> Result<(), DiskError> {
        self.set(PX_CMD, self.reg(PX_CMD) & !CMD_ST);
        spin_until(|| self.reg(PX_CMD) & CMD_CR == 0)?;
        self.set(PX_CMD, self.reg(PX_CMD) & !CMD_FRE);
        spin_until(|| self.reg(PX_CMD) & CMD_FR == 0)
    }

    fn start(&self) -> Result<(), DiskError> {
        self.set(PX_SERR, u32::MAX);
        self.set(PX_IS, u32::MAX);
        self.set(PX_CMD, self.reg(PX_CMD) | CMD_FRE | CMD_SUD | CMD_POD);
        self.set(PX_CMD, self.reg(PX_CMD) | CMD_ST);
        spin_until(|| self.reg(PX_TFD) & (TFD_BSY | TFD_DRQ) == 0)
    }

    /// Run one command on slot 0 and wait for it. `sectors` of data move
    /// through the bounce buffer: in from the disk, or out to it if `write`.
    fn issue(
        &mut self,
        command: u8,
        lba: u64,
        sectors: usize,
        write: bool,
    ) -> Result<(), DiskError> {
        let (table_phys, table) = self.table;
        let header = self.list.1.cast::<u32>();
        let bytes = (sectors * SECTOR) as u32;
        let [l0, l1, l2, l3, l4, l5, ..] = lba.to_le_bytes();
        let [c0, c1] = (sectors as u16).to_le_bytes();
        // Host-to-device register FIS: type, "this is a command", the command,
        // then the LBA48 address and count as the ATA registers would hold them.
        let fis = [0x27, 0x80, command, 0, l0, l1, l2, 0x40, l3, l4, l5, 0, c0, c1, 0, 0];
        let bounce = table_phys + BOUNCE as u64;
        // Safety: both frames belong to this port, and the port is idle (no
        // command issued), so the controller isn't reading them.
        unsafe {
            // Command header 0: FIS length 5 dwords, write flag, one PRDT entry
            // if there is data, and where the command table is.
            let prdt_entries = u32::from(sectors > 0);
            header.write_volatile(5 | u32::from(write) << 6 | prdt_entries << 16);
            header.add(1).write_volatile(0);
            header.add(2).write_volatile(table_phys.as_u64() as u32);
            header.add(3).write_volatile((table_phys.as_u64() >> 32) as u32);
            for (i, byte) in fis.into_iter().enumerate() {
                table.add(i).write_volatile(byte);
            }
            let prdt = table.add(PRDT).cast::<u32>();
            prdt.write_volatile(bounce.as_u64() as u32);
            prdt.add(1).write_volatile((bounce.as_u64() >> 32) as u32);
            prdt.add(2).write_volatile(0);
            prdt.add(3).write_volatile(bytes.saturating_sub(1));
        }

        self.set(PX_IS, u32::MAX);
        fence(Ordering::SeqCst);
        self.set(PX_CI, 1);
        let finished = spin_until(|| self.reg(PX_CI) & 1 == 0 || self.reg(PX_IS) & IS_TFES != 0);
        fence(Ordering::SeqCst);

        let failed = self.reg(PX_IS) & IS_TFES != 0 || self.reg(PX_TFD) & TFD_ERR != 0;
        if finished.is_err() || failed {
            let error = (self.reg(PX_TFD) >> 8) as u8;
            // A failed command halts the port until it is restarted.
            self.stop()?;
            self.start()?;
            finished?;
            return Err(DiskError::Device(error));
        }
        Ok(())
    }

    fn bounce(&self) -> *mut u8 {
        // Safety: the bounce buffer is the tail of the second frame.
        unsafe { self.table.1.add(BOUNCE) }
    }

    pub fn read(&mut self, lba: u64, buf: &mut [u8]) -> Result<(), DiskError> {
        for (i, chunk) in buf.chunks_mut(BOUNCE_SECTORS * SECTOR).enumerate() {
            let start = lba + (i * BOUNCE_SECTORS) as u64;
            self.issue(READ_DMA_EXT, start, chunk.len() / SECTOR, false)?;
            // Safety: the command finished, so the bounce buffer holds `chunk.len()` bytes.
            unsafe {
                core::ptr::copy_nonoverlapping(self.bounce(), chunk.as_mut_ptr(), chunk.len())
            };
        }
        Ok(())
    }

    pub fn write(&mut self, lba: u64, buf: &[u8]) -> Result<(), DiskError> {
        for (i, chunk) in buf.chunks(BOUNCE_SECTORS * SECTOR).enumerate() {
            let start = lba + (i * BOUNCE_SECTORS) as u64;
            // Safety: no command is in flight, so the bounce buffer is ours.
            unsafe { core::ptr::copy_nonoverlapping(chunk.as_ptr(), self.bounce(), chunk.len()) };
            self.issue(WRITE_DMA_EXT, start, chunk.len() / SECTOR, true)?;
        }
        self.issue(FLUSH_CACHE_EXT, 0, 0, false)
    }

    /// Point a stopped port at fresh memory and start it.
    fn init(regs: *mut u8, addresses_64bit: bool) -> Option<Port> {
        let (Some(list), Some(table)) = (alloc_dma_frame(), alloc_dma_frame()) else {
            serial_println!("simplos: AHCI: out of memory for a port");
            return None;
        };
        let above_4g = |(phys, _): (PhysAddr, *mut u8)| phys.as_u64() >> 32 != 0;
        if !addresses_64bit && (above_4g(list) || above_4g(table)) {
            serial_println!("simplos: AHCI: controller can't reach memory above 4 GiB");
            return None;
        }
        let port = Port { regs, list, table };
        port.stop().ok()?;
        let fis = list.0 + FIS_AREA as u64;
        port.set(PX_CLB, list.0.as_u64() as u32);
        port.set(PX_CLBU, (list.0.as_u64() >> 32) as u32);
        port.set(PX_FB, fis.as_u64() as u32);
        port.set(PX_FBU, (fis.as_u64() >> 32) as u32);
        port.set(PX_IE, 0);
        port.start().ok()?;
        Some(port)
    }
}

pub fn probe() -> Vec<(DiskInfo, Port)> {
    let mut found = Vec::new();
    for location in pci::find(0x01, 0x06, 0x01) {
        // BAR5 (ABAR) holds the registers. A 64-bit BAR placed above 4 GiB is
        // outside what the bootloader maps.
        let bar = pci::read32(location, 0x24);
        if bar & 0b110 == 0b100 && pci::read32(location, 0x28) != 0 {
            serial_println!("simplos: AHCI {:?}: registers above 4 GiB, skipped", location);
            continue;
        }
        let Some(hba) = phys_ptr(PhysAddr::new(u64::from(bar & !0xF))) else { continue };
        pci::enable_bus_master_and_memory(location);
        write(hba, GHC, read(hba, GHC) | GHC_AE);
        let addresses_64bit = read(hba, CAP) & CAP_S64A != 0;

        let implemented = read(hba, PI);
        for index in (0..32).filter(|i| implemented & 1 << i != 0) {
            // Safety: port register blocks follow the controller's, 0x80 each.
            let regs = unsafe { hba.add(0x100 + index * 0x80) };
            let status = read(regs, PX_SSTS);
            let (detected, active) = (status & 0xF, status >> 8 & 0xF);
            if detected != 3 || active != 1 || read(regs, PX_SIG) != SIG_ATA {
                continue;
            }
            let Some(mut port) = Port::init(regs, addresses_64bit) else { continue };
            if let Err(e) = port.issue(IDENTIFY, 0, 1, false) {
                serial_println!("simplos: AHCI port {}: IDENTIFY failed: {:?}", index, e);
                continue;
            }
            // Safety: IDENTIFY finished, so the bounce buffer holds its 512 bytes.
            let id = words(unsafe { core::slice::from_raw_parts(port.bounce(), SECTOR) });
            match parse_identify(&id) {
                Some((model, sectors)) => {
                    found.push((DiskInfo { kind: "ahci", model, sectors }, port))
                }
                None => {
                    serial_println!("simplos: skipping AHCI disk without 512-byte sectors");
                }
            }
        }
    }
    found
}
