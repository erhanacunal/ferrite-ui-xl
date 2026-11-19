//! Persistent storage on the eMMC. Everything here runs on CPU0.
//!
//! Two places, one at a time, chosen at boot ([`init`]):
//! - USERDATA (p10, ~13 GiB), ext4 with a journal (`ext4.rs`), once [`migrate`] has formatted it;
//! - CACHE (p8, FAT32, `fat.rs`), where the files were before, and stay until then.
//!
//! USERDATA wins whenever it holds an ext4; one that fails to mount turns persistence off rather
//! than falling back to CACHE's older files. uart_boot makes the same choice
//! (`uart_boot/src/cache.rs`).
//!
//! The firmware's own files live in [`DIR`] (`FERRITE/`):
//! - `FLASH.BIN`, the ferrite "flash" (`flash.rs`, a RAM region): [`load`] reads it at boot;
//!   [`save`] writes the used part back (`flash::save_if_dirty`: before a restart, and once
//!   writes have been quiet for [`IDLE_SAVE_MS`]);
//! - `FERRITE.BIN` (the firmware uart_boot starts, `net::install_firmware`), `EEPROM.BIN`
//!   (`eeprom.rs`); the board's settings are in [`SYSTEM_DIR`].
//!
//! Programs get their own tree, [`USER_DIR`] (`FERRITE/USER/`), through [`BoardStorage`]
//! (ferrite-core `storage`: `diskOpen`, `fileWrite`, `diskMkdir`…). The core checks every path
//! (8.3 components, no `..`), so a program cannot reach [`DIR`]'s files or anything else.
//!
//! Without either, persistence is off (logged at boot), the board runs from RAM only and
//! programs get `NO_STORAGE`.

mod ext4;
mod fat;
pub mod stress;

use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::fmt::Write;

use exynos_4412_rs::println;
use ferrite_core::storage::{self as core_storage, Kind, Path, StorageBackend};

pub const DIR: &str = "FERRITE";
pub const FILE: &str = "FLASH.BIN";
/// Programs' files: `DIR/USER_DIR`.
pub const USER_DIR: &str = "USER";
/// The board's own settings, shared with uart_boot (`uart_boot/src/settings.rs`, keep in
/// sync): `WIFI.TXT`, the WiFi chip's firmware and NVRAM (`net`). Out of programs' reach.
pub const SYSTEM_DIR: &str = "SYSTEM";
/// Save this long after the last write (a writefs in progress keeps writing
/// every chunk, so it is not saved half done unless the host stalls).
pub const IDLE_SAVE_MS: u32 = 1500;
/// Programs' written files reach the card this long after their last write
/// (or when closed, or before a restart).
const USER_FLUSH_MS: u32 = 1000;

/// Where the files are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Medium {
    None,
    Cache,
    UserData,
}

static mut MEDIUM: Medium = Medium::None;

fn medium() -> Medium {
    unsafe { MEDIUM }
}

/// Whether a medium is mounted and saves are possible.
pub fn available() -> bool {
    medium() != Medium::None
}

/// Mounts USERDATA, or CACHE when USERDATA holds no ext4. Call once, after `heap::init`.
pub fn init() {
    let medium = match ext4::init() {
        Some(Ok(())) => Medium::UserData,
        Some(Err(why)) => {
            println!("ferrite: storage off ({}), resources live in RAM only", why);
            Medium::None
        }
        None => match fat::init() {
            Ok(()) => Medium::Cache,
            Err(why) => {
                println!("ferrite: storage off ({}), resources live in RAM only", why);
                Medium::None
            }
        },
    };
    unsafe { MEDIUM = medium };
}

fn read_into(dir: &str, name: &str, region: &mut [u8]) -> Result<Option<usize>, ()> {
    match medium() {
        Medium::UserData => ext4::read_into(dir, name, region),
        Medium::Cache => fat::read_into(dir, name, region),
        Medium::None => Err(()),
    }
}

fn len_of(dir: &str, name: &str) -> Option<usize> {
    match medium() {
        Medium::UserData => ext4::len_of(dir, name),
        Medium::Cache => fat::len_of(dir, name),
        Medium::None => None,
    }
}

/// Reads the image file into `region`; returns its length (0 = no file yet).
pub fn load(region: &mut [u8]) -> usize {
    match read_into(DIR, FILE, region) {
        Ok(Some(n)) => {
            println!("ferrite: loaded {}/{}, {} bytes", DIR, FILE, n);
            n
        }
        Ok(None) => {
            println!("ferrite: no {}/{} yet", DIR, FILE);
            0
        }
        Err(()) => 0,
    }
}

/// Reads `dir`/`name` (8.3 names) whole, if it exists and is at most `max` bytes.
pub fn read_in(dir: &str, name: &str, max: usize) -> Option<Vec<u8>> {
    match medium() {
        Medium::UserData => ext4::read_in(dir, name, max),
        Medium::Cache => fat::read_in(dir, name, max),
        Medium::None => None,
    }
}

