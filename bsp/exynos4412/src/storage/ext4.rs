//! USERDATA (p10, ~13 GiB): ext4 with a journal, through `exynos_4412_ext4` (the
//! `ext4_rs` crate, which formats it itself: [`format`]). The same tree as on CACHE, and the
//! same API as `fat.rs`, but every name on disk is lower case (`ferrite/flash.bin`,
//! `ferrite/user/…`, `system/wifi.txt`): callers keep using the upper-case 8.3 names, and
//! [`lower`] turns them into the on-disk ones. Programs see their names in upper case, as the
//! core's paths are; names with upper-case letters (made elsewhere) stay out of their sight.
//!
//! Metadata changes go through the journal; file data is written before the transaction that
//! makes it part of a file. A transaction commits on [`sync`]-like calls: after every save, when
//! a program closes a written file or its writes have been quiet for `USER_FLUSH_MS`, and before
//! a restart. A power loss leaves the file system as of the last commit, never inconsistent.
//!
//! Saves ([`save_in`]) write `NAME.new` and rename it over `NAME`: power lost during a save of
//! `FLASH.BIN` or `FERRITE.BIN` leaves the old file or the new one, never a torn one.

use alloc::boxed::Box;
use alloc::format;
use alloc::string::String;
use alloc::vec::Vec;
use core::cell::UnsafeCell;

use exynos_4412_rs::drivers::i2c::{Bus, I2c};
use exynos_4412_rs::drivers::max77686::Rtc;
use exynos_4412_rs::drivers::mmc::{self, Mmc};
use exynos_4412_rs::drivers::timer::Mct;
use exynos_4412_ext4::PartitionBlocks;
use exynos_4412_ext4::ext4_rs::{self, BlockDevice, Error as E, Ext4, FormatOptions, Kind as FsKind, MountOptions};
use exynos_4412_rs::fat::MctClock;
use exynos_4412_rs::println;
use exynos_4412_rs::time::DateTime;
use ferrite_core::storage::{self as core_storage, Kind, MAX_OPEN, Path, StorageBackend, err};

use super::{DIR, USER_DIR, USER_FLUSH_MS};
use crate::systick;

/// USERDATA.
const PARTITION: u32 = 10;
/// Unaligned transfers (most of them: the heap gives 16-byte alignment) go through this.
const BOUNCE_BYTES: usize = 256 * 1024;
/// Where the superblock's magic is in block 0, and what it holds for ext2/3/4.
const MAGIC_AT: usize = 1024 + 0x38;
const MAGIC: u16 = 0xEF53;
/// 8 MiB of clean metadata blocks; a transaction commits by itself at 4 MiB.
const OPTIONS: MountOptions = MountOptions { cache_blocks: 2048, commit_blocks: 1024 };
const ADDRESSES: [u16; 2] = [1, 2];
const FALLBACK: DateTime = DateTime { year: 2026, month: 1, day: 1, hour: 0, minute: 0, second: 0 };

pub type Fs = Ext4<PartitionBlocks<'static>>;

#[repr(C, align(32))]
struct Bounce(UnsafeCell<[u8; BOUNCE_BYTES]>);

// SAFETY: taken once, by the first `device` (CPU0 only).
unsafe impl Sync for Bounce {}

static BOUNCE: Bounce = Bounce(UnsafeCell::new([0; BOUNCE_BYTES]));
static mut BOUNCE_TAKEN: bool = false;
static mut FS: Option<Box<Fs>> = None;
static mut CLOCK: Option<MctClock> = None;

fn fs() -> Option<&'static mut Fs> {
    // SAFETY: CPU0 only, never re-entered (the VM's builtins, frame_end, net::service).
    unsafe { (*(&raw mut FS)).as_deref_mut() }
}

/// Whether USERDATA is mounted.
pub fn available() -> bool {
    fs().is_some()
}

/// Seconds since 1970, local time (as the FAT timestamps): the PMIC RTC at boot plus the MCT.
fn now() -> u32 {
    unsafe { (*(&raw const CLOCK)).as_ref() }.map_or(0, |c| c.now().to_unix().max(0) as u32)
}

