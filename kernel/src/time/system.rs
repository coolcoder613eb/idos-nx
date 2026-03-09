//! Utilities for managing system time

use core::sync::atomic::{AtomicU32, Ordering};

use spin::Mutex;

use super::date::DateTime;
use crate::hardware::pit::{PIT, PIT_BASE_FREQ, PIT_DIVIDER};

// Derive tick timing from the PIT configuration.
// With a divider of 11932 and base freq of 1,193,182 Hz, each tick is
// approximately 10.0002 ms, or 100,002 units of 100ns.
pub const HUNDRED_NS_PER_TICK: u64 =
    (PIT_DIVIDER as u64 * 10_000_000) / PIT_BASE_FREQ as u64;
pub const MS_PER_TICK: u32 = (HUNDRED_NS_PER_TICK / 10000) as u32;

/// Stores the number of clock ticks since the kernel began execution. This is
/// used for relative time offsets within various kernel internals.
static SYSTEM_TICKS: AtomicU32 = AtomicU32::new(0);

/// CPU time accounting: each tick (10ms) is attributed to one of these buckets
/// based on what the timer interrupt interrupted.
static USER_TICKS: AtomicU32 = AtomicU32::new(0);
static KERNEL_TICKS: AtomicU32 = AtomicU32::new(0);
static IDLE_TICKS: AtomicU32 = AtomicU32::new(0);

/// Record one tick of CPU time in the appropriate bucket.
/// `is_user`: interrupted ring 3 or VM86 code
/// `is_idle`: interrupted the idle task
pub fn record_cpu_tick(is_user: bool, is_idle: bool) {
    if is_user {
        USER_TICKS.fetch_add(1, Ordering::Relaxed);
    } else if is_idle {
        IDLE_TICKS.fetch_add(1, Ordering::Relaxed);
    } else {
        KERNEL_TICKS.fetch_add(1, Ordering::Relaxed);
    }
}

/// Returns (user_ticks, kernel_ticks, idle_ticks).
pub fn get_cpu_ticks() -> (u32, u32, u32) {
    (
        USER_TICKS.load(Ordering::Relaxed),
        KERNEL_TICKS.load(Ordering::Relaxed),
        IDLE_TICKS.load(Ordering::Relaxed),
    )
}

/// Store a known fixed point in time, sourced from CMOS RTC, a NTP service, or
/// something similar. We use the programmable timer to update an offset
/// relative to this number.
static KNOWN_TIME: Mutex<TimestampHires> = Mutex::new(TimestampHires(0));
/// Store an offset, regularly updated by the programmable timer
static TIME_OFFSET: Mutex<TimestampHires> = Mutex::new(TimestampHires(0));

pub fn tick() {
    SYSTEM_TICKS.fetch_add(1, Ordering::SeqCst);
    increment_offset(HUNDRED_NS_PER_TICK);
}

pub fn get_system_ticks() -> u32 {
    SYSTEM_TICKS.load(Ordering::SeqCst)
}

/// Get the number of milliseconds since the kernel started, with sub-tick
/// precision. This reads the PIT's current countdown value to interpolate
/// within the current tick period, giving ~microsecond precision without
/// needing a higher interrupt rate.
pub fn get_monotonic_ms() -> u64 {
    // We need to read the tick counter and PIT counter atomically with
    // respect to the timer interrupt. Disable interrupts briefly to prevent
    // reading a stale tick count right as the PIT wraps around.
    let (ticks, remaining) = unsafe {
        let flags: u32;
        core::arch::asm!("pushfd; pop {0}; cli", out(reg) flags);
        let ticks = SYSTEM_TICKS.load(Ordering::SeqCst);
        let remaining = PIT::new().read_counter();
        // Restore interrupt flag if it was set
        if flags & 0x200 != 0 {
            core::arch::asm!("sti");
        }
        (ticks, remaining)
    };

    let base_ms = ticks as u64 * MS_PER_TICK as u64;
    // The PIT counts down from PIT_DIVIDER to 0. The elapsed portion of
    // the current tick is (PIT_DIVIDER - remaining) / PIT_DIVIDER.
    // In Mode 3 (square wave), the counter decrements by 2 each cycle,
    // so the effective range is 0..PIT_DIVIDER.
    let elapsed = (PIT_DIVIDER as u64).saturating_sub(remaining as u64);
    let sub_ms = (elapsed * MS_PER_TICK as u64) / PIT_DIVIDER as u64;

    base_ms + sub_ms
}

/// High-resolution 64-bit timestamp representing the number of 100ns
/// increments since midnight 1 January 1980
#[derive(Copy, Clone)]
#[repr(transparent)]
pub struct TimestampHires(pub u64);

impl TimestampHires {
    pub fn set(&mut self, value: u64) {
        self.0 = value;
    }

    pub fn increment(&mut self, value: u64) {
        self.0 += value;
    }

    pub fn in_ms(&self) -> u64 {
        self.0 / 10_000
    }

    pub fn in_seconds(&self) -> u64 {
        self.0 / 10_000_000
    }

    pub fn from_timestamp(ts: Timestamp) -> Self {
        Self(ts.0 as u64 * 10_000_000)
    }

    pub fn to_timestamp(&self) -> Timestamp {
        Timestamp(self.in_seconds() as u32)
    }
}

impl core::ops::Add for TimestampHires {
    type Output = TimestampHires;

    fn add(self, rhs: Self) -> Self::Output {
        TimestampHires(self.0 + rhs.0)
    }
}

/// Unsigned, 32-bit number representing the number of seconds passed since
/// midnight on 1 January 1980. It neglects leap seconds.
/// This is NOT the same as POSIX time!
#[derive(Copy, Clone, Debug, Eq, PartialEq, PartialOrd)]
pub struct Timestamp(pub u32);

impl Timestamp {
    pub fn to_datetime(&self) -> DateTime {
        DateTime::from_timestamp(*self)
    }

    pub fn total_minutes(&self) -> u32 {
        self.0 / 60
    }

    pub fn as_u32(&self) -> u32 {
        self.0
    }

    pub fn now() -> Self {
        let hires = get_system_time();
        hires.to_timestamp()
    }
}

impl core::ops::Add<u32> for Timestamp {
    type Output = Timestamp;

    fn add(self, rhs: u32) -> Self::Output {
        Timestamp(self.0 + rhs)
    }
}

/// Reset the reference point time
pub fn reset_known_time(time: u64) {
    // TODO: mark this as critical, not to be interrupted
    KNOWN_TIME.lock().set(time);
    TIME_OFFSET.lock().set(0);
}

pub fn get_system_time() -> TimestampHires {
    // TODO: mark this as critical, not to be interrupted
    let known = *KNOWN_TIME.lock();
    let offset = *TIME_OFFSET.lock();

    known + offset
}

pub fn get_offset_seconds() -> u64 {
    let offset = *TIME_OFFSET.lock();

    offset.in_seconds()
}

pub fn increment_offset(delta: u64) {
    TIME_OFFSET.lock().increment(delta);
}

pub fn initialize_time_from_rtc() {
    let cmos_time = crate::hardware::rtc::read_rtc_time();
    let timestamp = cmos_time.to_datetime().to_timestamp();
    let system_time = TimestampHires::from_timestamp(timestamp);
    reset_known_time(system_time.0)
}
