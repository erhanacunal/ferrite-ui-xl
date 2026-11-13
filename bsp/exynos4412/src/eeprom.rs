//! The user EEPROM (`eepromRead` / `eepromWrite`…) as a file on CACHE,
//! `FERRITE/EEPROM.BIN` ([`FILE`], outside programs' storage tree): a RAM
//! copy of [`SIZE`] bytes read at boot ([`load`]), written back once writes
//! have been quiet for [`SAVE_MS`] and before a restart or power-off
//! (`storage::sync`). Erased bytes are 0xFF, as on a chip. Without CACHE the
//! EEPROM still works, for this boot only.
//!
//! `EepromImpl::flush` (once per main-loop pass) only notes nothing: the save
//! is timed here, so a program writing a byte a frame costs one file write
//! a second, not sixty.

use core::cell::UnsafeCell;

use exynos_4412_rs::println;
use ferrite_core::eeprom::EepromBackend;

use crate::{storage, systick};

/// Bytes (`eepromSize()`).
pub const SIZE: u16 = 4096;
pub const FILE: &str = "EEPROM.BIN";
/// Save this long after the last write.
const SAVE_MS: u32 = 500;

struct Mem(UnsafeCell<[u8; SIZE as usize]>);

// SAFETY: CPU0 only (the VM's builtins, frame_end and the reset paths).
unsafe impl Sync for Mem {}

static MEM: Mem = Mem(UnsafeCell::new([0xFF; SIZE as usize]));
static mut DIRTY: bool = false;
static mut LAST_WRITE_MS: u32 = 0;

fn mem() -> &'static mut [u8; SIZE as usize] {
    // SAFETY: see `Mem`.
    unsafe { &mut *MEM.0.get() }
}

/// Reads the file into the RAM copy. Call once, after `storage::init`. A
/// missing file (or one of another size) leaves the EEPROM erased.
pub fn load() {
    if !storage::available() {
        return;
    }
    let mut buf = alloc::vec![0u8; SIZE as usize];
    if storage::read_exact(FILE, &mut buf) {
        mem().copy_from_slice(&buf);
        println!("ferrite: EEPROM loaded from {}/{}", storage::DIR, FILE);
    }
}

/// Writes the RAM copy to the file if it changed: always when `force`, else
/// once writes have been quiet for [`SAVE_MS`].
pub fn save_if_dirty(force: bool) {
    unsafe {
        if !DIRTY || !storage::available() {
            return;
        }
        if !force && systick::millis().wrapping_sub(LAST_WRITE_MS) < SAVE_MS {
            return;
        }
        // Cleared first: a failed save is not retried every pass.
        DIRTY = false;
    }
    if !storage::save_named(FILE, mem()) {
        println!("ferrite: EEPROM save failed");
    }
}

/// The board's `Platform::EepromB`: zero-sized, over the RAM copy.
pub struct FileEeprom;

impl EepromBackend for FileEeprom {
    const SIZE: u16 = SIZE;

    fn read(&self, addr: u16, buf: &mut [u8]) -> bool {
        let a = addr as usize;
        buf.copy_from_slice(&mem()[a..a + buf.len()]);
        true
    }

    fn write(&self, addr: u16, data: &[u8]) -> bool {
        let a = addr as usize;
        mem()[a..a + data.len()].copy_from_slice(data);
        unsafe {
            DIRTY = true;
            LAST_WRITE_MS = systick::millis();
        }
        true
    }
}
