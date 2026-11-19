//! Application packages (docs/apps.md): an FS image with a [`RES_APPINFO`]
//! manifest, installed under `ferrite/apps/{id}/` on the board's storage
//! instead of replacing the system image (`FLASH.BIN`).
//!
//! ```text
//! ferrite/apps/{id}/app.img      the package as uploaded (checked first)
//! ferrite/apps/{id}/appinfo.bin  its manifest, for listing without reading app.img
//! ferrite/apps/{id}/data/        the app's own files (storage root, EEPROM)
//! ```
//!
//! Every way in (writefs with `FS_FLAG_APP`, `POST /api/apps`) ends in
//! [`install`]. The files go through [`AppFiles`], which the BSP implements
//! over its storage (exynos4412: the ext4 USERDATA only; sim: a host
//! directory or RAM), so this module is tested with [`MemAppFiles`].
//!
//! The manifest layout is shared with ferrite-ui `ferrite_build.py`
//! (`build_appinfo` / `parse_appinfo`): keep in sync.
extern crate alloc;

use alloc::collections::BTreeMap;
use alloc::format;
use alloc::string::{String, ToString};
use alloc::vec::Vec;
use core::cell::RefCell;

use crate::flash::{FlashBackend, FlashImpl};
use crate::fs::{Fs, RES_APPINFO, RES_PROGRAM};

/// Where installed apps live, from the storage medium's root.
pub const APPS_DIR: &str = "ferrite/apps";
pub const IMAGE_FILE: &str = "app.img";
pub const INFO_FILE: &str = "appinfo.bin";
pub const DATA_DIR: &str = "data";
/// The manifest's resource name.
pub const APPINFO_NAME: &[u8] = b"appinfo";
pub const APPINFO_SIZE: usize = 108;
const APPINFO_MAGIC: &[u8; 2] = b"AI";
const APPINFO_VERSION: u8 = 1;
pub const ID_MAX: usize = 32;

/// Why an app operation failed: also the protocol error code a writefs with
/// `FS_FLAG_APP` answers (ferrite-ui `ferrite_cli.py` `ERROR_DESCRIPTIONS`,
/// keep in sync) and the `"code"` of the HTTP API's JSON error.
pub mod err {
    /// Not an FS image, or its CRC is wrong.
    pub const BAD_IMAGE: u8 = 11;
    /// An FS image without (a valid) `appinfo`: a system image.
    pub const NO_APPINFO: u8 = 12;
    pub const BAD_ID: u8 = 13;
    /// Built for features (caps) this firmware does not have.
    pub const CAPS: u8 = 14;
    /// No storage for apps (exynos4412: USERDATA is not ext4).
    pub const NO_STORAGE: u8 = 15;
    pub const WRITE: u8 = 16;
    pub const NOT_INSTALLED: u8 = 17;
    /// The entry program named in `appinfo` is not in the image.
    pub const NO_ENTRY: u8 = 18;
    pub const TOO_LARGE: u8 = 19;
    /// Four apps already run (`domain::MAX_APPS`).
    pub const LIMIT: u8 = 20;
    /// A library the program calls is not installed in that major version
    /// (also: a library package's install target, `libs.rs`).
    pub const LIB_MISSING: u8 = 21;
    /// An installed library lacks a function the program calls, or its
    /// arguments / return kind differ.
    pub const LIB_MISMATCH: u8 = 22;

    /// For logs and the HTTP API.
    pub fn text(code: u8) -> &'static str {
        match code {
            BAD_IMAGE => "not a valid FS image (format or CRC)",
            NO_APPINFO => "no appinfo: a system image, not an app",
            BAD_ID => "bad application_id",
            CAPS => "the app needs features this firmware lacks",
            NO_STORAGE => "no app storage (needs the ext4 USERDATA)",
            WRITE => "writing to storage failed",
            NOT_INSTALLED => "no such app",
            NO_ENTRY => "the entry program is not in the image",
            TOO_LARGE => "package too large",
            LIMIT => "four apps already run",
            LIB_MISSING => "a library it calls is not installed",
            LIB_MISMATCH => "an installed library does not match",
            _ => "app error",
        }
    }
}

