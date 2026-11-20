//! GPS syscalls (0x90..=0x94), shared by the boards: a board's [`GpsBackend`] powers its
//! receiver and reports what the receiver last said as a [`GpsData`]; this module turns that
//! into the syscalls' values.
//!
//! exynos4412: the u-blox G6010 on UART1 (`bsp/exynos4412/src/gps.rs`, HAL `drivers::g6010`).
//! sim: none, unless `FERRITE_SIM_GPS=LAT,LON[,ALT_M]` fakes a fix. Ids and codes are
//! duplicated in ferrite-ui `lib/gps.fl` and the BSPs' `platform.rs` — keep them in sync.
//!
//! Positions are degrees times 10^7 (an `i32` holds ±180°), so programs need no floating point.

use alloc::format;
use alloc::string::String;

use crate::pool::ObjPool;

/// `(on) -> 1` done, `-1` no receiver or it could not be switched.
pub const SYS_GPS_POWER: u8 = 0x90;
/// `() -> GPS_*` state below.
pub const SYS_GPS_STATE: u8 = 0x91;
/// `(field) -> int`, `FIELD_*` below; 0 when the receiver has not said it (`FIELD_FIX_AGE`: -1).
pub const SYS_GPS_GET: u8 = 0x92;
/// `() -> UTC Unix seconds` from the receiver's last time and date, 0 if none yet.
pub const SYS_GPS_TIME: u8 = 0x93;
/// `(field) -> string` (syscallStr), `TEXT_*` below; "" without a fix.
pub const SYS_GPS_TEXT: u8 = 0x94;

/// The receiver is off.
pub const GPS_OFF: i32 = 0;
/// On, but no good sentence for [`NO_DATA_MS`]: just powered, or not answering.
pub const GPS_NO_DATA: i32 = 1;
/// The receiver talks but has no valid position (yet): looking for satellites.
pub const GPS_SEARCHING: i32 = 2;
/// A valid position, at most [`FIX_STALE_MS`] old.
pub const GPS_FIX: i32 = 3;

/// Latitude, degrees × 10^7, north positive.
pub const FIELD_LAT: i32 = 0;
/// Longitude, degrees × 10^7, east positive.
pub const FIELD_LON: i32 = 1;
/// Altitude above mean sea level, decimetres.
pub const FIELD_ALT: i32 = 2;
/// Ground speed, km/h × 10.
pub const FIELD_SPEED: i32 = 3;
/// Course over ground, degrees × 100 from true north.
pub const FIELD_COURSE: i32 = 4;
/// Satellites used in the fix.
pub const FIELD_SATS_USED: i32 = 5;
/// Satellites in view.
pub const FIELD_SATS_VIEW: i32 = 6;
/// Horizontal dilution of precision × 100 (lower is better; 9999 = none).
pub const FIELD_HDOP: i32 = 7;
/// Milliseconds since the last valid position, -1 none since power-on.
pub const FIELD_FIX_AGE: i32 = 8;
/// NMEA `GGA` quality: 0 none, 1 GPS, 2 differential, 6 estimated.
pub const FIELD_QUALITY: i32 = 9;

/// `"41.0123456"`.
pub const TEXT_LAT: i32 = 0;
/// `"28.9876543"`.
pub const TEXT_LON: i32 = 1;
/// `"41.0123456, 28.9876543"`.
pub const TEXT_POSITION: i32 = 2;

/// Without a good sentence for this long, the receiver counts as silent ([`GPS_NO_DATA`]).
pub const NO_DATA_MS: u32 = 3000;
/// A valid position older than this no longer counts as a fix (receivers send one a second).
pub const FIX_STALE_MS: u32 = 3000;

/// What a board's receiver last said.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct GpsData {
    /// The receiver's supplies are on.
    pub powered: bool,
    /// Milliseconds since the last good sentence; `None` = none since power-on.
    pub sentence_age_ms: Option<u32>,
    /// Milliseconds since the last sentence with a valid position; `None` = none since power-on.
    pub fix_age_ms: Option<u32>,
    /// Degrees × 10^7 (last valid position).
    pub latitude: i32,
    /// Degrees × 10^7 (last valid position).
    pub longitude: i32,
    /// Decimetres above mean sea level.
    pub altitude_dm: i32,
    /// Thousandths of a knot.
    pub speed_milliknots: u32,
    /// Degrees × 100.
    pub course_centidegrees: i32,
    /// NMEA `GGA` quality.
    pub quality: u8,
    /// Satellites used.
    pub satellites_used: u8,
    /// Satellites in view.
    pub satellites_in_view: u8,
    /// HDOP × 100, 9999 when none.
    pub hdop_centi: u16,
    /// UTC Unix seconds of the receiver's last time with a date, 0 = none.
    pub utc_unix: i64,
}

