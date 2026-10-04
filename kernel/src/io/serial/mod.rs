use spin::{Lazy, Mutex};
use uart_16550::SerialPort;

pub static SERIAL1: Lazy<Mutex<SerialPort>> = Lazy::new(|| {
    let mut serial_port = unsafe { SerialPort::new(0x3F8) };
    serial_port.init();
    Mutex::new(serial_port)
});

#[doc(hidden)]
pub fn _print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    x86_64::instructions::interrupts::without_interrupts(|| {
        SERIAL1.lock().write_fmt(args).expect("Printing to serial failed");
    });
}

/// Write straight to the UART, bypassing [`SERIAL1`].
///
/// A panic can land while `_print` holds that lock — or in an interrupt that
/// interrupted it — and taking it again would spin forever, turning the one
/// message that explains the failure into yet another silent hang. The 16550
/// has no state worth preserving, so re-initialising it here is safe.
pub fn panic_print(args: ::core::fmt::Arguments) {
    use core::fmt::Write;
    let mut port = unsafe { SerialPort::new(0x3F8) };
    port.init();
    let _ = port.write_fmt(args);
}

#[macro_export]
macro_rules! serial_print {
    ($($arg:tt)*) => {
        $crate::serial::_print(format_args!($($arg)*));
    };
}

#[macro_export]
macro_rules! serial_println {
    () => ($crate::serial_print!("\n"));
    ($fmt:expr) => ($crate::serial_print!(concat!($fmt, "\n")));
    ($fmt:expr, $($arg:tt)*) => ($crate::serial_print!(
        concat!($fmt, "\n"), $($arg)*));
}