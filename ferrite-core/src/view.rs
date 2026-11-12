//! The render pool (plan step 3b): the renderer works on its own copy of the
//! widget tree, `Ctx::view`, never on the VM's pool.
//!
//! Before each render (`render::with_view`) the VM pool hands over
//! ([`ObjPool::sync_view`]):
//! - the scene (`scene.rs`): widgets plus referenced text/graph data; no VM
//!   object arena or pointer table is copied. The render pool keeps it: the
//!   first sync captures the whole tree, later ones repack only the objects
//!   the VM pool touched since (`ObjPool::touched`);
//! - the dirty bits (then cleared in the VM pool), `maybe_dirty` and the
//!   changed list;
//! - the [`RenderMsg`]s queued since the last sync, replayed in order on the
//!   render pool's render-owned state (`RenderState`, `layers`);
//! - the `on_paint` lists recorded since then;
//! - with a render context, the VM's drawing ([`Draw`]: its direct drawing
//!   and its `on_paint` drawing, recorded on the VM's core), replayed by the
//!   next job (`render::run_job`).
//!
//! The render pool keeps everything the renderer owns (`RenderState`: records,
//! epochs, display lists, geometry; the layers' activity), and its own pending
//! dirty bits across syncs. After the render the VM pool's `layers` becomes a
//! mirror of the render pool's (`mirror_layers`), for the VM's placement
//! decisions (`is_active_layer_root`).
//!
//! On one core, `render::with_view` swaps the two pools around the render, so
//! the render code still reads `ctx.objs`. Step 3c moves the render pool to
//! another core.

use alloc::vec::Vec;

use crate::layer::{Layers, Scope};
use crate::pool::{CHANGED_MAX, ObjPool, ObjPtr};
use crate::types::{Offset, Rect};
use crate::widget::{FLAG_DIRTY, FLAG_HAS_ON_PAINT, RenderRec, WidgetId};

/// Drawing the VM recorded for the render side (plan step 3d), in the order
/// it happened.
pub(crate) enum Draw {
    /// Drawing builtins outside `on_paint`, replayed on the screen before
    /// the render (they ran before it).
    Screen(crate::dl::List),
    /// A widget's `on_paint` drawing (screen coordinates, no scissor): into
    /// its capture before the render if the board renderer takes it, else on
    /// the screen after the render (`render::run_job`).
    Paint(WidgetId, crate::dl::List),
}

/// What the VM pool did to render-owned state, replayed on the render pool.
#[derive(Clone, Copy)]
pub(crate) enum RenderMsg {
    /// A widget was allocated at this pointer: forget the old record.
    Born(ObjPtr),
    /// `will_move`: the widget's on-screen rect before a move (taken if it
    /// is drawn and has no pending previous rect).
    Moved(WidgetId, Rect),
    /// `set(layer, n)`.
    SetLayer(WidgetId, i32),
    /// `set(layer_alpha, a)`.
    LayerAlpha(WidgetId, i32),
    /// An active layer root was moved / shown / hidden.
    Placed,
    /// Something drew over the screen outside the renderer.
    Touched,
    /// Every object dropped (program switch).
    Clear,
}

impl ObjPool {
    /// This is the render pool.
    #[inline]
    pub fn is_view(&self) -> bool {
        self.is_view
    }

    /// Queue `m` for the render pool (the render pool applies its own changes
    /// directly).
    #[inline]
    pub(crate) fn send(&mut self, m: RenderMsg) {
        if self.is_view {
            return;
        }
        if matches!(m, RenderMsg::Touched) && matches!(self.render.msgs.last(), Some(RenderMsg::Touched)) {
            return;
        }
        self.render.msgs.push(m);
    }

    /// The VM pool has something for the render pool.
    pub(crate) fn needs_sync(&self) -> bool {
        self.render.maybe_dirty
            || !self.render.msgs.is_empty()
            || !self.render.paints.borrow().is_empty()
            || !self.render.draws.is_empty()
            || self.render.screen_rec.busy()
    }

    /// The canvas the VM's direct drawing records into (`render::records`):
    /// a recording against `lcd` is started on first use.
    pub(crate) fn screen_rec<B: crate::lcd::LcdBackend>(&self, lcd: &crate::lcd::LcdImpl<B>) -> &crate::dl::Recorder {
        let rec = &self.render.screen_rec;
        if !rec.busy() {
            rec.begin(lcd);
        }
        rec
    }

    /// Closes the direct-drawing recording into the queue (before an
    /// `on_paint` recording, at a sync), keeping the order.
    pub(crate) fn close_screen_rec(&mut self) {
        let rec = &self.render.screen_rec;
        if !rec.busy() {
            return;
        }
        rec.end();
        let list = rec.list().compact();
        if !list.ops.is_empty() {
            self.render.draws.push(Draw::Screen(list));
        }
    }

    /// Live widgets with a dirty bit.
    fn dirty_widgets(&self) -> Vec<WidgetId> {
        let mut out = Vec::new();
        for p in 0..self.ptr_count() as ObjPtr {
            let id = WidgetId(p);
            if self.is_live(id) && self.get(id).is_dirty() {
                out.push(id);
            }
        }
        out
    }

