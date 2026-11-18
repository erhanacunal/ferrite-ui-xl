//! The board's device settings — UTC offset, NTP server, the backlight and volume last set —
//! and their syscalls (0x50..=0x56).
//!
//! A board keeps them in a text file next to the WiFi settings (exynos4412:
//! `system/settings.txt`), one `key=value` per line:
//!
//! ```text
//! # comment
//! utc_offset=+03:00   # minutes east of UTC: +3, -3:30, +05:45
//! ntp=pool.ntp.org    # NTP server; "off" = no sync; none = DEFAULT_NTP
//! brightness=40       # the backlight a program last set, applied at start
//! volume=70           # the volume a program last set, applied at start
//! ```
//!
//! The RTC keeps UTC; [`crate::rtc::RtcImpl`] shows programs (and the protocol's set-time)
//! local time, UTC + [`utc_offset`]. A board loads the file at start ([`apply`]), and saves it
//! ([`take_due`], [`current`]) once a change has settled for [`SAVE_AFTER_MS`]: a program's
//! `setBrightness` / `setVolume` (the VM calls [`note_brightness`] / [`note_volume`]) or
//! [`SYS_SET_UTC_OFFSET`]. Ids and codes are duplicated in ferrite-ui `lib/device.fl` and the
//! BSPs' `platform.rs` — keep them in sync.

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU32, Ordering};

use crate::pool::ObjPool;

/// The file's name in the board's settings directory (lower case, as on USERDATA's ext4;
/// FAT matches it in any case).
pub const SETTINGS_FILE: &str = "settings.txt";

/// `()` → minutes east of UTC.
pub const SYS_UTC_OFFSET: u8 = 0x50;
/// `(minutes)` → 1 set (saved once settled), -1 out of [`OFFSET_MIN`]..=[`OFFSET_MAX`].
pub const SYS_SET_UTC_OFFSET: u8 = 0x51;
/// `()` → 1 a sync was asked for, 0 none can run (NTP off, no network).
pub const SYS_NTP_SYNC: u8 = 0x52;
/// `()` → `NTP_*`.
pub const SYS_NTP_STATE: u8 = 0x53;
/// `()` → seconds since the last successful sync, -1 none yet.
pub const SYS_NTP_AGE: u8 = 0x54;
/// `()` → string (syscallStr): the NTP server, "" when off.
pub const SYS_NTP_SERVER: u8 = 0x55;
/// `()` → the volume last set (0..=100), -1 none (a program keeps its own default).
pub const SYS_VOLUME: u8 = 0x56;

/// NTP off in the settings (`ntp=off`), or a board without a network.
pub const NTP_OFF: i32 = 0;
/// Waiting for the network.
pub const NTP_WAITING: i32 = 1;
/// A query is out.
pub const NTP_RUNNING: i32 = 2;
/// The clock was set from the last query.
pub const NTP_SYNCED: i32 = 3;
/// The last query failed (no DNS answer, no reply); retried later.
pub const NTP_FAILED: i32 = 4;

pub const OFFSET_MIN: i32 = -12 * 60;
pub const OFFSET_MAX: i32 = 14 * 60;
pub const DEFAULT_NTP: &str = "pool.ntp.org";
/// A change is saved once nothing changed for this long (a slider drag is one save).
pub const SAVE_AFTER_MS: u32 = 2000;
/// The lowest backlight applied at start (a saved 0 would leave a dark screen).
pub const BRIGHTNESS_MIN: u8 = 5;

/// The settings file's contents.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeviceSettings {
    /// Minutes east of UTC.
    pub utc_offset: i32,
    /// The NTP server; empty = [`DEFAULT_NTP`], "off" = no sync.
    pub ntp: String,
    pub brightness: Option<u8>,
    pub volume: Option<u8>,
}

/// `+3`, `+03:00`, `-3:30`, `+0545`, `UTC+3` → minutes; `None` if malformed or out of range.
pub fn parse_offset(text: &str) -> Option<i32> {
    let t = text.trim();
    let t = t.strip_prefix("UTC").or_else(|| t.strip_prefix("utc")).unwrap_or(t).trim();
    if t.is_empty() {
        return Some(0);
    }
    let (sign, rest) = match t.as_bytes()[0] {
        b'-' => (-1, &t[1..]),
        b'+' => (1, &t[1..]),
        _ => (1, t),
    };
    let (h, m) = match rest.split_once(':') {
        Some((h, m)) => (h, m),
        None if rest.len() == 4 => rest.split_at(2),
        None => (rest, "0"),
    };
    let digits = |s: &str| !s.is_empty() && s.len() <= 2 && s.bytes().all(|b| b.is_ascii_digit());
    if !digits(h) || !digits(m) {
        return None;
    }
    let (h, m): (i32, i32) = (h.parse().ok()?, m.parse().ok()?);
    if m >= 60 {
        return None;
    }
    let min = sign * (h * 60 + m);
    (OFFSET_MIN..=OFFSET_MAX).contains(&min).then_some(min)
}

