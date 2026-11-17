//! WiFi (BCM4334 over SDIO) + HTTP server + TCP protocol bridge, on CPU3.
//!
//! The HAL's `uart_boot` / `wifi_async_test` steps, as a service next to the
//! runtime:
//!
//! 1. CPU0 at boot ([`start`]): reads the settings and the chip's firmware +
//!    NVRAM ([`load_config`]), powers the chip (`bcm4334::power_on`: PMIC over
//!    I2C0, which CPU0 owns, ~0.5 s), then starts CPU3. Nothing configured →
//!    no WiFi, no delay (except on the setup screen, which scans).
//! 2. CPU3, polled ([`cpu3`]): SDIO + firmware, scan, WPA2 join (retried
//!    every [`RETRY_MS`] until the network is there, sooner when the setup
//!    screen asks for a scan). Every scan's networks are kept ([`scan`]); with
//!    no network named (setup screen) it only scans.
//! 3. CPU3, Embassy: the WiFi runner (host wake interrupt routed to CPU3),
//!    embassy-net with DHCP (hostname from the config, default
//!    [`DEFAULT_HOSTNAME`], as uart_boot, so `ferrite_cli --host` finds
//!    both), [`http`] on port 80 and the protocol bridge on [`BRIDGE_PORT`].
//!
//! The UI core never waits for the network: CPU0 only touches the bridge
//! rings (`bridge.rs`) and, once per pass, [`service`] (an uploaded FS image
//! to save, a restart request).
//!
//! The files are the board's, in `SYSTEM/` on CACHE ([`storage::SYSTEM_DIR`]),
//! shared with uart_boot (`uart_boot/src/settings.rs`): [`WIFI_FILE`] (the
//! settings, format in `ferrite_core::setup`), [`FW_FILE`] and [`NVRAM_FILE`]
//! (the brcmfmac firmware and NVRAM). They come from the setup screen
//! (volume up held at start: `setup.rs`), `POST /api/system?name=` here or
//! `/system/<NAME>` in uart_boot (`ferrite_cli system`, `httpboot.py
//! --system`), or `uartboot.py --system`. Each falls back to the copy this
//! firmware used to keep in `FERRITE/` (`WIFI.CFG`, `WIFIFW.BIN`,
//! `WIFINV.TXT`), then to build-time `WIFI_SSID` / `WIFI_PASSWORD` /
//! `WIFI_BAND` (and `WIFI_FIRMWARE_DIR`, build.rs).
//!
//! [`storage::SYSTEM_DIR`]: crate::storage::SYSTEM_DIR
//! A lost link is not rejoined (the runner ends; restart the board, or go
//! through standby).
//!
//! Standby ([`suspend`] / [`resume`], `platform::enter_standby`): CPU3's tasks
//! end ([`GROUP`]), CPU3 turns the chip off (`bcm4334::power_off`) and stops
//! itself; at the wake CPU0 powers the chip again and starts CPU3, which joins
//! as at boot. While CPU3 is still joining, standby leaves it running.

mod http;

use alloc::boxed::Box;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::{Cell, RefCell};
use core::ptr::addr_of_mut;
use core::sync::atomic::{AtomicBool, AtomicI32, AtomicU8, Ordering};

use embassy_net::tcp::TcpSocket;
use embassy_net::{Config, DhcpConfig, Stack, StackResources};
use embassy_sync::blocking_mutex::CriticalSectionMutex;
use embassy_time::{Duration, Instant, Timer, with_timeout};
use exynos_4412_rs::crypto::{self, Hmac};
use exynos_4412_rs::drivers::bcm4334::{self, Activity, Bcm4334, Error};
use exynos_4412_rs::drivers::gic::Gic;
use exynos_4412_rs::drivers::max77686::Max77686;
use exynos_4412_rs::drivers::sdhci::Sdhci;
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::embassy::wifi;
use exynos_4412_rs::wpa::{self, Supplicant};
use exynos_4412_rs::{cpu, embassy, println, smp, soc};
use ferrite_core::setup::{self as fsetup, Network, ScanList, WifiSettings};

use crate::bridge;
use crate::hotplug::Group;

/// The settings, chip firmware and NVRAM in `SYSTEM/` (8.3 names; uart_boot's
/// `settings.rs` reads the same files — keep in sync) …
pub const WIFI_FILE: &str = "WIFI.TXT";
pub const FW_FILE: &str = "WIFIFW.BIN";
pub const NVRAM_FILE: &str = "WIFINV.TXT";
/// … and what this firmware called the settings in `FERRITE/` before.
const LEGACY_WIFI_FILE: &str = "WIFI.CFG";
const FW_MAX: usize = 2 << 20;
const NVRAM_MAX: usize = 64 << 10;
/// Networks kept from a scan.
pub const SCAN_MAX: usize = 32;
/// The USART protocol as a raw TCP stream (`ferrite_cli -p tcp://HOST:7000`).
pub const BRIDGE_PORT: u16 = 7000;
pub const HTTP_PORT: u16 = 80;
/// The core the network runs on (parked otherwise).
const CORE: usize = 3;
/// CPU3's tasks, ended for standby.
static GROUP: Group = Group::new();
/// How long [`suspend`] waits for CPU3 to end its tasks and lose power.
const STOP_MS: u64 = 3000;

static FIRMWARE: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wifi_firmware.bin"));
static NVRAM: &[u8] = include_bytes!(concat!(env!("OUT_DIR"), "/wifi_nvram.txt"));

const LAST_2G_CHANNEL: u8 = 14;
const SCAN_TIMEOUT_MS: u64 = 20_000;
const JOIN_TIMEOUT_MS: u64 = 30_000;
/// Wait between join attempts (network not found / refused).
const RETRY_MS: u64 = 10_000;
const COUNTS_PER_MS: u64 = soc::MCT_CLOCK_HZ as u64 / 1000;

const RX_FRAMES: usize = 16;
const TX_FRAMES: usize = 16;
/// DHCP + HTTP servers + the bridge, with room to spare.
const SOCKETS: usize = 8;
const BRIDGE_RX: usize = 16 * 1024;
const BRIDGE_TX: usize = 8 * 1024;
/// A bridge client silent this long is dropped (a writefs never pauses
/// that long; `ferrite_cli` reconnects per command).
const BRIDGE_IDLE: Duration = Duration::from_secs(120);

