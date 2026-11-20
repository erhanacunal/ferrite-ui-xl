//! GPS syscalls (`lib/gps.fl`, `ferrite_core::gps`): the u-blox G6010 on UART1 (HAL
//! `drivers::g6010`, `nmea`).
//!
//! Off at boot. The first `SYS_GPS_POWER(1)` sets UART1 up (9600 baud, the receive interrupt
//! to CPU0 into the HAL's 2 KiB ring: the receiver sends ~180 bytes a second, so the ring holds
//! ~10 s) and turns the supplies on (MAX77686 LDO9, 10 ms, LDO18; on I2C0 from CPU0, as the
//! audio codec's rails). [`poll`] parses what came, from every pass (`frame_end`) and from
//! each syscall. A program that ends leaves the receiver as it was. CPU0 only, except the
//! `LAST_*` figures that `/api/status` reads on CPU3.

use core::sync::atomic::{AtomicI32, Ordering};

use exynos_4412_rs::drivers::g6010;
use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::uart::UartRx;
use exynos_4412_rs::nmea::{Fix, Kind, LineBuffer};
use ferrite_core::gps::{GpsBackend, GpsData};
use ferrite_core::rtc::DateTime;

use crate::systick;

/// For `/api/status`: the `GPS_*` state at the last poll (-1 = never powered), satellites
/// used / in view, the last valid position (degrees × 10^7).
pub static LAST_STATE: AtomicI32 = AtomicI32::new(-1);
pub static LAST_SATS_USED: AtomicI32 = AtomicI32::new(0);
pub static LAST_SATS_VIEW: AtomicI32 = AtomicI32::new(0);
pub static LAST_LAT: AtomicI32 = AtomicI32::new(0);
pub static LAST_LON: AtomicI32 = AtomicI32::new(0);

struct Gps {
    rx: Option<UartRx>,
    powered: bool,
    lines: LineBuffer,
    fix: Fix,
    /// `systick::millis` of the last good sentence / the last one with a valid position.
    sentence_at: Option<u32>,
    fix_at: Option<u32>,
    /// The last valid position and motion (`data.latitude` .. `course_centidegrees`).
    data: GpsData,
}

static mut GPS: Gps = Gps {
    rx: None,
    powered: false,
    lines: LineBuffer::new(),
    fix: Fix {
        time: None,
        date: None,
        valid: false,
        latitude: None,
        longitude: None,
        speed_milliknots: None,
        course_centidegrees: None,
        quality: 0,
        satellites_used: 0,
        hdop_centi: None,
        altitude_dm: None,
        satellites_in_view: 0,
    },
    sentence_at: None,
    fix_at: None,
    data: GpsData {
        powered: false,
        sentence_age_ms: None,
        fix_age_ms: None,
        latitude: 0,
        longitude: 0,
        altitude_dm: 0,
        speed_milliknots: 0,
        course_centidegrees: 0,
        quality: 0,
        satellites_used: 0,
        satellites_in_view: 0,
        hdop_centi: 9999,
        utc_unix: 0,
    },
};

fn state() -> &'static mut Gps {
    // SAFETY: syscalls and `frame_end` run on CPU0 only, never reentrant.
    unsafe { &mut *(&raw mut GPS) }
}

/// The PMIC: I2C0 is set up at boot (main.rs).
fn pmic() -> Max77686 {
    Max77686::new(I2c::new(Bus::I2c0))
}