/// Minutes → `+03:00`.
pub fn format_offset(min: i32) -> String {
    let sign = if min < 0 { '-' } else { '+' };
    let a = min.unsigned_abs();
    format!("{}{:02}:{:02}", sign, a / 60, a % 60)
}

fn parse_percent(v: &str) -> Option<u8> {
    v.parse::<u32>().ok().filter(|&p| p <= 100).map(|p| p as u8)
}

impl DeviceSettings {
    /// Parses the settings file; unknown keys and bad lines are skipped.
    pub fn parse(text: &[u8]) -> Self {
        let mut s = Self::default();
        for line in text.split(|&b| b == b'\n') {
            let Ok(line) = core::str::from_utf8(line) else { continue };
            let line = line.split('#').next().unwrap_or("").trim();
            let Some((key, value)) = line.split_once('=') else { continue };
            let value = value.trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "utc_offset" | "utc" | "timezone" => {
                    if let Some(m) = parse_offset(value) {
                        s.utc_offset = m;
                    }
                }
                "ntp" => s.ntp = String::from(value),
                "brightness" => s.brightness = parse_percent(value),
                "volume" => s.volume = parse_percent(value),
                _ => {}
            }
        }
        s
    }

    /// The NTP server to ask, `None` when off.
    pub fn ntp_server(&self) -> Option<&str> {
        match self.ntp.as_str() {
            "" => Some(DEFAULT_NTP),
            s if s.eq_ignore_ascii_case("off") => None,
            s => Some(s),
        }
    }

    /// The settings file.
    pub fn format(&self) -> Vec<u8> {
        let mut out = String::from("# Device settings (ferrite-ui-xl)\n");
        out.push_str(&format!("utc_offset={}\n", format_offset(self.utc_offset)));
        if !self.ntp.is_empty() {
            out.push_str(&format!("ntp={}\n", self.ntp));
        }
        if let Some(b) = self.brightness {
            out.push_str(&format!("brightness={}\n", b));
        }
        if let Some(v) = self.volume {
            out.push_str(&format!("volume={}\n", v));
        }
        out.into_bytes()
    }
}

// --- live state (CPU0) ------------------------------------------------------------------------

static OFFSET: AtomicI32 = AtomicI32::new(0);
/// -1 = none set.
static BRIGHTNESS: AtomicI32 = AtomicI32::new(-1);
static VOLUME: AtomicI32 = AtomicI32::new(-1);
static CHANGED: AtomicBool = AtomicBool::new(false);
static CHANGED_AT: AtomicU32 = AtomicU32::new(0);

/// Takes the loaded settings as the live ones (at start: nothing to save).
pub fn apply(s: &DeviceSettings) {
    OFFSET.store(s.utc_offset, Ordering::Relaxed);
    BRIGHTNESS.store(s.brightness.map_or(-1, i32::from), Ordering::Relaxed);
    VOLUME.store(s.volume.map_or(-1, i32::from), Ordering::Relaxed);
}

/// Minutes east of UTC.
pub fn utc_offset() -> i32 {
    OFFSET.load(Ordering::Relaxed)
}

/// The backlight to apply at start, if one was saved.
pub fn boot_brightness() -> Option<u8> {
    u8::try_from(BRIGHTNESS.load(Ordering::Relaxed)).ok().map(|b| b.clamp(BRIGHTNESS_MIN, 100))
}

/// The volume last set, -1 none.
pub fn volume() -> i32 {
    VOLUME.load(Ordering::Relaxed)
}

fn change(cell: &AtomicI32, value: i32, now_ms: u32) {
    if cell.swap(value, Ordering::Relaxed) != value {
        CHANGED_AT.store(now_ms, Ordering::Relaxed);
        CHANGED.store(true, Ordering::Release);
    }
}

/// Sets the UTC offset; false if out of range.
pub fn set_utc_offset(min: i32, now_ms: u32) -> bool {
    if !(OFFSET_MIN..=OFFSET_MAX).contains(&min) {
        return false;
    }
    change(&OFFSET, min, now_ms);
    true
}