/// An app's manifest (`RES_APPINFO`).
#[derive(Clone, Debug, PartialEq)]
pub struct AppInfo {
    pub id: String,
    pub name: String,
    /// major << 16 | minor << 8 | patch.
    pub version: u32,
    pub entry: String,
    /// An image resource name, empty for none.
    pub icon: String,
    /// `Platform::CAPS` bits it was built for.
    pub caps: u32,
}

impl AppInfo {
    pub fn version_text(&self) -> String {
        let v = self.version;
        format!("{}.{}.{}", v >> 16 & 255, v >> 8 & 255, v & 255)
    }

    /// `ferrite/apps/{id}`.
    pub fn dir(&self) -> String {
        app_dir(&self.id)
    }
}

pub fn app_dir(id: &str) -> String {
    format!("{}/{}", APPS_DIR, id)
}

/// `[a-z0-9][a-z0-9._-]{0,31}`: one path component, lower case as on ext4.
pub fn valid_id(id: &str) -> bool {
    let b = id.as_bytes();
    !b.is_empty()
        && b.len() <= ID_MAX
        && (b[0].is_ascii_lowercase() || b[0].is_ascii_digit())
        && b.iter().all(|&c| c.is_ascii_lowercase() || c.is_ascii_digit() || matches!(c, b'.' | b'_' | b'-'))
}

fn text_field(b: &[u8]) -> Option<String> {
    let end = b.iter().position(|&c| c == 0).unwrap_or(b.len());
    core::str::from_utf8(&b[..end]).ok().map(|s| s.to_string())
}

/// Parses a manifest record (108 bytes, ferrite_build `build_appinfo`).
pub fn parse_appinfo(rec: &[u8]) -> Result<AppInfo, u8> {
    parse_manifest(rec, APPINFO_MAGIC)
}

/// [`parse_appinfo`] with another magic (a library's `libinfo`: "LI").
pub(crate) fn parse_manifest(rec: &[u8], magic: &[u8; 2]) -> Result<AppInfo, u8> {
    if rec.len() < APPINFO_SIZE || &rec[..2] != magic || rec[2] != APPINFO_VERSION {
        return Err(err::NO_APPINFO);
    }
    let n = rec[3] as usize;
    if n == 0 || n > ID_MAX {
        return Err(err::BAD_ID);
    }
    let id = core::str::from_utf8(&rec[4..4 + n]).map_err(|_| err::BAD_ID)?;
    if !valid_id(id) {
        return Err(err::BAD_ID);
    }
    let word = |at: usize| u32::from_le_bytes([rec[at], rec[at + 1], rec[at + 2], rec[at + 3]]);
    Ok(AppInfo {
        id: id.to_string(),
        name: text_field(&rec[36..68]).ok_or(err::NO_APPINFO)?,
        version: word(68),
        entry: text_field(&rec[72..88]).ok_or(err::NO_APPINFO)?,
        icon: text_field(&rec[88..104]).ok_or(err::NO_APPINFO)?,
        caps: word(104),
    })
}

/// An image in RAM read as flash at `base` (where the board mounts it), so
/// [`Fs`] can check a package before it is anywhere else. Past the end reads
/// erased (0xFF), so a short image fails its CRC.
pub struct SliceFlash<'a> {
    pub data: &'a [u8],
    pub base: u32,
}

impl FlashBackend for SliceFlash<'_> {
    fn read(&self, addr: u32, buf: &mut [u8]) {
        let start = addr.wrapping_sub(self.base) as usize;
        for (i, b) in buf.iter_mut().enumerate() {
            *b = start.checked_add(i).and_then(|at| self.data.get(at)).copied().unwrap_or(0xFF);
        }
    }
    fn write(&self, _: u32, _: &[u8]) {}
    fn erase_sector(&self, _: u32) {}
    fn read_id(&self) -> (u8, u8, u8) {
        (0, 0, 0)
    }
}