/// The partition as a block device. Once per boot: it takes the bounce buffer.
fn device() -> Result<PartitionBlocks<'static>, &'static str> {
    if unsafe { BOUNCE_TAKEN } {
        return Err("USERDATA device made twice");
    }
    let card = Mmc::new().map_err(|_| "eMMC controller")?;
    match card.read_ext_csd() {
        Ok(ext) if ext.reads_user_area() => {}
        Ok(_) => return Err("eMMC not on its user area"),
        Err(_) => return Err("EXT_CSD read"),
    }
    let rca = ADDRESSES
        .into_iter()
        .find(|rca| card.read_status(*rca).is_ok_and(|s| mmc::card_state(s) == 4))
        .ok_or("no card in transfer state")?;
    if unsafe { (*(&raw const CLOCK)).is_none() } {
        let mct = Mct::new();
        let start = Rtc::new(I2c::new(Bus::I2c0)).read(mct).ok().and_then(|r| r.time());
        unsafe { *(&raw mut CLOCK) = Some(MctClock::new(start.unwrap_or(FALLBACK), mct)) };
    }
    // SAFETY: `BOUNCE_TAKEN` makes this the only reference that outlives this call.
    let bounce: &'static mut [u8] = unsafe { &mut *BOUNCE.0.get() };
    let device = PartitionBlocks::new(card, rca, PARTITION, bounce, now).map_err(|_| "USERDATA partition")?;
    unsafe { BOUNCE_TAKEN = true };
    Ok(device)
}

/// Mounts USERDATA. `None`: it holds no ext4 of ours (none at all, or one with features this
/// crate does not handle, such as a Linux rootfs): CACHE is the place, until `migrate`.
/// `Some(Err)`: ours, but it failed to mount (storage off: no falling back to stale CACHE).
pub fn init() -> Option<Result<(), &'static str>> {
    let mut device = match device() {
        Ok(d) => d,
        Err(why) => {
            println!("ferrite: USERDATA: {}", why);
            return None;
        }
    };
    let mut block = alloc::vec![0u8; ext4_rs::BLOCK_SIZE];
    if device.read(0, &mut block).is_err() {
        return Some(Err("USERDATA read"));
    }
    if u16::from_le_bytes([block[MAGIC_AT], block[MAGIC_AT + 1]]) != MAGIC {
        // Keep the device for `format`.
        unsafe { *(&raw mut SPARE) = Some(device) };
        return None;
    }
    match mount(device) {
        Err(Mount::Foreign) => None,
        Err(Mount::Failed) => Some(Err("USERDATA ext4 does not mount")),
        Ok(()) => Some(Ok(())),
    }
}

/// Why [`mount`] failed.
enum Mount {
    /// An ext4 this crate did not make (`Unsupported`): left alone.
    Foreign,
    Failed,
}

/// The device `init` made when USERDATA held no ext4, for `format`.
static mut SPARE: Option<PartitionBlocks<'static>> = None;

fn mount(device: PartitionBlocks<'static>) -> Result<(), Mount> {
    let blocks = device.blocks();
    let mounted = Ext4::mount(device, OPTIONS);
    if mounted.is_err() {
        // The device went down with the failed mount: the bounce buffer is free again.
        unsafe { BOUNCE_TAKEN = false };
    }
    match mounted {
        Ok(fs) => {
            let s = fs.stats();
            println!(
                "ferrite: USERDATA p{} ext4, {} MiB, {} MiB free, {} files free",
                PARTITION,
                blocks / 256,
                s.free_blocks / 256,
                s.free_inodes
            );
            unsafe { *(&raw mut FS) = Some(Box::new(fs)) };
            Ok(())
        }
        Err(e @ E::Unsupported(_)) => {
            println!("ferrite: USERDATA holds an ext4 not made here ({}): left alone", e);
            Err(Mount::Foreign)
        }
        Err(e) => {
            println!("ferrite: USERDATA mount ERROR {}", e);
            Err(Mount::Failed)
        }
    }
}

