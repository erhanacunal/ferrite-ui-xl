//! Persistent storage — the FAT32 file system on the eMMC CACHE partition
//! (p8), through `exynos_4412_rs::fat` (as its `fat_test`). Everything here
//! runs on CPU0.
//!
//! The firmware's own files live in [`DIR`] (`FERRITE/`):
//! - `FLASH.BIN`, the ferrite "flash" (`flash.rs`, a RAM region): [`init`]
//!   mounts CACHE and [`load`] reads it at boot; [`save`] writes the used
//!   part back (`flash::save_if_dirty`: before a restart, and once writes
//!   have been quiet for [`IDLE_SAVE_MS`]);
//! - `FERRITE.BIN` (the firmware uart_boot starts, `net::install_firmware`),
//!   the WiFi files (`net`), `EEPROM.BIN` (`eeprom.rs`).
//!
//! Programs get their own tree, [`USER_DIR`] (`FERRITE/USER/`), through
//! [`CacheStorage`] (ferrite-core `storage`: `diskOpen`, `fileWrite`,
//! `diskMkdir`…). The core checks every path (8.3 components, no `..`), so a
//! program cannot reach [`DIR`]'s files or anything else on CACHE.
//!
//! CACHE is never formatted here: without a FAT32 on it persistence is off
//! (logged at boot), the board runs from RAM only and programs get
//! `NO_STORAGE`.
//!
//! `FLASH.BIN` is rewritten in place: power lost during a save leaves a torn
//! image, which the boot-time CRC check (`VERIFY_FS_ON_BOOT`) turns into
//! Code 9 + recovery, so a new `writefs` fixes it.

use alloc::boxed::Box;
#[cfg(feature = "wifi")]
use alloc::vec::Vec;
use core::cell::UnsafeCell;
use core::ops::ControlFlow;

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_rs::fat::embedded_sdmmc::{Mode, RawDirectory, RawFile, RawVolume, VolumeIdx, VolumeManager};
use exynos_4412_rs::fat::{self, MctClock, PartitionDevice};
use exynos_4412_rs::println;
use exynos_4412_rs::time::DateTime;
use ferrite_core::storage::{self as core_storage, Kind, MAX_OPEN, Path, StorageBackend, err};

use crate::systick;

/// CACHE.
const PARTITION: u32 = 8;
pub const DIR: &str = "FERRITE";
pub const FILE: &str = "FLASH.BIN";
/// Programs' files: `DIR/USER_DIR`.
pub const USER_DIR: &str = "USER";
/// Save this long after the last write (a writefs in progress keeps writing
/// every chunk, so it is not saved half done unless the host stalls).
pub const IDLE_SAVE_MS: u32 = 1500;
/// Programs' written files reach the card this long after their last write
/// (or when closed, or before a restart).
const USER_FLUSH_MS: u32 = 1000;
/// Device buffer as `fat_test`: read-ahead + single blocks + a 256 KiB write run.
const DEVICE_BYTES: usize = fat::MIN_BUFFER - 512 + 256 * 1024;
/// Addresses U-Boot may have given the card (it does not report it).
const RCA_GUESSES: [u16; 2] = [1, 2];
/// Clock start when the RTC holds no valid time (only file timestamps).
const FALLBACK: DateTime = DateTime { year: 2026, month: 1, day: 1, hour: 0, minute: 0, second: 0 };
/// Directories open at once: a path walk holds two, an operation in it one more.
const MAX_DIRS: usize = 6;
/// Files open at once: programs' plus one the firmware reads or writes.
const MAX_FILES: usize = MAX_OPEN + 2;

type FsError = fat::embedded_sdmmc::Error<fat::Error>;
type Volumes = VolumeManager<PartitionDevice<'static>, MctClock, MAX_DIRS, MAX_FILES, 1>;

#[repr(C, align(32))]
struct Buffer(UnsafeCell<[u8; DEVICE_BYTES]>);

// SAFETY: taken once, by `init`.
unsafe impl Sync for Buffer {}

static DEVICE: Buffer = Buffer(UnsafeCell::new([0; DEVICE_BYTES]));

