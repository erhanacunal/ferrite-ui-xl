//! Renderer-facing, allocator-independent projection of the widget tree.
//!
//! A captured scene owns every byte and sample it exposes. It contains no
//! `ObjPool` reference and can be moved to another core without keeping the
//! VM's arena alive. Render-owned bookkeeping remains outside this snapshot.

use alloc::vec::Vec;
use core::ops::Range;

use crate::pool::{ObjPool, PTR_NONE};
use crate::widget::{KIND_GRAPH, Widget, WidgetExt, WidgetId};

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
}

/// Immutable renderer input, packed in depth-first paint order.
///
/// Build this on the VM core, publish it by ownership transfer, and keep it
/// unchanged until the render core returns the buffer. The caller may then
/// reuse its allocation for the next scene.
pub struct RenderSnapshot {
    widgets: Vec<SceneWidget>,
    text: Vec<u8>,
    graph: Vec<i32>,
    text_by_id: Vec<(usize, usize)>,
    graph_by_id: Vec<(usize, usize)>,
    by_id: Vec<u32>,
    dirty: Vec<WidgetId>,
    changed: Vec<WidgetId>,
    pub root: WidgetId,
    pub overlay: Option<crate::types::Rect>,
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
        self.dirty.clear();
        self.changed.clear();
        self.by_id.resize(pool.ptr_count(), 0);
        self.text_by_id.resize(pool.ptr_count(), (usize::MAX, 0));
        self.graph_by_id.resize(pool.ptr_count(), (usize::MAX, 0));
        self.text_by_id.fill((usize::MAX, 0));
        self.graph_by_id.fill((usize::MAX, 0));

        for i in 0..pool.dfs_len() {
            let id = pool.dfs_at(i);
            if !pool.is_live(id) {
                continue;
            }

            let mut widget = *pool.get(id);
            let mut ext = pool.ext(id);
            if widget.is_dirty() {
                self.dirty.push(id);
            }
            let text_id = ext.text_id;
            let has_text = text_id != PTR_NONE && pool.type_of(text_id) == crate::pool::T_STRING;
            let (text_start, text_end) = if text_id != PTR_NONE {
                let slot = text_id as usize;
                if let Some(&(start, end)) = self
                    .text_by_id
                    .get(slot)
                    .filter(|&&(start, _)| start != usize::MAX)
                {
                    (start, end)
                } else {
                    let start = self.text.len();
                    self.text.extend_from_slice(pool.str_get(text_id));
                    let end = self.text.len();
                    if let Some(range) = self.text_by_id.get_mut(slot) {
                        *range = (start, end);
                    }
                    (start, end)
                }
            } else {
                (self.text.len(), self.text.len())
            };
            ext.text_id = PTR_NONE;

            let (graph_start, graph_end, has_graph) = if widget.kind == KIND_GRAPH {
                let array_id = widget.value as crate::pool::ObjPtr;
                let slot = array_id as usize;
                if let Some(&(start, end)) = self
                    .graph_by_id
                    .get(slot)
                    .filter(|&&(start, _)| start != usize::MAX)
                {
                    (start, end, pool.arr(array_id).is_some())
                } else {
                    let start = self.graph.len();
                    let values = pool.arr(array_id);
                    if let Some(values) = values {
                        self.graph.extend_from_slice(values);
                    }
                    let end = self.graph.len();
                    if let Some(range) = self.graph_by_id.get_mut(slot) {
                        *range = (start, end);
                    }
                    (start, end, values.is_some())
                }
            } else {
                (self.graph.len(), self.graph.len(), false)
            };
            if widget.kind == KIND_GRAPH {
                widget.value = 0;
                ext.value = 0;
            }

            let index = self.widgets.len();
            self.by_id[id.index()] = index as u32 + 1;
            self.widgets.push(SceneWidget {
                id,
                widget,
                ext,
                has_text,
                has_graph,
                text: text_start..text_end,
                graph: graph_start..graph_end,
            });
        }

        self.root = pool.root;
        self.overlay = pool.overlay;
        let changed_n = (pool.render.changed_n as usize).min(crate::pool::CHANGED_MAX);
        self.changed
            .extend_from_slice(&pool.render.changed[..changed_n]);
    }

    pub fn new() -> Self {
        Self {
            widgets: Vec::new(),
            text: Vec::new(),
            graph: Vec::new(),
            text_by_id: Vec::new(),
            graph_by_id: Vec::new(),
            by_id: Vec::new(),
            dirty: Vec::new(),
            changed: Vec::new(),
            root: WidgetId::NONE,
            overlay: None,
        }
    }

    pub fn widgets(&self) -> &[SceneWidget] {
        &self.widgets
    }

    pub fn dirty(&self) -> &[WidgetId] {
        &self.dirty
    }

    pub fn changed(&self) -> &[WidgetId] {
        &self.changed
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
            + self.text_by_id.capacity() * size_of::<(usize, usize)>()
            + self.graph_by_id.capacity() * size_of::<(usize, usize)>()
            + self.by_id.capacity() * size_of::<u32>()
            + (self.dirty.capacity() + self.changed.capacity()) * size_of::<WidgetId>()
    }
}

impl Default for RenderSnapshot {
    fn default() -> Self {
        Self::new()
    }
}