/// Makes a new, empty file system on USERDATA (everything there is lost) and mounts it.
pub fn format() -> Result<(), &'static str> {
    if available() {
        return Err("USERDATA is mounted");
    }
    let mut device = match unsafe { (*(&raw mut SPARE)).take() } {
        Some(d) => d,
        None => device()?,
    };
    let seed = Mct::new().count() ^ u64::from(now()).rotate_left(32);
    let mut uuid = [0u8; 16];
    let mut x = seed | 1;
    for b in uuid.iter_mut() {
        x ^= x << 13;
        x ^= x >> 7;
        x ^= x << 17;
        *b = x as u8;
    }
    // RFC 4122 version 4 (random) and variant bits, as mke2fs makes them.
    uuid[6] = (uuid[6] & 0x0F) | 0x40;
    uuid[8] = (uuid[8] & 0x3F) | 0x80;
    let mut label = [0u8; 16];
    label[..7].copy_from_slice(b"ferrite");
    let options = FormatOptions { uuid, label, ..FormatOptions::default() };
    let start = systick::millis();
    ext4_rs::format(&mut device, &options).map_err(|e| {
        println!("ferrite: USERDATA format ERROR {}", e);
        "USERDATA format failed"
    })?;
    println!("ferrite: USERDATA formatted in {} ms", systick::millis().wrapping_sub(start));
    mount(device).map_err(|_| "USERDATA does not mount after formatting")
}

/// What the file system holds: (blocks, free blocks, inodes, free inodes).
pub fn stats() -> Option<ext4_rs::Stats> {
    fs().map(|fs| fs.stats())
}

/// `s` as it is on disk: lower case.
fn lower(s: &str) -> String {
    s.to_ascii_lowercase()
}

fn path(dir: &str, name: &str) -> String {
    lower(&format!("/{}/{}", dir, name))
}

fn log(what: &str, at: &str, e: E) {
    println!("ferrite: {} {} ERROR {}", what, at, e);
}

/// Reads `dir`/`name` into `region`, at most its length: the bytes read, None when there is no
/// such file.
pub fn read_into(dir: &str, name: &str, region: &mut [u8]) -> Result<Option<usize>, ()> {
    let fs = fs().ok_or(())?;
    let at = path(dir, name);
    let r = (|| {
        let Some(ino) = fs.lookup(&at)? else { return Ok(None) };
        let len = (fs.metadata(ino)?.size as usize).min(region.len());
        Ok(Some(fs.read_at(ino, 0, &mut region[..len])?))
    })();
    match r {
        Ok(n) => Ok(n),
        Err(E::NotFound) => Ok(None),
        Err(e) => {
            log("read", &at, e);
            Err(())
        }
    }
}

/// The length of `dir`/`name`.
pub fn len_of(dir: &str, name: &str) -> Option<usize> {
    let fs = fs()?;
    let ino = fs.lookup(&path(dir, name)).ok()??;
    let m = fs.metadata(ino).ok()?;
    (m.kind == FsKind::File).then_some(m.size as usize)
}

/// Reads `dir`/`name` whole, if it exists and is at most `max` bytes.
pub fn read_in(dir: &str, name: &str, max: usize) -> Option<Vec<u8>> {
    let len = len_of(dir, name)?;
    if len > max {
        println!("ferrite: {}/{} too large ({} bytes)", dir, name, len);
        return None;
    }
    let mut buf = alloc::vec![0u8; len];
    let n = read_into(dir, name, &mut buf).ok()??;
    buf.truncate(n);
    Some(buf)
}

fn ensure_dir(fs: &mut Fs, at: &str) -> Result<(), E> {
    match fs.mkdir(at) {
        Ok(_) | Err(E::Exists) => Ok(()),
        Err(e) => Err(e),
    }
}

/// Writes `dir`/`name` (`dir` made if needed): as `name.new`, renamed over `name`, then
/// committed, so it is on the card, whole, when this returns.
pub fn save_in(dir: &str, name: &str, data: &[u8]) -> bool {
    let Some(fs) = fs() else { return false };
    let target = path(dir, name);
    let temporary = format!("{}.new", target);
    let r = (|| {
        ensure_dir(fs, &lower(&format!("/{}", dir)))?;
        match fs.remove(&temporary) {
            Ok(()) | Err(E::NotFound) => {}
            Err(e) => return Err(e),
        }
        let ino = fs.create(&temporary)?;
        if fs.write_at(ino, 0, data)? != data.len() {
            return Err(E::NoSpace);
        }
        fs.rename(&temporary, &target)?;
        fs.sync()
    })();
    match r {
        Ok(()) => true,
        Err(e) => {
            log("save", &target, e);
            // Don't leave the partial file taking space.
            let _ = fs.remove(&temporary);
            let _ = fs.sync();
            false
        }
    }
}