/// Checks a package as the board would run it at `base` (`Platform::FS_BASE`)
/// with `caps` (`Platform::CAPS`): a whole FS image with a good CRC, a valid
/// manifest, nothing needed that the firmware lacks, its entry program there.
pub fn check(image: &[u8], base: u32, caps: u32) -> Result<AppInfo, u8> {
    let flash = FlashImpl::with_backend(SliceFlash { data: image, base });
    let fs = Fs::mount(&flash, base).map_err(|_| err::BAD_IMAGE)?;
    if !fs.verify(&flash) {
        return Err(err::BAD_IMAGE);
    }
    let entry = fs.find_kind(&flash, APPINFO_NAME, RES_APPINFO).ok_or(err::NO_APPINFO)?;
    let mut rec = [0u8; APPINFO_SIZE];
    if (entry.size as usize) < APPINFO_SIZE {
        return Err(err::NO_APPINFO);
    }
    fs.read_resource(&flash, &entry, 0, &mut rec);
    let info = parse_appinfo(&rec)?;
    if info.caps & !caps != 0 {
        return Err(err::CAPS);
    }
    if fs.find_kind(&flash, info.entry.as_bytes(), RES_PROGRAM).is_none() {
        return Err(err::NO_ENTRY);
    }
    Ok(info)
}

/// The board's storage as apps need it. `dir` is `/`-separated from the
/// medium's root (`ferrite/apps/x`), names are single components; both as on
/// disk (lower case).
pub trait AppFiles {
    /// Whether apps can be stored at all.
    fn available(&self) -> bool;
    /// `dir/name` whole, if it exists and is at most `max` bytes.
    fn read(&self, dir: &str, name: &str, max: usize) -> Option<Vec<u8>>;
    /// Writes `dir/name` (making `dir`): whole or not at all (a new file
    /// renamed over the old one), on the medium when this returns.
    fn save(&self, dir: &str, name: &str, data: &[u8]) -> bool;
    /// Makes `dir` and its parents unless there.
    fn make_dirs(&self, dir: &str) -> bool;
    /// The entries of `dir`: (name, is a directory); None if it is not there.
    fn list(&self, dir: &str) -> Option<Vec<(String, bool)>>;
    fn delete(&self, dir: &str, name: &str) -> bool;
    /// Removes the empty directory `dir`.
    fn remove_dir(&self, dir: &str) -> bool;
    /// Makes the deletions so far durable.
    fn commit(&self) -> bool {
        true
    }
}

/// What [`install`] did.
#[derive(Debug, PartialEq)]
pub struct Installed {
    pub info: AppInfo,
    /// An app with this id was installed before (its `data/` is kept).
    pub replaced: bool,
}

/// Checks `image` ([`check`]) and stores it as `apps/{id}/app.img`, keeping
/// the app's `data/` if it was installed before.
pub fn install(files: &dyn AppFiles, image: &[u8], base: u32, caps: u32) -> Result<Installed, u8> {
    let info = check(image, base, caps)?;
    if !files.available() {
        return Err(err::NO_STORAGE);
    }
    let dir = info.dir();
    let replaced = files.list(&dir).is_some();
    let data = format!("{}/{}", dir, DATA_DIR);
    let mut rec = [0u8; APPINFO_SIZE];
    {
        let flash = FlashImpl::with_backend(SliceFlash { data: image, base });
        let fs = Fs::mount(&flash, base).map_err(|_| err::BAD_IMAGE)?;
        let e = fs.find_kind(&flash, APPINFO_NAME, RES_APPINFO).ok_or(err::NO_APPINFO)?;
        fs.read_resource(&flash, &e, 0, &mut rec);
    }
    // The image first: appinfo.bin only lists it, app.img's own manifest is
    // what runs.
    changed();
    if !(files.make_dirs(&data) && files.save(&dir, IMAGE_FILE, image) && files.save(&dir, INFO_FILE, &rec)) {
        return Err(err::WRITE);
    }
    Ok(Installed { info, replaced })
}

/// What [`install_package`] installed.
pub enum Package {
    App(Installed),
    /// A library package (`libs.rs`): its manifest, whether it replaced one.
    Library(AppInfo, bool),
}

/// Installs an app or a library package, whichever `image` is (a library
/// has a `libinfo`): the one way in for writefs `FS_FLAG_APP`, `POST
/// /api/apps` / `/api/libs` and the sim's `--install`.
pub fn install_package(files: &dyn AppFiles, image: &[u8], base: u32, caps: u32) -> Result<Package, u8> {
    if crate::libs::is_library(image, base) {
        crate::libs::install(files, image, base, caps).map(|(info, replaced)| Package::Library(info, replaced))
    } else {
        install(files, image, base, caps).map(Package::App)
    }
}

