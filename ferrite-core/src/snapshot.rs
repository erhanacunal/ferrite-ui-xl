//! Render-owned property reads (plan, step 3 preparation: read-only model).
//!
//! Once rendering runs on its own core, some values a program can `get` will
//! belong to the render core: a property under a running animation (tweened
//! there), a scroll container's `scroll_y` (inertial scrolling there). The VM
//! then reads them from a [`Snapshot`] the render core publishes after each
//! render pass: at most one frame old.
//!
//! On one core the snapshot is taken in the runtime's render phase
//! (`runtime::snapshot_fill`) while [`TRACK`] is on, and `OP_W_GET` goes
//! through [`Snapshot::read`]: it counts reads of render-owned values
//! ([`READS`]), and with [`STALE`] it answers them from the snapshot, as the
//! render core will ([`STALE_DIFF`] = answers that differ from the live
//! value). Programs that break with it on would break in step 3. Sim:
//! `FERRITE_SIM_STALE_READS=1` (`golden.py --stale`).
//!
//! Classes of `W_GET` reads (plan): A = owned by the VM (almost every prop,
//! flex results included), B = derived on the VM core (absolute rects,
//! visibility, text measurement; no `W_GET` prop yet), C = owned by the
//! render core (this module). Which (widget, prop) pairs are C is decided per
//! render pass, not per prop id: `loc_x` is A until an animation runs on it.

use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::collections::BTreeMap;

use crate::widget::WidgetId;

/// Take a snapshot after each render pass and count render-owned reads.
pub static TRACK: AtomicBool = AtomicBool::new(false);
/// Answer render-owned reads from the snapshot (implies tracking).
pub static STALE: AtomicBool = AtomicBool::new(false);

/// `W_GET`s of a render-owned value; those answered from the snapshot with a
/// value differing from the live one.
pub static READS: AtomicU32 = AtomicU32::new(0);
pub static STALE_DIFF: AtomicU32 = AtomicU32::new(0);

#[inline]
pub fn tracking() -> bool {
    TRACK.load(Ordering::Relaxed) || STALE.load(Ordering::Relaxed)
}

#[inline]
pub fn stale() -> bool {
    STALE.load(Ordering::Relaxed)
}

/// Render-owned values as of the last render pass, by (widget, prop).
#[derive(Default)]
pub struct Snapshot {
    vals: BTreeMap<(WidgetId, u8), i32>,
}

impl Snapshot {
    pub const fn new() -> Self {
        Snapshot { vals: BTreeMap::new() }
    }

    pub fn clear(&mut self) {
        self.vals.clear();
    }

    /// Record a render-owned value.
    pub fn put(&mut self, id: WidgetId, prop: u8, v: i32) {
        self.vals.insert((id, prop), v);
    }

    pub fn get(&self, id: WidgetId, prop: u8) -> Option<i32> {
        self.vals.get(&(id, prop)).copied()
    }

    pub fn len(&self) -> usize {
        self.vals.len()
    }

    pub fn is_empty(&self) -> bool {
        self.vals.is_empty()
    }

    /// What `W_GET` answers for (`id`, `prop`) whose live value is `live`.
    #[inline]
    pub fn read(&self, id: WidgetId, prop: u8, live: i32) -> i32 {
        if self.vals.is_empty() {
            return live;
        }
        self.read_owned(id, prop, live)
    }

    #[inline(never)]
    fn read_owned(&self, id: WidgetId, prop: u8, live: i32) -> i32 {
        let Some(snap) = self.get(id, prop) else {
            return live;
        };
        READS.fetch_add(1, Ordering::Relaxed);
        if !stale() {
            return live;
        }
        if snap != live {
            STALE_DIFF.fetch_add(1, Ordering::Relaxed);
        }
        snap
    }
}
