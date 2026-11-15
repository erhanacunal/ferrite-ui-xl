//! The board's settings and the setup screen's syscalls (0x70..=0x7A).
//!
//! The settings live in files a board keeps outside the programs' storage (exynos4412: `SYSTEM/`
//! on CACHE, shared with uart_boot). The WiFi settings are text, one `key=value` per line:
//!
//! ```text
//! # comment (whole lines only: a password may hold a '#')
//! ssid=MyNetwork
//! password=passphrase
//! band=2.4          # optional: 2.4 or 5; inline comments after band / hostname only
//! hostname=exynos   # optional DHCP hostname
//! ```
//!
//! A board answers the syscalls through [`SystemBackend`] ([`syscall`]); the setup program
//! (`system/setup/main.fl`) and `lib/system.fl` use them. Ids and codes are duplicated in
//! `lib/system.fl` and `uart_boot/src/settings.rs` (the file format) — keep them in sync.

use alloc::string::String;
use alloc::vec::Vec;

use crate::pool::ObjPool;

/// `()` → the WiFi state, `WIFI_*`.
pub const SYS_WIFI_STATE: u8 = 0x70;
/// `(field)` → string (`syscallStr`), `INFO_*`; "" when unknown.
pub const SYS_WIFI_INFO: u8 = 0x71;
/// `()` → 1 if a new scan was asked for, 0 if none can run now (joined, chip off).
pub const SYS_WIFI_SCAN: u8 = 0x72;
/// `()` → networks in the last scan; -1 before the first one ends.
pub const SYS_WIFI_SCAN_COUNT: u8 = 0x73;
/// `(i)` → the name of network `i` of the last scan (`syscallStr`), strongest first.
pub const SYS_WIFI_SCAN_SSID: u8 = 0x74;
/// `(i)` → its signal, dBm (0 past the end).
pub const SYS_WIFI_SCAN_RSSI: u8 = 0x75;
/// `(i)` → its channel | 0x100 if it can be joined (WPA2-PSK) (0 past the end).
pub const SYS_WIFI_SCAN_INFO: u8 = 0x76;
/// `(ssid, password, band, hostname)` → `SAVE_*`: writes the WiFi settings (strings; band 0 / 2 /
/// 5). They count from the next start.
pub const SYS_WIFI_SAVE: u8 = 0x77;
/// `()` → what the board has, `FILE_*` bits.
pub const SYS_SYSTEM_FILES: u8 = 0x78;
/// `()` → why the setup program runs, `SETUP_*`.
pub const SYS_SETUP_REASON: u8 = 0x79;
/// `()` → scans completed since boot (changes when the list does).
pub const SYS_WIFI_SCANS: u8 = 0x7A;

pub const WIFI_OFF: i32 = 0;
pub const WIFI_JOINING: i32 = 1;
pub const WIFI_DHCP: i32 = 2;
pub const WIFI_UP: i32 = 3;
pub const WIFI_FAILED: i32 = 4;
pub const WIFI_DOWN: i32 = 5;
pub const WIFI_ASLEEP: i32 = 6;
/// Chip up, no network named: scanning only (setup).
pub const WIFI_SCANNING: i32 = 7;
/// No firmware for the chip.
pub const WIFI_NO_FIRMWARE: i32 = 8;

pub const INFO_SSID: i32 = 0;
pub const INFO_PASSWORD: i32 = 1;
/// "2.4", "5" or "".
pub const INFO_BAND: i32 = 2;
pub const INFO_HOSTNAME: i32 = 3;
/// "a.b.c.d" once DHCP gave one.
pub const INFO_ADDRESS: i32 = 4;
pub const INFO_MAC: i32 = 5;
/// The network joined (the settings' while joining).
pub const INFO_JOINED: i32 = 6;

pub const SAVE_OK: i32 = 1;
pub const SAVE_FAILED: i32 = 0;
/// A value is not valid: see [`WifiSettings::validate`].
pub const SAVE_INVALID: i32 = -1;

pub const FILE_SETTINGS: i32 = 1;
pub const FILE_FIRMWARE: i32 = 2;
pub const FILE_NVRAM: i32 = 4;
/// A program image of the user's is saved (the setup program is not it).
pub const FILE_PROGRAM: i32 = 8;
/// The settings can be written.
pub const FILE_STORAGE: i32 = 16;