static mut WIFI_STATE: wifi::State<RX_FRAMES, TX_FRAMES> = wifi::State::new();
static mut RESOURCES: StackResources<SOCKETS> = StackResources::new();
/// CPU3: the chip, given back by its runner when the connection was lost, so that
/// [`cpu3`] joins again with it (instead of stopping) once the tasks have ended.
static mut LOST_CHIP: Option<Bcm4334> = None;
/// Set by [`suspend`] (CPU0) while CPU3's tasks end for standby: a connection lost meanwhile
/// is not joined again. Cleared by [`resume`].
static STANDBY: AtomicBool = AtomicBool::new(false);

// --- configuration (CPU0 at boot) ---------------------------------------------------------

pub(crate) struct WifiConfig {
    pub(crate) settings: WifiSettings,
    /// Empty: none found (no WiFi).
    firmware: &'static [u8],
    nvram: &'static [u8],
    /// Which files were found in `SYSTEM/` (`fsetup::FILE_*` bits).
    pub(crate) files: i32,
}

impl WifiConfig {
    fn ssid(&self) -> &str {
        &self.settings.ssid
    }

    fn hostname(&self) -> &str {
        self.settings.hostname()
    }
}

/// Written once by CPU0 at boot (before CPU3 starts), only read after.
static mut CONFIG: Option<WifiConfig> = None;

pub(crate) fn config() -> &'static WifiConfig {
    // SAFETY: see CONFIG.
    unsafe { (*(&raw const CONFIG)).as_ref().expect("set at boot") }
}

/// The config, if [`start`] read it (false on a build or boot without it).
pub(crate) fn config_loaded() -> Option<&'static WifiConfig> {
    // SAFETY: see CONFIG.
    unsafe { (*(&raw const CONFIG)).as_ref() }
}

/// `SYSTEM/name`, else `FERRITE/legacy`: its content and where it came from.
fn wifi_file(name: &str, legacy: &str, max: usize) -> Option<(Vec<u8>, &'static str)> {
    use crate::storage::{DIR, SYSTEM_DIR, read_in};
    if let Some(data) = read_in(SYSTEM_DIR, name, max) {
        return Some((data, SYSTEM_DIR));
    }
    read_in(DIR, legacy, max).map(|data| (data, DIR))
}

/// Reads the settings and the chip's files (CPU0, boot).
fn load_config() -> WifiConfig {
    let mut cfg = WifiConfig { settings: WifiSettings::default(), firmware: FIRMWARE, nvram: NVRAM, files: 0 };
    match wifi_file(WIFI_FILE, LEGACY_WIFI_FILE, 4096) {
        Some((text, from)) => {
            cfg.settings = WifiSettings::parse(&text);
            if from == crate::storage::SYSTEM_DIR {
                cfg.files |= fsetup::FILE_SETTINGS;
            }
            println!("ferrite: wifi settings from {}/", from);
        }
        None => {
            cfg.settings.ssid = String::from(option_env!("WIFI_SSID").unwrap_or(""));
            cfg.settings.password = String::from(option_env!("WIFI_PASSWORD").unwrap_or(""));
            cfg.settings.band = option_env!("WIFI_BAND").map_or(0, fsetup::parse_band);
        }
    }
    let fw = wifi_file(FW_FILE, FW_FILE, FW_MAX);
    let nv = wifi_file(NVRAM_FILE, NVRAM_FILE, NVRAM_MAX);
    for (file, bit) in [(&fw, fsetup::FILE_FIRMWARE), (&nv, fsetup::FILE_NVRAM)] {
        if file.as_ref().is_some_and(|(_, from)| *from == crate::storage::SYSTEM_DIR) {
            cfg.files |= bit;
        }
    }
    if let (Some((fw, _)), Some((nv, _))) = (fw, nv)
        && !fw.is_empty()
        && !nv.is_empty()
    {
        cfg.firmware = Box::leak(fw.into_boxed_slice());
        cfg.nvram = Box::leak(nv.into_boxed_slice());
    }
    cfg
}

/// CPU0, at boot after `storage::init`, `Gic::init` and `embassy::init`:
/// reads the settings, powers the chip and hands the rest to CPU3. `setup`:
/// the setup screen runs, which scans even with no network named. Not fatal
/// on any error.
pub fn start(pmic: Max77686, mct: Mct, setup: bool) {
    let cfg = load_config();
    let named = !cfg.ssid().is_empty();
    let has_firmware = !cfg.firmware.is_empty();
    // SAFETY: CPU3 is not running yet; set once.
    unsafe { *(&raw mut CONFIG) = Some(cfg) };
    if !has_firmware {
        println!(
            "ferrite: wifi off (no chip firmware: {}/{} + {})",
            crate::storage::SYSTEM_DIR,
            FW_FILE,
            NVRAM_FILE
        );
        set_state(State::NoFirmware);
        return;
    }
    if !named && !setup {
        println!("ferrite: wifi off ({}/{} names no network)", crate::storage::SYSTEM_DIR, WIFI_FILE);
        return;
    }
    // PMIC rails + 32 kHz clock over I2C0: on CPU0, which owns that bus.
    if let Err(e) = bcm4334::power_on(pmic, mct) {
        println!("ferrite: wifi power ERROR {:?}", e);
        return;
    }
    set_state(State::Joining);
    STANDBY.store(false, Ordering::Release);
    GROUP.arm();
    if let Err(e) = smp::start_cpu(CORE, cpu3) {
        println!("ferrite: CPU{} ERROR {:?}, wifi off", CORE, e);
        set_state(State::Failed);
    }
}

// --- status, read by /api/status ------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
pub enum State {
    Off = 0,
    Joining = 1,
    /// Joined, waiting for DHCP.
    Joined = 2,
    Up = 3,
    Failed = 4,
    /// The WiFi runner stopped (link lost).
    Down = 5,
    /// Off for standby ([`suspend`]).
    Asleep = 6,
    /// Chip up, no network named: scanning only (setup screen).
    Scanning = 7,
    /// No firmware for the chip.
    NoFirmware = 8,
}

static STATE: AtomicU8 = AtomicU8::new(State::Off as u8);

fn set_state(s: State) {
    STATE.store(s as u8, Ordering::Release);
}

/// Whether CPU3's executor runs (joined, with or without an address).
pub fn running() -> bool {
    matches!(STATE.load(Ordering::Acquire), 2 | 3 | 5)
}

