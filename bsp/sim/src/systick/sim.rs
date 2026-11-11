use std::sync::OnceLock;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::time::Instant;

static START: OnceLock<Instant> = OnceLock::new();

/// Virtual clock (`--dump`): `millis` is `NOW`, advanced by `advance` once
/// per frame and by `delay_ms`, so a run is the same on every machine.
static VIRTUAL: AtomicBool = AtomicBool::new(false);
static NOW: AtomicU32 = AtomicU32::new(0);

pub fn init() {
    START.get_or_init(Instant::now);
}

/// Switch to the virtual clock (before the runtime starts).
pub fn set_virtual() {
    VIRTUAL.store(true, Ordering::Relaxed);
}

pub fn is_virtual() -> bool {
    VIRTUAL.load(Ordering::Relaxed)
}

/// Advance the virtual clock by `ms`.
pub fn advance(ms: u32) {
    NOW.fetch_add(ms, Ordering::Relaxed);
}

pub fn millis() -> u32 {
    if is_virtual() {
        return NOW.load(Ordering::Relaxed);
    }
    let start = START.get_or_init(Instant::now);
    start.elapsed().as_millis() as u32
}

pub fn delay_ms(ms: u32) {
    if is_virtual() {
        advance(ms);
        return;
    }
    std::thread::sleep(std::time::Duration::from_millis(ms as u64));
}
