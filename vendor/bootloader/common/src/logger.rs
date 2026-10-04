use crate::framebuffer::FrameBufferWriter;
use bootloader_api::info::FrameBufferInfo;
use conquer_once::spin::OnceCell;
use core::fmt::Write;
use spinning_top::Spinlock;
use uart_16550::backend::PioBackend;
use uart_16550::{Config, Uart16550Tty};

/// The global logger instance used for the `log` crate.
pub static LOGGER: OnceCell<LockedLogger> = OnceCell::uninit();

/// A logger instance protected by a spinlock.
pub struct LockedLogger {
    framebuffer: Option<Spinlock<FrameBufferWriter>>,
    serial: Option<Spinlock<Uart16550Tty<PioBackend>>>,
}

impl LockedLogger {
    /// Create a new instance that logs to the given framebuffer.
    pub fn new(
        framebuffer: &'static mut [u8],
        info: FrameBufferInfo,
        frame_buffer_logger_status: bool,
        serial_logger_status: bool,
    ) -> Self {
        let framebuffer = match frame_buffer_logger_status {
            true => Some(Spinlock::new(FrameBufferWriter::new(framebuffer, info))),
            false => None,
        };

        let serial = match serial_logger_status {
            true => {
                // SAFETY: We have exclusive access to the device.
                //
                // This returns `None` if the config is invalid or the self-test fails.
                // We do not panic here because we want to continue booting.
                unsafe {
                    Uart16550Tty::new_port(0x3f8, Config::default())
                        .ok()
                        .map(Spinlock::new)
                }
            }
            false => None,
        };

        LockedLogger {
            framebuffer,
            serial,
        }
    }

    /// Force-unlocks the logger to prevent a deadlock.
    ///
    /// ## Safety
    /// This method is not memory safe and should be only used when absolutely necessary.
    pub unsafe fn force_unlock(&self) {
        if let Some(framebuffer) = &self.framebuffer {
            unsafe { framebuffer.force_unlock() };
        }
        if let Some(serial) = &self.serial {
            unsafe { serial.force_unlock() };
        }
    }
}

impl log::Log for LockedLogger {
    fn enabled(&self, _metadata: &log::Metadata) -> bool {
        true
    }

    fn log(&self, record: &log::Record) {
        if let Some(framebuffer) = &self.framebuffer {
            let mut framebuffer = framebuffer.lock();
            writeln!(framebuffer, "{:5}: {}", record.level(), record.args()).unwrap();
        }
        if let Some(serial) = &self.serial {
            let mut serial = serial.lock();
            writeln!(serial, "{:5}: {}", record.level(), record.args()).unwrap();
        }
    }

    fn flush(&self) {}
}