pub const SETUP_NONE: i32 = 0;
/// Volume up held at start.
pub const SETUP_KEY: i32 = 1;
/// No program image to run.
pub const SETUP_NO_PROGRAM: i32 = 2;

pub const DEFAULT_HOSTNAME: &str = "exynos";
pub const SSID_MAX: usize = 32;
pub const HOSTNAME_MAX: usize = 32;

/// The WiFi settings.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct WifiSettings {
    pub ssid: String,
    pub password: String,
    /// 2 = 2.4 GHz only, 5 = 5 GHz only, 0 = either.
    pub band: u8,
    /// Empty = [`DEFAULT_HOSTNAME`].
    pub hostname: String,
}

pub fn parse_band(v: &str) -> u8 {
    match v.trim() {
        "2.4" | "2" => 2,
        "5" => 5,
        _ => 0,
    }
}

fn band_text(band: u8) -> &'static str {
    match band {
        2 => "2.4",
        5 => "5",
        _ => "",
    }
}

impl WifiSettings {
    /// Parses the settings file; unknown keys and bad lines are skipped.
    pub fn parse(text: &[u8]) -> Self {
        let mut s = Self::default();
        for line in text.split(|&b| b == b'\n') {
            let Ok(line) = core::str::from_utf8(line) else { continue };
            let line = line.trim();
            if line.starts_with('#') {
                continue;
            }
            let Some((key, value)) = line.split_once('=') else { continue };
            // Name and passphrase are taken whole; a '#' may be part of them.
            let short = || value.split('#').next().unwrap_or("").trim();
            match key.trim().to_ascii_lowercase().as_str() {
                "ssid" => s.ssid = String::from(value.trim()),
                "password" | "psk" => s.password = String::from(value.trim()),
                "band" => s.band = parse_band(short()),
                "hostname" => s.hostname = String::from(short()),
                _ => {}
            }
        }
        s
    }

    /// The hostname to give DHCP.
    pub fn hostname(&self) -> &str {
        if self.hostname.is_empty() { DEFAULT_HOSTNAME } else { &self.hostname }
    }

    /// Whether it can be saved and joined: a name of 1..=32 bytes, a WPA2 passphrase of 8..=63
    /// characters (or 64 hex digits), a hostname of letters, digits and '-' (or none), and no
    /// line breaks anywhere.
    pub fn validate(&self) -> bool {
        let one_line = |s: &str| !s.contains(['\n', '\r']);
        let pass_ok = match self.password.len() {
            8..=63 => true,
            64 => self.password.bytes().all(|b| b.is_ascii_hexdigit()),
            _ => false,
        };
        let host_ok = self.hostname.len() <= HOSTNAME_MAX
            && self.hostname.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-');
        (1..=SSID_MAX).contains(&self.ssid.len())
            && self.ssid.trim() == self.ssid
            && self.password.trim() == self.password
            && one_line(&self.ssid)
            && one_line(&self.password)
            && pass_ok
            && host_ok
            && matches!(self.band, 0 | 2 | 5)
    }

    /// The settings file, if [`validate`](Self::validate) passes.
    pub fn format(&self) -> Option<Vec<u8>> {
        if !self.validate() {
            return None;
        }
        let mut out = String::from("# WiFi settings (ferrite-ui-xl setup screen)\n");
        for (key, value) in [
            ("ssid", self.ssid.as_str()),
            ("password", self.password.as_str()),
            ("band", band_text(self.band)),
            ("hostname", self.hostname.as_str()),
        ] {
            if !value.is_empty() {
                out.push_str(key);
                out.push('=');
                out.push_str(value);
                out.push('\n');
            }
        }
        Some(out.into_bytes())
    }
}

/// One network a scan found.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Network {
    pub ssid: [u8; SSID_MAX],
    pub ssid_len: u8,
    pub rssi: i16,
    pub channel: u8,
    /// WPA2-PSK, which this board can join.
    pub joinable: bool,
}

impl Network {
    pub const EMPTY: Self = Self { ssid: [0; SSID_MAX], ssid_len: 0, rssi: 0, channel: 0, joinable: false };

    pub fn name(&self) -> &[u8] {
        &self.ssid[..usize::from(self.ssid_len).min(SSID_MAX)]
    }
}

/// The networks of one scan: one entry per name (the strongest access point), hidden networks
/// left out, strongest first once [`finish`](Self::finish)ed.
#[derive(Clone, Copy, Debug)]
pub struct ScanList<const N: usize> {
    pub nets: [Network; N],
    pub count: usize,
}