pub fn state_name() -> &'static str {
    match STATE.load(Ordering::Acquire) {
        0 => "off",
        1 => "joining",
        2 => "dhcp",
        3 => "up",
        4 => "failed",
        5 => "down",
        6 => "asleep",
        7 => "scanning",
        _ => "no firmware",
    }
}

/// `ferrite_core::setup::WIFI_*` (the same numbers as [`State`]).
pub fn state_code() -> i32 {
    i32::from(STATE.load(Ordering::Acquire))
}

// --- what scans saw of the configured network (diagnostics, `GET /api/wifi`) ----------------

/// Results kept: one per (BSSID, elements) pair, so a beacon and a probe response that differ
/// both show.
pub const SEEN_MAX: usize = 12;
/// Elements kept of a result (the rest cut).
pub const SEEN_IE_MAX: usize = 640;

#[derive(Clone, Copy)]
pub struct Seen {
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i16,
    /// Results of this BSSID with these elements.
    pub count: u16,
    pub ie_length: u16,
    pub ie: [u8; SEEN_IE_MAX],
}

/// The last scan's results of the configured network, and the last RSN mismatch
/// (expected, carried by message 3, BSSID).
pub struct Diag {
    pub seen: [Option<Seen>; SEEN_MAX],
    pub scans: u32,
    pub mismatches: u32,
    pub expected: ([u8; wpa::RSN_MAX], usize),
    pub carried: ([u8; wpa::RSN_MAX], usize),
    pub mismatch_bssid: [u8; 6],
    /// The BSSID the last join went to, its channel.
    pub target: ([u8; 6], u8),
}

pub static DIAG: CriticalSectionMutex<RefCell<Diag>> = CriticalSectionMutex::new(RefCell::new(Diag {
    seen: [None; SEEN_MAX],
    scans: 0,
    mismatches: 0,
    expected: ([0; wpa::RSN_MAX], 0),
    carried: ([0; wpa::RSN_MAX], 0),
    mismatch_bssid: [0; 6],
    target: ([0; 6], 0),
}));

fn hex(bytes: &[u8]) -> String {
    use core::fmt::Write;
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        let _ = write!(s, "{b:02x}");
    }
    s
}

// --- scan results, read by the setup screen ---------------------------------------------------

struct Scans {
    list: ScanList<SCAN_MAX>,
    /// Completed scans; 0 = no list yet.
    done: u32,
}

static SCANS: CriticalSectionMutex<RefCell<Scans>> =
    CriticalSectionMutex::new(RefCell::new(Scans { list: ScanList::new(), done: 0 }));
/// The setup screen asked for a scan (CPU3 takes it while it may scan).
static SCAN_WANTED: AtomicBool = AtomicBool::new(false);

/// Calls `f` with the last scan's networks (None before the first) and the
/// number of scans done.
pub fn with_scan(f: &mut dyn FnMut(Option<&[Network]>, u32)) {
    SCANS.lock(|s| {
        let s = s.borrow();
        f((s.done > 0).then(|| &s.list.nets[..s.list.count]), s.done)
    })
}

/// Asks CPU3 for a scan; false when it can't scan (not joining or scanning).
pub fn request_scan() -> bool {
    let can = matches!(STATE.load(Ordering::Acquire), 1 | 7);
    if can {
        SCAN_WANTED.store(true, Ordering::Release);
    }
    can
}

fn publish_scan(list: ScanList<SCAN_MAX>) {
    SCANS.lock(|s| {
        let mut s = s.borrow_mut();
        s.list = list;
        s.done = s.done.wrapping_add(1).max(1);
    });
}

/// CPU3: waits up to `ms`, less if a scan is asked for.
fn wait_or_scan_request(mct: Mct, ms: u64) {
    let start = mct.count();
    while mct.count() - start < ms * COUNTS_PER_MS {
        if SCAN_WANTED.swap(false, Ordering::AcqRel) {
            return;
        }
        core::hint::spin_loop();
    }
}

#[derive(Clone, Copy, Default)]
pub struct Link {
    pub mac: [u8; 6],
    pub bssid: [u8; 6],
    pub channel: u8,
    pub rssi: i16,
    pub address: [u8; 4],
    pub prefix: u8,
    pub joined_ms: u64,
}

static LINK: CriticalSectionMutex<Cell<Link>> = CriticalSectionMutex::new(Cell::new(Link {
    mac: [0; 6],
    bssid: [0; 6],
    channel: 0,
    rssi: 0,
    address: [0; 4],
    prefix: 0,
    joined_ms: 0,
}));

pub fn link() -> Link {
    LINK.lock(Cell::get)
}

fn update_link(f: impl FnOnce(&mut Link)) {
    LINK.lock(|c| {
        let mut l = c.get();
        f(&mut l);
        c.set(l);
    });
}

// --- CPU3: polled bring-up + join -----------------------------------------------------------

fn report(activity: Activity) {
    match activity {
        Activity::Event(event)
            if event.code == bcm4334::EVENT_DEAUTH_IND || event.code == bcm4334::EVENT_DISASSOC_IND =>
        {
            println!("ferrite: wifi event {}, reason {}", event.code, event.reason);
        }
        // After the join: a rekey (4-way again) or a new group key. Rare, and what a reason 15
        // (handshake timeout) from the access point needs explaining with.
        Activity::Handshake(message) if matches!(STATE.load(Ordering::Acquire), 2 | 3) => {
            println!("ferrite: wifi handshake {:?} answered", message);
        }
        Activity::Ignored(error) => println!("ferrite: wifi handshake frame ignored: {:?}", error),
        _ => {}
    }
}

struct Target {
    bssid: [u8; 6],
    rssi: i16,
    channel: u8,
    rsn: [u8; wpa::RSN_MAX],
    rsn_length: usize,
}

struct Joined {
    chip: Bcm4334,
    supplicant: Supplicant,
    seed: u64,
}

/// SDIO + firmware. Returns the chip with its radio prepared and its MAC.
fn bring_up(cfg: &WifiConfig, mct: Mct) -> Result<(Bcm4334, [u8; 6]), Error> {
    let (mut chip, _rate) = Bcm4334::start(Sdhci::new()?, mct, bcm4334::HIGH_SPEED_MAX_HZ)?;
    chip.attach()?;
    chip.load_firmware(cfg.firmware, cfg.nvram)?;
    chip.start_firmware()?;
    chip.enable_host_wake()?;
    chip.disable_glomming()?;
    let mut address = [0; 32];
    chip.get_variable("cur_etheraddr", &mut address)?;
    let mut own = [0; 6];
    own.copy_from_slice(&address[..6]);
    chip.prepare_radio()?;
    Ok((chip, own))
}

