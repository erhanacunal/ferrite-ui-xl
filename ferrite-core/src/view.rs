//! The render pool (plan step 3b): the renderer works on its own copy of the
//! widget tree, `Ctx::view`, never on the VM's pool.
//!
//! Before each render (`render::with_view`) the VM pool hands over a
//! [`Commit`]: data only, built on the VM pool ([`ObjPool::build_commit`])
//! and applied on the render pool without reading the VM pool
//! ([`ObjPool::apply_commit`]): at once ([`ObjPool::sync_view`]), or by the
//! render job, on whichever core runs it ([`ObjPool::post_commit`],
//! [`ObjPool::apply_pending`]). It holds:
//! - the scene (`scene.rs`): widgets plus referenced text/graph data; no VM
//!   object arena or pointer table is copied. The render pool keeps it: the
//!   first sync captures the whole tree, later ones repack only the objects
//!   the VM pool touched since (`ObjPool::touched`, `scene::Delta`);
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

/// One sync's data, from the VM pool to the render pool. Owns everything it
/// refers to. Its buffers are reused: the applied commit goes back to the VM
/// pool as the next one's storage (`RenderState::commit`).
pub(crate) struct Commit {
    /// On the render pool: not applied yet.
    pub(crate) pending: bool,
    /// `scene` is a capture of the whole tree (swapped with the render
    /// pool's); else `delta` is what changed.
    full: bool,
    scene: crate::scene::RenderSnapshot,
    delta: crate::scene::Delta,
    /// `scene::VERIFY`: a capture of the whole tree to compare the updated
    /// scene with.
    verify: bool,
    whole: crate::scene::RenderSnapshot,
    /// The VM pool's sync count with this commit.
    generation: u32,
    /// The VM pool's pointer count, root and overlay.
    ptrs: usize,
    root: WidgetId,
    overlay: Option<Rect>,
    msgs: Vec<RenderMsg>,
    draws: Vec<Draw>,
    paints: alloc::collections::BTreeMap<WidgetId, crate::dl::List>,
    maybe_dirty: bool,
    shape_changed: bool,
    changed: [WidgetId; CHANGED_MAX],
    changed_n: u8,
}

impl Commit {
    fn new() -> Self {
        Self {
            pending: false,
            full: false,
            scene: crate::scene::RenderSnapshot::new(),
            delta: crate::scene::Delta::default(),
            verify: false,
            whole: crate::scene::RenderSnapshot::new(),
            generation: 0,
            ptrs: 0,
            root: WidgetId::NONE,
            overlay: None,
            msgs: Vec::new(),
            draws: Vec::new(),
            paints: alloc::collections::BTreeMap::new(),
            maybe_dirty: false,
            shape_changed: false,
            changed: [WidgetId::NONE; CHANGED_MAX],
            changed_n: 0,
        }
    }
}

#[inline]
fn bit(bits: &[u32], p: ObjPtr) -> bool {
    bits.get(p as usize >> 5).is_some_and(|w| w & (1 << (p & 31)) != 0)
}

