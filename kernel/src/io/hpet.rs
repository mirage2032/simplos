//! HPET (High Precision Event Timer) driver
//!
//! Provides high-resolution timing (nanosecond precision) using the HPET hardware.
// TODO: REWORK
use core::ptr::{read_volatile, write_volatile};
use spin::{Lazy, Mutex};
use x86_64::VirtAddr;

// HPET register offsets
const HPET_CAP_ID: usize = 0x00;       // Capabilities and ID
const HPET_CONFIG: usize = 0x10;       // Configuration
const HPET_COUNTER: usize = 0xF0;      // Main counter value (64-bit)

// Configuration register bits
const HPET_CFG_ENABLE: u64 = 1 << 0;   // Enable main counter

/// HPET driver instance
pub struct Hpet {
    base: VirtAddr,
    period_fs: u64,     // Period in femtoseconds (10^-15 seconds)
    start_count: u64,   // Counter value at initialization
}

impl Hpet {
    /// Create a new HPET driver from the physical base address
    ///
    /// # Safety
    /// The caller must ensure that `hpet_phys_addr` is a valid HPET base address
    /// and `phys_mem_offset` correctly maps physical to virtual addresses.
    pub unsafe fn new(hpet_phys_addr: u64, phys_mem_offset: VirtAddr) -> Self {
        let base = phys_mem_offset + hpet_phys_addr;

        // Read capabilities register to get the period
        let cap = read_volatile((base + HPET_CAP_ID as u64).as_ptr::<u64>());
        let period_fs = cap >> 32; // Upper 32 bits = period in femtoseconds

        // Enable the main counter
        let mut config = read_volatile((base + HPET_CONFIG as u64).as_ptr::<u64>());
        config |= HPET_CFG_ENABLE;
        write_volatile((base + HPET_CONFIG as u64).as_mut_ptr::<u64>(), config);

        // Read initial counter value
        let start_count = read_volatile((base + HPET_COUNTER as u64).as_ptr::<u64>());

        Hpet {
            base,
            period_fs,
            start_count,
        }
    }

    /// Read the current counter value
    #[inline]
    pub fn read_counter(&self) -> u64 {
        unsafe { read_volatile((self.base + HPET_COUNTER as u64).as_ptr::<u64>()) }
    }

    /// Get elapsed ticks since initialization
    #[inline]
    pub fn elapsed_ticks(&self) -> u64 {
        self.read_counter().wrapping_sub(self.start_count)
    }

    /// Get elapsed time in nanoseconds since initialization
    pub fn elapsed_nanos(&self) -> u64 {
        let ticks = self.elapsed_ticks();
        // period_fs is in femtoseconds, divide by 1_000_000 to get nanoseconds
        ticks * self.period_fs / 1_000_000
    }

    /// Get elapsed time in microseconds since initialization
    pub fn elapsed_micros(&self) -> u64 {
        self.elapsed_nanos() / 1_000
    }

    /// Get elapsed time in milliseconds since initialization
    pub fn elapsed_millis(&self) -> u64 {
        self.elapsed_nanos() / 1_000_000
    }

    /// Get the HPET frequency in Hz
    pub fn frequency(&self) -> u64 {
        // frequency = 10^15 / period_fs
        1_000_000_000_000_000 / self.period_fs
    }

    /// Get the period in femtoseconds
    pub fn period_femtoseconds(&self) -> u64 {
        self.period_fs
    }
}

/// Global HPET instance
pub static HPET: Lazy<Mutex<Option<Hpet>>> = Lazy::new(|| Mutex::new(None));

/// Initialize the HPET from ACPI tables
///
/// # Arguments
/// * `rsdp_addr` - Physical address of RSDP (from bootloader)
/// * `phys_mem_offset` - Virtual address offset for physical memory mapping
pub fn init_hpet(rsdp_addr: u64, phys_mem_offset: VirtAddr) -> Result<(), &'static str> {
    use acpi::{AcpiHandler, AcpiTables, PhysicalMapping, HpetInfo};
    use core::ptr::NonNull;

    /// Handler for the acpi crate to map physical memory
    #[derive(Clone)]
    struct IdentityMappedHandler {
        phys_offset: u64,
    }

    impl AcpiHandler for IdentityMappedHandler {
        unsafe fn map_physical_region<T>(
            &self,
            physical_address: usize,
            size: usize,
        ) -> PhysicalMapping<Self, T> {
            let virtual_address = physical_address + self.phys_offset as usize;
            PhysicalMapping::new(
                physical_address,
                NonNull::new(virtual_address as *mut T).unwrap(),
                size,
                size,
                self.clone(),
            )
        }

        fn unmap_physical_region<T>(_region: &PhysicalMapping<Self, T>) {
            // Memory is identity-mapped, nothing to unmap
        }
    }

    let handler = IdentityMappedHandler {
        phys_offset: phys_mem_offset.as_u64(),
    };

    // Parse ACPI tables
    let tables = unsafe {
        AcpiTables::from_rsdp(handler, rsdp_addr as usize)
            .map_err(|_| "Failed to parse ACPI tables")?
    };

    // Get HPET info from platform info
    let hpet_info = HpetInfo::new(&tables)
        .map_err(|_| "HPET not found in ACPI tables")?;

    let hpet_base = hpet_info.base_address as u64;

    // Initialize HPET driver
    let hpet = unsafe { Hpet::new(hpet_base, phys_mem_offset) };

    *HPET.lock() = Some(hpet);

    Ok(())
}

/// Get elapsed milliseconds since HPET initialization
pub fn elapsed_millis() -> Option<u64> {
    HPET.lock().as_ref().map(|h| h.elapsed_millis())
}

/// Get elapsed microseconds since HPET initialization  
pub fn elapsed_micros() -> Option<u64> {
    HPET.lock().as_ref().map(|h| h.elapsed_micros())
}

