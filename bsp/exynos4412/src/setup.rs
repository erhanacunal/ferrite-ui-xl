//! The setup screen: a built-in program that runs instead of the saved one
//! when volume up is held as the firmware starts, or when there is no saved
//! program to run (it replaces the old recovery screen).
//!
//! Its source is `system/setup/` (a `.fl` program); `scripts/build_setup.py`
//! builds it into `system/setup/setup.img`, an FS image embedded here
//! ([`IMAGE`]). [`install`] puts it into this boot's RAM region in place of
//! `FERRITE/FLASH.BIN`, without touching the file ([`crate::flash::overlay`]);
//! a `writefs`, `bootflash` or `POST /flash` meanwhile replaces the program as
//! usual (and is saved).
//!
//! The program talks to the board through the `ferrite_core::setup` syscalls
//! (0x70..=0x7A, `lib/system.fl`): the WiFi state and settings, a network
//! scan (CPU3 scans while it isn't joined: `net`), and saving the settings
//! as `SYSTEM/WIFI.TXT` on CACHE, which uart_boot reads too. WiFi starts on
//! the setup screen even with no network named, so it can scan.
//!
//! uart_boot uses volume down for itself (hold it to stay in uart_boot), so
//! volume up passes through it to here.

use alloc::string::String;
use core::sync::atomic::{AtomicI32, Ordering};

use exynos_4412_rs::drivers::gpio::{self, Pin, Pull};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::{println, soc};
use ferrite_core::pool::ObjPool;
use ferrite_core::setup::{self as fsetup, Network, SystemBackend, WifiSettings};

/// Held at start (pressed = low): the setup screen. `exynos4412-mehmet.dts`,
/// as the HAL's `power::PowerKeys`.
const VOLUME_UP: Pin = gpio::GPX3.pin(4);

/// The setup program's FS image (`scripts/build_setup.py`).
static IMAGE: &[u8] = include_bytes!("../../../system/setup/setup.img");

/// `fsetup::SETUP_*`: why the setup program runs (0 = it doesn't).
static REASON: AtomicI32 = AtomicI32::new(fsetup::SETUP_NONE);
/// `fsetup::FILE_PROGRAM` when a saved program image was found.
static PROGRAM: AtomicI32 = AtomicI32::new(0);

/// Whether the setup program runs this boot.
pub fn active() -> bool {
    REASON.load(Ordering::Relaxed) != fsetup::SETUP_NONE
}

/// `fsetup::SETUP_*`.
pub fn reason() -> i32 {
    REASON.load(Ordering::Relaxed)
}

/// CPU0, at boot after `flash::init`: decides whether the setup program runs
/// (volume up held, or no saved program) and if so puts it in the region.
pub fn install(mct: Mct) {
    use ferrite_core::platform::Platform;
    let base = crate::platform::Exynos4412Platform::FS_BASE as usize;
    let mut magic = [0u8; 4];
    <crate::flash::RamFlash as ferrite_core::flash::FlashBackend>::read(
        &crate::flash::RamFlash,
        base as u32,
        &mut magic,
    );
    let has_program = &magic == b"FERR";
    if has_program {
        PROGRAM.store(fsetup::FILE_PROGRAM, Ordering::Relaxed);
    }
    VOLUME_UP.set_input(Pull::Up);
    let start = mct.count();
    while mct.count() - start < 5 * (soc::MCT_CLOCK_HZ as u64 / 1000) {} // the pull-up settles
    let reason = if !VOLUME_UP.read() {
        fsetup::SETUP_KEY
    } else if !has_program {
        fsetup::SETUP_NO_PROGRAM
    } else {
        return;
    };
    REASON.store(reason, Ordering::Relaxed);
    crate::flash::overlay(base, IMAGE);
    println!(
        "ferrite: setup screen ({})",
        if reason == fsetup::SETUP_KEY { "volume up held" } else { "no saved program" }
    );
}

/// `fsetup::FILE_*` bits.
pub fn files() -> i32 {
    let storage = if crate::storage::available() { fsetup::FILE_STORAGE } else { 0 };
    #[cfg(feature = "wifi")]
    let found = crate::net::config_loaded().map_or(0, |c| c.files);
    #[cfg(not(feature = "wifi"))]
    let found = 0;
    found | storage | PROGRAM.load(Ordering::Relaxed)
}