impl<const N: usize> Default for ScanList<N> {
    fn default() -> Self {
        Self::new()
    }
}

impl<const N: usize> ScanList<N> {
    pub const fn new() -> Self {
        Self { nets: [Network::EMPTY; N], count: 0 }
    }

    /// Adds an access point; a stronger one replaces a weaker of the same name (and keeps it
    /// joinable if either is), and when full the weakest goes.
    pub fn add(&mut self, ssid: &[u8], rssi: i16, channel: u8, joinable: bool) {
        if ssid.is_empty() || ssid.iter().all(|&b| b == 0) {
            return;
        }
        let ssid = &ssid[..ssid.len().min(SSID_MAX)];
        let mut net = Network { ssid: [0; SSID_MAX], ssid_len: ssid.len() as u8, rssi, channel, joinable };
        net.ssid[..ssid.len()].copy_from_slice(ssid);
        if let Some(known) = self.nets[..self.count].iter_mut().find(|n| n.name() == ssid) {
            if rssi > known.rssi {
                net.joinable |= known.joinable;
                *known = net;
            } else {
                known.joinable |= joinable;
            }
            return;
        }
        if self.count < N {
            self.nets[self.count] = net;
            self.count += 1;
        } else if let Some(weakest) = self.nets.iter_mut().min_by_key(|n| n.rssi)
            && weakest.rssi < rssi
        {
            *weakest = net;
        }
    }

    /// Strongest first.
    pub fn finish(&mut self) {
        self.nets[..self.count].sort_by(|a, b| b.rssi.cmp(&a.rssi));
    }

    pub fn get(&self, i: i32) -> Option<&Network> {
        usize::try_from(i).ok().filter(|&i| i < self.count).map(|i| &self.nets[i])
    }
}

/// What a board answers the setup syscalls with.
pub trait SystemBackend {
    /// `WIFI_*`.
    fn wifi_state(&self) -> i32;
    /// The saved settings (what the board read at start, or last saved).
    fn settings(&self) -> WifiSettings;
    /// "a.b.c.d" / "aa:bb:…" / the network joined; "" when unknown.
    fn address(&self) -> String;
    fn mac(&self) -> String;
    fn joined(&self) -> String;
    /// Asks for a new scan; false if none can run now.
    fn request_scan(&mut self) -> bool;
    /// Calls `f` with the last scan's list (`None` before the first one ends) and the number of
    /// scans completed.
    fn with_scan(&self, f: &mut dyn FnMut(Option<&[Network]>, u32));
    /// Writes the settings file (already validated); false if it could not be written.
    fn save(&mut self, settings: &WifiSettings, file: &[u8]) -> bool;
    /// `FILE_*` bits.
    fn files(&self) -> i32;
    /// `SETUP_*`.
    fn setup_reason(&self) -> i32;
}

fn arg(args: &[i32], i: usize) -> i32 {
    args.get(i).copied().unwrap_or(0)
}

fn string_arg(objs: &ObjPool, args: &[i32], i: usize) -> String {
    match u32::try_from(arg(args, i)) {
        Ok(p) => String::from_utf8_lossy(objs.str_get(p as _)).into_owned(),
        Err(_) => String::new(),
    }
}

fn string_result(objs: &mut ObjPool, s: &[u8]) -> Option<i32> {
    objs.str_alloc(s).map(|id| id as i32)
}

