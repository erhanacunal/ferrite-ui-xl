//! Renderer-facing, allocator-independent projection of the widget tree.
//!
//! The scene owns every byte and sample it exposes. It contains no `ObjPool`
//! reference; it lives in the render pool (`ObjPool::view`), which reads its
//! widgets from it. Render-owned bookkeeping remains outside it.
//!
//! The first sync captures the whole tree ([`RenderSnapshot::capture`]);
//! later ones only repack what the VM pool touched since (`ObjPool::touched`:
//! every mutable access to an object goes through `ObjPool::payload_mut`,
//! which notes its pointer). The VM side packs those into a [`Delta`]
//! (`view::Commit`), which owns its data too; the render side applies it
//! ([`RenderSnapshot::apply`]) without reading the VM pool, so it may do so
//! on another core. A full capture again whenever the tree's shape changed
//! (the widgets are packed in paint order), after a program switch, when too
//! many objects were touched, or when the packed text / graph data holds too
//! much garbage. [`VERIFY`] compares every update with a full capture.

use alloc::vec::Vec;
use core::ops::Range;
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use crate::pool::{ObjPool, ObjPtr, PTR_NONE, T_STRING};
use crate::widget::{FLAG_DIRTY, KIND_GRAPH, Widget, WidgetExt, WidgetId};

/// Later syncs repack only what changed; off = a full capture every sync
/// (exynos `POST /api/rcore?scene=0|1`, for A/B measurements).
pub static INCREMENTAL: AtomicBool = AtomicBool::new(true);
/// Every incremental update is compared with a full capture, panicking on a
/// difference (tests, sim `FERRITE_SIM_SCENE_VERIFY=1`, `golden.py`).
pub static VERIFY: AtomicBool = AtomicBool::new(cfg!(test));
/// Syncs that captured the whole tree / that updated it, and the widgets the
/// updates repacked, since boot.
pub static FULLS: AtomicU32 = AtomicU32::new(0);
pub static UPDATES: AtomicU32 = AtomicU32::new(0);
pub static UPDATED: AtomicU32 = AtomicU32::new(0);

/// A string's / array's packed copy: start, end, and whether the object is
/// one (a freed or retyped object packs as nothing).
type Packed = (usize, usize, bool);

/// No packed copy of this object.
const NOWHERE: Packed = (usize::MAX, 0, false);

/// What changed in a tree of unchanged shape, packed on the VM side
/// (`ObjPool::build_commit`) for [`RenderSnapshot::apply`]: the touched
/// widgets, and the data of the strings / arrays the render side keeps a
/// copy of that were touched or are shown for the first time.
#[derive(Default)]
pub(crate) struct Delta {
    /// Touched widgets as packed by `pack_raw` (their ranges are filled in
    /// by `apply`).
    widgets: Vec<SceneWidget>,
    /// Object, whether it is a string, its bytes in `bytes`.
    texts: Vec<(ObjPtr, bool, Range<usize>)>,
    /// Object, whether it is an array, its samples in `samples`.
    graphs: Vec<(ObjPtr, bool, Range<usize>)>,
    bytes: Vec<u8>,
    samples: Vec<i32>,
}

impl Delta {
    pub(crate) fn clear(&mut self) {
        self.widgets.clear();
        self.texts.clear();
        self.graphs.clear();
        self.bytes.clear();
        self.samples.clear();
    }

    /// Packs widget `id`; returns the string and the array it shows.
    pub(crate) fn widget(&mut self, pool: &ObjPool, id: WidgetId) -> (ObjPtr, ObjPtr) {
        let w = RenderSnapshot::pack_raw(pool, id);
        let src = (w.text_src, w.graph_src);
        self.widgets.push(w);
        src
    }

    /// Packs string `p`'s bytes (none if it is no string any more).
    pub(crate) fn text(&mut self, pool: &ObjPool, p: ObjPtr) {
        let start = self.bytes.len();
        self.bytes.extend_from_slice(pool.str_get(p));
        self.texts.push((p, pool.type_of(p) == T_STRING, start..self.bytes.len()));
    }

