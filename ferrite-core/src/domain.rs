//! Loaded images (docs/apps.md, "Step 2"): domain 0 is the system image at
//! `Platform::FS_BASE` (never moved), domains 1..=[`MAX_APPS`] are running apps, each in
//! its own range of the flash region above the system image. Every VM carries its domain
//! (`Vm::domain`): which `Fs` its programs, resources and `F_READ` / `F_WRITE` see, which
//! EEPROM and which storage root.
//!
//! An app's EEPROM is a buffer here (not the board's), read from
//! `ferrite/apps/{id}/data/eeprom.bin` through `AppFiles` when the domain opens and saved
//! once writes settle ([`flush_eeproms`], from the runtime's pass and the BSP's restart
//! path), when it closes, and never into another app's file. The buffers are CPU0 statics
//! so that a BSP restart path, which has no `Ctx`, can save them.
extern crate alloc;

use alloc::string::String;
use alloc::vec::Vec;

use crate::apps::{AppFiles, AppInfo};
use crate::fs::Fs;

/// Apps running at once (the system image not counted).
pub const MAX_APPS: usize = 4;
/// The system image's domain.
pub const SYSTEM: u8 = 0;
/// App ranges start on this boundary.
pub const ALIGN: u32 = 64 << 10;
/// An app's EEPROM file, in its `data/`.
pub const EEPROM_FILE: &str = "eeprom.bin";
/// An app's EEPROM is saved this long after its last write.
pub const EEPROM_SETTLE_MS: u32 = 2000;

/// A running app's image.
pub struct AppDomain {
    pub info: AppInfo,
    /// Flash address of its FS header (`fs`'s base).
    pub base: u32,
    /// Bytes of the region kept for it from `base`.
    pub len: u32,
    pub fs: Fs,
    /// `ferrite/apps/{id}/data`: its storage root and its EEPROM file's directory.
    pub data: String,
    /// Unique per open (`StorageImpl::select_root` remembers the last one).
    pub serial: u32,
}

impl AppDomain {
    /// `addr..addr + len` as seen by the app (built for `fs_base`) moved into its range;
    /// None if it falls outside.
    pub fn flash_addr(&self, fs_base: u32, addr: u32, len: u32) -> Option<u32> {
        let off = addr.checked_sub(fs_base)?;
        let end = off.checked_add(len)?;
        (end <= self.len).then(|| self.base + off)
    }
}

pub struct Domains {
    apps: [Option<AppDomain>; MAX_APPS],
    next_serial: u32,
}

impl Default for Domains {
    fn default() -> Self {
        Self::new()
    }
}

impl Domains {
    pub const fn new() -> Self {
        Self { apps: [const { None }; MAX_APPS], next_serial: 1 }
    }

    /// App domain `d` (None for the system image or a closed one).
    pub fn get(&self, d: u8) -> Option<&AppDomain> {
        let i = usize::from(d).checked_sub(1)?;
        self.apps.get(i)?.as_ref()
    }

    /// The open app domains' ids.
    pub fn ids(&self) -> impl Iterator<Item = u8> + '_ {
        self.apps.iter().enumerate().filter(|(_, a)| a.is_some()).map(|(i, _)| i as u8 + 1)
    }

    /// The domain running app `id`.
    pub fn find(&self, id: &str) -> Option<u8> {
        self.ids().find(|&d| self.get(d).is_some_and(|a| a.info.id == id))
    }

    /// A free range of `len` bytes in `lo..hi` (first fit, [`ALIGN`]ed).
    pub fn place(&self, lo: u32, hi: u32, len: u32) -> Option<u32> {
        let mut used: Vec<(u32, u32)> = self.apps.iter().flatten().map(|a| (a.base, a.base + a.len)).collect();
        used.sort_unstable();
        let mut at = lo.next_multiple_of(ALIGN);
        for (start, end) in used {
            if at.checked_add(len)? <= start {
                break;
            }
            at = at.max(end.next_multiple_of(ALIGN));
        }
        (at.checked_add(len)? <= hi).then_some(at)
    }

    /// Opens a domain for `info`, its image mounted as `fs` at `base` (`len` bytes kept).
    /// None if [`MAX_APPS`] are open.
    pub fn open(&mut self, info: AppInfo, base: u32, len: u32, fs: Fs) -> Option<u8> {
        let i = self.apps.iter().position(Option::is_none)?;
        let data = alloc::format!("{}/{}", info.dir(), crate::apps::DATA_DIR);
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.apps[i] = Some(AppDomain { info, base, len, fs, data, serial });
        Some(i as u8 + 1)
    }

    /// Closes app domain `d`.
    pub fn close(&mut self, d: u8) -> Option<AppDomain> {
        let i = usize::from(d).checked_sub(1)?;
        self.apps.get_mut(i)?.take()
    }
}