/// Answers a setup syscall; `None` for any other id.
pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool, b: &mut impl SystemBackend) -> Option<i32> {
    match id {
        SYS_WIFI_STATE => Some(b.wifi_state()),
        SYS_WIFI_INFO => {
            let s = b.settings();
            let text = match arg(args, 0) {
                INFO_SSID => s.ssid,
                INFO_PASSWORD => s.password,
                INFO_BAND => String::from(band_text(s.band)),
                INFO_HOSTNAME => String::from(s.hostname()),
                INFO_ADDRESS => b.address(),
                INFO_MAC => b.mac(),
                INFO_JOINED => b.joined(),
                _ => String::new(),
            };
            string_result(objs, text.as_bytes())
        }
        SYS_WIFI_SCAN => Some(b.request_scan() as i32),
        SYS_WIFI_SCAN_COUNT | SYS_WIFI_SCANS | SYS_WIFI_SCAN_RSSI | SYS_WIFI_SCAN_INFO => {
            let i = arg(args, 0);
            let mut out = 0;
            b.with_scan(&mut |list, scans| {
                let net = list.and_then(|l| usize::try_from(i).ok().and_then(|i| l.get(i)));
                out = match id {
                    SYS_WIFI_SCAN_COUNT => list.map_or(-1, |l| l.len() as i32),
                    SYS_WIFI_SCANS => scans as i32,
                    SYS_WIFI_SCAN_RSSI => net.map_or(0, |n| i32::from(n.rssi)),
                    _ => net.map_or(0, |n| i32::from(n.channel) | if n.joinable { 0x100 } else { 0 }),
                };
            });
            Some(out)
        }
        SYS_WIFI_SCAN_SSID => {
            let i = arg(args, 0);
            let mut name = Vec::new();
            b.with_scan(&mut |list, _| {
                if let Some(n) = list.and_then(|l| usize::try_from(i).ok().and_then(|i| l.get(i))) {
                    name.extend_from_slice(n.name());
                }
            });
            string_result(objs, &name)
        }
        SYS_WIFI_SAVE => {
            let s = WifiSettings {
                ssid: string_arg(objs, args, 0),
                password: string_arg(objs, args, 1),
                band: match arg(args, 2) {
                    2 => 2,
                    5 => 5,
                    _ => 0,
                },
                hostname: string_arg(objs, args, 3),
            };
            Some(match s.format() {
                None => SAVE_INVALID,
                Some(file) if b.save(&s, &file) => SAVE_OK,
                Some(_) => SAVE_FAILED,
            })
        }
        SYS_SYSTEM_FILES => Some(b.files()),
        SYS_SETUP_REASON => Some(b.setup_reason()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn settings_survive_a_save_and_a_read() {
        let s = WifiSettings {
            ssid: String::from("Ev Ağı #2"),
            password: String::from("pa#ss word"),
            band: 5,
            hostname: String::from("tablet-1"),
        };
        let file = s.format().unwrap();
        assert_eq!(WifiSettings::parse(&file), s);
    }

    #[test]
    fn parse_takes_whole_line_comments_and_inline_ones_after_band_and_hostname() {
        let text = b"# home\nssid = Net\npassword=a#b c\nband=2.4          # optional\nhostname=exynos   # name\nother=1\n";
        let s = WifiSettings::parse(text);
        assert_eq!(s.ssid, "Net");
        assert_eq!(s.password, "a#b c");
        assert_eq!(s.band, 2);
        assert_eq!(s.hostname, "exynos");
        assert_eq!(WifiSettings::default().hostname(), DEFAULT_HOSTNAME);
    }

    #[test]
    fn validate_rejects_what_cannot_be_joined_or_saved() {
        let ok = WifiSettings { ssid: "n".into(), password: "12345678".into(), band: 0, hostname: String::new() };
        assert!(ok.validate());
        let bad = |f: &dyn Fn(&mut WifiSettings)| {
            let mut s = ok.clone();
            f(&mut s);
            s.validate()
        };
        assert!(!bad(&|s| s.ssid.clear()));
        assert!(!bad(&|s| s.ssid = "x".repeat(33)));
        assert!(!bad(&|s| s.password = "short".into()));
        assert!(!bad(&|s| s.password = "x".repeat(64)));
        assert!(bad(&|s| s.password = "a".repeat(64)));
        assert!(!bad(&|s| s.password = "line\nbreak".into()));
        assert!(!bad(&|s| s.hostname = "bad name".into()));
        assert!(!bad(&|s| s.band = 3));
    }

    #[test]
    fn scan_list_keeps_the_strongest_of_each_name_and_sorts() {
        let mut l = ScanList::<3>::new();
        l.add(b"a", -70, 1, false);
        l.add(b"b", -50, 6, true);
        l.add(b"a", -40, 11, false);
        l.add(b"", -10, 1, true);
        l.add(b"c", -80, 3, true);
        l.add(b"d", -60, 5, true); // full: replaces c, the weakest
        l.add(b"e", -90, 5, true); // weaker than all: dropped
        l.finish();
        let names: Vec<&[u8]> = l.nets[..l.count].iter().map(|n| n.name()).collect();
        assert_eq!(names, [&b"a"[..], b"b", b"d"]);
        assert_eq!(l.nets[0].channel, 11);
        assert_eq!(l.get(3), None);
    }
}