/// The mounted CACHE volume; None = persistence off.
struct Mounted {
    volumes: Volumes,
    volume: RawVolume,
}

static mut MOUNTED: Option<Box<Mounted>> = None;

fn mounted() -> Option<&'static Mounted> {
    unsafe { (*(&raw const MOUNTED)).as_deref() }
}

/// Whether CACHE was mounted and saves are possible.
pub fn available() -> bool {
    mounted().is_some()
}

/// Opens the eMMC and checks CACHE holds a FAT32. Call once, after `heap::init`.
pub fn init() {
    match open() {
        Ok(m) => unsafe { *(&raw mut MOUNTED) = Some(m) },
        Err(why) => println!("ferrite: storage off ({}), resources live in RAM only", why),
    }
}

fn open() -> Result<Box<Mounted>, &'static str> {
    let card = Mmc::new().map_err(|_| "eMMC controller")?;
    match card.read_ext_csd() {
        Ok(ext) if ext.reads_user_area() => {}
        Ok(_) => return Err("eMMC not on its user area"),
        Err(_) => return Err("EXT_CSD read"),
    }
    let rca = RCA_GUESSES
        .into_iter()
        .find(|rca| card.read_status(*rca).is_ok_and(|s| mmc::card_state(s) == 4))
        .ok_or("no card in transfer state")?;
    // SAFETY: the only reference to the buffer, taken once.
    let buffer: &'static mut [u8] = unsafe { &mut *DEVICE.0.get() };
    let device = PartitionDevice::new(card, rca, PARTITION, buffer).map_err(|_| "CACHE partition")?;
    if !device.probe().map_err(|_| "CACHE read")? {
        return Err("no FAT32 on CACHE (not formatted here)");
    }
    let p = device.partition();
    println!("ferrite: CACHE p{} FAT32, {} MiB", p.number, p.count / 2048);
    let volumes = VolumeManager::new_with_limits(device, clock(), 5000);
    let volume = volumes.open_raw_volume(VolumeIdx(0)).map_err(|_| "CACHE volume")?;
    Ok(Box::new(Mounted { volumes, volume }))
}

/// File timestamps: the PMIC RTC (which `settime` sets to the host's local time).
fn clock() -> MctClock {
    let mct = Mct::new();
    let now = Rtc::new(I2c::new(Bus::I2c0)).read(mct).ok().and_then(|r| r.time());
    MctClock::new(now.unwrap_or(FALLBACK), mct)
}

impl Mounted {
    /// Opens the directory at `components` (from the volume's root, each one
    /// existing). The caller closes it.
    fn walk<'a>(&self, components: impl IntoIterator<Item = &'a str>) -> Result<RawDirectory, FsError> {
        let v = &self.volumes;
        let mut dir = v.open_root_dir(self.volume)?;
        for c in components {
            let next = v.open_dir(dir, c);
            v.close_dir(dir)?;
            dir = next?;
        }
        Ok(dir)
    }

    /// `walk`, runs `f` in the directory, closes it.
    fn in_dir<'a, R>(
        &self,
        components: impl IntoIterator<Item = &'a str>,
        f: impl FnOnce(&Volumes, RawDirectory) -> Result<R, FsError>,
    ) -> Result<R, FsError> {
        let dir = self.walk(components)?;
        let r = f(&self.volumes, dir);
        self.volumes.close_dir(dir)?;
        r
    }

    /// Makes `name` in the directory at `parent` unless it is there.
    fn ensure_dir(&self, parent: &[&str], name: &str) -> Result<(), FsError> {
        self.in_dir(parent.iter().copied(), |v, d| match v.make_dir_in_dir(d, name) {
            Ok(()) | Err(FsError::DirAlreadyExists) => Ok(()),
            Err(e) => Err(e),
        })
    }

    fn flush_device(&self) -> Result<(), FsError> {
        Ok(self.volumes.device(|d| d.flush())?)
    }
}