/// One scan + join. `Ok(None)`: the network is not there (or asks for what
/// this cannot do): try again later.
fn associate(cfg: &WifiConfig, chip: &mut Bcm4334, own: [u8; 6], mct: Mct) -> Result<Option<(Supplicant, u64)>, Error> {
    let band_allows = |channel: u8| match cfg.settings.band {
        2 => channel <= LAST_2G_CHANNEL,
        5 => channel > LAST_2G_CHANNEL,
        _ => true,
    };
    let mut target: Option<Target> = None;
    let mut pool = Hmac::new(&own).sign(&[&mct.count().to_le_bytes()[..]]);
    let mut found = ScanList::<SCAN_MAX>::new();
    // On the heap: ~8 KB.
    let mut seen: Vec<Option<Seen>> = alloc::vec![None; SEEN_MAX];
    chip.scan_elements(SCAN_TIMEOUT_MS, |bss, rsn, elements| {
        pool = Hmac::new(&pool).sign(&[&mct.count().to_le_bytes()[..], &bss.bssid, &bss.rssi.to_le_bytes()]);
        let name = &bss.ssid[..usize::from(bss.ssid_length).min(bss.ssid.len())];
        found.add(name, bss.rssi, bss.channel, wpa::rsn_supported(rsn) && rsn.len() <= wpa::RSN_MAX);
        let named = !cfg.ssid().is_empty() && cfg.ssid().as_bytes() == name;
        if named {
            let ie = &elements[..elements.len().min(SEEN_IE_MAX)];
            let same = |s: &Seen| s.bssid == bss.bssid && &s.ie[..usize::from(s.ie_length)] == ie;
            if let Some(s) = seen.iter_mut().flatten().find(|s| same(s)) {
                s.count = s.count.saturating_add(1);
                s.rssi = s.rssi.max(bss.rssi);
            } else if let Some(slot) = seen.iter_mut().find(|s| s.is_none()) {
                let mut copy = [0; SEEN_IE_MAX];
                copy[..ie.len()].copy_from_slice(ie);
                *slot = Some(Seen { bssid: bss.bssid, channel: bss.channel, rssi: bss.rssi, count: 1, ie_length: ie.len() as u16, ie: copy });
            }
        }
        if named
            && band_allows(bss.channel)
            && wpa::rsn_supported(rsn)
            && rsn.len() <= wpa::RSN_MAX
            && target.as_ref().is_none_or(|known| bss.rssi > known.rssi)
        {
            let mut copy = [0; wpa::RSN_MAX];
            copy[..rsn.len()].copy_from_slice(rsn);
            target = Some(Target { bssid: bss.bssid, rssi: bss.rssi, channel: bss.channel, rsn: copy, rsn_length: rsn.len() });
        }
    })?;
    found.finish();
    publish_scan(found);
    if !cfg.ssid().is_empty() {
        DIAG.lock(|d| {
            let mut d = d.borrow_mut();
            d.seen.copy_from_slice(&seen);
            d.scans = d.scans.wrapping_add(1);
        });
        for s in seen.iter().flatten() {
            let b = s.bssid;
            println!(
                "ferrite: wifi seen {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ch {} {} dBm x{} ({} element bytes: /api/wifi)",
                b[0], b[1], b[2], b[3], b[4], b[5], s.channel, s.rssi, s.count, s.ie_length
            );
        }
    }
    if cfg.ssid().is_empty() {
        return Ok(None);
    }
    let Some(target) = target else {
        println!("ferrite: wifi network {} not found", cfg.ssid());
        return Ok(None);
    };
    let key = crypto::wpa_key(cfg.settings.password.as_bytes(), cfg.ssid().as_bytes());
    let mut nonce = [0; 32];
    crypto::prf(&pool, b"Init Counter", &mct.count().to_le_bytes(), &mut nonce);
    let mut seed = [0; 8];
    crypto::prf(&pool, b"Stack seed", &mct.count().to_le_bytes(), &mut seed);
    let Some(mut supplicant) = Supplicant::new(key, own, target.bssid, &target.rsn[..target.rsn_length], nonce) else {
        println!("ferrite: wifi access point asks for what this cannot do");
        return Ok(None);
    };
    let b = target.bssid;
    println!(
        "ferrite: wifi joining {:02x}:{:02x}:{:02x}:{:02x}:{:02x}:{:02x} ch {} {} dBm",
        b[0], b[1], b[2], b[3], b[4], b[5], target.channel, target.rssi
    );
    DIAG.lock(|d| d.borrow_mut().target = (target.bssid, target.channel));
    let joined = chip.connect(cfg.ssid().as_bytes(), &mut supplicant, JOIN_TIMEOUT_MS, report);
    if let Err(Error::Wpa(wpa::Error::RsnMismatch)) = joined {
        let (expected, carried) = (supplicant.network_rsn(), supplicant.mismatched_rsn());
        println!("ferrite: wifi RSN expected {}", hex(expected));
        println!("ferrite: wifi RSN message 3 {}", hex(carried));
        DIAG.lock(|d| {
            let mut d = d.borrow_mut();
            d.mismatches = d.mismatches.wrapping_add(1);
            d.expected.0[..expected.len()].copy_from_slice(expected);
            d.expected.1 = expected.len();
            d.carried.0[..carried.len()].copy_from_slice(carried);
            d.carried.1 = carried.len();
            d.mismatch_bssid = target.bssid;
        });
    }
    joined?;
    update_link(|l| {
        l.bssid = target.bssid;
        l.channel = target.channel;
        l.rssi = target.rssi;
    });
    Ok(Some((supplicant, u64::from_le_bytes(seed))))
}

fn join(cfg: &WifiConfig, mct: Mct) -> Result<Joined, Error> {
    let begin = mct.count();
    let (chip, own) = bring_up(cfg, mct)?;
    println!("ferrite: wifi chip up in {} ms", (mct.count() - begin) / COUNTS_PER_MS);
    update_link(|l| l.mac = own);
    if cfg.ssid().is_empty() {
        // The setup screen with no network named: scan, now and then or when asked.
        set_state(State::Scanning);
        println!("ferrite: wifi scanning only (no network named)");
    }
    Ok(associate_until_joined(cfg, chip, own, mct, begin))
}

