//! The time: of day, as the CMOS real-time clock tells it at boot and the HPET
//! carries on from there; since boot; and programs' alarms, which the alarm
//! thread sends them as SIGALRM.

use crate::io::hpet;
use crate::io::interrupts::pit::TIMER_HZ;
use crate::process;
use crate::signal::{self, Info};
use crate::thread;
use core::sync::atomic::{AtomicU64, Ordering};
use x86_64::instructions::port::Port;

pub const NANOS: u64 = 1_000_000_000;

/// Nanoseconds a timer tick lasts.
pub const TICK: u64 = NANOS / TIMER_HZ as u64;

/// Nanoseconds since boot: by the HPET, or by the timer's ticks without one.
pub fn since_boot() -> u64 {
    hpet::elapsed_nanos().unwrap_or_else(|| thread::ticks() * TICK)
}

/// The time of day at boot, in nanoseconds since 1970, UTC.
static BOOT: AtomicU64 = AtomicU64::new(0);

/// The time of day, in nanoseconds since 1970, UTC.
pub fn now() -> u64 {
    BOOT.load(Ordering::Relaxed) + since_boot()
}

/// Read the real-time clock, and start the alarm thread.
pub fn init() {
    let seconds = rtc();
    BOOT.store((seconds * NANOS).saturating_sub(since_boot()), Ordering::Relaxed);
    thread::spawn(alarms).expect("memory for the alarm thread");
}

/// Every tick, SIGALRM to the programs whose alarm has come, set again for
/// the next if it repeats. By the clock programs read, which the timer's
/// ticks can run ahead of when the host is busy: an alarm can come a tick
/// late, but never early.
fn alarms() {
    loop {
        thread::sleep(1000 / TIMER_HZ as u64);
        let now = since_boot();
        for program in process::all() {
            let due = {
                let mut alarm = program.alarm.lock();
                let due = alarm.due != 0 && alarm.due <= now;
                if due {
                    alarm.due = if alarm.every == 0 { 0 } else { now + alarm.every };
                }
                due
            };
            if due {
                signal::send(&program, signal::SIGALRM, Info::kernel());
            }
        }
    }
}

/// A program's alarm: when it's due, in nanoseconds since boot, 0 for none;
/// and how many nanoseconds apart it repeats, 0 for not at all.
#[derive(Clone, Copy, Default)]
pub struct Alarm {
    pub due: u64,
    pub every: u64,
}

/// Seconds since 1970 by the CMOS clock, which QEMU keeps in UTC, as Linux
/// expects of it.
fn rtc() -> u64 {
    // Two reads alike, so none was torn by the clock moving on in between.
    let mut last = read_rtc();
    loop {
        let again = read_rtc();
        if again == last {
            return seconds(again);
        }
        last = again;
    }
}

/// Seconds, minutes, hours, day, month, year and century, as the clock's
/// registers hold them, with its status register B.
type Registers = ([u8; 7], u8);

fn read_rtc() -> Registers {
    let cmos = |register: u8| {
        // Safety: the CMOS's index and data ports; reading a register has no
        // effect but its value.
        unsafe {
            Port::<u8>::new(0x70).write(register);
            Port::<u8>::new(0x71).read()
        }
    };
    // Not while it's updating, when the registers are between one second
    // and the next.
    while cmos(0x0A) & 0x80 != 0 {}
    ([0x00, 0x02, 0x04, 0x07, 0x08, 0x09, 0x32].map(cmos), cmos(0x0B))
}

fn seconds((registers, status): Registers) -> u64 {
    const BINARY: u8 = 0x04;
    const HOURS_24: u8 = 0x02;
    const PM: u8 = 0x80;
    let number = |byte: u8| match status & BINARY {
        0 => (byte >> 4) * 10 + (byte & 0x0F),
        _ => byte,
    };
    let [second, minute, hour, day, month, year, century] = registers;
    let pm = status & HOURS_24 == 0 && hour & PM != 0;
    // In 12-hour mode, 12 is midnight or noon.
    let hour = match (status & HOURS_24 != 0, number(hour & !PM)) {
        (true, hour) => hour,
        (false, 12) => 12 * u8::from(pm),
        (false, hour) => hour + 12 * u8::from(pm),
    };
    // A clock without the century register reads it as 0: this one.
    let century = match number(century) {
        0 => 20,
        century => century,
    };
    let year = i64::from(century) * 100 + i64::from(number(year));
    let days = days_since_1970(year, number(month), number(day));
    let seconds =
        i64::from(hour) * 3600 + i64::from(number(minute)) * 60 + i64::from(number(second));
    (days * 86400 + seconds).max(0) as u64
}

/// Days from 1970-01-01 to `year-month-day`, by Howard Hinnant's
/// `days_from_civil`.
fn days_since_1970(year: i64, month: u8, day: u8) -> i64 {
    let year = if month <= 2 { year - 1 } else { year };
    let era = year.div_euclid(400);
    let year_of_era = year - era * 400;
    // Months from March, so the leap day is the year's last.
    let day_of_year = (153 * ((i64::from(month) + 9) % 12) + 2) / 5 + i64::from(day) - 1;
    let day_of_era = year_of_era * 365 + year_of_era / 4 - year_of_era / 100 + day_of_year;
    era * 146_097 + day_of_era - 719_468
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test_case]
    fn days_count_from_1970() {
        assert_eq!(days_since_1970(1970, 1, 1), 0);
        assert_eq!(days_since_1970(2000, 3, 1), 11_017);
        assert_eq!(days_since_1970(2024, 2, 29), 19_782);
        assert_eq!(days_since_1970(2026, 10, 15), 20_741);
    }

    #[test_case]
    fn the_clock_s_registers_read_as_bcd_or_binary_and_12_or_24_hours() {
        // 2026-10-15 13:45:30, in BCD with 24 hours, then binary with 12.
        let at = 20_741 * 86400 + 13 * 3600 + 45 * 60 + 30;
        assert_eq!(seconds(([0x30, 0x45, 0x13, 0x15, 0x10, 0x26, 0x20], 0x02)), at);
        assert_eq!(seconds(([30, 45, 0x80 | 1, 15, 10, 26, 20], 0x04)), at);
        // 12 AM is midnight; no century register, this one.
        assert_eq!(seconds(([0, 0, 12, 15, 10, 26, 0], 0x04)), at - (13 * 3600 + 45 * 60 + 30));
    }
}