    /// Packs array `p`'s samples (none if it is no array any more).
    pub(crate) fn graph(&mut self, pool: &ObjPool, p: ObjPtr) {
        let start = self.samples.len();
        let values = pool.arr(p);
        if let Some(values) = values {
            self.samples.extend_from_slice(values);
        }
        self.graphs.push((p, values.is_some(), start..self.samples.len()));
    }
}

/// One widget in depth-first paint order. Text and graph data refer to the
/// snapshot's packed buffers rather than VM object pointers.
#[derive(Clone)]
pub struct SceneWidget {
    pub id: WidgetId,
    pub widget: Widget,
    pub ext: WidgetExt,
    has_text: bool,
    has_graph: bool,
    text: Range<usize>,
    graph: Range<usize>,
    /// The VM objects the text and the graph samples were packed from (an
    /// update repacks them when those objects were touched).
    text_src: ObjPtr,
    graph_src: ObjPtr,
}

/// Renderer input, packed in depth-first paint order.
pub struct RenderSnapshot {
    widgets: Vec<SceneWidget>,
    text: Vec<u8>,
    graph: Vec<i32>,
    /// Where each string / array is packed, by object pointer (shared by the
    /// widgets that show it; dropped when the object is touched).
    text_by_id: Vec<Packed>,
    graph_by_id: Vec<Packed>,
    by_id: Vec<u32>,
    pub root: WidgetId,
    pub overlay: Option<crate::types::Rect>,
    /// The VM pool's sync count this scene is current for
    /// (`ObjPool::sync_gen`): an update needs every sync since the capture.
    pub(crate) generation: Option<u32>,
    /// `text` / `graph` lengths at the last capture (garbage since then).
    text_full: usize,
    graph_full: usize,
}

impl RenderSnapshot {
    /// Capture the renderer-visible tree and the string/graph payloads it
    /// refers to. Reuses this snapshot's allocations when called repeatedly.
    pub fn capture(&mut self, pool: &mut ObjPool) {
        pool.ensure_dfs();
        self.widgets.clear();
        self.text.clear();
        self.graph.clear();
        self.by_id.clear();
        self.by_id.resize(pool.ptr_count(), 0);
        self.text_by_id.clear();
        self.text_by_id.resize(pool.ptr_count(), NOWHERE);
        self.graph_by_id.clear();
        self.graph_by_id.resize(pool.ptr_count(), NOWHERE);

        for i in 0..pool.dfs_len() {
            let id = pool.dfs_at(i);
            if !pool.is_live(id) {
                continue;
            }
            let packed = self.pack(pool, id);
            let index = self.widgets.len();
            self.by_id[id.index()] = index as u32 + 1;
            self.widgets.push(packed);
        }

        self.root = pool.root;
        self.overlay = pool.overlay;
        self.text_full = self.text.len();
        self.graph_full = self.graph.len();
    }