/// Scans and joins until it works (the chip is up, its radio prepared): at start, and again
/// after the connection was lost.
fn associate_until_joined(cfg: &WifiConfig, mut chip: Bcm4334, own: [u8; 6], mct: Mct, begin: u64) -> Joined {
    loop {
        match associate(cfg, &mut chip, own, mct) {
            Ok(Some((supplicant, seed))) => {
                let l = link();
                println!(
                    "ferrite: wifi joined {} in {} ms, channel {}, {} dBm",
                    cfg.ssid(),
                    (mct.count() - begin) / COUNTS_PER_MS,
                    l.channel,
                    l.rssi
                );
                return Joined { chip, supplicant, seed };
            }
            Ok(None) => {}
            // A refused / timed-out handshake: the chip is still usable.
            Err(e) => println!("ferrite: wifi join ERROR {:?}", e),
        }
        wait_or_scan_request(mct, RETRY_MS);
    }
}

/// CPU3: join, then the network tasks.
fn cpu3(_core: usize) -> ! {
    println!("ferrite: CPU{} up, joining", CORE);
    Gic::new().init_this_cpu();
    cpu::enable_irq();
    let cfg = config();
    let mct = Mct::new();
    let mut joined = match join(cfg, mct) {
        Ok(j) => j,
        Err(e) => {
            println!("ferrite: wifi ERROR {:?}, wifi off", e);
            set_state(State::Failed);
            cpu::halt();
        }
    };
    loop {
        serve(cfg, joined);
        // The tasks have ended: for standby, or because the connection was lost and the runner
        // gave the chip back (`wifi_task`): then join again with it, and serve anew.
        // SAFETY: CPU3 only, and its tasks (`wifi_task`) have ended.
        let lost = unsafe { (*addr_of_mut!(LOST_CHIP)).take() };
        let Some(chip) = lost.filter(|_| !STANDBY.load(Ordering::Acquire)) else {
            break;
        };
        update_link(|l| {
            l.address = [0; 4];
            l.prefix = 0;
        });
        set_state(State::Joining);
        println!("ferrite: wifi joining again");
        GROUP.arm();
        let own = link().mac;
        joined = associate_until_joined(cfg, chip, own, mct, mct.count());
    }
    bcm4334::power_off();
    update_link(|l| {
        l.address = [0; 4];
        l.prefix = 0;
    });
    set_state(State::Asleep);
    println!("ferrite: wifi off, CPU{} stopping", CORE);
    smp::stop_this_cpu()
}

/// CPU3: the network tasks over a joined chip, until they have all ended.
fn serve(cfg: &WifiConfig, joined: Joined) {
    set_state(State::Joined);
    update_link(|l| l.joined_ms = embassy::now() / 1000);

    let mut dhcp = DhcpConfig::default();
    dhcp.hostname = cfg.hostname().try_into().ok();
    // SAFETY: the only references to these, taken here once per start of this core; the last
    // start's tasks have ended (`GROUP`), and with them every use of the old ones.
    let (state, resources) = unsafe {
        *addr_of_mut!(WIFI_STATE) = wifi::State::new();
        *addr_of_mut!(RESOURCES) = StackResources::new();
        (&mut *addr_of_mut!(WIFI_STATE), &mut *addr_of_mut!(RESOURCES))
    };
    let (device, runner) = wifi::new(state, joined.chip, joined.supplicant, report);
    // `wifi::new` routes the host wake interrupt to CPU0: its task runs here.
    if let Some(eint) = bcm4334::HOST_WAKE.eint() {
        Gic::new().set_target(eint.irq(), CORE);
    }
    let (stack, net_runner) = embassy_net::new(device, Config::dhcpv4(dhcp), resources, joined.seed);

    embassy::run_until(
        |spawner| {
            GROUP.begin(4 + http::SERVERS as u32);
            spawner.spawn(wifi_task(runner).unwrap());
            spawner.spawn(net_task(net_runner).unwrap());
            spawner.spawn(dhcp_task(stack).unwrap());
            spawner.spawn(bridge_task(stack).unwrap());
            for _ in 0..http::SERVERS {
                spawner.spawn(http_task(stack).unwrap());
            }
        },
        || GROUP.done(),
        // Standby: every task has ended, the chip's runner with them; no wake-up from the
        // chip's host wake for it.
        || {
            if let Some(eint) = bcm4334::HOST_WAKE.eint() {
                Gic::new().disable(eint.irq());
                eint.mask();
                eint.clear();
            }
        },
    );
}

/// CPU0, entering standby: ends CPU3's tasks and waits for it to stop. True
/// if it did (then [`resume`] at the wake); false while it is still joining
/// (left running: its polled join cannot be ended) or did not stop in time.
pub fn suspend() -> bool {
    if !matches!(STATE.load(Ordering::Acquire), 2 | 3 | 5) {
        return false;
    }
    STANDBY.store(true, Ordering::Release);
    GROUP.stop();
    match smp::wait_offline(CORE, STOP_MS) {
        Ok(_) => true,
        Err(status) => {
            println!("ferrite: CPU{} did not stop (status {:#x}), wifi left as it is", CORE, status);
            false
        }
    }
}

/// CPU0, at the wake after a [`suspend`] that returned true: powers the chip
/// (~0.45 s: PMIC over I2C0, which CPU0 owns, then the chip's reset) and
/// starts CPU3, which joins again.
pub fn resume(pmic: Max77686, mct: Mct) {
    if let Err(e) = bcm4334::power_on(pmic, mct) {
        println!("ferrite: wifi power ERROR {:?}", e);
        set_state(State::Failed);
        return;
    }
    set_state(State::Joining);
    STANDBY.store(false, Ordering::Release);
    GROUP.arm();
    if let Err(e) = smp::start_cpu(CORE, cpu3) {
        println!("ferrite: CPU{} ERROR {:?}, wifi off", CORE, e);
        set_state(State::Failed);
    }
}

