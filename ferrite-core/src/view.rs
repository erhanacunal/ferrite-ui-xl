//! The render pool (plan step 3b): the renderer works on its own copy of the
//! widget tree, `Ctx::view`, never on the VM's pool.
//!
//! Before each render (`render::with_view`) the VM pool hands over
//! ([`ObjPool::sync_view`]):
//! - the tree: used arena words + live pointer table entries + allocator
//!   state, root and overlay (`ObjPool::copy_tree_into`, a memcpy);
//! - the dirty bits (then cleared in the VM pool), `maybe_dirty` and the
//!   changed list;
//! - the [`RenderMsg`]s queued since the last sync, replayed in order on the
//!   render pool's render-owned state (`RenderState`, `layers`);
//! - the `on_paint` lists recorded since then.
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
        self.render.maybe_dirty || !self.render.msgs.is_empty() || !self.render.paints.borrow().is_empty()
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

    /// Clear every widget's dirty bit.
    fn clear_all_dirty(&mut self) {
        for p in 0..self.ptr_count() as ObjPtr {
            let id = WidgetId(p);
            if self.is_live(id) {
                self.get_mut(id).clear_dirty();
            }
        }
    }

    /// Hand the tree and everything queued for the renderer over to the
    /// render pool `view` (self = the VM pool).
    pub(crate) fn sync_view(&mut self, view: &mut ObjPool) {
        debug_assert!(!self.is_view && view.is_view);
        let mut msgs = core::mem::take(&mut self.render.msgs);
        let cleared = msgs.iter().any(|m| matches!(m, RenderMsg::Clear));
        // The render pool's own pending dirty bits survive the copy.
        let keep = if view.render.maybe_dirty && !cleared { view.dirty_widgets() } else { Vec::new() };
        self.copy_tree_into(view);
        if self.render.shape_changed {
            view.dfs_valid = false;
        }
        for &id in &keep {
            if view.is_live(id) {
                view.get_mut(id).flags |= FLAG_DIRTY;
            }
        }
        for &m in &msgs {
            view.replay(m);
        }
        if !keep.is_empty() {
            view.render.maybe_dirty = true;
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
            self.clear_all_dirty();
        }
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