/// The installed apps, sorted by id (from each one's `appinfo.bin`; a
/// directory without a readable one is left out).
pub fn list(files: &dyn AppFiles) -> Vec<AppInfo> {
    if !files.available() {
        return Vec::new();
    }
    let mut out: Vec<AppInfo> = files
        .list(APPS_DIR)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, is_dir)| *is_dir && valid_id(name))
        .filter_map(|(id, _)| {
            let rec = files.read(&app_dir(&id), INFO_FILE, APPINFO_SIZE)?;
            parse_appinfo(&rec).ok().filter(|i| i.id == id)
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// The installed app `id`'s manifest.
pub fn find(files: &dyn AppFiles, id: &str) -> Option<AppInfo> {
    if !valid_id(id) || !files.available() {
        return None;
    }
    let rec = files.read(&app_dir(id), INFO_FILE, APPINFO_SIZE)?;
    parse_appinfo(&rec).ok().filter(|i| i.id == id)
}

/// Deletes app `id` with its data.
pub fn remove(files: &dyn AppFiles, id: &str) -> Result<(), u8> {
    if !valid_id(id) {
        return Err(err::BAD_ID);
    }
    if !files.available() {
        return Err(err::NO_STORAGE);
    }
    let dir = app_dir(id);
    if files.list(&dir).is_none() {
        return Err(err::NOT_INSTALLED);
    }
    changed();
    remove_tree(files, dir)
}

/// Deletes directory `dir` with everything in it, then commits.
pub(crate) fn remove_tree(files: &dyn AppFiles, dir: String) -> Result<(), u8> {
    // Depth first without recursion: a directory is removed once a pass
    // finds it empty.
    let mut stack = alloc::vec![dir];
    while let Some(d) = stack.last().cloned() {
        let entries = files.list(&d).ok_or(err::WRITE)?;
        let mut sub = None;
        for (name, is_dir) in entries {
            if is_dir {
                sub = Some(format!("{}/{}", d, name));
                break;
            }
            if !files.delete(&d, &name) {
                return Err(err::WRITE);
            }
        }
        match sub {
            Some(s) => stack.push(s),
            None => {
                if !files.remove_dir(&d) {
                    return Err(err::WRITE);
                }
                stack.pop();
            }
        }
    }
    if files.commit() { Ok(()) } else { Err(err::WRITE) }
}

/// Boards without app storage: nothing is available.
pub struct NoAppFiles;
impl AppFiles for NoAppFiles {
    fn available(&self) -> bool {
        false
    }
    fn read(&self, _: &str, _: &str, _: usize) -> Option<Vec<u8>> {
        None
    }
    fn save(&self, _: &str, _: &str, _: &[u8]) -> bool {
        false
    }
    fn make_dirs(&self, _: &str) -> bool {
        false
    }
    fn list(&self, _: &str) -> Option<Vec<(String, bool)>> {
        None
    }
    fn delete(&self, _: &str, _: &str) -> bool {
        false
    }
    fn remove_dir(&self, _: &str) -> bool {
        false
    }
}

// --- running apps (CPU0) -----------------------------------------------------------------------

/// `()` → the number of installed apps (0 without app storage).
pub const SYS_APP_COUNT: u8 = 0x80;
/// `(i)` → string: app `i`'s id (sorted by id), "" past the end.
pub const SYS_APP_ID: u8 = 0x81;
/// `(i)` → string: app `i`'s display name, "" past the end.
pub const SYS_APP_NAME: u8 = 0x82;
/// `(id: string)` → ends every program and runs app `id` ("" = the system image) from the
/// next pass, like `runProgram`: nothing of the caller runs after it. 0 if `id` is not
/// installed (the caller goes on).
pub const SYS_APP_RUN: u8 = 0x83;
/// `()` → string: the calling task's app id, "" for the system image (answered by the VM).
pub const SYS_APP_CURRENT: u8 = 0x84;
/// `(id: string)` → the app's first task, started (from the next pass) if it does not run,
/// and shown; -1 not installed, -2 [`crate::domain::MAX_APPS`] already run, -3 no task
/// slot. A start that fails later ends that task (`on_task_exit(task, -1)`).
pub const SYS_APP_START: u8 = 0x85;
/// `(task)` → shows the app `task` belongs to (0 = the launcher): 1, or 0 if it is no app.
pub const SYS_APP_SHOW: u8 = 0x86;
/// `(id: string)` → the app's first task if it runs (or is starting), else -1.
pub const SYS_APP_TASK: u8 = 0x87;
/// `()` → ends the calling app (every task of it); in the system image 0 and nothing.
pub const SYS_APP_EXIT: u8 = 0x88;

/// What an app syscall did.
pub enum Call {
    Value(i32),
    /// `SYS_APP_RUN` took: the calling task ends now.
    Exit,
}

/// Single-core state (CPU0: the VM, the runtime, a BSP's network service between passes).
struct Cpu0<T>(core::cell::UnsafeCell<T>);
// SAFETY: touched by CPU0 only (see the functions below).
unsafe impl<T> Sync for Cpu0<T> {}

impl<T> Cpu0<T> {
    #[allow(clippy::mut_from_ref)]
    fn get(&self) -> &mut T {
        // SAFETY: CPU0 only, and no reference outlives a call here.
        unsafe { &mut *self.0.get() }
    }
}

/// The running app (None = the system image).
static CURRENT: Cpu0<Option<AppInfo>> = Cpu0(core::cell::UnsafeCell::new(None));
/// The apps `startApp` runs beside `main` (docs/apps.md "Step 2").
static STARTED: Cpu0<Vec<String>> = Cpu0(core::cell::UnsafeCell::new(Vec::new()));
/// A run asked for: Some(None) = back to the system image.
static RUN_REQUEST: Cpu0<Option<Option<String>>> = Cpu0(core::cell::UnsafeCell::new(None));
/// [`list`] as last read, with the [`GENERATION`] it was read at.
static LIST_CACHE: Cpu0<Option<(u32, Vec<AppInfo>)>> = Cpu0(core::cell::UnsafeCell::new(None));
/// Bumped by every install and remove.
static GENERATION: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

fn changed() {
    GENERATION.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
}

/// The running app's manifest, None for the system image.
pub fn current() -> Option<AppInfo> {
    CURRENT.get().clone()
}

/// The runtime switched to `app`.
pub fn set_current(app: Option<AppInfo>) {
    *CURRENT.get() = app;
}

/// The apps started beside `main` (`startApp`), in domain order.
pub fn started() -> Vec<String> {
    STARTED.get().clone()
}

/// The runtime's started apps changed.
pub fn set_started(ids: Vec<String>) {
    *STARTED.get() = ids;
}

/// App `id` runs (as `main` or started).
pub fn is_running(id: &str) -> bool {
    current().is_some_and(|a| a.id == id) || STARTED.get().iter().any(|s| s == id)
}

/// Asks the runtime to run app `id` (None = the system image) from the next pass.
pub fn request_run(id: Option<&str>) {
    *RUN_REQUEST.get() = Some(id.map(String::from));
}

/// The run asked for, once.
pub fn take_run_request() -> Option<Option<String>> {
    RUN_REQUEST.get().take()
}

/// [`list`], read again only after an install or remove.
pub fn list_cached(files: &dyn AppFiles) -> Vec<AppInfo> {
    let generation = GENERATION.load(core::sync::atomic::Ordering::Relaxed);
    let cache = LIST_CACHE.get();
    match cache {
        Some((g, l)) if *g == generation => l.clone(),
        _ => {
            let l = list(files);
            *cache = Some((generation, l.clone()));
            l
        }
    }
}

/// Reads and checks installed app `id` for running: (its manifest, the image).
pub fn load(files: &dyn AppFiles, id: &str, base: u32, caps: u32, max: usize) -> Result<(AppInfo, Vec<u8>), u8> {
    if !valid_id(id) {
        return Err(err::BAD_ID);
    }
    if !files.available() {
        return Err(err::NO_STORAGE);
    }
    if files.list(&app_dir(id)).is_none() {
        return Err(err::NOT_INSTALLED);
    }
    let image = files.read(&app_dir(id), IMAGE_FILE, max).ok_or(err::TOO_LARGE)?;
    let info = check(&image, base, caps)?;
    if info.id != id {
        return Err(err::BAD_ID);
    }
    Ok((info, image))
}

/// Answers an app syscall (`SYS_APP_*`); None for any other id.
pub fn syscall(id: u8, args: &[i32], objs: &mut crate::pool::ObjPool, files: &dyn AppFiles) -> Option<Call> {
    let arg0 = args.first().copied().unwrap_or(0);
    let string = |objs: &mut crate::pool::ObjPool, s: &str| {
        objs.str_alloc(s.as_bytes()).map(|p| Call::Value(p as i32))
    };
    match id {
        SYS_APP_COUNT => Some(Call::Value(list_cached(files).len() as i32)),
        SYS_APP_ID | SYS_APP_NAME => {
            let apps = list_cached(files);
            let app = usize::try_from(arg0).ok().and_then(|i| apps.get(i));
            let text = app.map_or("", |a| if id == SYS_APP_ID { a.id.as_str() } else { a.name.as_str() });
            string(objs, text)
        }
        SYS_APP_RUN => {
            let raw = objs.str_get(arg0 as crate::pool::ObjPtr);
            let Ok(target) = core::str::from_utf8(raw).map(String::from) else {
                return Some(Call::Value(0));
            };
            if target.is_empty() {
                request_run(None);
                return Some(Call::Exit);
            }
            if find(files, &target).is_none() {
                return Some(Call::Value(0));
            }
            request_run(Some(&target));
            Some(Call::Exit)
        }
        SYS_APP_CURRENT => {
            let cur = current();
            string(objs, cur.as_ref().map_or("", |a| a.id.as_str()))
        }
        _ => None,
    }
}

/// A storage tree in RAM (tests; the sim without `--storage`).
#[derive(Default)]
pub struct MemAppFiles {
    /// Path → None for a directory, Some(data) for a file.
    nodes: RefCell<BTreeMap<String, Option<Vec<u8>>>>,
}

impl MemAppFiles {
    fn child_of<'a>(path: &'a str, dir: &str) -> Option<&'a str> {
        let rest = path.strip_prefix(dir)?.strip_prefix('/')?;
        (!rest.contains('/')).then_some(rest)
    }
}