#[embassy_executor::task]
async fn wifi_task(runner: wifi::Runner<'static>) {
    GROUP
        .run(async {
            let (error, chip) = runner.run_keeping_chip().await;
            set_state(State::Down);
            println!("ferrite: wifi stopped: {:?}", error);
            // A lost connection (the access point left, a handshake it gave up on): end the
            // other tasks so that `cpu3` joins again with the chip. Other errors are of the
            // chip or its bus: it stays down, as before.
            if matches!(error, Error::Disconnected { .. }) && !STANDBY.load(Ordering::Acquire) {
                // SAFETY: CPU3 only; `cpu3` reads it after every task has ended.
                unsafe { *addr_of_mut!(LOST_CHIP) = Some(chip) };
                GROUP.stop();
            }
        })
        .await
}

#[embassy_executor::task]
async fn net_task(mut runner: embassy_net::Runner<'static, wifi::Device<'static>>) {
    GROUP.run(runner.run()).await
}

#[embassy_executor::task(pool_size = http::SERVERS)]
async fn http_task(stack: Stack<'static>) {
    GROUP.run(http::server(stack)).await
}

/// Logs (and records) the address each time DHCP gives one.
#[embassy_executor::task]
async fn dhcp_task(stack: Stack<'static>) {
    GROUP.run(dhcp_loop(stack)).await
}

async fn dhcp_loop(stack: Stack<'static>) {
    loop {
        stack.wait_config_up().await;
        if let Some(c) = stack.config_v4() {
            update_link(|l| {
                l.address = c.address.address().octets();
                l.prefix = c.address.prefix_len();
            });
            set_state(State::Up);
            println!(
                "ferrite: http://{}/ ({}), protocol bridge tcp://{}:{}",
                c.address.address(),
                config().hostname(),
                c.address.address(),
                BRIDGE_PORT
            );
        }
        stack.wait_config_down().await;
        set_state(State::Joined);
    }
}

// --- TCP protocol bridge ----------------------------------------------------------------------

#[embassy_executor::task]
async fn bridge_task(stack: Stack<'static>) {
    GROUP.run(bridge_loop(stack)).await
}

async fn bridge_loop(stack: Stack<'static>) {
    let mut rx = [0; BRIDGE_RX];
    let mut tx = [0; BRIDGE_TX];
    loop {
        let mut socket = TcpSocket::new(stack, &mut rx, &mut tx);
        socket.set_timeout(Some(BRIDGE_IDLE));
        if socket.accept(BRIDGE_PORT).await.is_err() {
            continue;
        }
        socket.set_nagle_enabled(false);
        match bridge::Session::take(bridge::TCP) {
            Some(session) => bridge_serve(&mut socket, &session).await,
            None => println!("ferrite: bridge busy (an HTTP request holds the protocol)"),
        }
        socket.close();
        let _ = with_timeout(Duration::from_secs(1), socket.flush()).await;
        socket.abort();
    }
}

/// Moves bytes both ways until the client closes or goes quiet.
async fn bridge_serve(socket: &mut TcpSocket<'_>, session: &bridge::Session) {
    // The runtime drops an earlier session's leftovers first.
    let begin = Instant::now();
    while !session.ready() && begin.elapsed() < Duration::from_secs(1) {
        Timer::after(Duration::from_millis(1)).await;
    }
    let mut buf = [0u8; 2048];
    let mut pending = 0usize; // received, not yet in bridge::RX
    let mut last = Instant::now();
    loop {
        // Runtime → client.
        let mut out = [0u8; 1024];
        let n = bridge::TX.pop_slice(&mut out);
        if n > 0 {
            if write_all(socket, &out[..n]).await.is_err() {
                return;
            }
        }
        // Client → runtime, as far as the ring takes it.
        if pending > 0 {
            let taken = bridge::RX.push_slice(&buf[..pending]);
            buf.copy_within(taken..pending, 0);
            pending -= taken;
        }
        if pending == 0 && socket.can_recv() {
            match socket.read(&mut buf).await {
                Ok(0) | Err(_) => return,
                Ok(n) => {
                    pending = n;
                    last = Instant::now();
                    continue;
                }
            }
        }
        if !socket.may_recv() && bridge::TX.is_empty() {
            return;
        }
        if last.elapsed() > BRIDGE_IDLE {
            return;
        }
        if n == 0 {
            Timer::after(Duration::from_millis(1)).await;
        }
    }
}

pub(crate) async fn write_all(socket: &mut TcpSocket<'_>, mut data: &[u8]) -> Result<(), embassy_net::tcp::Error> {
    while !data.is_empty() {
        let written = socket.write(data).await?;
        data = &data[written..];
    }
    Ok(())
}

// --- CPU0 mailbox: uploaded FS images, restarts -----------------------------------------------

/// What an upload is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub enum Upload {
    /// `POST /flash`: an FS image, saved as `FERRITE/FLASH.BIN`.
    Fs,
    /// `POST /save`: a firmware `.bin`, saved as `FERRITE/FERRITE.BIN` for uart_boot to boot.
    Firmware,
    /// `POST /api/system?name=`: a file for `SYSTEM/` (its checked 8.3 name and length),
    /// saved without a restart.
    System([u8; 12], u8),
}

impl Upload {
    pub fn name(self) -> &'static str {
        match self {
            Upload::Fs => "flash",
            Upload::Firmware => "firmware",
            Upload::System(..) => "system",
        }
    }
}

/// `POST /flash` / `/save`: an image (its kind and CRC-32) for CPU0 to check and save
/// ([`service`]).
static UPLOAD: CriticalSectionMutex<RefCell<Option<(Upload, Vec<u8>, u32)>>> =
    CriticalSectionMutex::new(RefCell::new(None));
/// [`UPLOAD`]'s outcome, `FLASH_*`.
static FLASH_STATUS: AtomicU8 = AtomicU8::new(FLASH_IDLE);
pub const FLASH_IDLE: u8 = 0;
pub const FLASH_PENDING: u8 = 1;
pub const FLASH_SAVED: u8 = 2;
pub const FLASH_BAD_IMAGE: u8 = 3;
pub const FLASH_TOO_LARGE: u8 = 4;
pub const FLASH_NO_STORAGE: u8 = 5;
pub const FLASH_SAVE_FAILED: u8 = 6;
/// Saved, and no restart follows (a `SYSTEM/` file).
pub const FLASH_KEPT: u8 = 7;
/// The HTTP answer has gone out: CPU0 may restart.
static ANSWERED: AtomicBool = AtomicBool::new(false);
/// `POST /restart` (set after its answer has gone out).
static RESTART: AtomicBool = AtomicBool::new(false);