/// Reads the image file into `region`; returns its length (0 = no file yet).
pub fn load(region: &mut [u8]) -> usize {
    let Some(m) = mounted() else { return 0 };
    match read_file(m, FILE, region) {
        Ok(n) => {
            println!("ferrite: loaded {}/{}, {} bytes", DIR, FILE, n);
            n
        }
        Err(FsError::NotFound) => {
            println!("ferrite: no {}/{} yet", DIR, FILE);
            0
        }
        Err(e) => {
            println!("ferrite: load {}/{} ERROR {:?}", DIR, FILE, e);
            0
        }
    }
}

/// Reads [`DIR`]/`name` into `region`, at most its length; the bytes read.
fn read_file(m: &Mounted, name: &str, region: &mut [u8]) -> Result<usize, FsError> {
    m.in_dir([DIR], |v, dir| {
        let file = v.open_file_in_dir(dir, name, Mode::ReadOnly)?;
        let len = (v.file_length(file)? as usize).min(region.len());
        let mut done = 0;
        let r = loop {
            if done >= len {
                break Ok(done);
            }
            match v.read(file, &mut region[done..len]) {
                Ok(0) => break Ok(done),
                Ok(n) => done += n,
                Err(e) => break Err(e),
            }
        };
        v.close_file(file)?;
        r
    })
}

/// The length of [`DIR`]/`name`.
fn file_len(m: &Mounted, name: &str) -> Result<usize, FsError> {
    m.in_dir([DIR], |v, dir| {
        let file = v.open_file_in_dir(dir, name, Mode::ReadOnly)?;
        let len = v.file_length(file);
        v.close_file(file)?;
        Ok(len? as usize)
    })
}

/// Reads [`DIR`]/`name` (an 8.3 name) whole, if it exists and is at most
/// `max` bytes.
#[cfg(feature = "wifi")]
pub fn read_named(name: &str, max: usize) -> Option<Vec<u8>> {
    let m = mounted()?;
    let len = file_len(m, name).ok()?;
    if len > max {
        println!("ferrite: {}/{} too large ({} bytes)", DIR, name, len);
        return None;
    }
    let mut buf = alloc::vec![0u8; len];
    let n = read_file(m, name, &mut buf).ok()?;
    buf.truncate(n);
    Some(buf)
}

/// Reads [`DIR`]/`name` into `buf` if it exists and is exactly its size.
pub fn read_exact(name: &str, buf: &mut [u8]) -> bool {
    let Some(m) = mounted() else { return false };
    file_len(m, name).is_ok_and(|len| len == buf.len()) && read_file(m, name, buf).is_ok_and(|n| n == buf.len())
}

/// Writes `data` as the image file (made or truncated), then flushes the
/// device so it is on the card when this returns.
pub fn save(data: &[u8]) -> bool {
    save_named(FILE, data)
}

/// Writes [`DIR`]/`name` (an 8.3 name, made or truncated) and flushes.
pub fn save_named(name: &str, data: &[u8]) -> bool {
    let Some(m) = mounted() else { return false };
    match write_file(m, name, data).and_then(|()| m.flush_device()) {
        Ok(()) => true,
        Err(e) => {
            println!("ferrite: save {}/{} ERROR {:?}", DIR, name, e);
            false
        }
    }
}

fn write_file(m: &Mounted, name: &str, data: &[u8]) -> Result<(), FsError> {
    m.ensure_dir(&[], DIR)?;
    m.in_dir([DIR], |v, dir| {
        let file = v.open_file_in_dir(dir, name, Mode::ReadWriteCreateOrTruncate)?;
        let r = v.write(file, data);
        v.close_file(file)?;
        r
    })
}

/// Everything written reaches the card: programs' open files and the
/// EEPROM file. Before a restart or power-off (`force`), and from
/// `frame_end` (`force` false: once writes have settled).
pub fn sync(force: bool) {
    CacheStorage.flush(force);
    crate::eeprom::save_if_dirty(force);
}

// ---------------------------------------------------------------------------
// Programs' files: ferrite-core `StorageBackend`, rooted at DIR/USER_DIR.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct UserFile {
    raw: RawFile,
    write: bool,
    /// Written since the last flush.
    dirty: bool,
}