/// Appends `data` to `dir`/`name` (made if missing); commits if `commit`.
pub fn append(dir: &str, name: &str, data: &[u8], commit: bool) -> Result<(), E> {
    let fs = fs().ok_or(E::Io)?;
    let at = path(dir, name);
    let ino = match fs.lookup(&at)? {
        Some(ino) => ino,
        None => fs.create(&at)?,
    };
    let size = fs.metadata(ino)?.size;
    if fs.write_at(ino, size, data)? != data.len() {
        return Err(E::NoSpace);
    }
    if commit { fs.sync() } else { Ok(()) }
}

/// Writes `dir`/`name` (replacing it) without committing.
pub fn write_new(dir: &str, name: &str, data: &[u8]) -> Result<(), E> {
    let fs = fs().ok_or(E::Io)?;
    let at = path(dir, name);
    let ino = match fs.lookup(&at)? {
        Some(ino) => {
            fs.set_len(ino, 0)?;
            ino
        }
        None => fs.create(&at)?,
    };
    if fs.write_at(ino, 0, data)? != data.len() {
        return Err(E::NoSpace);
    }
    Ok(())
}

/// Removes `dir`/`name` if it is there, without committing.
pub fn remove(dir: &str, name: &str) -> Result<(), E> {
    match fs().ok_or(E::Io)?.remove(&path(dir, name)) {
        Ok(()) | Err(E::NotFound) => Ok(()),
        Err(e) => Err(e),
    }
}

/// The names in `dir` (`/`-separated from the root), as on disk: (name, is a directory).
pub fn list_dir(dir: &str) -> Option<Vec<(String, bool)>> {
    let fs = fs()?;
    let ino = fs.lookup(&lower(&format!("/{}", dir))).ok()??;
    let mut out = Vec::new();
    fs.read_dir(ino, |e| {
        out.push((String::from_utf8_lossy(e.name).into_owned(), e.kind == FsKind::Dir));
        true
    })
    .ok()?;
    Some(out)
}

/// Commits the running transaction to the journal only (`Ext4::commit_to_log_only`): the file
/// system is read-only after it, and the next mount must replay it.
pub fn commit_to_log_only() -> Result<(), E> {
    fs().ok_or(E::Io)?.commit_to_log_only()
}

/// What mount found: (not cleanly unmounted, a transaction replayed).
pub fn mount_state() -> Option<(bool, bool)> {
    fs().map(|fs| fs.mount_state())
}

/// `Ext4::check` of the whole file system.
pub fn check() -> Result<ext4_rs::CheckReport, E> {
    fs().ok_or(E::Io)?.check()
}

/// Makes directory `dir` (`/`-separated from the root) unless it is there.
pub fn make_dirs(dir: &str) -> bool {
    let Some(fs) = fs() else { return false };
    let mut at = String::new();
    for c in dir.split('/').filter(|c| !c.is_empty()) {
        at.push('/');
        at.push_str(&lower(c));
        if let Err(e) = ensure_dir(fs, &at) {
            log("mkdir", &at, e);
            return false;
        }
    }
    true
}

/// Commits the running transaction.
pub fn commit() -> bool {
    match fs().map(|fs| fs.sync()) {
        Some(Ok(())) => true,
        Some(Err(e)) => {
            log("commit", "USERDATA", e);
            false
        }
        None => false,
    }
}

// ---------------------------------------------------------------------------
// Programs' files: ferrite-core `StorageBackend`, rooted at DIR/USER_DIR.
// ---------------------------------------------------------------------------

#[derive(Clone, Copy)]
struct UserFile {
    ino: ext4_rs::Ino,
    pos: u64,
    write: bool,
    /// Written since the last commit.
    dirty: bool,
}

static mut FILES: [Option<UserFile>; MAX_OPEN] = [None; MAX_OPEN];
/// `systick::millis` of the last write to a program's file.
static mut LAST_WRITE_MS: u32 = 0;
/// [`USER_DIR`] made once per boot.
static mut USER_READY: bool = false;
/// A running app's root instead of [`USER_DIR`] (`ferrite/apps/{id}/data`, [`set_user_root`]).
static mut USER_ROOT: Option<String> = None;

/// Programs' files from now on under `root` (an app's `data/`), or [`USER_DIR`] with `None`.
/// Every file must be closed.
pub fn set_user_root(root: Option<String>) {
    // SAFETY: CPU0 only.
    unsafe {
        *(&raw mut USER_ROOT) = root;
        USER_READY = false;
    }
}