impl GpsData {
    /// The `GPS_*` state.
    pub fn state(&self) -> i32 {
        if !self.powered {
            GPS_OFF
        } else if self.sentence_age_ms.is_none_or(|age| age > NO_DATA_MS) {
            GPS_NO_DATA
        } else if self.fix_age_ms.is_some_and(|age| age <= FIX_STALE_MS) {
            GPS_FIX
        } else {
            GPS_SEARCHING
        }
    }

    /// The value of `SYS_GPS_GET`.
    pub fn field(&self, field: i32) -> i32 {
        match field {
            FIELD_LAT => self.latitude,
            FIELD_LON => self.longitude,
            FIELD_ALT => self.altitude_dm,
            // 1 knot = 1.852 km/h: milliknots × 1.852 / 100 = (km/h × 10).
            FIELD_SPEED => (u64::from(self.speed_milliknots) * 1852 / 100_000) as i32,
            FIELD_COURSE => self.course_centidegrees,
            FIELD_SATS_USED => i32::from(self.satellites_used),
            FIELD_SATS_VIEW => i32::from(self.satellites_in_view),
            FIELD_HDOP => i32::from(self.hdop_centi),
            FIELD_FIX_AGE => self.fix_age_ms.map_or(-1, |age| age.min(i32::MAX as u32) as i32),
            FIELD_QUALITY => i32::from(self.quality),
            _ => 0,
        }
    }

    /// The value of `SYS_GPS_TEXT`, "" without a fix since power-on.
    pub fn text(&self, field: i32) -> String {
        if self.fix_age_ms.is_none() {
            return String::new();
        }
        match field {
            TEXT_LAT => degrees(self.latitude),
            TEXT_LON => degrees(self.longitude),
            TEXT_POSITION => format!("{}, {}", degrees(self.latitude), degrees(self.longitude)),
            _ => String::new(),
        }
    }
}

/// `"-41.0123456"` from degrees × 10^7.
pub fn degrees(value: i32) -> String {
    let sign = if value < 0 { "-" } else { "" };
    let magnitude = value.unsigned_abs();
    format!("{}{}.{:07}", sign, magnitude / 10_000_000, magnitude % 10_000_000)
}

/// A board's receiver.
pub trait GpsBackend {
    /// Turns the receiver on or off. Returns false if there is none or it failed.
    fn power(&mut self, on: bool) -> bool;
    /// What it last said; also takes what it sent since the last call.
    fn data(&mut self) -> GpsData;
}

/// The GPS syscalls. `None` for an id outside 0x90..=0x94.
pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool, b: &mut impl GpsBackend) -> Option<i32> {
    let arg0 = args.first().copied().unwrap_or(0);
    match id {
        SYS_GPS_POWER => Some(if b.power(arg0 != 0) { 1 } else { -1 }),
        SYS_GPS_STATE => Some(b.data().state()),
        SYS_GPS_GET => Some(b.data().field(arg0)),
        SYS_GPS_TIME => Some(b.data().utc_unix.clamp(0, i64::from(i32::MAX)) as i32),
        SYS_GPS_TEXT => objs.str_alloc(b.data().text(arg0).as_bytes()).map(|id| id as i32),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn states() {
        let mut d = GpsData::default();
        assert_eq!(d.state(), GPS_OFF);
        d.powered = true;
        assert_eq!(d.state(), GPS_NO_DATA);
        d.sentence_age_ms = Some(200);
        assert_eq!(d.state(), GPS_SEARCHING);
        d.fix_age_ms = Some(900);
        assert_eq!(d.state(), GPS_FIX);
        d.fix_age_ms = Some(FIX_STALE_MS + 1);
        assert_eq!(d.state(), GPS_SEARCHING);
        d.sentence_age_ms = Some(NO_DATA_MS + 1);
        assert_eq!(d.state(), GPS_NO_DATA);
    }

    #[test]
    fn fields_and_text() {
        let mut d = GpsData { latitude: 410_123_456, longitude: -289_876_543, speed_milliknots: 10_000, ..Default::default() };
        assert_eq!(d.field(FIELD_SPEED), 185); // 10 knots = 18.52 km/h
        assert_eq!(d.field(FIELD_FIX_AGE), -1);
        assert_eq!(d.text(TEXT_POSITION), "");
        d.fix_age_ms = Some(5);
        assert_eq!(d.text(TEXT_LAT), "41.0123456");
        assert_eq!(d.text(TEXT_POSITION), "41.0123456, -28.9876543");
        assert_eq!(degrees(-5), "-0.0000005");
        assert_eq!(d.field(99), 0);
    }
}