/// CPU3: hands an uploaded image over. False if one is already pending.
pub(crate) fn submit_image(kind: Upload, image: Vec<u8>, crc: u32) -> bool {
    if FLASH_STATUS.compare_exchange(FLASH_IDLE, FLASH_PENDING, Ordering::AcqRel, Ordering::Relaxed).is_err() {
        return false;
    }
    ANSWERED.store(false, Ordering::Release);
    UPLOAD.lock(|u| *u.borrow_mut() = Some((kind, image, crc)));
    true
}

pub(crate) fn flash_status() -> u8 {
    FLASH_STATUS.load(Ordering::Acquire)
}

/// CPU3: the answer to a finished upload has been sent. A failed upload
/// frees the mailbox for the next one.
pub(crate) fn answered() {
    if FLASH_STATUS.load(Ordering::Acquire) != FLASH_SAVED {
        FLASH_STATUS.store(FLASH_IDLE, Ordering::Release);
    }
    ANSWERED.store(true, Ordering::Release);
}

pub(crate) fn request_restart() {
    RESTART.store(true, Ordering::Release);
}

/// A `FlashBackend` over an uploaded image, to check it before saving.
struct SliceFlash<'a>(&'a [u8]);

impl ferrite_core::flash::FlashBackend for SliceFlash<'_> {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let start = (addr as usize).min(self.0.len());
        let n = buf.len().min(self.0.len() - start);
        buf[..n].copy_from_slice(&self.0[start..start + n]);
        buf[n..].fill(0xFF);
    }
    fn write(&self, _addr: u32, _data: &[u8]) {}
    fn erase_sector(&self, _addr: u32) {}
    fn read_id(&self) -> (u8, u8, u8) {
        (0, 0, 0)
    }
}

/// CPU0: checks and saves an uploaded image as `FERRITE/FLASH.BIN`. An FS
/// image from `ferrite_build` (starts with `FERR`) keeps the region's config
/// sector in front, as a `writefs` does; anything else is a whole
/// `FLASH.BIN`. The running program keeps its region until the restart.
fn install(image: Vec<u8>) -> u8 {
    use ferrite_core::platform::Platform;
    let base = crate::platform::Exynos4412Platform::FS_BASE as usize;
    let full = if image.starts_with(b"FERR") {
        let mut full = Vec::new();
        if full.try_reserve_exact(base + image.len()).is_err() {
            return FLASH_TOO_LARGE;
        }
        full.extend_from_slice(crate::flash::prefix(base));
        full.extend_from_slice(&image);
        drop(image);
        full
    } else {
        image
    };
    if full.len() > crate::flash::SIZE {
        return FLASH_TOO_LARGE;
    }
    let flash = ferrite_core::flash::FlashImpl::with_backend(SliceFlash(&full));
    if !ferrite_core::fs::Fs::mount(&flash, base as u32).is_ok_and(|f| f.verify(&flash)) {
        return FLASH_BAD_IMAGE;
    }
    if !crate::storage::available() {
        return FLASH_NO_STORAGE;
    }
    // The old region's unsaved changes must not overwrite the new file at
    // the restart.
    crate::flash::forget_changes();
    if crate::storage::save(&full) { FLASH_SAVED } else { FLASH_SAVE_FAILED }
}

/// uart_boot's saved application (`uart_boot/src/cache.rs`, keep in sync): `FERRITE/FERRITE.BIN`
/// = `EXYNBOOT` + length u32 LE + CRC-32 u32 LE + the image.
const FIRMWARE_FILE: &str = "FERRITE.BIN";
const FIRMWARE_MAGIC: &[u8; 8] = b"EXYNBOOT";

/// CPU0: saves an uploaded firmware (checked by CPU3: CRC, zImage magic) as uart_boot's
/// application. The running firmware is in RAM, so its file may change under it.
fn install_firmware(image: Vec<u8>, crc: u32) -> u8 {
    if !crate::storage::available() {
        return FLASH_NO_STORAGE;
    }
    let mut file = Vec::new();
    if file.try_reserve_exact(16 + image.len()).is_err() {
        return FLASH_TOO_LARGE;
    }
    file.extend_from_slice(FIRMWARE_MAGIC);
    file.extend_from_slice(&(image.len() as u32).to_le_bytes());
    file.extend_from_slice(&crc.to_le_bytes());
    file.extend_from_slice(&image);
    drop(image);
    if crate::storage::save_named(FIRMWARE_FILE, &file) { FLASH_SAVED } else { FLASH_SAVE_FAILED }
}

/// Screenshot mailbox (HTTP `GET /api/screen`): CPU3 raises [`SCREEN_WANTED`], CPU0 copies the
/// shadow between passes into [`SCREEN`] (None = no memory) and raises [`SCREEN_READY`].
static SCREEN_WANTED: AtomicBool = AtomicBool::new(false);
static SCREEN_READY: AtomicBool = AtomicBool::new(false);
static SCREEN: CriticalSectionMutex<RefCell<Option<Vec<u32>>>> = CriticalSectionMutex::new(RefCell::new(None));

/// CPU3: the screen as CPU0 holds it between passes (XRGB8888, rows top to bottom), or None
/// (no memory, or CPU0 did not answer within 3 s).
pub async fn screenshot() -> Option<Vec<u32>> {
    SCREEN_READY.store(false, Ordering::Release);
    SCREEN.lock(|s| *s.borrow_mut() = None);
    SCREEN_WANTED.store(true, Ordering::Release);
    let start = Instant::now();
    while !SCREEN_READY.load(Ordering::Acquire) {
        if start.elapsed() > Duration::from_secs(3) {
            SCREEN_WANTED.store(false, Ordering::Release);
            return None;
        }
        Timer::after(Duration::from_millis(2)).await;
    }
    SCREEN.lock(|s| s.borrow_mut().take())
}

/// Storage mailbox (HTTP `GET /api/storage`, `POST /api/storage/migrate`): CPU3 puts the
/// request in [`STORAGE_WANTED`], CPU0 answers in [`STORAGE_TEXT`] between passes
/// ([`storage_service`]): whether it worked, and the text.
static STORAGE_WANTED: AtomicU8 = AtomicU8::new(STORAGE_NONE);
const STORAGE_NONE: u8 = 0;
pub const STORAGE_STATUS: u8 = 1;
pub const STORAGE_MIGRATE: u8 = 2;
pub const STORAGE_STRESS_ON: u8 = 3;
pub const STORAGE_STRESS_OFF: u8 = 4;
pub const STORAGE_CHECK: u8 = 5;
static STORAGE_TEXT: CriticalSectionMutex<RefCell<Option<(bool, String)>>> = CriticalSectionMutex::new(RefCell::new(None));