fn user_root() -> String {
    // SAFETY: CPU0 only.
    match unsafe { (*(&raw const USER_ROOT)).as_ref() } {
        Some(r) => lower(&format!("/{}", r)),
        None => path(DIR, USER_DIR),
    }
}

fn files() -> &'static mut [Option<UserFile>; MAX_OPEN] {
    // SAFETY: CPU0 only, never re-entered.
    unsafe { &mut *(&raw mut FILES) }
}

fn map_err(e: E) -> i32 {
    match e {
        E::NotFound => err::NOT_FOUND,
        E::Exists => err::EXISTS,
        E::NotEmpty => err::NOT_EMPTY,
        E::NoSpace | E::TooLarge => err::FULL,
        E::NotDir | E::IsDir => err::WRONG_KIND,
        E::BadName => err::BAD_PATH,
        e => {
            println!("ferrite: storage ERROR {}", e);
            err::IO
        }
    }
}

/// The file system with [`USER_DIR`] in place.
fn user() -> Result<&'static mut Fs, i32> {
    let fs = fs().ok_or(err::NO_STORAGE)?;
    if !unsafe { USER_READY } {
        let root = user_root();
        let mut at = String::new();
        for c in root.split('/').filter(|c| !c.is_empty()) {
            at.push('/');
            at.push_str(c);
            ensure_dir(fs, &at).map_err(map_err)?;
        }
        unsafe { USER_READY = true };
    }
    Ok(fs)
}

fn user_path(path: &Path) -> String {
    let mut s = user_root();
    for c in path.components() {
        s.push('/');
        s.push_str(c);
    }
    lower(&s)
}

/// What `path` is, and its inode.
fn find(fs: &mut Fs, path: &Path) -> Result<Option<(ext4_rs::Ino, Kind)>, i32> {
    let Some(ino) = fs.lookup(&user_path(path)).map_err(map_err)? else { return Ok(None) };
    let kind = match fs.metadata(ino).map_err(map_err)?.kind {
        FsKind::Dir => Kind::Dir,
        _ => Kind::File,
    };
    Ok(Some((ino, kind)))
}

fn is_open(ino: ext4_rs::Ino) -> bool {
    files().iter().flatten().any(|f| f.ino == ino)
}

/// Programs' files on USERDATA: zero-sized, over the statics above.
pub struct Ext4Storage;

impl Ext4Storage {
    fn file(slot: usize) -> Result<UserFile, i32> {
        files()[slot].ok_or(err::IO)
    }
}

impl StorageBackend for Ext4Storage {
    fn open(&self, slot: usize, path: &Path, mode: core_storage::Mode) -> Result<(), i32> {
        use core_storage::Mode as M;
        let fs = user()?;
        let found = find(fs, path)?;
        if let Some((ino, kind)) = found {
            if kind == Kind::Dir {
                return Err(err::WRONG_KIND);
            }
            if is_open(ino) {
                return Err(err::BUSY);
            }
        }
        let (ino, pos) = match (mode, found) {
            (M::Read, None) => return Err(err::NOT_FOUND),
            (M::Read, Some((ino, _))) => (ino, 0),
            (M::Write, Some((ino, _))) => {
                fs.set_len(ino, 0).map_err(map_err)?;
                (ino, 0)
            }
            (M::Append, Some((ino, _))) => (ino, fs.metadata(ino).map_err(map_err)?.size),
            (M::Write | M::Append, None) => (fs.create(&user_path(path)).map_err(map_err)?, 0),
        };
        let write = mode != M::Read;
        files()[slot] = Some(UserFile { ino, pos, write, dirty: write && (found.is_none() || mode == M::Write) });
        Ok(())
    }

    fn close(&self, slot: usize) -> Result<(), i32> {
        let f = files()[slot].take().ok_or(err::IO)?;
        if f.dirty {
            fs().ok_or(err::NO_STORAGE)?.sync().map_err(map_err)?;
        }
        Ok(())
    }

    fn read(&self, slot: usize, buf: &mut [u8]) -> Result<usize, i32> {
        let f = Self::file(slot)?;
        let n = fs().ok_or(err::NO_STORAGE)?.read_at(f.ino, f.pos, buf).map_err(map_err)?;
        if let Some(f) = files()[slot].as_mut() {
            f.pos += n as u64;
        }
        Ok(n)
    }