/// A program set the backlight.
pub fn note_brightness(percent: u8, now_ms: u32) {
    change(&BRIGHTNESS, i32::from(percent.min(100)), now_ms);
}

/// A program set the volume.
pub fn note_volume(vol: u8, now_ms: u32) {
    change(&VOLUME, i32::from(vol.min(100)), now_ms);
}

/// True (once) when a change should be saved now: settled for [`SAVE_AFTER_MS`], or any
/// change at all with `force` (before a restart or power-off).
pub fn take_due(now_ms: u32, force: bool) -> bool {
    if !CHANGED.load(Ordering::Acquire) {
        return false;
    }
    if !force && now_ms.wrapping_sub(CHANGED_AT.load(Ordering::Relaxed)) < SAVE_AFTER_MS {
        return false;
    }
    CHANGED.store(false, Ordering::Release);
    true
}

/// `base` (as loaded: the NTP server) with the live values.
pub fn current(base: &DeviceSettings) -> DeviceSettings {
    let pct = |c: &AtomicI32| u8::try_from(c.load(Ordering::Relaxed)).ok();
    DeviceSettings {
        utc_offset: utc_offset(),
        ntp: base.ntp.clone(),
        brightness: pct(&BRIGHTNESS),
        volume: pct(&VOLUME),
    }
}

// --- syscalls -----------------------------------------------------------------------------------

/// What a board's network says about the clock sync.
pub trait NtpBackend {
    /// Ask for a sync now; false if none can run (NTP off, no network).
    fn ntp_request(&mut self) -> bool;
    /// `NTP_*`.
    fn ntp_state(&self) -> i32;
    /// Seconds since the last successful sync, -1 none.
    fn ntp_age(&self) -> i32;
    /// The server, "" when off.
    fn ntp_server(&self) -> String;
}

/// A board without a network.
pub struct NoNtp;

impl NtpBackend for NoNtp {
    fn ntp_request(&mut self) -> bool {
        false
    }
    fn ntp_state(&self) -> i32 {
        NTP_OFF
    }
    fn ntp_age(&self) -> i32 {
        -1
    }
    fn ntp_server(&self) -> String {
        String::new()
    }
}

/// Answers a device settings syscall; `None` for any other id.
pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool, now_ms: u32, b: &mut impl NtpBackend) -> Option<i32> {
    let arg0 = args.first().copied().unwrap_or(0);
    match id {
        SYS_UTC_OFFSET => Some(utc_offset()),
        SYS_SET_UTC_OFFSET => Some(if set_utc_offset(arg0, now_ms) { 1 } else { -1 }),
        SYS_NTP_SYNC => Some(b.ntp_request() as i32),
        SYS_NTP_STATE => Some(b.ntp_state()),
        SYS_NTP_AGE => Some(b.ntp_age()),
        SYS_NTP_SERVER => objs.str_alloc(b.ntp_server().as_bytes()).map(|id| id as i32),
        SYS_VOLUME => Some(volume()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn offsets_parse_and_format() {
        assert_eq!(parse_offset("+3"), Some(180));
        assert_eq!(parse_offset("+03:00"), Some(180));
        assert_eq!(parse_offset("UTC+3"), Some(180));
        assert_eq!(parse_offset("-3:30"), Some(-210));
        assert_eq!(parse_offset("+0545"), Some(345));
        assert_eq!(parse_offset("0"), Some(0));
        assert_eq!(parse_offset("+15"), None);
        assert_eq!(parse_offset("+3:60"), None);
        assert_eq!(parse_offset("abc"), None);
        assert_eq!(format_offset(180), "+03:00");
        assert_eq!(format_offset(-210), "-03:30");
        assert_eq!(format_offset(0), "+00:00");
    }

    #[test]
    fn settings_survive_a_save_and_a_read() {
        let s = DeviceSettings { utc_offset: 345, ntp: "time.google.com".into(), brightness: Some(40), volume: Some(0) };
        assert_eq!(DeviceSettings::parse(&s.format()), s);
        let d = DeviceSettings::default();
        assert_eq!(DeviceSettings::parse(&d.format()), d);
        assert_eq!(d.ntp_server(), Some(DEFAULT_NTP));
        let off = DeviceSettings::parse(b"ntp=off # no sync\nvolume=101\nbrightness=x\n");
        assert_eq!(off.ntp_server(), None);
        assert_eq!((off.volume, off.brightness), (None, None));
    }
}