/// Moves an image built for flash address `from` to `to`: its resource table's offsets
/// are absolute (`Fs`). The CRC no longer matches afterwards: check it before. False if
/// it is not an FS image.
pub fn rebase(image: &mut [u8], from: u32, to: u32) -> bool {
    const HEADER: usize = 16;
    const ENTRY: usize = 32;
    if image.len() < HEADER || &image[..4] != b"FERR" {
        return false;
    }
    let count = usize::from(u16::from_le_bytes([image[10], image[11]]));
    if image.len() < HEADER + count * ENTRY {
        return false;
    }
    for i in 0..count {
        let at = HEADER + i * ENTRY + 20;
        let off = u32::from_le_bytes([image[at], image[at + 1], image[at + 2], image[at + 3]]);
        let moved = off.wrapping_sub(from).wrapping_add(to);
        image[at..at + 4].copy_from_slice(&moved.to_le_bytes());
    }
    true
}

// --- app EEPROMs (CPU0 statics, see the module doc) -------------------------------------------

struct AppEeprom {
    /// `ferrite/apps/{id}/data`.
    dir: String,
    mem: Vec<u8>,
    dirty: bool,
    last_write_ms: u32,
}

struct Cpu0<T>(core::cell::UnsafeCell<T>);
// SAFETY: CPU0 only (the VM, the runtime, the BSP's restart path on CPU0).
unsafe impl<T> Sync for Cpu0<T> {}

static EEPROMS: Cpu0<[Option<AppEeprom>; MAX_APPS]> = Cpu0(core::cell::UnsafeCell::new([const { None }; MAX_APPS]));

fn eeproms() -> &'static mut [Option<AppEeprom>; MAX_APPS] {
    // SAFETY: CPU0 only; no reference outlives a call here.
    unsafe { &mut *EEPROMS.0.get() }
}

fn eeprom(d: u8) -> Option<&'static mut AppEeprom> {
    let i = usize::from(d).checked_sub(1)?;
    eeproms().get_mut(i)?.as_mut()
}

/// Domain `d`'s EEPROM (`size` bytes) from its file, erased (0xFF) where the file has none.
pub fn open_eeprom(d: u8, files: &dyn AppFiles, dir: &str, size: usize) {
    let Some(i) = usize::from(d).checked_sub(1).filter(|&i| i < MAX_APPS) else { return };
    let mut mem = alloc::vec![0xFFu8; size];
    if let Some(saved) = files.read(dir, EEPROM_FILE, size) {
        mem[..saved.len()].copy_from_slice(&saved);
    }
    eeproms()[i] = Some(AppEeprom { dir: String::from(dir), mem, dirty: false, last_write_ms: 0 });
}

/// Saves domain `d`'s EEPROM if written, and drops it.
pub fn close_eeprom(d: u8, files: &dyn AppFiles) {
    let Some(i) = usize::from(d).checked_sub(1).filter(|&i| i < MAX_APPS) else { return };
    if let Some(e) = eeproms()[i].take()
        && e.dirty
    {
        files.save(&e.dir, EEPROM_FILE, &e.mem);
    }
}

/// Reads `buf.len()` bytes at `addr` of domain `d`'s EEPROM.
pub fn eeprom_read(d: u8, addr: i32, buf: &mut [u8]) -> bool {
    let Some(e) = eeprom(d) else { return false };
    let Some(src) = usize::try_from(addr).ok().and_then(|a| e.mem.get(a..a + buf.len())) else {
        return false;
    };
    buf.copy_from_slice(src);
    true
}

/// Writes `data` at `addr` of domain `d`'s EEPROM.
pub fn eeprom_write(d: u8, addr: i32, data: &[u8], now_ms: u32) -> bool {
    let Some(e) = eeprom(d) else { return false };
    let Some(dst) = usize::try_from(addr).ok().and_then(|a| e.mem.get_mut(a..a + data.len())) else {
        return false;
    };
    if dst != data {
        dst.copy_from_slice(data);
        e.dirty = true;
        e.last_write_ms = now_ms;
    }
    true
}