/// Turns the receiver on or off. False if the PMIC did not take it.
pub fn power(on: bool) -> bool {
    let st = state();
    if on == st.powered {
        return true;
    }
    let mct = Mct::new();
    if on {
        if st.rx.is_none() {
            let baud = g6010::setup_uart(g6010::DEFAULT_BAUD);
            st.rx = UartRx::new(g6010::UART);
            exynos_4412_rs::println!("ferrite: GPS UART1 at {} baud", baud);
        }
        // What the last power-on said is no longer current.
        st.lines = LineBuffer::new();
        st.fix = Fix::default();
        st.sentence_at = None;
        st.fix_at = None;
        st.data = GpsData { hdop_centi: 9999, ..GpsData::default() };
        if let Err(e) = g6010::power_on(&pmic(), mct) {
            exynos_4412_rs::println!("ferrite: GPS power on ERROR {:?}", e);
            let _ = g6010::power_off(&pmic(), mct);
            return false;
        }
    } else if let Err(e) = g6010::power_off(&pmic(), mct) {
        exynos_4412_rs::println!("ferrite: GPS power off ERROR {:?}", e);
        return false;
    }
    st.powered = on;
    publish(st);
    true
}

/// Parses what the receiver sent since the last call. Cheap while it was never powered.
pub fn poll() {
    let st = state();
    let Some(rx) = st.rx else { return };
    let mut buf = [0u8; 64];
    loop {
        let n = rx.try_read(&mut buf);
        if n == 0 {
            break;
        }
        if !st.powered {
            continue; // the line floats while off: drop it
        }
        for &byte in &buf[..n] {
            let Some(sentence) = st.lines.push(byte) else { continue };
            let Ok(kind) = st.fix.update(sentence) else { continue };
            let now = systick::millis();
            st.sentence_at = Some(now);
            take(&mut st.data, &st.fix);
            if matches!(kind, Kind::Rmc | Kind::Gga) && st.fix.valid && st.fix.latitude.is_some() {
                st.fix_at = Some(now);
            }
        }
    }
    publish(st);
}

/// Copies what `fix` carries into `data`; the position and motion only while it is valid.
fn take(data: &mut GpsData, fix: &Fix) {
    data.quality = fix.quality;
    data.satellites_used = fix.satellites_used;
    data.satellites_in_view = fix.satellites_in_view;
    data.hdop_centi = fix.hdop_centi.unwrap_or(9999);
    if let (Some(t), Some(d)) = (fix.time, fix.date) {
        let dt = DateTime {
            year: (d.year.saturating_sub(2000)).min(99) as u8,
            month: d.month,
            day: d.day,
            weekday: 0,
            hour: t.hour,
            minute: t.minute,
            second: t.second.min(59),
        };
        data.utc_unix = dt.to_unix();
    }
    if fix.valid {
        if let (Some(lat), Some(lon)) = (fix.latitude, fix.longitude) {
            data.latitude = lat;
            data.longitude = lon;
        }
        if let Some(alt) = fix.altitude_dm {
            data.altitude_dm = alt;
        }
        if let Some(speed) = fix.speed_milliknots {
            data.speed_milliknots = speed;
        }
        if let Some(course) = fix.course_centidegrees {
            data.course_centidegrees = i32::from(course);
        }
    }
}

fn current(st: &Gps) -> GpsData {
    let now = systick::millis();
    GpsData {
        powered: st.powered,
        sentence_age_ms: st.sentence_at.map(|t| now.wrapping_sub(t)),
        fix_age_ms: st.fix_at.map(|t| now.wrapping_sub(t)),
        ..st.data
    }
}

fn publish(st: &Gps) {
    let d = current(st);
    LAST_STATE.store(d.state(), Ordering::Relaxed);
    LAST_SATS_USED.store(i32::from(d.satellites_used), Ordering::Relaxed);
    LAST_SATS_VIEW.store(i32::from(d.satellites_in_view), Ordering::Relaxed);
    LAST_LAT.store(d.latitude, Ordering::Relaxed);
    LAST_LON.store(d.longitude, Ordering::Relaxed);
}

/// The receiver, for `ferrite_core::gps::syscall`.
pub struct Board;

impl GpsBackend for Board {
    fn power(&mut self, on: bool) -> bool {
        power(on)
    }

    fn data(&mut self) -> GpsData {
        poll();
        current(state())
    }
}