    /// Apply what the VM side packed since the last sync (`delta`): the
    /// touched widgets, and the text / graph data of the widgets showing a
    /// string / array that came with it. The tree's shape must be the
    /// captured one. A widget keeps the dirty bit the render side still
    /// holds for it. `ptrs` = the VM pool's pointer count. Returns the
    /// widgets repacked.
    pub(crate) fn apply(
        &mut self,
        delta: &Delta,
        ptrs: usize,
        root: WidgetId,
        overlay: Option<crate::types::Rect>,
    ) -> usize {
        if self.by_id.len() < ptrs {
            self.by_id.resize(ptrs, 0);
        }
        if self.text_by_id.len() < ptrs {
            self.text_by_id.resize(ptrs, NOWHERE);
        }
        if self.graph_by_id.len() < ptrs {
            self.graph_by_id.resize(ptrs, NOWHERE);
        }
        for (p, has, r) in &delta.texts {
            let start = self.text.len();
            self.text.extend_from_slice(&delta.bytes[r.clone()]);
            if let Some(slot) = self.text_by_id.get_mut(*p as usize) {
                *slot = (start, self.text.len(), *has);
            }
        }
        for (p, has, r) in &delta.graphs {
            let start = self.graph.len();
            self.graph.extend_from_slice(&delta.samples[r.clone()]);
            if let Some(slot) = self.graph_by_id.get_mut(*p as usize) {
                *slot = (start, self.graph.len(), *has);
            }
        }
        let mut repacked = 0;
        for packed in &delta.widgets {
            let at = self.by_id.get(packed.id.index()).copied().unwrap_or(0);
            if at == 0 {
                // Not in the tree (not under the root).
                continue;
            }
            let i = at as usize - 1;
            let mut packed = packed.clone();
            packed.widget.flags |= self.widgets[i].widget.flags & FLAG_DIRTY;
            self.place(&mut packed);
            self.widgets[i] = packed;
            repacked += 1;
        }
        if !delta.texts.is_empty() || !delta.graphs.is_empty() {
            // Untouched widgets showing a string / array that came anew.
            for i in 0..self.widgets.len() {
                let w = &self.widgets[i];
                let text = self.text_by_id.get(w.text_src as usize).copied().filter(|r| r.0 != usize::MAX);
                let graph = self.graph_by_id.get(w.graph_src as usize).copied().filter(|r| r.0 != usize::MAX);
                let stale = |now: Option<Packed>, r: &Range<usize>, has: bool| {
                    now.is_some_and(|(start, end, h)| start != r.start || end != r.end || h != has)
                };
                if stale(text, &w.text, w.has_text) || stale(graph, &w.graph, w.has_graph) {
                    let mut w = w.clone();
                    self.place(&mut w);
                    self.widgets[i] = w;
                    repacked += 1;
                }
            }
        }
        self.root = root;
        self.overlay = overlay;
        repacked
    }

    /// Point `w` at the packed copies of the string and the array it shows.
    fn place(&self, w: &mut SceneWidget) {
        if w.text_src != PTR_NONE {
            let (start, end, has) = self.text_by_id.get(w.text_src as usize).copied().unwrap_or(NOWHERE);
            debug_assert!(start != usize::MAX, "scene delta: text of widget {} not sent", w.id.0);
            let (start, end) = if start == usize::MAX { (0, 0) } else { (start, end) };
            w.text = start..end;
            w.has_text = has;
        }
        if w.graph_src != PTR_NONE {
            let (start, end, has) = self.graph_by_id.get(w.graph_src as usize).copied().unwrap_or(NOWHERE);
            debug_assert!(start != usize::MAX, "scene delta: samples of widget {} not sent", w.id.0);
            let (start, end) = if start == usize::MAX { (0, 0) } else { (start, end) };
            w.graph = start..end;
            w.has_graph = has;
        }
    }