/// Programs' files under `root` (a running app's `data/`, on USERDATA's ext4), or back
/// under [`USER_DIR`] with `None`. Every program file must be closed.
pub fn set_user_root(root: Option<String>) {
    if medium() == Medium::UserData {
        ext4::set_user_root(root);
    }
}

/// Writes `data` as the image file, on the card when this returns.
pub fn save(data: &[u8]) -> bool {
    save_named(FILE, data)
}

/// Writes [`DIR`]/`name` (an 8.3 name).
pub fn save_named(name: &str, data: &[u8]) -> bool {
    save_in(DIR, name, data)
}

/// Writes `dir`/`name` (8.3 names; `dir` made if needed), on the card when this returns. On
/// USERDATA the old file stays whole until the new one is.
pub fn save_in(dir: &str, name: &str, data: &[u8]) -> bool {
    match medium() {
        Medium::UserData => ext4::save_in(dir, name, data),
        Medium::Cache => fat::save_in(dir, name, data),
        Medium::None => false,
    }
}

/// Everything written reaches the card: programs' open files and the
/// EEPROM file. Before a restart or power-off (`force`), and from
/// `frame_end` (`force` false: once writes have settled).
pub fn sync(force: bool) {
    if !force && medium() == Medium::UserData {
        stress::step();
    }
    BoardStorage.flush(force);
    crate::eeprom::save_if_dirty(force);
    crate::settings::save_if_due(force);
}

/// For `GET /api/storage`: where the files are, and how full it is.
pub fn status_text() -> String {
    let mut s = String::new();
    match medium() {
        Medium::UserData => {
            let _ = write!(s, "medium=USERDATA fs=ext4");
            if let Some((unclean, replayed)) = ext4::mount_state() {
                let _ = write!(s, " mount_unclean={} replayed={}", unclean as u8, replayed as u8);
            }
            if let Some(st) = ext4::stats() {
                let _ = write!(
                    s,
                    " size_mib={} free_mib={} inodes={} free_inodes={}",
                    st.blocks / 256,
                    st.free_blocks / 256,
                    st.inodes,
                    st.free_inodes
                );
            }
        }
        Medium::Cache => {
            let _ = write!(s, "medium=CACHE fs=fat32");
        }
        Medium::None => {
            let _ = write!(s, "medium=none");
        }
    }
    s.push('\n');
    s
}

// ---------------------------------------------------------------------------
// The move from CACHE to USERDATA.
// ---------------------------------------------------------------------------

/// The trees `migrate` copies.
const TREES: [&str; 2] = [DIR, SYSTEM_DIR];
/// Directory levels it follows (`FERRITE/USER/` plus the 8 a program path can have).
const MAX_DEPTH: usize = 12;

/// Formats USERDATA (whatever it held is lost), copies `FERRITE/` and `SYSTEM/` from CACHE
/// there and switches to it; CACHE keeps its files. Only while the files are on CACHE (or
/// persistence is off because USERDATA's ext4 does not mount) and no program has a file open.
/// Returns a report, one line per step.
pub fn migrate() -> Result<String, String> {
    let mut report = String::new();
    if medium() == Medium::UserData {
        return Err("already on USERDATA\n".into());
    }
    if fat::files_open() {
        return Err("a program has a file open: close it first\n".into());
    }
    crate::flash::save_if_dirty(true);
    sync(true);
    if !fat::available() {
        // USERDATA's ext4 failed to mount: CACHE was never opened.
        fat::init().map_err(|why| format!("CACHE: {}\n", why))?;
    }
    // Read everything first, so a CACHE read error leaves USERDATA untouched.
    let mut dirs = Vec::new();
    let mut files: Vec<(String, String, Vec<u8>)> = Vec::new();
    for tree in TREES {
        collect(tree, 0, &mut dirs, &mut files)?;
    }
    let bytes: usize = files.iter().map(|f| f.2.len()).sum();
    let _ = writeln!(report, "CACHE: {} directories, {} files, {} bytes", dirs.len(), files.len(), bytes);
    ext4::format().map_err(|why| format!("{}{}\n", report, why))?;
    let _ = writeln!(report, "USERDATA formatted");
    for dir in &dirs {
        if !ext4::make_dirs(dir) {
            return Err(format!("{}mkdir {} failed (USERDATA is mounted but incomplete)\n", report, dir));
        }
    }
    for (dir, name, data) in &files {
        if !ext4::save_in(dir, name, data) {
            return Err(format!("{}copy {}/{} failed (USERDATA is mounted but incomplete)\n", report, dir, name));
        }
        let back = ext4::read_in(dir, name, data.len());
        if back.as_deref() != Some(&data[..]) {
            return Err(format!("{}{}/{} reads back different\n", report, dir, name));
        }
    }
    if !ext4::commit() {
        return Err(format!("{}commit failed\n", report));
    }
    unsafe { MEDIUM = Medium::UserData };
    let _ = writeln!(report, "copied and verified; files are on USERDATA now (CACHE keeps its copy)");
    println!("ferrite: storage moved to USERDATA: {} files, {} bytes", files.len(), bytes);
    Ok(report)
}

