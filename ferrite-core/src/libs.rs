//! Library packages (docs/apps.md "Step 3"): an FS image with a
//! `RES_LIBINFO` manifest and one program, `lib`, installed under
//! `ferrite/libs/{id}/` on its own and loaded when a program that calls it
//! starts.
//!
//! ```text
//! ferrite/libs/{id}/lib.img      the package as uploaded (checked first)
//! ferrite/libs/{id}/libinfo.bin  its manifest, for listing without reading lib.img
//! ```
//!
//! A loaded library ([`LibImage`]) is shared: while any VM uses it, the next
//! program asking for the same id and major gets the same code (`REGISTRY`
//! holds weak references, so it goes once nobody uses it). The manifest is
//! the app manifest's layout with magic "LI" (ferrite-ui `ferrite_build.py`
//! `build_libinfo`); its errors are `apps::err` codes.
extern crate alloc;

use alloc::format;
use alloc::string::String;
use alloc::sync::{Arc, Weak};
use alloc::vec::Vec;

use crate::apps::{AppFiles, AppInfo, SliceFlash, err, parse_manifest, valid_id};
use crate::flash::FlashImpl;
use crate::fs::{Fs, RES_LIBINFO, RES_PROGRAM};
use crate::vm::LibImage;

pub const LIBS_DIR: &str = "ferrite/libs";
pub const IMAGE_FILE: &str = "lib.img";
pub const INFO_FILE: &str = "libinfo.bin";
pub const LIBINFO_NAME: &[u8] = b"libinfo";
const LIBINFO_MAGIC: &[u8; 2] = b"LI";
/// A library's one program.
pub const LIB_PROGRAM: &[u8] = b"lib";

pub fn lib_dir(id: &str) -> String {
    format!("{}/{}", LIBS_DIR, id)
}

/// Parses a library manifest record.
pub fn parse_libinfo(rec: &[u8]) -> Result<AppInfo, u8> {
    parse_manifest(rec, LIBINFO_MAGIC)
}

/// Whether `image` is a library package (an FS image with a `libinfo`;
/// [`check`] says whether a good one).
pub fn is_library(image: &[u8], base: u32) -> bool {
    let flash = FlashImpl::with_backend(SliceFlash { data: image, base });
    Fs::mount(&flash, base).is_ok_and(|fs| fs.find_kind(&flash, LIBINFO_NAME, RES_LIBINFO).is_some())
}

/// Checks a library package as the board would load it at `base` with
/// `caps`: a whole FS image with a good CRC, a valid manifest, nothing needed
/// that the firmware lacks, a `lib` program that is a library. Its manifest
/// record, manifest and loaded code.
pub fn check(image: &[u8], base: u32, caps: u32) -> Result<([u8; crate::apps::APPINFO_SIZE], AppInfo, LibImage), u8> {
    let flash = FlashImpl::with_backend(SliceFlash { data: image, base });
    let fs = Fs::mount(&flash, base).map_err(|_| err::BAD_IMAGE)?;
    if !fs.verify(&flash) {
        return Err(err::BAD_IMAGE);
    }
    let entry = fs.find_kind(&flash, LIBINFO_NAME, RES_LIBINFO).ok_or(err::NO_APPINFO)?;
    let mut rec = [0u8; crate::apps::APPINFO_SIZE];
    if (entry.size as usize) < rec.len() {
        return Err(err::NO_APPINFO);
    }
    fs.read_resource(&flash, &entry, 0, &mut rec);
    let info = parse_libinfo(&rec)?;
    if info.caps & !caps != 0 {
        return Err(err::CAPS);
    }
    let prog = fs.find_kind(&flash, LIB_PROGRAM, RES_PROGRAM).ok_or(err::NO_ENTRY)?;
    let mut code = Vec::new();
    code.try_reserve_exact(prog.size as usize).map_err(|_| err::TOO_LARGE)?;
    code.resize(prog.size as usize, 0);
    fs.read_resource(&flash, &prog, 0, &mut code);
    let lib = LibImage::parse(&info.id, info.version, code).ok_or(err::BAD_IMAGE)?;
    Ok((rec, info, lib))
}