    /// The strings and the arrays this scene holds a packed copy of.
    pub(crate) fn packed_texts(&self) -> impl Iterator<Item = ObjPtr> + '_ {
        self.text_by_id.iter().enumerate().filter(|(_, r)| r.0 != usize::MAX).map(|(p, _)| p as ObjPtr)
    }

    pub(crate) fn packed_graphs(&self) -> impl Iterator<Item = ObjPtr> + '_ {
        self.graph_by_id.iter().enumerate().filter(|(_, r)| r.0 != usize::MAX).map(|(p, _)| p as ObjPtr)
    }

    /// The packed text / graph data has grown to more than twice what the
    /// last capture packed (replaced strings stay until the next capture).
    pub fn wasteful(&self) -> bool {
        self.text.len() > 2 * self.text_full + 4096 || self.graph.len() > 2 * self.graph_full + 1024
    }

    /// `id` as the renderer reads it: its header and optional fields, its
    /// text and graph samples packed (or found packed already).
    fn pack(&mut self, pool: &ObjPool, id: WidgetId) -> SceneWidget {
        let mut w = Self::pack_raw(pool, id);
        let (start, end) = self.text_range(pool, w.text_src);
        w.text = start..end;
        if w.graph_src != PTR_NONE {
            let (start, end) = self.graph_range(pool, w.graph_src);
            w.graph = start..end;
        }
        w
    }

    /// `id`'s header and optional fields, and which string and array it
    /// shows; the ranges of their packed copies are not set.
    fn pack_raw(pool: &ObjPool, id: WidgetId) -> SceneWidget {
        let mut widget = *pool.get(id);
        let mut ext = pool.ext(id);
        let text_src = ext.text_id;
        let has_text = text_src != PTR_NONE && pool.type_of(text_src) == T_STRING;
        ext.text_id = PTR_NONE;

        let is_graph = widget.kind == KIND_GRAPH;
        let graph_src = if is_graph { widget.value as ObjPtr } else { PTR_NONE };
        let has_graph = is_graph && pool.arr(graph_src).is_some();
        if is_graph {
            widget.value = 0;
            ext.value = 0;
        }
        SceneWidget { id, widget, ext, has_text, has_graph, text: 0..0, graph: 0..0, text_src, graph_src }
    }

    /// Where string `text_id`'s bytes are packed, packing them if they are
    /// not (or no longer).
    fn text_range(&mut self, pool: &ObjPool, text_id: ObjPtr) -> (usize, usize) {
        if text_id == PTR_NONE {
            return (0, 0);
        }
        let slot = text_id as usize;
        if let Some(&(start, end, _)) = self.text_by_id.get(slot).filter(|r| r.0 != usize::MAX) {
            return (start, end);
        }
        let start = self.text.len();
        self.text.extend_from_slice(pool.str_get(text_id));
        let end = self.text.len();
        if let Some(range) = self.text_by_id.get_mut(slot) {
            *range = (start, end, pool.type_of(text_id) == T_STRING);
        }
        (start, end)
    }

    /// As [`Self::text_range`], for the samples of array `array_id`.
    fn graph_range(&mut self, pool: &ObjPool, array_id: ObjPtr) -> (usize, usize) {
        let slot = array_id as usize;
        if let Some(&(start, end, _)) = self.graph_by_id.get(slot).filter(|r| r.0 != usize::MAX) {
            return (start, end);
        }
        let start = self.graph.len();
        let values = pool.arr(array_id);
        if let Some(values) = values {
            self.graph.extend_from_slice(values);
        }
        let end = self.graph.len();
        if let Some(range) = self.graph_by_id.get_mut(slot) {
            *range = (start, end, values.is_some());
        }
        (start, end)
    }

    /// Panics unless this (updated) scene reads like `full`, a capture of the
    /// same tree: the same widgets in the same order, each with the same
    /// header (the dirty bit aside: the render side keeps its own), optional
    /// fields, text and samples.
    pub fn assert_matches(&self, full: &RenderSnapshot) {
        assert_eq!(self.widgets.len(), full.widgets.len(), "scene update: widget count");
        assert!(self.root == full.root && self.overlay == full.overlay, "scene update: root / overlay");
        for (a, b) in self.widgets.iter().zip(&full.widgets) {
            let id = a.id.0;
            assert!(a.id == b.id, "scene update: order at widget {id}");
            assert!(same_header(&a.widget, &b.widget), "scene update: stale header of widget {id}");
            assert!(same_ext(&a.ext, &b.ext), "scene update: stale fields of widget {id}");
            assert!(
                a.has_text == b.has_text && self.text(a) == full.text(b),
                "scene update: stale text of widget {id}"
            );
            assert!(
                a.has_graph == b.has_graph && self.graph(a) == full.graph(b),
                "scene update: stale graph of widget {id}"
            );
        }
    }

    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            text: Vec::new(),
            graph: Vec::new(),
            text_by_id: Vec::new(),
            graph_by_id: Vec::new(),
            by_id: Vec::new(),
            root: WidgetId::NONE,
            overlay: None,
            generation: None,
            text_full: 0,
            graph_full: 0,
        }
    }

    pub fn widgets(&self) -> &[SceneWidget] {
        &self.widgets
    }

    pub fn widget(&self, id: WidgetId) -> Option<&SceneWidget> {
        let slot = *self.by_id.get(id.index())?;
        (slot != 0).then(|| &self.widgets[slot as usize - 1])
    }

    pub fn widget_mut(&mut self, id: WidgetId) -> Option<&mut SceneWidget> {
        let slot = *self.by_id.get(id.index())?;
        (slot != 0).then(|| &mut self.widgets[slot as usize - 1])
    }

    pub fn len(&self) -> usize {
        self.widgets.len()
    }

    pub fn ids(&self) -> impl Iterator<Item = WidgetId> + '_ {
        self.widgets.iter().map(|widget| widget.id)
    }

    pub fn text(&self, widget: &SceneWidget) -> &[u8] {
        &self.text[widget.text.clone()]
    }

    pub fn text_for(&self, id: WidgetId) -> Option<&[u8]> {
        self.widget(id)
            .filter(|widget| widget.has_text)
            .map(|widget| self.text(widget))
    }

    pub fn graph(&self, widget: &SceneWidget) -> &[i32] {
        &self.graph[widget.graph.clone()]
    }

    pub fn graph_for(&self, id: WidgetId) -> Option<&[i32]> {
        self.widget(id)
            .filter(|widget| widget.has_graph)
            .map(|widget| self.graph(widget))
    }

    pub fn heap_bytes(&self) -> usize {
        use core::mem::size_of;
        self.widgets.capacity() * size_of::<SceneWidget>()
            + self.text.capacity()
            + self.graph.capacity() * size_of::<i32>()
            + self.text_by_id.capacity() * size_of::<Packed>()
            + self.graph_by_id.capacity() * size_of::<Packed>()
            + self.by_id.capacity() * size_of::<u32>()
    }
}