/// CPU3: asks CPU0 for `what` (`STORAGE_STATUS` or `STORAGE_MIGRATE`); None when it does not
/// answer within `secs`.
pub async fn storage_request(what: u8, secs: u64) -> Option<(bool, String)> {
    STORAGE_TEXT.lock(|t| *t.borrow_mut() = None);
    STORAGE_WANTED.store(what, Ordering::Release);
    let start = Instant::now();
    loop {
        if let Some(answer) = STORAGE_TEXT.lock(|t| t.borrow_mut().take()) {
            return Some(answer);
        }
        if start.elapsed() > Duration::from_secs(secs) {
            // Too late to take it back if CPU0 has started; it still finishes.
            let _ = STORAGE_WANTED.compare_exchange(what, STORAGE_NONE, Ordering::AcqRel, Ordering::Acquire);
            return None;
        }
        Timer::after(Duration::from_millis(5)).await;
    }
}

/// CPU0, once per pass: answers a storage request. A migration blocks the UI while it runs.
fn storage_service() {
    let what = STORAGE_WANTED.swap(STORAGE_NONE, Ordering::AcqRel);
    let answer = match what {
        STORAGE_STATUS => (true, crate::storage::status_text()),
        STORAGE_MIGRATE => match crate::storage::migrate() {
            Ok(report) => (true, report),
            Err(report) => (false, report),
        },
        STORAGE_STRESS_ON | STORAGE_STRESS_OFF => match crate::storage::stress::set(what == STORAGE_STRESS_ON) {
            Ok(t) => (true, t),
            Err(t) => (false, t),
        },
        STORAGE_CHECK => match crate::storage::stress::check() {
            Ok(t) => (true, t),
            Err(t) => (false, t),
        },
        _ => return,
    };
    STORAGE_TEXT.lock(|t| *t.borrow_mut() = Some(answer));
}

/// Display-list dump mailbox (HTTP `GET /api/dl[?id=N]`): CPU3 puts the
/// widget id (`DL_ALL` = the summary) in [`DL_WANTED`], CPU0 writes
/// `render::dl_dump` into [`DL_TEXT`] between passes ([`dl_service`]).
static DL_WANTED: AtomicI32 = AtomicI32::new(DL_NONE);
const DL_NONE: i32 = i32::MIN;
pub const DL_ALL: i32 = -1;
static DL_READY: AtomicBool = AtomicBool::new(false);
static DL_TEXT: CriticalSectionMutex<RefCell<Option<String>>> = CriticalSectionMutex::new(RefCell::new(None));

/// CPU3: the display-list dump for widget `id` (`DL_ALL` = all widgets, one
/// line each), or None when CPU0 doesn't answer within 3 s.
pub async fn dl_dump(id: i32) -> Option<String> {
    DL_READY.store(false, Ordering::Release);
    DL_TEXT.lock(|t| *t.borrow_mut() = None);
    DL_WANTED.store(id, Ordering::Release);
    let start = Instant::now();
    while !DL_READY.load(Ordering::Acquire) {
        if start.elapsed() > Duration::from_secs(3) {
            DL_WANTED.store(DL_NONE, Ordering::Release);
            return None;
        }
        Timer::after(Duration::from_millis(2)).await;
    }
    DL_TEXT.lock(|t| t.borrow_mut().take())
}

/// CPU0, once per pass (`frame_end`): answers a display-list dump request.
pub fn dl_service(ctx: &mut ferrite_core::ctx::Ctx<crate::platform::Exynos4412Platform>) {
    let id = DL_WANTED.swap(DL_NONE, Ordering::AcqRel);
    if id == DL_NONE {
        return;
    }
    let mut s = String::new();
    let which = (id != DL_ALL).then(|| ferrite_core::widget::WidgetId::from_vm(id));
    let _ = ferrite_core::render::dl_dump(ctx, which, &mut s);
    DL_TEXT.lock(|t| *t.borrow_mut() = Some(s));
    DL_READY.store(true, Ordering::Release);
}

/// CPU0, once per main-loop pass (`frame_end`): cheap unless the network
/// asked for something. `screen_ok`: nothing draws on the shadow now (no
/// render job on CPU2), so a screen grab may read it.
pub fn service(screen_ok: bool) {
    storage_service();
    if screen_ok && SCREEN_WANTED.swap(false, Ordering::AcqRel) {
        let shot = crate::lcd::grab();
        SCREEN.lock(|s| *s.borrow_mut() = shot);
        SCREEN_READY.store(true, Ordering::Release);
    }
    if FLASH_STATUS.load(Ordering::Acquire) == FLASH_PENDING
        && let Some((kind, image, crc)) = UPLOAD.lock(|u| u.borrow_mut().take())
    {
        let len = image.len();
        let status = match kind {
            Upload::Fs => install(image),
            Upload::Firmware => install_firmware(image, crc),
            Upload::System(name, len) => {
                let name = core::str::from_utf8(&name[..usize::from(len)]).unwrap_or("");
                if !crate::storage::available() {
                    FLASH_NO_STORAGE
                } else if crate::storage::save_in(crate::storage::SYSTEM_DIR, name, &image) {
                    println!("ferrite: saved {}/{}", crate::storage::SYSTEM_DIR, name);
                    FLASH_KEPT
                } else {
                    FLASH_SAVE_FAILED
                }
            }
        };
        println!("ferrite: HTTP {} upload, {} bytes: status {}", kind.name(), len, status);
        FLASH_STATUS.store(status, Ordering::Release);
        if status == FLASH_SAVED {
            // Let CPU3 answer before the board goes away.
            let start = crate::systick::millis();
            while !ANSWERED.load(Ordering::Acquire) && crate::systick::millis().wrapping_sub(start) < 3000 {
                core::hint::spin_loop();
            }
            <crate::platform::Exynos4412Platform as ferrite_core::runtime::PlatformRuntime>::reset();
        }
    }
    if RESTART.load(Ordering::Acquire) {
        <crate::platform::Exynos4412Platform as ferrite_core::runtime::PlatformRuntime>::reset();
    }
}
