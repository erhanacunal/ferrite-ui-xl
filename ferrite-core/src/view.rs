//! The VM's drawing for the render (plan step 3d). The render works on the
//! widget tree itself (`render::with_view`, `render::submit`); with a render
//! context (`render::records`) the screen may belong to the render side, so
//! the VM's drawing builtins record instead of drawing: direct drawing into
//! `RenderState::screen_rec`, closed into a [`Draw::Screen`] at the next
//! `on_paint` recording or render; `on_paint` drawing into a
//! [`Draw::Paint`]. The next render job takes them in order
//! (`render::run_job`).
//!
//! Until 2026-11-23 the render worked on a copy of the tree (a "render
//! pool", synced by commits: docs/notes/xl.md "One tree").

/// Drawing the VM recorded for the render, in the order it happened.
pub(crate) enum Draw {
    /// Drawing builtins outside `on_paint`, replayed on the screen before
    /// the render (they ran before it).
    Screen(crate::dl::List),
    /// A widget's `on_paint` drawing (screen coordinates, no scissor): kept
    /// as its paint list and drawn with it (`render::run_job`).
    Paint(crate::widget::WidgetId, crate::dl::List),
}

impl crate::pool::ObjPool {
    /// Something to render: a widget may be dirty, or the VM drew (recorded
    /// for the render, `render::records`).
    pub(crate) fn needs_render(&self) -> bool {
        self.render.maybe_dirty || !self.render.draws.is_empty() || self.render.screen_rec.busy()
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
    /// `on_paint` recording, at a render), keeping the order.
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
}
