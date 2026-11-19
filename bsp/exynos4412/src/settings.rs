//! The device settings (`ferrite_core::settings`): `system/settings.txt`, read at boot
//! ([`load`]: UTC offset, NTP server, the backlight and volume programs last set) and written
//! once a change has settled ([`save_if_due`], from `storage::sync`).

use alloc::string::String;
use core::sync::atomic::{AtomicU32, Ordering};

use exynos_4412_rs::println;
use ferrite_core::pool::ObjPool;
use ferrite_core::settings::{self as fs, DeviceSettings, NtpBackend};

use crate::storage;

const MAX: usize = 4096;

/// Written once by CPU0 at boot (before CPU3 starts), only read after.
static mut LOADED: Option<DeviceSettings> = None;

fn loaded() -> Option<&'static DeviceSettings> {
    // SAFETY: see LOADED.
    unsafe { (*(&raw const LOADED)).as_ref() }
}

/// Reads the file (after `storage::init`) and makes it the live settings.
pub fn load() {
    let s = storage::read_in(storage::SYSTEM_DIR, fs::SETTINGS_FILE, MAX)
        .map(|t| DeviceSettings::parse(&t))
        .unwrap_or_default();
    println!(
        "ferrite: settings UTC{} ntp={} brightness={:?} volume={:?}",
        fs::format_offset(s.utc_offset),
        s.ntp_server().unwrap_or("off"),
        s.brightness,
        s.volume
    );
    fs::apply(&s);
    // SAFETY: CPU0 (see BOOT_APP).
    unsafe { *(&raw mut BOOT_APP) = s.app.clone() };
    // SAFETY: CPU0 at boot, before CPU3 starts (see LOADED).
    unsafe { *(&raw mut LOADED) = Some(s) };
}

/// The app to start at boot (`app=`), changed by `set_boot_app`. CPU0 only.
static mut BOOT_APP: String = String::new();

/// The app to start at boot, if one is set.
pub fn boot_app() -> Option<String> {
    // SAFETY: CPU0 only (see BOOT_APP).
    let app = unsafe { (*(&raw const BOOT_APP)).clone() };
    (!app.is_empty()).then_some(app)
}

/// Sets (or with "" clears) the app to start at boot; saved like any setting change.
pub fn set_boot_app(id: &str) {
    // SAFETY: CPU0 only (see BOOT_APP).
    unsafe { *(&raw mut BOOT_APP) = String::from(id) };
    fs::mark_changed(crate::systick::millis());
}

/// The NTP server to ask (CPU3 reads it too), `None` when off.
pub fn ntp_server() -> Option<&'static str> {
    loaded().map_or(Some(fs::DEFAULT_NTP), |s| s.ntp_server())
}

/// Saves the live settings once a change has settled (any change with `force`).
pub fn save_if_due(force: bool) {
    if !fs::take_due(crate::systick::millis(), force) {
        return;
    }
    let mut s = fs::current(loaded().unwrap_or(&DeviceSettings::default()));
    s.app = boot_app().unwrap_or_default();
    if !storage::save_in(storage::SYSTEM_DIR, fs::SETTINGS_FILE, &s.format()) {
        println!("ferrite: saving {}/{} failed", storage::SYSTEM_DIR, fs::SETTINGS_FILE);
    }
}

// --- NTP state, shared with CPU3 (`net::ntp`) ----------------------------------------------

/// `fs::NTP_*`, set by CPU3.
pub static NTP_STATE: AtomicU32 = AtomicU32::new(fs::NTP_WAITING as u32);
/// `systick::millis` at the last successful sync, 0 none.
pub static NTP_SYNCED_AT: AtomicU32 = AtomicU32::new(0);
/// Set by a program (CPU0), taken by CPU3.
pub static NTP_REQUEST: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Seconds since the last successful sync, -1 none.
pub fn ntp_age() -> i32 {
    match NTP_SYNCED_AT.load(Ordering::Acquire) {
        0 => -1,
        at => (crate::systick::millis().wrapping_sub(at) / 1000) as i32,
    }
}

struct Ntp;

impl NtpBackend for Ntp {
    fn ntp_request(&mut self) -> bool {
        if ntp_server().is_none() || !cfg!(feature = "wifi") {
            return false;
        }
        #[cfg(feature = "wifi")]
        if !crate::net::running() {
            return false;
        }
        NTP_REQUEST.store(true, Ordering::Release);
        true
    }

    fn ntp_state(&self) -> i32 {
        if ntp_server().is_none() || !cfg!(feature = "wifi") {
            return fs::NTP_OFF;
        }
        NTP_STATE.load(Ordering::Acquire) as i32
    }

    fn ntp_age(&self) -> i32 {
        ntp_age()
    }

    fn ntp_server(&self) -> String {
        String::from(ntp_server().unwrap_or(""))
    }
}

pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
    fs::syscall(id, args, objs, crate::systick::millis(), &mut Ntp)
}