/// Saves the app EEPROMs written [`EEPROM_SETTLE_MS`] ago (all written ones with `force`:
/// before a restart).
pub fn flush_eeproms(files: &dyn AppFiles, now_ms: u32, force: bool) {
    for e in eeproms().iter_mut().flatten() {
        if e.dirty && (force || now_ms.wrapping_sub(e.last_write_ms) >= EEPROM_SETTLE_MS) && files.save(&e.dir, EEPROM_FILE, &e.mem) {
            e.dirty = false;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn info(id: &str) -> AppInfo {
        AppInfo {
            id: id.into(),
            name: id.into(),
            version: 0x010000,
            entry: "main".into(),
            icon: String::new(),
            caps: 0,
        }
    }

    fn fs() -> Fs {
        let mut img = alloc::vec![0u8; 16];
        img[..4].copy_from_slice(b"FERR");
        img[4] = 3;
        let flash = crate::apps::SliceFlash { data: &img, base: 0 };
        Fs::mount(&crate::flash::FlashImpl::with_backend(flash), 0).ok().unwrap()
    }

    #[test]
    fn ranges_first_fit_and_reuse() {
        let mut d = Domains::new();
        let lo = 0x50_0123;
        let a = d.place(lo, 0x400_0000, 0x2_0000).unwrap();
        assert_eq!(a, 0x51_0000);
        let da = d.open(info("a"), a, 0x2_0000, fs()).unwrap();
        let b = d.place(lo, 0x400_0000, 0x1000).unwrap();
        assert_eq!(b, 0x53_0000);
        let db = d.open(info("b"), b, 0x1000, fs()).unwrap();
        assert_eq!((da, db), (1, 2));
        assert_eq!(d.find("b"), Some(2));
        d.close(da);
        // The first hole fits again; a big one goes after b.
        assert_eq!(d.place(lo, 0x400_0000, 0x1_0000), Some(0x51_0000));
        assert_eq!(d.place(lo, 0x400_0000, 0x3_0000), Some(0x54_0000));
        assert_eq!(d.place(lo, 0x54_0000, 0x3_0000), None);
        for n in ["c", "d", "e"] {
            assert!(d.open(info(n), 0, 0, fs()).is_some());
        }
        assert!(d.open(info("f"), 0, 0, fs()).is_none(), "four at most");
    }

    #[test]
    fn rebase_moves_offsets() {
        let mut img = alloc::vec![0u8; 16 + 32];
        img[..4].copy_from_slice(b"FERR");
        img[10] = 1;
        img[36..40].copy_from_slice(&0x1030u32.to_le_bytes());
        assert!(rebase(&mut img, 0x1000, 0x60_0000));
        assert_eq!(u32::from_le_bytes(img[36..40].try_into().unwrap()), 0x60_0030);
        let a = AppDomain { info: info("a"), base: 0x60_0000, len: 0x100, fs: fs(), data: String::new(), serial: 1 };
        assert_eq!(a.flash_addr(0x1000, 0x1010, 0x10), Some(0x60_0010));
        assert_eq!(a.flash_addr(0x1000, 0x10f8, 0x10), None);
        assert_eq!(a.flash_addr(0x1000, 0xff0, 1), None);
    }

    #[test]
    fn app_eeprom_reads_saves_and_settles() {
        let files = crate::apps::MemAppFiles::default();
        files.save("ferrite/apps/a/data", EEPROM_FILE, &[1, 2]);
        open_eeprom(3, &files, "ferrite/apps/a/data", 8);
        let mut b = [0u8; 3];
        assert!(eeprom_read(3, 0, &mut b));
        assert_eq!(b, [1, 2, 0xFF]);
        assert!(!eeprom_read(3, 6, &mut b), "past the end");
        assert!(eeprom_write(3, 2, &[9], 100));
        flush_eeproms(&files, 100 + EEPROM_SETTLE_MS - 1, false);
        assert_eq!(files.read("ferrite/apps/a/data", EEPROM_FILE, 8).unwrap(), [1, 2]);
        flush_eeproms(&files, 100 + EEPROM_SETTLE_MS, false);
        assert_eq!(files.read("ferrite/apps/a/data", EEPROM_FILE, 8).unwrap()[..3], [1, 2, 9]);
        assert!(eeprom_write(3, 0, &[7], 200));
        close_eeprom(3, &files);
        assert_eq!(files.read("ferrite/apps/a/data", EEPROM_FILE, 8).unwrap()[0], 7);
        assert!(!eeprom_read(3, 0, &mut b), "closed");
    }
}
