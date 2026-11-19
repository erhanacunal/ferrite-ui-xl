//! Loaded images (docs/apps.md, "Step 2"): domain 0 is the system image at
//! `Platform::FS_BASE` (never moved), domains 1..=[`MAX_APPS`] are running apps, each in
//! its own range of the flash region above the system image. Every VM carries its domain
//! (`Vm::domain`): which `Fs` its programs, resources and `F_READ` / `F_WRITE` see, which
//! EEPROM and which storage root.
//!
//! Font and image ids are `u8`s each image numbers on its own, while widgets, the font and
//! image lists and the renderer know one global id each. The system image keeps its ids; an
//! app's are mapped to free global ids when it opens ([`IdMaps`], [`Domains::assign_ids`])
//! and the VM translates at its boundary (`W_SET` / `W_GET` of the font and image props,
//! the draw builtins).
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

/// An app's font and image ids → global ids (0 = none: the embedded font, no image).
#[derive(Clone)]
pub struct IdMaps {
    pub fonts: [u8; 256],
    pub images: [u8; 256],
}

impl Default for IdMaps {
    fn default() -> Self {
        Self { fonts: [0; 256], images: [0; 256] }
    }
}

impl IdMaps {
    /// The global ids these maps use, one flag each (fonts, images).
    pub fn used(&self) -> ([bool; 256], [bool; 256]) {
        let mut f = [false; 256];
        let mut i = [false; 256];
        for &g in self.fonts.iter().filter(|&&g| g != 0) {
            f[usize::from(g)] = true;
        }
        for &g in self.images.iter().filter(|&&g| g != 0) {
            i[usize::from(g)] = true;
        }
        (f, i)
    }
}

/// Global `id`'s local id in `map` (0 if it is none of the app's).
pub fn local_of(map: &[u8; 256], id: u8) -> u8 {
    if id == 0 {
        return 0;
    }
    map.iter().position(|&g| g == id).map_or(0, |l| l as u8)
}