    fn write(&self, slot: usize, data: &[u8]) -> Result<usize, i32> {
        let f = Self::file(slot)?;
        if !f.write {
            return Err(err::BAD_PATH);
        }
        let n = fs().ok_or(err::NO_STORAGE)?.write_at(f.ino, f.pos, data).map_err(map_err)?;
        if n == 0 && !data.is_empty() {
            return Err(err::FULL);
        }
        if let Some(f) = files()[slot].as_mut() {
            f.pos += n as u64;
            f.dirty = true;
        }
        unsafe { LAST_WRITE_MS = systick::millis() };
        Ok(n)
    }

    fn seek(&self, slot: usize, pos: u32) -> Result<(), i32> {
        Self::file(slot)?;
        if let Some(f) = files()[slot].as_mut() {
            f.pos = u64::from(pos);
        }
        Ok(())
    }

    fn size(&self, slot: usize) -> Result<u32, i32> {
        let f = Self::file(slot)?;
        let size = fs().ok_or(err::NO_STORAGE)?.metadata(f.ino).map_err(map_err)?.size;
        Ok(size.min(u64::from(u32::MAX)) as u32)
    }

    fn kind(&self, path: &Path) -> Result<Option<Kind>, i32> {
        let fs = user()?;
        if path.is_root() {
            return Ok(Some(Kind::Dir));
        }
        Ok(find(fs, path)?.map(|(_, k)| k))
    }

    fn delete(&self, path: &Path) -> Result<(), i32> {
        let fs = user()?;
        match find(fs, path)? {
            Some((ino, Kind::File)) if is_open(ino) => Err(err::BUSY),
            Some((_, Kind::File)) => {
                fs.remove(&user_path(path)).map_err(map_err)?;
                fs.sync().map_err(map_err)
            }
            Some((_, Kind::Dir)) => Err(err::WRONG_KIND),
            None => Err(err::NOT_FOUND),
        }
    }

    fn make_dir(&self, path: &Path) -> Result<(), i32> {
        let fs = user()?;
        fs.mkdir(&user_path(path)).map_err(map_err)?;
        fs.sync().map_err(map_err)
    }

    fn remove_dir(&self, path: &Path) -> Result<(), i32> {
        let fs = user()?;
        match find(fs, path)? {
            Some((_, Kind::Dir)) if !path.is_root() => {
                fs.remove(&user_path(path)).map_err(map_err)?;
                fs.sync().map_err(map_err)
            }
            Some(_) => Err(err::WRONG_KIND),
            None => Err(err::NOT_FOUND),
        }
    }

    fn entry(&self, path: &Path, index: u32, name: &mut [u8; 12]) -> Result<Option<(usize, bool)>, i32> {
        let fs = user()?;
        let ino = if path.is_root() {
            fs.lookup(&user_path(path)).map_err(map_err)?.ok_or(err::NOT_FOUND)?
        } else {
            match find(fs, path)? {
                Some((ino, Kind::Dir)) => ino,
                Some(_) => return Err(err::WRONG_KIND),
                None => return Err(err::NOT_FOUND),
            }
        };
        let mut seen = 0;
        let mut found = None;
        fs.read_dir(ino, |e| {
            // Names a program could not have made (longer than 8.3, upper case) stay out of
            // its sight.
            if e.name.len() > name.len() || e.name.iter().any(u8::is_ascii_uppercase) {
                return true;
            }
            if seen == index {
                name[..e.name.len()].copy_from_slice(e.name);
                name[..e.name.len()].make_ascii_uppercase();
                found = Some((e.name.len(), e.kind == FsKind::Dir));
                return false;
            }
            seen += 1;
            true
        })
        .map_err(map_err)?;
        Ok(found)
    }

    fn flush(&self, force: bool) {
        let Some(fs) = fs() else { return };
        if !files().iter().flatten().any(|f| f.dirty) {
            return;
        }
        if !force && systick::millis().wrapping_sub(unsafe { LAST_WRITE_MS }) < USER_FLUSH_MS {
            return;
        }
        if let Err(e) = fs.sync() {
            println!("ferrite: storage flush ERROR {}", e);
        }
        for f in files().iter_mut().flatten() {
            f.dirty = false;
        }
    }
}