/// Sets `p`'s bit; false if `bits` has none for it.
#[inline]
fn set_bit(bits: &mut [u32], p: ObjPtr) -> bool {
    match bits.get_mut(p as usize >> 5) {
        Some(w) => {
            *w |= 1 << (p & 31);
            true
        }
        None => false,
    }
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
        if !list.is_empty() {
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
        let clear = |pool: &mut ObjPool, p: ObjPtr| {
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
    /// render pool `view` (self = the VM pool), here and now.
    pub(crate) fn sync_view(&mut self, view: &mut ObjPool) {
        let mut c = self.build_commit(view);
        view.apply_commit(&mut c);
        self.render.commit = Some(c);
    }

    /// As [`Self::sync_view`], but `view` applies it later: at the start of
    /// its next render job (`apply_pending`), on whichever core runs it.
    pub(crate) fn post_commit(&mut self, view: &mut ObjPool) {
        debug_assert!(!view.render.commit.as_ref().is_some_and(|c| c.pending), "a commit is still pending");
        let mut c = self.build_commit(view);
        c.pending = true;
        view.render.commit = Some(c);
    }

    /// Render pool: apply the commit posted for this job, if any.
    pub(crate) fn apply_pending(&mut self) {
        if let Some(mut c) = self.render.commit.take() {
            if c.pending {
                self.apply_commit(&mut c);
            }
            self.render.commit = Some(c);
        }
    }

    /// Pack everything queued for the render pool `view` since the last sync
    /// (self = the VM pool). Of `view` only what tells whether its scene can
    /// be updated is read (the caller holds it: no job runs on it), and its
    /// last applied commit is taken for its buffers.
    fn build_commit(&mut self, view: &mut ObjPool) -> alloc::boxed::Box<Commit> {
        use core::sync::atomic::Ordering::Relaxed;
        debug_assert!(!self.is_view && view.is_view);
        let mut c = match self.render.commit.take() {
            Some(c) => c,
            None => match view.render.commit.take() {
                Some(c) if !c.pending => c,
                _ => alloc::boxed::Box::new(Commit::new()),
            },
        };
        self.close_screen_rec();
        debug_assert!(c.draws.is_empty() && c.msgs.is_empty() && c.paints.is_empty());
        core::mem::swap(&mut c.draws, &mut self.render.draws);
        core::mem::swap(&mut c.msgs, &mut self.render.msgs);
        core::mem::swap(&mut c.paints, self.render.paints.get_mut());
        let cleared = c.msgs.iter().any(|m| matches!(m, RenderMsg::Clear));
        let t0 = crate::perf::now();
        // The whole tree: the first time, when its shape changed (the scene
        // is packed in paint order), when `view` missed a sync, when more
        // was touched than noted, or to drop the packed data's garbage.
        c.full = self.render.shape_changed
            || cleared
            || self.touched_over()
            || view.render.scene.generation != Some(self.sync_gen)
            || view.render.scene.wasteful()
            || !crate::scene::INCREMENTAL.load(Relaxed);
        c.verify = false;
        if c.full {
            c.scene.capture(self);
            // What the render pool holds a copy of from now on.
            self.text_sent.fill(0);
            self.graph_sent.fill(0);
            for p in c.scene.packed_texts() {
                set_bit(&mut self.text_sent, p);
            }
            for p in c.scene.packed_graphs() {
                set_bit(&mut self.graph_sent, p);
            }
        } else {
            self.build_delta(&mut c.delta);
            if crate::scene::VERIFY.load(Relaxed) {
                c.verify = true;
                c.whole.capture(self);
            }
        }
        self.sync_gen = self.sync_gen.wrapping_add(1);
        c.generation = self.sync_gen;
        c.ptrs = self.ptr_count();
        c.root = self.root;
        c.overlay = self.overlay;
        crate::perf::add_sub(
            crate::perf::phase::SCENE_CAPTURE,
            crate::perf::now().wrapping_sub(t0),
        );
        c.maybe_dirty = self.render.maybe_dirty;
        c.shape_changed = self.render.shape_changed;
        c.changed = self.render.changed;
        c.changed_n = self.render.changed_n;
        // Handed over.
        if self.render.maybe_dirty {
            self.clear_synced_dirty();
        }
        self.clear_touched();
        self.render.maybe_dirty = false;
        self.render.changed_n = 0;
        self.render.shape_changed = false;
        c
    }

    /// Pack what was touched since the last sync into `delta`: the touched
    /// widgets, the touched strings / arrays the render pool holds a copy
    /// of, and those a touched widget shows that it holds none of.
    fn build_delta(&mut self, delta: &mut crate::scene::Delta) {
        use crate::pool::{PTR_NONE, T_WIDGET};
        delta.clear();
        for i in 0..self.touched().len() {
            let p = self.touched()[i];
            if bit(&self.text_sent, p) {
                delta.text(self, p);
            }
            if bit(&self.graph_sent, p) {
                delta.graph(self, p);
            }
            if self.type_of(p) != T_WIDGET {
                continue;
            }
            let (text, graph) = delta.widget(self, WidgetId(p));
            // Without a bit to remember it by, the data goes every time.
            if text != PTR_NONE && !bit(&self.text_sent, text) {
                set_bit(&mut self.text_sent, text);
                delta.text(self, text);
            }
            if graph != PTR_NONE && !bit(&self.graph_sent, graph) {
                set_bit(&mut self.graph_sent, graph);
                delta.graph(self, graph);
            }
        }
    }

    /// Take a commit in (self = the render pool). Reads nothing of the VM
    /// pool; leaves `c` empty, with its buffers.
    pub(crate) fn apply_commit(&mut self, c: &mut Commit) {
        debug_assert!(self.is_view);
        c.pending = false;
        self.render.draws.append(&mut c.draws);
        let cleared = c.msgs.iter().any(|m| matches!(m, RenderMsg::Clear));
        let mut repacked = 0;
        if c.full {
            // The render pool's own pending dirty bits survive the capture.
            let keep = if self.render.maybe_dirty && !cleared { self.dirty_widgets() } else { Vec::new() };
            self.set_view_metadata(c.ptrs, c.root, c.overlay);
            core::mem::swap(&mut self.render.scene, &mut c.scene);
            for &id in &keep {
                if self.is_live(id) {
                    self.get_mut(id).flags |= FLAG_DIRTY;
                }
            }
            if !keep.is_empty() {
                self.render.maybe_dirty = true;
            }
        } else {
            debug_assert!(self.render.scene.generation == Some(c.generation.wrapping_sub(1)), "a sync was missed");
            self.set_view_metadata(c.ptrs, c.root, c.overlay);
            repacked = self.render.scene.apply(&c.delta, c.ptrs, c.root, c.overlay);
            if c.verify {
                self.render.scene.assert_matches(&c.whole);
            }
        }
        crate::scene::count(c.full, repacked);
        self.render.scene.generation = Some(c.generation);
        if c.shape_changed {
            self.dfs_valid = false;
        }
        for i in 0..c.msgs.len() {
            let m = c.msgs[i];
            self.replay(m);
        }
        c.msgs.clear();
        self.render.maybe_dirty |= c.maybe_dirty;
        let n = c.changed_n as usize;
        if n > CHANGED_MAX {
            self.render.changed_n = CHANGED_MAX as u8 + 1;
        } else {
            for i in 0..n {
                self.note_changed(c.changed[i]);
            }
        }
        if !c.paints.is_empty() {
            let mut theirs = core::mem::take(self.render.paints.get_mut());
            theirs.append(&mut c.paints);
            if theirs.len() > 2 * crate::render::PAINT_MAX {
                theirs.retain(|&w, _| self.get(w).flags & FLAG_HAS_ON_PAINT != 0);
            }
            *self.render.paints.get_mut() = theirs;
        }
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