/// Checks `image` ([`check`]) and stores it as `libs/{id}/lib.img`. Whether
/// a library with this id was installed before. Programs running keep the
/// version they loaded; the next one to start gets this one.
pub fn install(files: &dyn AppFiles, image: &[u8], base: u32, caps: u32) -> Result<(AppInfo, bool), u8> {
    let (rec, info, _) = check(image, base, caps)?;
    if !files.available() {
        return Err(err::NO_STORAGE);
    }
    let dir = lib_dir(&info.id);
    let replaced = files.list(&dir).is_some();
    forget();
    if !(files.make_dirs(&dir) && files.save(&dir, IMAGE_FILE, image) && files.save(&dir, INFO_FILE, &rec)) {
        return Err(err::WRITE);
    }
    Ok((info, replaced))
}

/// The installed libraries, sorted by id.
pub fn list(files: &dyn AppFiles) -> Vec<AppInfo> {
    if !files.available() {
        return Vec::new();
    }
    let mut out: Vec<AppInfo> = files
        .list(LIBS_DIR)
        .unwrap_or_default()
        .into_iter()
        .filter(|(name, is_dir)| *is_dir && valid_id(name))
        .filter_map(|(id, _)| {
            let rec = files.read(&lib_dir(&id), INFO_FILE, crate::apps::APPINFO_SIZE)?;
            parse_libinfo(&rec).ok().filter(|i| i.id == id)
        })
        .collect();
    out.sort_by(|a, b| a.id.cmp(&b.id));
    out
}

/// Deletes library `id` (programs using it keep their loaded copy).
pub fn remove(files: &dyn AppFiles, id: &str) -> Result<(), u8> {
    if !valid_id(id) {
        return Err(err::BAD_ID);
    }
    if !files.available() {
        return Err(err::NO_STORAGE);
    }
    let dir = lib_dir(id);
    if files.list(&dir).is_none() {
        return Err(err::NOT_INSTALLED);
    }
    forget();
    crate::apps::remove_tree(files, dir)
}

/// Single-core state (CPU0: the runtime and the BSP's network service between passes).
struct Cpu0<T>(core::cell::UnsafeCell<T>);
// SAFETY: touched by CPU0 only.
unsafe impl<T> Sync for Cpu0<T> {}

/// Libraries loaded by a running VM (weak: dropped with their last user).
static REGISTRY: Cpu0<Vec<Weak<LibImage>>> = Cpu0(core::cell::UnsafeCell::new(Vec::new()));

fn registry() -> &'static mut Vec<Weak<LibImage>> {
    // SAFETY: CPU0 only, and no reference outlives a call here.
    unsafe { &mut *REGISTRY.0.get() }
}

/// An install or remove: the next program loads from storage again.
fn forget() {
    registry().clear();
}

/// Libraries loaded now (for the memory report): (id, version, code bytes).
pub fn loaded() -> Vec<(String, u32, usize)> {
    registry()
        .iter()
        .filter_map(Weak::upgrade)
        .map(|l| (l.id.clone(), l.version, l.code_len()))
        .collect()
}

/// Library `id` in major version `major` for a starting program: the one
/// already loaded, else read from storage. `apps::err::LIB_MISSING` when no
/// such library (or major) is installed; other `apps::err` codes for a
/// package that fails its check.
pub fn load(files: &dyn AppFiles, id: &str, major: u8, base: u32, caps: u32, max: usize) -> Result<Arc<LibImage>, u8> {
    let reg = registry();
    reg.retain(|w| w.strong_count() > 0);
    if let Some(l) = reg.iter().filter_map(Weak::upgrade).find(|l| l.id == id && l.major() == major) {
        return Ok(l);
    }
    if !valid_id(id) || !files.available() || files.list(&lib_dir(id)).is_none() {
        return Err(err::LIB_MISSING);
    }
    let image = files.read(&lib_dir(id), IMAGE_FILE, max).ok_or(err::TOO_LARGE)?;
    let (_, info, lib) = check(&image, base, caps)?;
    if info.id != id {
        return Err(err::BAD_ID);
    }
    if lib.major() != major {
        return Err(err::LIB_MISSING);
    }
    let lib = Arc::new(lib);
    registry().push(Arc::downgrade(&lib));
    Ok(lib)
}
