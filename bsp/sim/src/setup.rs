//! The setup syscalls (0x70..=0x7A, `ferrite_core::setup`) on the host: a fake
//! WiFi chip that is scanning (no network named) and finds a few networks, and
//! settings kept in RAM (gone at exit), so `system/setup` can be run in the sim.

use std::sync::Mutex;

use ferrite_core::pool::ObjPool;
use ferrite_core::setup::{self, Network, ScanList, SystemBackend, WifiSettings};

use crate::systick;

/// A scan takes this long (the board's takes ~2 s).
const SCAN_MS: u32 = 1500;

const NETWORKS: &[(&str, i16, u8, bool)] = &[
    ("HomeNet", -48, 6, true),
    ("Office 5G", -61, 44, true),
    ("Guest", -70, 1, false),
    ("Workshop", -77, 11, true),
    ("Neighbour", -86, 3, true),
];

struct Fake {
    settings: WifiSettings,
    saved: bool,
    list: ScanList<32>,
    scans: u32,
    /// When the running scan ends (`None`: none running).
    scan_ends: Option<u32>,
}

static FAKE: Mutex<Option<Fake>> = Mutex::new(None);

impl Fake {
    fn new() -> Self {
        Self { settings: WifiSettings::default(), saved: false, list: ScanList::new(), scans: 0, scan_ends: Some(systick::millis() + SCAN_MS) }
    }

    /// Ends the running scan when its time is up.
    fn tick(&mut self) {
        if let Some(t) = self.scan_ends
            && systick::millis().wrapping_sub(t) as i32 >= 0
        {
            self.scan_ends = None;
            self.list = ScanList::new();
            for &(ssid, rssi, channel, joinable) in NETWORKS {
                // A little noise so a rescan reorders nothing but shows new numbers.
                let rssi = rssi - (self.scans % 3) as i16;
                self.list.add(ssid.as_bytes(), rssi, channel, joinable);
            }
            self.list.finish();
            self.scans += 1;
        }
    }
}

impl SystemBackend for Fake {
    fn wifi_state(&self) -> i32 {
        setup::WIFI_SCANNING
    }
    fn settings(&self) -> WifiSettings {
        self.settings.clone()
    }
    fn address(&self) -> String {
        String::new()
    }
    fn mac(&self) -> String {
        "02:00:00:5e:00:01".into()
    }
    fn joined(&self) -> String {
        String::new()
    }
    fn request_scan(&mut self) -> bool {
        if self.scan_ends.is_none() {
            self.scan_ends = Some(systick::millis() + SCAN_MS);
        }
        true
    }
    fn with_scan(&self, f: &mut dyn FnMut(Option<&[Network]>, u32)) {
        let list = (self.scans > 0).then(|| &self.list.nets[..self.list.count]);
        f(list, self.scans);
    }
    fn save(&mut self, settings: &WifiSettings, file: &[u8]) -> bool {
        eprintln!("sim: SYSTEM/WIFI.TXT (not kept):\n{}", String::from_utf8_lossy(file));
        self.settings = settings.clone();
        self.saved = true;
        true
    }
    fn files(&self) -> i32 {
        let settings = if self.saved { setup::FILE_SETTINGS } else { 0 };
        settings | setup::FILE_FIRMWARE | setup::FILE_NVRAM | setup::FILE_STORAGE
    }
    fn setup_reason(&self) -> i32 {
        setup::SETUP_KEY
    }
}

pub fn syscall(id: u8, args: &[i32], objs: &mut ObjPool) -> Option<i32> {
    let mut fake = FAKE.lock().unwrap();
    let fake = fake.get_or_insert_with(Fake::new);
    fake.tick();
    setup::syscall(id, args, objs, fake)
}
