/// Backend abstraction for real-time clock.
/// Hardware backends talk to AT8563 (I2C) or ESP32 built-in RTC; sim uses system clock.
///
/// The backend keeps UTC; [`RtcImpl`] reads and sets local time, UTC +
/// [`crate::settings::utc_offset`].

/// Date and time.
#[derive(Clone, Copy)]
pub struct DateTime {
    pub year: u8,    // 0-99 (offset from 2000)
    pub month: u8,   // 1-12
    pub day: u8,     // 1-31
    pub weekday: u8, // 0-6 (0=Sunday)
    pub hour: u8,    // 0-23
    pub minute: u8,  // 0-59
    pub second: u8,  // 0-59
}

impl DateTime {
    pub const fn zero() -> Self {
        Self {
            year: 0,
            month: 1,
            day: 1,
            weekday: 0,
            hour: 0,
            minute: 0,
            second: 0,
        }
    }

    /// Unix seconds (years 2000..=2099).
    pub fn to_unix(&self) -> i64 {
        // Days from 1970-01-01 (Howard Hinnant's days_from_civil).
        let y = 2000 + i64::from(self.year) - i64::from(self.month <= 2);
        let m = i64::from(self.month.clamp(1, 12));
        let era = y.div_euclid(400);
        let yoe = y - era * 400;
        let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + i64::from(self.day.max(1)) - 1;
        let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
        let days = era * 146_097 + doe - 719_468;
        days * 86_400 + i64::from(self.hour) * 3600 + i64::from(self.minute) * 60 + i64::from(self.second)
    }

    /// From Unix seconds; years outside 2000..=2099 are clamped.
    pub fn from_unix(secs: i64) -> Self {
        let secs = secs.clamp(946_684_800, 4_102_444_799);
        let days = secs.div_euclid(86_400);
        let rem = secs.rem_euclid(86_400);
        // civil_from_days.
        let z = days + 719_468;
        let era = z.div_euclid(146_097);
        let doe = z - era * 146_097;
        let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
        let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
        let mp = (5 * doy + 2) / 153;
        let day = doy - (153 * mp + 2) / 5 + 1;
        let month = if mp < 10 { mp + 3 } else { mp - 9 };
        let year = yoe + era * 400 + i64::from(month <= 2);
        Self {
            year: (year - 2000) as u8,
            month: month as u8,
            day: day as u8,
            // 1970-01-01 was a Thursday (4).
            weekday: (days + 4).rem_euclid(7) as u8,
            hour: (rem / 3600) as u8,
            minute: (rem / 60 % 60) as u8,
            second: (rem % 60) as u8,
        }
    }

    /// The same moment `minutes` later (another time zone).
    pub fn shifted(&self, minutes: i32) -> Self {
        if minutes == 0 {
            return *self;
        }
        Self::from_unix(self.to_unix() + i64::from(minutes) * 60)
    }
}

pub trait RtcBackend {
    fn read_time(&self) -> DateTime;
    fn set_time(&self, dt: &DateTime);
    fn is_valid(&self) -> bool;
    fn clear_vl_flag(&self);
}

pub struct RtcImpl<B: RtcBackend> {
    be: B,
}

impl<B: RtcBackend> RtcImpl<B> {
    pub fn with_backend(be: B) -> Self {
        Self { be }
    }

    /// Local time (UTC + the offset).
    #[inline]
    pub fn read_time(&self) -> DateTime {
        self.be.read_time().shifted(crate::settings::utc_offset())
    }

    /// Sets the clock from a local time.
    #[inline]
    pub fn set_time(&self, dt: &DateTime) {
        self.be.set_time(&dt.shifted(-crate::settings::utc_offset()))
    }

    /// The backend's time (UTC).
    #[inline]
    pub fn read_utc(&self) -> DateTime {
        self.be.read_time()
    }

    #[inline]
    pub fn is_valid(&self) -> bool {
        self.be.is_valid()
    }

    #[inline]
    pub fn clear_vl_flag(&self) {
        self.be.clear_vl_flag()
    }
}

#[cfg(feature = "mock")]
pub type Rtc = RtcImpl<crate::mock::MockRtc>;

#[cfg(test)]
mod tests {
    use super::DateTime;

    #[test]
    fn unix_round_trips_and_shifts() {
        // 2026-11-18 15:57:30 UTC, a Wednesday.
        let t = DateTime { year: 26, month: 11, day: 18, weekday: 3, hour: 15, minute: 57, second: 30 };
        assert_eq!(t.to_unix(), 1_795_017_450);
        let back = DateTime::from_unix(t.to_unix());
        assert_eq!((back.year, back.month, back.day, back.weekday), (26, 11, 18, 3));
        assert_eq!((back.hour, back.minute, back.second), (15, 57, 30));
        let local = t.shifted(180);
        assert_eq!((local.hour, local.minute, local.day), (18, 57, 18));
        // Across a year end and a leap day.
        let ny = DateTime { year: 24, month: 12, day: 31, weekday: 2, hour: 22, minute: 0, second: 0 }.shifted(180);
        assert_eq!((ny.year, ny.month, ny.day, ny.hour, ny.weekday), (25, 1, 1, 1, 3));
        let leap = DateTime { year: 24, month: 2, day: 28, weekday: 3, hour: 23, minute: 0, second: 0 }.shifted(60);
        assert_eq!((leap.month, leap.day, leap.hour), (2, 29, 0));
        assert_eq!(DateTime::from_unix(946_684_800).to_unix(), 946_684_800);
    }
}