    /// Clear the dirty bits handed over: of the touched widgets (a dirty bit
    /// is set through `get_mut`, which notes the widget), or of every widget
    /// when not all of those are known.
    fn clear_synced_dirty(&mut self) {
        use crate::pool::T_WIDGET;
        let mut clear = |pool: &mut ObjPool, p: ObjPtr| {
            if let Some(w) = pool.payload_mut_quiet(p, T_WIDGET) {
                crate::widget::header_mut(w).clear_dirty();
            }
        };
        if self.touched_over() {
            for p in 0..self.ptr_count() as ObjPtr {
                clear(self, p);
            }
        } else {
            for i in 0..self.touched().len() {
                let p = self.touched()[i];
                clear(self, p);
            }
        }
    }

    /// Hand the tree and everything queued for the renderer over to the
    /// render pool `view` (self = the VM pool).
    pub(crate) fn sync_view(&mut self, view: &mut ObjPool) {
        debug_assert!(!self.is_view && view.is_view);
        self.close_screen_rec();
        view.render.draws.append(&mut self.render.draws);
        let mut msgs = core::mem::take(&mut self.render.msgs);
        let cleared = msgs.iter().any(|m| matches!(m, RenderMsg::Clear));
        let t0 = crate::perf::now();
        // The whole tree: the first time, when its shape changed (the scene
        // is packed in paint order), when `view` missed a sync, when more
        // was touched than noted, or to drop the packed data's garbage.
        let full = self.render.shape_changed
            || cleared
            || self.touched_over()
            || view.render.scene.generation != Some(self.sync_gen)
            || view.render.scene.wasteful()
            || !crate::scene::INCREMENTAL.load(core::sync::atomic::Ordering::Relaxed);
        let mut repacked = 0;
        if full {
            // The render pool's own pending dirty bits survive the capture.
            let keep = if view.render.maybe_dirty && !cleared { view.dirty_widgets() } else { Vec::new() };
            self.sync_view_metadata(view);
            view.render.scene.capture(self);
            for &id in &keep {
                if view.is_live(id) {
                    view.get_mut(id).flags |= FLAG_DIRTY;
                }
            }
            if !keep.is_empty() {
                view.render.maybe_dirty = true;
            }
        } else {
            self.sync_view_metadata(view);
            repacked = view.render.scene.update(self);
            if crate::scene::VERIFY.load(core::sync::atomic::Ordering::Relaxed) {
                let mut whole = crate::scene::RenderSnapshot::new();
                whole.capture(self);
                view.render.scene.assert_matches(&whole);
            }
        }
        crate::scene::count(full, repacked);
        self.sync_gen = self.sync_gen.wrapping_add(1);
        view.render.scene.generation = Some(self.sync_gen);
        crate::perf::add_sub(
            crate::perf::phase::SCENE_CAPTURE,
            crate::perf::now().wrapping_sub(t0),
        );
        if self.render.shape_changed {
            view.dfs_valid = false;
        }
        for &m in &msgs {
            view.replay(m);
        }
        view.render.maybe_dirty |= self.render.maybe_dirty;
        let n = self.render.changed_n as usize;
        if n > CHANGED_MAX {
            view.render.changed_n = CHANGED_MAX as u8 + 1;
        } else {
            for i in 0..n {
                view.note_changed(self.render.changed[i]);
            }
        }
        {
            let mut mine = self.render.paints.borrow_mut();
            if !mine.is_empty() {
                let mut theirs = view.render.paints.borrow_mut();
                theirs.append(&mut mine);
                if theirs.len() > 2 * crate::render::PAINT_MAX {
                    theirs.retain(|&w, _| view.get(w).flags & FLAG_HAS_ON_PAINT != 0);
                }
            }
        }
        // Handed over.
        if self.render.maybe_dirty {
            self.clear_synced_dirty();
        }
        self.clear_touched();
        self.render.maybe_dirty = false;
        self.render.changed_n = 0;
        self.render.shape_changed = false;
        msgs.clear();
        self.render.msgs = msgs;
    }

    fn replay(&mut self, m: RenderMsg) {
        match m {
            RenderMsg::Born(p) => {
                if let Some(r) = self.render.recs.get_mut(p as usize) {
                    *r = RenderRec::EMPTY;
                }
            }
            RenderMsg::Moved(id, rect) => {
                if self.is_live(id)
                    && self.rendered(id)
                    && !self.is_active_layer_root(id)
                    && self.prev_rect(id).is_empty()
                {
                    self.set_prev_rect(id, rect);
                }
            }
            RenderMsg::SetLayer(id, n) => self.set_layer(id, n),
            RenderMsg::LayerAlpha(id, a) => self.set_layer_alpha(id, a),
            RenderMsg::Placed => {
                self.layers.placed = true;
                self.render.maybe_dirty = true;
            }
            RenderMsg::Touched => self.screen_touched(),
            RenderMsg::Clear => {
                self.render.reset();
                self.layers = Layers::new();
                self.dfs_valid = false;
                self.dfs_cache.clear();
            }
        }
    }

    /// After a render: the VM pool's `layers` = the render pool's, outside
    /// any pass.
    pub(crate) fn mirror_layers(&mut self, view: &ObjPool) {
        let mut l = view.layers;
        l.scope = Scope::All;
        l.origin = Offset { x: 0, y: 0 };
        self.layers = l;
    }
}
