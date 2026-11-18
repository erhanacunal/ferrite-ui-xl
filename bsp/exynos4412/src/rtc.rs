//! RTC backend — the MAX77686 PMIC's battery-backed clock (I2C0).
//!
//! A read of the chip asks it to copy its counters to the readable
//! registers and waits for that (`RTC_UPDATE_US`, 16 ms). The tablet reads
//! the time in every `loop()` (every 30–50 ms): that was a 16.6 ms stall per
//! read, a third of the main loop, and a dropped frame each time. So the
//! chip is read once, then the time runs on the MCT ([`systick::millis`])
//! and the chip is read again every [`RESYNC_MS`] and after `set_time`.
//!
//! The chip keeps UTC (`ferrite_core::rtc::RtcImpl` adds the UTC offset). A
//! time from NTP (CPU3, [`set_from_ntp`]) is taken on CPU0's next read: the
//! chip is set from it once, and every reader runs on it from then.

use core::cell::Cell;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::time;
use ferrite_core::rtc::{DateTime, RtcBackend};

use crate::systick;

/// How long the time runs on the MCT before the chip is read again (one
/// 16 ms stall per period; the two clocks drift apart by far less).
const RESYNC_MS: u32 = 10 * 60 * 1000;

/// The last NTP time: Unix seconds, `systick::millis` at that second, and a
/// generation (0 = none) that readers compare with theirs.
static NTP_SECS: AtomicU32 = AtomicU32::new(0);
static NTP_AT: AtomicU32 = AtomicU32::new(0);
static NTP_GEN: AtomicU32 = AtomicU32::new(0);
/// The chip is still to be set from the last NTP time.
static NTP_CHIP: AtomicBool = AtomicBool::new(false);

/// CPU3: UTC from an NTP server, `secs` Unix seconds at `systick::millis() == at_ms`.
pub fn set_from_ntp(secs: u32, at_ms: u32) {
    NTP_SECS.store(secs, Ordering::Relaxed);
    NTP_AT.store(at_ms, Ordering::Relaxed);
    NTP_CHIP.store(true, Ordering::Relaxed);
    NTP_GEN.fetch_add(1, Ordering::Release);
}

pub struct PmicRtc {
    rtc: Rtc,
    /// Unix seconds the chip last gave, and `systick::millis` then.
    base: Cell<Option<(i64, u32)>>,
    /// The NTP generation taken.
    ntp_gen: Cell<u32>,
}

pub fn new() -> PmicRtc {
    // I2C0 was initialised by main (PMIC bus).
    PmicRtc { rtc: Rtc::new(I2c::new(Bus::I2c0)), base: Cell::new(None), ntp_gen: Cell::new(0) }
}

impl PmicRtc {
    /// The chip's time (a 16 ms wait).
    fn read_chip(&self) -> Option<time::DateTime> {
        self.rtc.read(Mct::new()).ok()?.time()
    }

    /// The current time: the chip's last reading plus the MCT time since,
    /// or a new reading when there is none or it is [`RESYNC_MS`] old.
    fn now(&self) -> Option<time::DateTime> {
        let ms = systick::millis();
        let generation = NTP_GEN.load(Ordering::Acquire);
        if generation != self.ntp_gen.get() {
            self.ntp_gen.set(generation);
            let (secs, at) = (i64::from(NTP_SECS.load(Ordering::Relaxed)), NTP_AT.load(Ordering::Relaxed));
            if NTP_CHIP.swap(false, Ordering::Relaxed) {
                let now = secs + i64::from(ms.wrapping_sub(at) / 1000);
                let _ = self.rtc.set(time::DateTime::from_unix(now), Mct::new());
            }
            self.base.set(Some((secs, at)));
        }
        if let Some((secs, at)) = self.base.get() {
            let age = ms.wrapping_sub(at);
            if age < RESYNC_MS {
                return Some(time::DateTime::from_unix(secs + (age / 1000) as i64));
            }
        }
        let t = self.read_chip();
        // An invalid reading is not kept: the next call reads again.
        self.base.set(t.filter(|t| t.is_valid()).map(|t| (t.to_unix(), ms)));
        t
    }
}

impl RtcBackend for PmicRtc {
    fn read_time(&self) -> DateTime {
        let Some(t) = self.now() else {
            return DateTime { year: 0, month: 1, day: 1, weekday: 6, hour: 0, minute: 0, second: 0 };
        };
        // 1970-01-01 was a Thursday (4).
        let weekday = ((t.to_unix().div_euclid(86_400) + 4).rem_euclid(7)) as u8;
        DateTime {
            year: t.year.saturating_sub(2000).min(99) as u8,
            month: t.month,
            day: t.day,
            weekday,
            hour: t.hour,
            minute: t.minute,
            second: t.second,
        }
    }

    fn set_time(&self, dt: &DateTime) {
        let t = time::DateTime {
            year: 2000 + dt.year as u16,
            month: dt.month,
            day: dt.day,
            hour: dt.hour,
            minute: dt.minute,
            second: dt.second,
        };
        let _ = self.rtc.set(t, Mct::new());
        // Read it back on the next call.
        self.base.set(None);
    }

    fn is_valid(&self) -> bool {
        self.now().is_some_and(|t| t.is_valid())
    }

    fn clear_vl_flag(&self) {}
}
