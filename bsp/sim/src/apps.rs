//! App packages in the sim (ferrite-core `apps`, docs/apps.md). With `--storage DIR` they
//! live in `DIR/ferrite/apps/{id}/` (kept across runs; the system image's programs see that
//! directory too, unlike on the board), else in RAM. `--install PKG` installs a package at
//! start (repeatable), `--app ID` starts an installed app instead of the system image.
//!
//! A running app's files and EEPROM: `DIR/ferrite/apps/{id}/data/` (`storage.rs`,
//! `eeprom.rs`); without `--storage` its EEPROM starts erased and its files share the RAM
//! tree.

use std::fs;
use std::path::PathBuf;
use std::sync::OnceLock;

use ferrite_core::apps::{AppFiles, MemAppFiles};
use ferrite_core::flash::FlashImpl;

use crate::flash::sim::FileFlash;

/// `AppFiles` over a host directory.
pub struct HostAppFiles {
    root: PathBuf,
}

impl HostAppFiles {
    fn at(&self, dir: &str) -> PathBuf {
        dir.split('/').filter(|c| !c.is_empty()).fold(self.root.clone(), |p, c| p.join(c))
    }
}

impl AppFiles for HostAppFiles {
    fn available(&self) -> bool {
        true
    }
    fn read(&self, dir: &str, name: &str, max: usize) -> Option<Vec<u8>> {
        let p = self.at(dir).join(name);
        (fs::metadata(&p).ok()?.len() as usize <= max).then(|| fs::read(&p).ok()).flatten()
    }
    fn save(&self, dir: &str, name: &str, data: &[u8]) -> bool {
        let d = self.at(dir);
        let tmp = d.join(format!("{name}.new"));
        fs::create_dir_all(&d).is_ok() && fs::write(&tmp, data).is_ok() && fs::rename(&tmp, d.join(name)).is_ok()
    }
    fn make_dirs(&self, dir: &str) -> bool {
        fs::create_dir_all(self.at(dir)).is_ok()
    }
    fn list(&self, dir: &str) -> Option<Vec<(String, bool)>> {
        let rd = fs::read_dir(self.at(dir)).ok()?;
        let mut out: Vec<(String, bool)> = rd
            .flatten()
            .filter_map(|e| Some((e.file_name().into_string().ok()?, e.file_type().ok()?.is_dir())))
            .collect();
        out.sort();
        Some(out)
    }
    fn delete(&self, dir: &str, name: &str) -> bool {
        fs::remove_file(self.at(dir).join(name)).is_ok()
    }
    fn remove_dir(&self, dir: &str) -> bool {
        fs::remove_dir(self.at(dir)).is_ok()
    }
}

enum Files {
    Host(HostAppFiles),
    Mem(MemAppFiles),
}

struct SimApps {
    files: Files,
    /// `--storage DIR`.
    storage: Option<PathBuf>,
}

// SAFETY: the sim's runtime runs on one thread (the window's); nothing else touches it.
unsafe impl Sync for SimApps {}
unsafe impl Send for SimApps {}

static APPS: OnceLock<SimApps> = OnceLock::new();
static BOOT_APP: OnceLock<Option<String>> = OnceLock::new();

/// Call once at start: where apps live, the boot app, packages to install.
pub fn init(storage: Option<&str>, boot_app: Option<String>, install: &[String], base: u32, caps: u32) {
    let storage = storage.map(PathBuf::from);
    let store = match &storage {
        Some(root) => Files::Host(HostAppFiles { root: root.clone() }),
        None => Files::Mem(MemAppFiles::default()),
    };
    let _ = APPS.set(SimApps { files: store, storage });
    let _ = BOOT_APP.set(boot_app);
    for pkg in install {
        match fs::read(pkg) {
            Ok(image) => match ferrite_core::apps::install(files(), &image, base, caps) {
                Ok(done) => println!("app: installed {} {}", done.info.id, done.info.version_text()),
                Err(code) => eprintln!("--install {pkg}: {}", ferrite_core::apps::err::text(code)),
            },
            Err(e) => eprintln!("--install {pkg}: {e}"),
        }
    }
}

pub fn files() -> &'static dyn AppFiles {
    match APPS.get().map(|a| &a.files) {
        Some(Files::Host(h)) => h,
        Some(Files::Mem(m)) => m,
        None => &ferrite_core::apps::NoAppFiles,
    }
}

pub fn boot_app() -> Option<String> {
    BOOT_APP.get().cloned().flatten()
}

/// `PlatformRuntime::place_image`: an app's image at `addr` of the flash (`len` bytes kept),
/// or that range erased again.
pub fn place_image(flash: &FlashImpl<FileFlash>, addr: u32, len: u32, image: Option<&[u8]>) -> bool {
    let end = addr as usize + len as usize;
    if end > crate::flash::sim::FLASH_SIZE || image.is_some_and(|i| i.len() > len as usize) {
        return false;
    }
    let mut at = addr as usize & !4095;
    while at < end {
        flash.erase_sector(at as u32);
        at += 4096;
    }
    for (i, chunk) in image.unwrap_or(&[]).chunks(4096).enumerate() {
        flash.write(addr + (i * 4096) as u32, chunk);
    }
    true
}