impl Default for RenderSnapshot {
    fn default() -> Self {
        Self::new()
    }
}

/// The same header, the dirty bit aside.
fn same_header(a: &Widget, b: &Widget) -> bool {
    let words = |w: &Widget| {
        let mut w = *w;
        w.flags &= !FLAG_DIRTY;
        // SAFETY: `Widget` is 32 bytes of plain integers without padding
        // (asserted in widget.rs), word aligned.
        unsafe { core::mem::transmute::<Widget, [u32; 8]>(w) }
    };
    words(a) == words(b)
}

fn same_ext(a: &WidgetExt, b: &WidgetExt) -> bool {
    a.group_id == b.group_id
        && a.flex == b.flex
        && a.flex_gap == b.flex_gap
        && a.flex_grow == b.flex_grow
        && a.border == b.border
        && a.padding == b.padding
        && a.text_color == b.text_color
        && a.font_id == b.font_id
        && a.text_align == b.text_align
        && a.text_id == b.text_id
        && a.border_radius == b.border_radius
        && a.press_color == b.press_color
        && a.image_id == b.image_id
        && a.max_length == b.max_length
        && a.value == b.value
        && a.gradient_color == b.gradient_color
        && a.gradient_dir == b.gradient_dir
        && a.alpha == b.alpha
}

/// Counts a sync for the counters (`FULLS` / `UPDATES` / `UPDATED`).
pub(crate) fn count(full: bool, repacked: usize) {
    if full {
        FULLS.fetch_add(1, Ordering::Relaxed);
    } else {
        UPDATES.fetch_add(1, Ordering::Relaxed);
        UPDATED.fetch_add(repacked as u32, Ordering::Relaxed);
    }
}