impl AppFiles for MemAppFiles {
    fn available(&self) -> bool {
        true
    }
    fn read(&self, dir: &str, name: &str, max: usize) -> Option<Vec<u8>> {
        match self.nodes.borrow().get(&format!("{}/{}", dir, name)) {
            Some(Some(d)) if d.len() <= max => Some(d.clone()),
            _ => None,
        }
    }
    fn save(&self, dir: &str, name: &str, data: &[u8]) -> bool {
        self.make_dirs(dir) && {
            self.nodes.borrow_mut().insert(format!("{}/{}", dir, name), Some(data.to_vec()));
            true
        }
    }
    fn make_dirs(&self, dir: &str) -> bool {
        let mut nodes = self.nodes.borrow_mut();
        let mut at = String::new();
        for c in dir.split('/').filter(|c| !c.is_empty()) {
            if !at.is_empty() {
                at.push('/');
            }
            at.push_str(c);
            match nodes.get(&at) {
                Some(Some(_)) => return false,
                Some(None) => {}
                None => {
                    nodes.insert(at.clone(), None);
                }
            }
        }
        true
    }
    fn list(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        let nodes = self.nodes.borrow();
        if !matches!(nodes.get(dir), Some(None)) {
            return None;
        }
        Some(
            nodes
                .iter()
                .filter_map(|(p, n)| Self::child_of(p, dir).map(|c| (c.to_string(), n.is_none())))
                .collect(),
        )
    }
    fn delete(&self, dir: &str, name: &str) -> bool {
        let mut nodes = self.nodes.borrow_mut();
        let key = format!("{}/{}", dir, name);
        matches!(nodes.get(&key), Some(Some(_))) && nodes.remove(&key).is_some()
    }
    fn remove_dir(&self, dir: &str) -> bool {
        let empty = self.list(dir).is_some_and(|l| l.is_empty());
        empty && self.nodes.borrow_mut().remove(dir).is_some()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec;

    const BASE: u32 = 0x1000;

    /// An FS v3 image at `BASE` holding `resources` (name, kind, data), as
    /// ferrite_build writes it.
    fn image(resources: &[(&str, u8, Vec<u8>)]) -> Vec<u8> {
        let n = resources.len();
        let mut table = Vec::new();
        let mut data = Vec::new();
        let data_start = 16 + n * 32;
        for (name, kind, d) in resources {
            let mut e = [0u8; 32];
            e[..name.len()].copy_from_slice(name.as_bytes());
            e[16] = *kind;
            let off = BASE + (data_start + data.len()) as u32;
            e[20..24].copy_from_slice(&off.to_le_bytes());
            e[24..28].copy_from_slice(&(d.len() as u32).to_le_bytes());
            table.extend_from_slice(&e);
            data.extend_from_slice(d);
        }
        let mut head = Vec::new();
        head.extend_from_slice(b"FERR");
        head.extend_from_slice(&3u16.to_le_bytes());
        head.extend_from_slice(&1280u16.to_le_bytes());
        head.extend_from_slice(&800u16.to_le_bytes());
        head.extend_from_slice(&(n as u16).to_le_bytes());
        let mut rest = table;
        rest.extend_from_slice(&data);
        let crc = !crate::fs::crc32_update(crate::fs::crc32_update(!0, &head), &rest);
        head.extend_from_slice(&crc.to_le_bytes());
        head.extend_from_slice(&rest);
        head
    }

    fn appinfo(id: &str, version: u32, entry: &str, caps: u32) -> Vec<u8> {
        let mut r = vec![0u8; APPINFO_SIZE];
        r[..2].copy_from_slice(b"AI");
        r[2] = 1;
        r[3] = id.len() as u8;
        r[4..4 + id.len()].copy_from_slice(id.as_bytes());
        r[36..40].copy_from_slice(b"Name");
        r[68..72].copy_from_slice(&version.to_le_bytes());
        r[72..72 + entry.len()].copy_from_slice(entry.as_bytes());
        r[104..108].copy_from_slice(&caps.to_le_bytes());
        r
    }

    fn package(id: &str, version: u32) -> Vec<u8> {
        image(&[
            ("main", RES_PROGRAM, vec![1, 2, 3]),
            ("appinfo", RES_APPINFO, appinfo(id, version, "main", 0b1)),
        ])
    }

    #[test]
    fn check_accepts_a_package_and_rejects_the_rest() {
        let info = check(&package("com.test.a", 0x010203), BASE, 0b1).unwrap();
        assert_eq!((info.id.as_str(), info.version_text().as_str()), ("com.test.a", "1.2.3"));
        assert_eq!(info.entry, "main");

        let mut bad = package("a", 1);
        *bad.last_mut().unwrap() ^= 1;
        assert_eq!(check(&bad, BASE, 1), Err(err::BAD_IMAGE));
        assert_eq!(check(&package("a", 1)[..40], BASE, 1), Err(err::BAD_IMAGE));
        assert_eq!(check(b"nope", BASE, 1), Err(err::BAD_IMAGE));
        let system = image(&[("main", RES_PROGRAM, vec![1])]);
        assert_eq!(check(&system, BASE, 1), Err(err::NO_APPINFO));
        assert_eq!(check(&package("a", 1), BASE, 0), Err(err::CAPS));
        let no_entry = image(&[("appinfo", RES_APPINFO, appinfo("a", 1, "main", 0))]);
        assert_eq!(check(&no_entry, BASE, 0), Err(err::NO_ENTRY));
        let bad_id = image(&[("main", RES_PROGRAM, vec![1]), ("appinfo", RES_APPINFO, appinfo("Bad", 1, "main", 0))]);
        assert_eq!(check(&bad_id, BASE, 0), Err(err::BAD_ID));
        // Built for another base: its offsets point elsewhere.
        assert_eq!(check(&package("a", 1), BASE + 0x1000, 1), Err(err::BAD_IMAGE));
    }

    #[test]
    fn valid_ids() {
        for ok in ["a", "0", "com.acme.clock", "x_y-z", &"a".repeat(32)] {
            assert!(valid_id(ok), "{ok}");
        }
        for bad in ["", "A", ".a", "-a", "a/b", "a b", "ä", &"a".repeat(33), ".."] {
            assert!(!valid_id(bad), "{bad}");
        }
    }

    #[test]
    fn install_replace_list_remove() {
        let files = MemAppFiles::default();
        assert_eq!(list(&files), vec![]);
        let a = install(&files, &package("b.app", 1), BASE, 1).unwrap();
        assert!(!a.replaced);
        install(&files, &package("a.app", 1), BASE, 1).unwrap();
        // The app's data survives an update.
        assert!(files.save("ferrite/apps/b.app/data", "eeprom.bin", &[7]));
        let b2 = install(&files, &package("b.app", 0x20000), BASE, 1).unwrap();
        assert!(b2.replaced);
        assert_eq!(files.read("ferrite/apps/b.app/data", "eeprom.bin", 10), Some(vec![7]));
        let ids: Vec<_> = list(&files).into_iter().map(|i| (i.id, i.version)).collect();
        assert_eq!(ids, vec![("a.app".into(), 1), ("b.app".into(), 0x20000)]);
        assert_eq!(files.read("ferrite/apps/b.app", IMAGE_FILE, usize::MAX), Some(package("b.app", 0x20000)));
        assert_eq!(find(&files, "a.app").map(|i| i.version), Some(1));

        assert_eq!(remove(&files, "b.app"), Ok(()));
        assert_eq!(remove(&files, "b.app"), Err(err::NOT_INSTALLED));
        assert_eq!(remove(&files, "../x"), Err(err::BAD_ID));
        assert!(files.list("ferrite/apps/b.app").is_none());
        assert!(find(&files, "b.app").is_none());
        assert_eq!(list(&files).len(), 1);
    }

    #[test]
    fn a_bad_package_installs_nothing() {
        let files = MemAppFiles::default();
        assert_eq!(install(&files, &image(&[("main", RES_PROGRAM, vec![1])]), BASE, 1), Err(err::NO_APPINFO));
        assert!(files.list(APPS_DIR).is_none());
    }

    #[test]
    fn syscalls_list_and_ask_for_a_run() {
        let files = MemAppFiles::default();
        let mut objs = crate::pool::ObjPool::new();
        let value = |c: Option<Call>| match c {
            Some(Call::Value(v)) => v,
            _ => panic!("not a value"),
        };
        let text = |objs: &crate::pool::ObjPool, p: i32| objs.str_get(p as crate::pool::ObjPtr).to_vec();
        assert_eq!(value(syscall(SYS_APP_COUNT, &[], &mut objs, &files)), 0);
        install(&files, &package("b.app", 1), BASE, 1).unwrap();
        install(&files, &package("a.app", 1), BASE, 1).unwrap();
        // The cache follows installs.
        assert_eq!(value(syscall(SYS_APP_COUNT, &[], &mut objs, &files)), 2);
        let id = value(syscall(SYS_APP_ID, &[1], &mut objs, &files));
        assert_eq!(text(&objs, id), b"b.app");
        let name = value(syscall(SYS_APP_NAME, &[0], &mut objs, &files));
        assert_eq!(text(&objs, name), b"Name");
        let past = value(syscall(SYS_APP_ID, &[5], &mut objs, &files));
        assert_eq!(text(&objs, past), b"");

        let missing = objs.str_alloc(b"c.app").unwrap() as i32;
        assert_eq!(value(syscall(SYS_APP_RUN, &[missing], &mut objs, &files)), 0);
        assert_eq!(take_run_request(), None);
        let a = objs.str_alloc(b"a.app").unwrap() as i32;
        assert!(matches!(syscall(SYS_APP_RUN, &[a], &mut objs, &files), Some(Call::Exit)));
        assert_eq!(take_run_request(), Some(Some("a.app".into())));
        assert_eq!(take_run_request(), None);
        let empty = objs.str_alloc(b"").unwrap() as i32;
        assert!(matches!(syscall(SYS_APP_RUN, &[empty], &mut objs, &files), Some(Call::Exit)));
        assert_eq!(take_run_request(), Some(None));

        set_current(find(&files, "a.app"));
        let cur = value(syscall(SYS_APP_CURRENT, &[], &mut objs, &files));
        assert_eq!(text(&objs, cur), b"a.app");
        set_current(None);
        remove(&files, "a.app").unwrap();
        assert_eq!(value(syscall(SYS_APP_COUNT, &[], &mut objs, &files)), 1);
        let (info, image) = load(&files, "b.app", BASE, 1, usize::MAX).unwrap();
        assert_eq!((info.id.as_str(), image), ("b.app", package("b.app", 1)));
        assert_eq!(load(&files, "a.app", BASE, 1, usize::MAX).map(|_| ()), Err(err::NOT_INSTALLED));
        assert_eq!(load(&files, "b.app", BASE, 0, usize::MAX).map(|_| ()), Err(err::CAPS));
    }

    #[test]
    fn no_storage() {
        assert_eq!(install(&NoAppFiles, &package("a", 1), BASE, 1), Err(err::NO_STORAGE));
        assert_eq!(remove(&NoAppFiles, "a"), Err(err::NO_STORAGE));
        assert!(list(&NoAppFiles).is_empty());
    }
}