static mut FILES: [Option<UserFile>; MAX_OPEN] = [None; MAX_OPEN];
/// `systick::millis` of the last write to a program's file.
static mut LAST_WRITE_MS: u32 = 0;

fn files() -> &'static mut [Option<UserFile>; MAX_OPEN] {
    // SAFETY: CPU0 only, never re-entered (the VM's builtins and frame_end).
    unsafe { &mut *(&raw mut FILES) }
}

fn map_err(e: FsError) -> i32 {
    use FsError as E;
    match e {
        E::NotFound => err::NOT_FOUND,
        E::FileAlreadyExists | E::DirAlreadyExists => err::EXISTS,
        E::DeleteNonEmptyDir => err::NOT_EMPTY,
        E::NotEnoughSpace | E::DiskFull | E::TooManyOpenFiles | E::TooManyOpenDirs => err::FULL,
        E::OpenedDirAsFile | E::OpenedFileAsDir => err::WRONG_KIND,
        E::FileAlreadyOpen | E::DirAlreadyOpen => err::BUSY,
        E::FilenameError(_) | E::ReadOnly => err::BAD_PATH,
        e => {
            println!("ferrite: storage ERROR {:?}", e);
            err::IO
        }
    }
}

/// [`USER_DIR`] made once per boot (it may have been deleted on a PC).
static mut USER_READY: bool = false;

/// The mounted volume with [`USER_DIR`] in place.
fn user() -> Result<&'static Mounted, i32> {
    let m = mounted().ok_or(err::NO_STORAGE)?;
    if !unsafe { USER_READY } {
        m.ensure_dir(&[], DIR).and_then(|()| m.ensure_dir(&[DIR], USER_DIR)).map_err(map_err)?;
        unsafe { USER_READY = true };
    }
    Ok(m)
}

/// Runs `f` in the directory holding `path`'s last component.
fn in_parent<R>(path: &Path, f: impl FnOnce(&Volumes, RawDirectory, &str) -> Result<R, FsError>) -> Result<R, i32> {
    let m = user()?;
    let (parent, leaf) = path.split_last().ok_or(err::WRONG_KIND)?;
    let comps = [DIR, USER_DIR].into_iter().chain(parent.split('/').filter(|c| !c.is_empty()));
    m.in_dir(comps, |v, d| f(v, d, leaf)).map_err(map_err)
}

/// The kind of `path`'s entry in its parent.
fn kind_of(path: &Path) -> Result<Option<Kind>, i32> {
    if path.is_root() {
        user()?;
        return Ok(Some(Kind::Dir));
    }
    match in_parent(path, |v, d, leaf| v.find_directory_entry(d, leaf)) {
        Ok(e) => Ok(Some(if e.attributes.is_directory() { Kind::Dir } else { Kind::File })),
        Err(err::NOT_FOUND) => Ok(None),
        Err(e) => Err(e),
    }
}

/// The board's `Platform::StorageB`: zero-sized, over the statics above.
pub struct CacheStorage;

impl CacheStorage {
    fn file(slot: usize) -> Result<UserFile, i32> {
        files()[slot].ok_or(err::IO)
    }
}

impl StorageBackend for CacheStorage {
    fn open(&self, slot: usize, path: &Path, mode: core_storage::Mode) -> Result<(), i32> {
        use core_storage::Mode as M;
        let fat_mode = match mode {
            M::Read => Mode::ReadOnly,
            M::Write => Mode::ReadWriteCreateOrTruncate,
            M::Append => Mode::ReadWriteCreateOrAppend,
        };
        let raw = in_parent(path, |v, d, leaf| v.open_file_in_dir(d, leaf, fat_mode))?;
        files()[slot] = Some(UserFile { raw, write: mode != M::Read, dirty: false });
        Ok(())
    }