/// Adds `dir` (on CACHE, if there) and everything under it.
fn collect(dir: &str, depth: usize, dirs: &mut Vec<String>, files: &mut Vec<(String, String, Vec<u8>)>) -> Result<(), String> {
    let Ok(entries) = fat::list(dir) else {
        // A missing top-level tree (no SYSTEM/ yet) is fine; deeper ones were just listed.
        return if depth == 0 { Ok(()) } else { Err(format!("CACHE: cannot list {}\n", dir)) };
    };
    dirs.push(dir.into());
    for (name, is_dir) in entries {
        if is_dir {
            if depth + 1 >= MAX_DEPTH {
                return Err(format!("CACHE: {}/{} too deep\n", dir, name));
            }
            collect(&format!("{}/{}", dir, name), depth + 1, dirs, files)?;
        } else {
            let len = fat::len_of(dir, &name).ok_or_else(|| format!("CACHE: {}/{} length\n", dir, name))?;
            let data = fat::read_in(dir, &name, len).ok_or_else(|| format!("CACHE: read {}/{}\n", dir, name))?;
            if data.len() != len {
                return Err(format!("CACHE: {}/{} short read\n", dir, name));
            }
            files.push((dir.into(), name, data));
        }
    }
    Ok(())
}

/// The board's `Platform::StorageB`: programs' files on whichever medium [`init`] chose.
pub struct BoardStorage;

impl StorageBackend for BoardStorage {
    fn open(&self, slot: usize, path: &Path, mode: core_storage::Mode) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.open(slot, path, mode),
            Medium::Cache => fat::FatStorage.open(slot, path, mode),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn close(&self, slot: usize) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.close(slot),
            Medium::Cache => fat::FatStorage.close(slot),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.read(slot, buf),
            Medium::Cache => fat::FatStorage.read(slot, buf),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.write(slot, data),
            Medium::Cache => fat::FatStorage.write(slot, data),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.seek(slot, pos),
            Medium::Cache => fat::FatStorage.seek(slot, pos),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn size(&self, slot: usize) -> Result<u32, i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.size(slot),
            Medium::Cache => fat::FatStorage.size(slot),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.kind(path),
            Medium::Cache => fat::FatStorage.kind(path),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn delete(&self, path: &Path) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.delete(path),
            Medium::Cache => fat::FatStorage.delete(path),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.make_dir(path),
            Medium::Cache => fat::FatStorage.make_dir(path),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.remove_dir(path),
            Medium::Cache => fat::FatStorage.remove_dir(path),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.entry(path, index, name),
            Medium::Cache => fat::FatStorage.entry(path, index, name),
            Medium::None => Err(core_storage::err::NO_STORAGE),
        }
    }
    fn flush(&self, force: bool) {
        match medium() {
            Medium::UserData => ext4::Ext4Storage.flush(force),
            Medium::Cache => fat::FatStorage.flush(force),
            Medium::None => {}
        }
    }
}

/// Where app packages live (ferrite-core `apps`, docs/apps.md): `ferrite/apps/` on the ext4
/// USERDATA only (CACHE's FAT32 has 8.3 names).
pub struct BoardApps;

impl ferrite_core::apps::AppFiles for BoardApps {
    fn available(&self) -> bool {
        medium() == Medium::UserData
    }
    fn read(&self, dir: &str, name: &str, max: usize) -> Option<Vec<u8>> {
        if !self.available() {
            return None;
        }
        ext4::read_in(dir, name, max)
    }
    fn save(&self, dir: &str, name: &str, data: &[u8]) -> bool {
        self.available() && ext4::make_dirs(dir) && ext4::save_in(dir, name, data)
    }
    fn make_dirs(&self, dir: &str) -> bool {
        self.available() && ext4::make_dirs(dir) && ext4::commit()
    }
    fn list(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        if !self.available() {
            return None;
        }
        ext4::list_dir(dir)
    }
    fn delete(&self, dir: &str, name: &str) -> bool {
        self.available() && ext4::remove(dir, name).is_ok()
    }
    fn remove_dir(&self, dir: &str) -> bool {
        let Some((parent, name)) = dir.rsplit_once('/') else { return false };
        self.available() && ext4::remove(parent, name).is_ok()
    }
    fn commit(&self) -> bool {
        ext4::commit()
    }
}