/// A `SYSTEM/` file name from a request: upper-cased into `out` if it is an
/// 8.3 name of letters, digits, `_` and `-`. Its length. Same rule as
/// uart_boot's `settings::system_name`.
pub fn system_name(raw: &[u8], out: &mut [u8; 12]) -> Option<usize> {
    let (base, ext) = match raw.iter().position(|&b| b == b'.') {
        Some(dot) => (&raw[..dot], &raw[dot + 1..]),
        None => (raw, &b""[..]),
    };
    let ok = |part: &[u8], max: usize| part.len() <= max && part.iter().all(|b| b.is_ascii_alphanumeric() || *b == b'_' || *b == b'-');
    if base.is_empty() || !ok(base, 8) || !ok(ext, 3) || raw.ends_with(b".") {
        return None;
    }
    for (o, b) in out.iter_mut().zip(raw) {
        *o = b.to_ascii_uppercase();
    }
    Some(raw.len())
}

/// The settings last saved by the setup screen (shown instead of what was
/// read at boot). CPU0 only.
static mut SAVED: Option<WifiSettings> = None;

struct Board;

impl SystemBackend for Board {
    fn wifi_state(&self) -> i32 {
        #[cfg(feature = "wifi")]
        return crate::net::state_code();
        #[cfg(not(feature = "wifi"))]
        fsetup::WIFI_OFF
    }

    fn settings(&self) -> WifiSettings {
        // SAFETY: CPU0 only (syscalls run on the VM's core).
        if let Some(s) = unsafe { (*(&raw const SAVED)).clone() } {
            return s;
        }
        #[cfg(feature = "wifi")]
        if let Some(c) = crate::net::config_loaded() {
            return c.settings.clone();
        }
        WifiSettings::default()
    }

    fn address(&self) -> String {
        #[cfg(feature = "wifi")]
        {
            let l = crate::net::link();
            if l.address != [0; 4] {
                let [a, b, c, d] = l.address;
                return alloc::format!("{}.{}.{}.{}", a, b, c, d);
            }
        }
        String::new()
    }

    fn mac(&self) -> String {
        #[cfg(feature = "wifi")]
        {
            let m = crate::net::link().mac;
            if m != [0; 6] {
                return alloc::format!("{:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x}", m[0], m[1], m[2], m[3], m[4], m[5]);
            }
        }
        String::new()
    }

    fn joined(&self) -> String {
        #[cfg(feature = "wifi")]
        if crate::net::running() {
            return crate::net::config().settings.ssid.clone();
        }
        String::new()
    }

    fn request_scan(&mut self) -> bool {
        #[cfg(feature = "wifi")]
        return crate::net::request_scan();
        #[cfg(not(feature = "wifi"))]
        false
    }

    fn with_scan(&self, f: &mut dyn FnMut(Option<&[Network]>, u32)) {
        #[cfg(feature = "wifi")]
        crate::net::with_scan(f);
        #[cfg(not(feature = "wifi"))]
        f(None, 0);
    }

    fn save(&mut self, settings: &WifiSettings, file: &[u8]) -> bool {
        #[cfg(feature = "wifi")]
        let name = crate::net::WIFI_FILE;
        #[cfg(not(feature = "wifi"))]
        let name = "WIFI.TXT";
        let ok = crate::storage::save_in(crate::storage::SYSTEM_DIR, name, file);
        if ok {
            println!("ferrite: setup saved {}/{} (network {})", crate::storage::SYSTEM_DIR, name, settings.ssid);
            // SAFETY: CPU0 only.
            unsafe { *(&raw mut SAVED) = Some(settings.clone()) };
        }
        ok
    }

    fn files(&self) -> i32 {
        files()
    }

    fn setup_reason(&self) -> i32 {
        reason()
    }
}

/// The setup syscalls (`ferrite_core::setup`); `None` for other ids.
pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
    fsetup::syscall(id, args, objs, &mut Board)
}