    fn close(&self, slot: usize) -> Result<(), i32> {
        let f = files()[slot].take().ok_or(err::IO)?;
        let m = mounted().ok_or(err::NO_STORAGE)?;
        m.volumes.close_file(f.raw).map_err(map_err)?;
        if f.dirty {
            m.flush_device().map_err(map_err)?;
        }
        Ok(())
    }

    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        let f = Self::file(slot)?;
        let v = &mounted().ok_or(err::NO_STORAGE)?.volumes;
        match v.read(f.raw, buf) {
            Ok(n) => Ok(n),
            Err(FsError::EndOfFile) => Ok(0),
            Err(e) => Err(map_err(e)),
        }
    }

    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
        let f = Self::file(slot)?;
        if !f.write {
            return Err(err::BAD_PATH);
        }
        let v = &mounted().ok_or(err::NO_STORAGE)?.volumes;
        v.write(f.raw, data).map_err(map_err)?;
        if let Some(f) = files()[slot].as_mut() {
            f.dirty = true;
        }
        unsafe { LAST_WRITE_MS = systick::millis() };
        Ok(data.len())
    }

    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        let f = Self::file(slot)?;
        let v = &mounted().ok_or(err::NO_STORAGE)?.volumes;
        v.file_seek_from_start(f.raw, pos).map_err(map_err)
    }

    fn size(&self, slot: usize) -> Result<u32, i32> {
        let f = Self::file(slot)?;
        let v = &mounted().ok_or(err::NO_STORAGE)?.volumes;
        v.file_length(f.raw).map_err(map_err)
    }

    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        kind_of(path)
    }

    fn delete(&self, path: &Path) -> Result<(), i32> {
        match kind_of(path)? {
            Some(Kind::File) => in_parent(path, |v, d, leaf| v.delete_entry_in_dir(d, leaf)),
            Some(Kind::Dir) => Err(err::WRONG_KIND),
            None => Err(err::NOT_FOUND),
        }
    }

    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        in_parent(path, |v, d, leaf| v.make_dir_in_dir(d, leaf))?;
        mounted().ok_or(err::NO_STORAGE)?.flush_device().map_err(map_err)
    }

    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        match kind_of(path)? {
            Some(Kind::Dir) => in_parent(path, |v, d, leaf| v.delete_entry_in_dir(d, leaf))?,
            Some(Kind::File) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        mounted().ok_or(err::NO_STORAGE)?.flush_device().map_err(map_err)
    }

    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        match kind_of(path)? {
            Some(Kind::Dir) => {}
            Some(Kind::File) => return Err(err::WRONG_KIND),
            None => return Err(err::NOT_FOUND),
        }
        let m = user()?;
        let comps = [DIR, USER_DIR].into_iter().chain(path.components());
        let mut seen = 0;
        let mut found = None;
        m.in_dir(comps, |v, d| {
            v.iterate_dir(d, |e| {
                let a = e.attributes;
                let (base, ext) = (e.name.base_name(), e.name.extension());
                if a.is_lfn() || a.is_volume() || base.first() == Some(&b'.') {
                    return ControlFlow::Continue(());
                }
                if seen == index {
                    let mut n = base.len();
                    name[..n].copy_from_slice(base);
                    if !ext.is_empty() {
                        name[n] = b'.';
                        name[n + 1..n + 1 + ext.len()].copy_from_slice(ext);
                        n += 1 + ext.len();
                    }
                    found = Some((n, a.is_directory()));
                    return ControlFlow::Break(());
                }
                seen += 1;
                ControlFlow::Continue(())
            })
        })
        .map_err(map_err)?;
        Ok(found)
    }

    fn flush(&self, force: bool) {
        let Some(m) = mounted() else { return };
        if !files().iter().flatten().any(|f| f.dirty) {
            return;
        }
        if !force && systick::millis().wrapping_sub(unsafe { LAST_WRITE_MS }) < USER_FLUSH_MS {
            return;
        }
        for f in files().iter_mut().flatten().filter(|f| f.dirty) {
            if let Err(e) = m.volumes.flush_file(f.raw) {
                println!("ferrite: storage flush ERROR {:?}", e);
            }
            f.dirty = false;
        }
        if let Err(e) = m.flush_device() {
            println!("ferrite: storage flush ERROR {:?}", e);
        }
    }
}