/// Highest global id handed to an app (255 is no font id: a V1 font's "none").
const TOP_ID: u8 = 254;

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
    /// Its font and image ids → global ids.
    pub ids: IdMaps,
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

    /// Domain `d`'s font id `local` as the global one (the system image's as it is).
    pub fn font(&self, d: u8, local: u8) -> u8 {
        self.get(d).map_or(local, |a| a.ids.fonts[usize::from(local)])
    }

    /// Domain `d`'s image id `local` as the global one.
    pub fn image(&self, d: u8, local: u8) -> u8 {
        self.get(d).map_or(local, |a| a.ids.images[usize::from(local)])
    }

    /// Global font id `global` as domain `d` numbers it (0 if not one of its fonts).
    pub fn font_local(&self, d: u8, global: u8) -> u8 {
        self.get(d).map_or(global, |a| local_of(&a.ids.fonts, global))
    }

    /// Global image id `global` as domain `d` numbers it (0 if not one of its images).
    pub fn image_local(&self, d: u8, global: u8) -> u8 {
        self.get(d).map_or(global, |a| local_of(&a.ids.images, global))
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

    /// Global ids for an app holding the font / image ids `fonts` / `images` flags: free
    /// ones (from [`TOP_ID`] down) that neither the system image (`system_*`) nor another
    /// open app uses. None if there are not enough.
    pub fn assign_ids(
        &self,
        fonts: &[bool; 256],
        images: &[bool; 256],
        system_fonts: &[bool; 256],
        system_images: &[bool; 256],
    ) -> Option<IdMaps> {
        let mut taken_f = *system_fonts;
        let mut taken_i = *system_images;
        for app in self.apps.iter().flatten() {
            let (f, i) = app.ids.used();
            for g in 0..256 {
                taken_f[g] |= f[g];
                taken_i[g] |= i[g];
            }
        }
        let mut maps = IdMaps::default();
        for (wanted, taken, map) in [(fonts, &mut taken_f, &mut maps.fonts), (images, &mut taken_i, &mut maps.images)] {
            let mut next = TOP_ID;
            for local in 1..256 {
                if !wanted[local] {
                    continue;
                }
                while taken[usize::from(next)] {
                    next = next.checked_sub(1).filter(|&n| n > 0)?;
                }
                map[local] = next;
                taken[usize::from(next)] = true;
            }
        }
        Some(maps)
    }

    /// Opens a domain for `info`, its image mounted as `fs` at `base` (`len` bytes kept),
    /// its ids mapped by `ids`. None if [`MAX_APPS`] are open.
    pub fn open(&mut self, info: AppInfo, base: u32, len: u32, fs: Fs, ids: IdMaps) -> Option<u8> {
        let i = self.apps.iter().position(Option::is_none)?;
        let data = alloc::format!("{}/{}", info.dir(), crate::apps::DATA_DIR);
        let serial = self.next_serial;
        self.next_serial = self.next_serial.wrapping_add(1).max(1);
        self.apps[i] = Some(AppDomain { info, base, len, fs, data, serial, ids });
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
        let da = d.open(info("a"), a, 0x2_0000, fs(), IdMaps::default()).unwrap();
        let b = d.place(lo, 0x400_0000, 0x1000).unwrap();
        assert_eq!(b, 0x53_0000);
        let db = d.open(info("b"), b, 0x1000, fs(), IdMaps::default()).unwrap();
        assert_eq!((da, db), (1, 2));
        assert_eq!(d.find("b"), Some(2));
        d.close(da);
        // The first hole fits again; a big one goes after b.
        assert_eq!(d.place(lo, 0x400_0000, 0x1_0000), Some(0x51_0000));
        assert_eq!(d.place(lo, 0x400_0000, 0x3_0000), Some(0x54_0000));
        assert_eq!(d.place(lo, 0x54_0000, 0x3_0000), None);
        for n in ["c", "d", "e"] {
            assert!(d.open(info(n), 0, 0, fs(), IdMaps::default()).is_some());
        }
        assert!(d.open(info("f"), 0, 0, fs(), IdMaps::default()).is_none(), "four at most");
    }

    #[test]
    fn rebase_moves_offsets() {
        let mut img = alloc::vec![0u8; 16 + 32];
        img[..4].copy_from_slice(b"FERR");
        img[10] = 1;
        img[36..40].copy_from_slice(&0x1030u32.to_le_bytes());
        assert!(rebase(&mut img, 0x1000, 0x60_0000));
        assert_eq!(u32::from_le_bytes(img[36..40].try_into().unwrap()), 0x60_0030);
        let a = AppDomain {
            info: info("a"),
            base: 0x60_0000,
            len: 0x100,
            fs: fs(),
            data: String::new(),
            serial: 1,
            ids: IdMaps::default(),
        };
        assert_eq!(a.flash_addr(0x1000, 0x1010, 0x10), Some(0x60_0010));
        assert_eq!(a.flash_addr(0x1000, 0x10f8, 0x10), None);
        assert_eq!(a.flash_addr(0x1000, 0xff0, 1), None);
    }

    #[test]
    fn ids_avoid_the_system_and_other_apps() {
        let flags = |ids: &[u8]| {
            let mut f = [false; 256];
            for &i in ids {
                f[usize::from(i)] = true;
            }
            f
        };
        let mut d = Domains::new();
        let sys_f = flags(&[1, 2, 254]);
        let sys_i = flags(&[1, 253]);
        let a = d.assign_ids(&flags(&[1, 3]), &flags(&[1, 2]), &sys_f, &sys_i).unwrap();
        assert_eq!((a.fonts[1], a.fonts[3], a.fonts[0]), (253, 252, 0));
        assert_eq!((a.images[1], a.images[2]), (254, 252));
        assert_eq!((local_of(&a.fonts, 252), local_of(&a.fonts, 2), local_of(&a.images, 0)), (3, 0, 0));
        d.open(info("a"), 0, 0, fs(), a).unwrap();
        let b = d.assign_ids(&flags(&[1]), &flags(&[]), &sys_f, &sys_i).unwrap();
        assert_eq!(b.fonts[1], 251, "past the system's and a's");
        let all = flags(&(1..=254).collect::<Vec<u8>>());
        assert!(d.assign_ids(&flags(&[1]), &flags(&[]), &all, &sys_i).is_none());
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
