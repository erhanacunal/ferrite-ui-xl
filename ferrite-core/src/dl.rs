//! Display lists (plan step 2b): a `Canvas` that records what widget drawing
//! does, and `replay` onto another canvas.
//!
//! A `List` is the calls in order, with their slice arguments (pixel runs,
//! coverage rows, points) in shared arenas, so recording a glyph row or a
//! pixel run costs no allocation once the buffers have grown. Replaying a
//! list onto a canvas makes exactly the calls the drawing code made, so the
//! pixels are the same as drawing directly; that includes the scissor
//! changes and a streaming pen's segments interleaved with other calls
//! (a graph fills under each segment as it goes).
//!
//! The recorder answers the state queries (`width`, `has_alpha`, `smooth`,
//! `scissor`…) from the canvas it is about to replay onto (`Recorder::begin`)
//! and from the scissor changes recorded so far.
//!
//! For now (2b) the renderer records each widget and replays it at once
//! when `ENABLED` is on (`render::draw_widget`); the per-widget cache comes
//! next (2c). Lists hold absolute screen coordinates.

use core::cell::{Cell, RefCell};
use core::sync::atomic::{AtomicBool, Ordering};

use alloc::vec::Vec;

use crate::canvas::{Canvas, Pen};
use crate::types::{Offset, Rect};

/// Record widget drawing and replay it (instead of drawing directly).
/// Off by default; the sim turns it on with `FERRITE_SIM_DL=1`, exynos with
/// `POST /api/dl?on=1`.
pub static ENABLED: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

/// A slice of one of the list's arenas.
#[derive(Clone, Copy, Debug)]
pub struct Span {
    start: u32,
    len: u32,
}

impl Span {
    #[inline]
    fn range(self) -> core::ops::Range<usize> {
        self.start as usize..(self.start + self.len) as usize
    }
}

/// One recorded call (the `Canvas` method of the same name).
#[derive(Clone, Copy)]
pub enum Op {
    SetScissor(Option<Rect>),
    FillRect { x: u16, y: u16, w: u16, h: u16, color: u16 },
    BlendRect { x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8 },
    GradientRect { x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16 },
    DrawRect { x: u16, y: u16, w: u16, h: u16, color: u16 },
    DrawPixel { x: u16, y: u16, color: u16 },
    BeginPixels { x: u16, y: u16, w: u16, h: u16 },
    /// `write_pixel` / `write_pixels` (consecutive ones merged), in `u16s`.
    Pixels(Span),
    Run { color: u16, n: usize },
    /// Coverage in `u8s`.
    BlendSpan { x: i32, y: i32, color: u16, alphas: Span },
    /// Colours in `u16s`, coverage in `u8s` (same length).
    BlendRow { x: i32, y: i32, colors: Span, alphas: Span },
    Line { x0: i16, y0: i16, x1: i16, y1: i16, color: u16 },
    LineThick { x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16 },
    /// Points in `pts`.
    Polyline { points: Span, color: u16, closed: bool, thickness: u16, hard: bool },
    Wedge { ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16 },
    /// `pen(color, thickness)`; the pen's `to`/`finish`/drop follow.
    PenBegin { color: u16, thickness: u16 },
    PenTo { x: i16, y: i16 },
    PenFinish,
    PenEnd,
    FillPolygon { points: Span, color: u16 },
    Circle { cx: i16, cy: i16, r: i16, color: u16 },
    FillCircle { cx: i16, cy: i16, r: i16, color: u16 },
    Ellipse { cx: i16, cy: i16, rx: i16, ry: i16, color: u16 },
    FillEllipse { cx: i16, cy: i16, rx: i16, ry: i16, color: u16 },
    StrokeEllipse { cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16 },
    Oval { x: i16, y: i16, w: u16, h: u16, t: u16, color: u16 },
    Arc { cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16 },
    RoundedRect { x: u16, y: u16, w: u16, h: u16, r: u16, color: u16 },
    FillRoundedRect { x: u16, y: u16, w: u16, h: u16, r: u16, color: u16 },
    FillRoundedRectAt { x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8 },
    GradientRoundedRectAt { x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8 },
    StrokeRoundedRect { x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16 },
}

/// Recorded calls and their slice data.
#[derive(Default)]
pub struct List {
    pub ops: Vec<Op>,
    u8s: Vec<u8>,
    u16s: Vec<u16>,
    pts: Vec<Offset>,
}

impl List {
    pub const fn new() -> Self {
        Self { ops: Vec::new(), u8s: Vec::new(), u16s: Vec::new(), pts: Vec::new() }
    }

    /// Empty it, keeping the buffers.
    pub fn clear(&mut self) {
        self.ops.clear();
        self.u8s.clear();
        self.u16s.clear();
        self.pts.clear();
    }

    /// Heap bytes held (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.ops.capacity() * core::mem::size_of::<Op>()
            + self.u8s.capacity()
            + self.u16s.capacity() * 2
            + self.pts.capacity() * core::mem::size_of::<Offset>()
    }

    fn put_u8s(&mut self, d: &[u8]) -> Span {
        let start = self.u8s.len() as u32;
        self.u8s.extend_from_slice(d);
        Span { start, len: d.len() as u32 }
    }
    fn put_u16s(&mut self, d: &[u16]) -> Span {
        let start = self.u16s.len() as u32;
        self.u16s.extend_from_slice(d);
        Span { start, len: d.len() as u32 }
    }
    fn put_pts(&mut self, d: &[Offset]) -> Span {
        let start = self.pts.len() as u32;
        self.pts.extend_from_slice(d);
        Span { start, len: d.len() as u32 }
    }

    /// Append streamed pixels, extending a `Pixels` op that ends the list.
    fn put_pixels(&mut self, colors: &[u16]) {
        let end = self.u16s.len() as u32;
        if let Some(Op::Pixels(s)) = self.ops.last_mut()
            && s.start + s.len == end
        {
            s.len += colors.len() as u32;
            self.u16s.extend_from_slice(colors);
            return;
        }
        let s = self.put_u16s(colors);
        self.ops.push(Op::Pixels(s));
    }

    /// Make the recorded calls on `c`.
    pub fn replay<C: Canvas>(&self, c: &C) {
        let mut pen: Option<C::Pen<'_>> = None;
        for op in &self.ops {
            match *op {
                Op::SetScissor(r) => c.set_scissor(r),
                Op::FillRect { x, y, w, h, color } => c.fill_rect(x, y, w, h, color),
                Op::BlendRect { x, y, w, h, color, alpha } => c.blend_rect(x, y, w, h, color, alpha),
                Op::GradientRect { x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h } => {
                    c.fill_gradient_rect(x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h)
                }
                Op::DrawRect { x, y, w, h, color } => c.draw_rect(x, y, w, h, color),
                Op::DrawPixel { x, y, color } => c.draw_pixel(x, y, color),
                Op::BeginPixels { x, y, w, h } => c.begin_pixels(x, y, w, h),
                Op::Pixels(s) => c.write_pixels(&self.u16s[s.range()]),
                Op::Run { color, n } => c.write_run(color, n),
                Op::BlendSpan { x, y, color, alphas } => c.blend_span(x, y, color, &self.u8s[alphas.range()]),
                Op::BlendRow { x, y, colors, alphas } => {
                    c.blend_row(x, y, &self.u16s[colors.range()], &self.u8s[alphas.range()])
                }
                Op::Line { x0, y0, x1, y1, color } => c.draw_line(x0, y0, x1, y1, color),
                Op::LineThick { x0, y0, x1, y1, color, thickness } => {
                    c.draw_line_thick(x0, y0, x1, y1, color, thickness)
                }
                Op::Polyline { points, color, closed, thickness, hard } => {
                    let p = &self.pts[points.range()];
                    if hard {
                        c.draw_polyline_hard(p, color, closed, thickness)
                    } else {
                        c.draw_polyline(p, color, closed, thickness)
                    }
                }
                Op::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color } => c.draw_wedge(ax, ay, bx, by, ra_q8, rb_q8, color),
                Op::PenBegin { color, thickness } => {
                    drop(pen.take());
                    pen = Some(c.pen(color, thickness));
                }
                Op::PenTo { x, y } => {
                    if let Some(p) = pen.as_mut() {
                        p.to(x, y);
                    }
                }
                Op::PenFinish => {
                    if let Some(p) = pen.as_mut() {
                        p.finish();
                    }
                }
                Op::PenEnd => pen = None,
                Op::FillPolygon { points, color } => c.fill_polygon(&self.pts[points.range()], color),
                Op::Circle { cx, cy, r, color } => c.draw_circle(cx, cy, r, color),
                Op::FillCircle { cx, cy, r, color } => c.fill_circle(cx, cy, r, color),
                Op::Ellipse { cx, cy, rx, ry, color } => c.draw_ellipse(cx, cy, rx, ry, color),
                Op::FillEllipse { cx, cy, rx, ry, color } => c.fill_ellipse(cx, cy, rx, ry, color),
                Op::StrokeEllipse { cx, cy, rx, ry, thickness, color } => {
                    c.stroke_ellipse(cx, cy, rx, ry, thickness, color)
                }
                Op::Oval { x, y, w, h, t, color } => c.draw_oval(x, y, w, h, t, color),
                Op::Arc { cx, cy, r, ir, start, end, color } => c.fill_arc(cx, cy, r, ir, start, end, color),
                Op::RoundedRect { x, y, w, h, r, color } => c.draw_rounded_rect(x, y, w, h, r, color),
                Op::FillRoundedRect { x, y, w, h, r, color } => c.fill_rounded_rect(x, y, w, h, r, color),
                Op::FillRoundedRectAt { x, y, w, h, r, color, alpha } => {
                    c.fill_rounded_rect_at(x, y, w, h, r, color, alpha)
                }
                Op::GradientRoundedRectAt { x, y, w, h, r, c1, c2, dir } => {
                    c.fill_gradient_rounded_rect_at(x, y, w, h, r, c1, c2, dir)
                }
                Op::StrokeRoundedRect { x, y, w, h, r, t, color } => c.stroke_rounded_rect(x, y, w, h, r, t, color),
            }
        }
    }
}

/// What a canvas says about itself (the recorder repeats the target's).
#[derive(Clone, Copy, Default)]
struct Info {
    width: u16,
    height: u16,
    has_alpha: bool,
    smooth: bool,
}

/// A `Canvas` that records into a `List`.
pub struct Recorder {
    list: RefCell<List>,
    info: Cell<Info>,
    scissor: Cell<Option<Rect>>,
    /// Between `begin` and `end` (a nested drawing draws directly).
    busy: Cell<bool>,
}

impl Default for Recorder {
    fn default() -> Self {
        Self::new()
    }
}

impl Recorder {
    pub const fn new() -> Self {
        Self {
            list: RefCell::new(List::new()),
            info: Cell::new(Info { width: 0, height: 0, has_alpha: false, smooth: false }),
            scissor: Cell::new(None),
            busy: Cell::new(false),
        }
    }

    /// Start a recording meant for `target`: empty list, `target`'s state.
    /// False while a recording is going on (draw directly then).
    pub fn begin<C: Canvas>(&self, target: &C) -> bool {
        if self.busy.get() {
            return false;
        }
        self.busy.set(true);
        self.list.borrow_mut().clear();
        self.info.set(Info {
            width: target.width(),
            height: target.height(),
            has_alpha: target.has_alpha(),
            smooth: target.smooth(),
        });
        self.scissor.set(target.scissor());
        true
    }

    /// End the recording and replay it onto `target`.
    pub fn end_replay<C: Canvas>(&self, target: &C) {
        self.busy.set(false);
        self.list.borrow().replay(target);
    }

    /// The recorded list.
    pub fn list(&self) -> core::cell::Ref<'_, List> {
        self.list.borrow()
    }

    pub fn heap_bytes(&self) -> usize {
        self.list.borrow().heap_bytes()
    }

    #[inline]
    fn push(&self, op: Op) {
        self.list.borrow_mut().ops.push(op);
    }
}

/// The recorder's pen: records `to`/`finish` and, when dropped, the end.
pub struct RecPen<'a> {
    rec: &'a Recorder,
}

impl Pen for RecPen<'_> {
    fn to(&mut self, x: i16, y: i16) {
        self.rec.push(Op::PenTo { x, y });
    }
    fn finish(&mut self) {
        self.rec.push(Op::PenFinish);
    }
}

impl Drop for RecPen<'_> {
    fn drop(&mut self) {
        self.rec.push(Op::PenEnd);
    }
}

impl Canvas for Recorder {
    type Pen<'a> = RecPen<'a>;

    fn width(&self) -> u16 {
        self.info.get().width
    }
    fn height(&self) -> u16 {
        self.info.get().height
    }
    fn has_alpha(&self) -> bool {
        self.info.get().has_alpha
    }
    fn smooth(&self) -> bool {
        self.info.get().smooth
    }
    fn set_scissor(&self, rect: Option<Rect>) {
        self.scissor.set(rect);
        self.push(Op::SetScissor(rect));
    }
    fn scissor(&self) -> Option<Rect> {
        self.scissor.get()
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        self.push(Op::FillRect { x, y, w, h, color });
    }
    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        self.push(Op::BlendRect { x, y, w, h, color, alpha });
    }
    fn fill_gradient_rect(&self, x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16) {
        self.push(Op::GradientRect { x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h });
    }
    fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        self.push(Op::DrawRect { x, y, w, h, color });
    }
    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        self.push(Op::DrawPixel { x, y, color });
    }
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.push(Op::BeginPixels { x, y, w, h });
    }
    fn write_pixel(&self, color: u16) {
        self.list.borrow_mut().put_pixels(&[color]);
    }
    fn write_pixels(&self, colors: &[u16]) {
        self.list.borrow_mut().put_pixels(colors);
    }
    fn write_run(&self, color: u16, n: usize) {
        self.push(Op::Run { color, n });
    }
    fn blend_span(&self, x: i32, y: i32, color: u16, alphas: &[u8]) {
        let mut l = self.list.borrow_mut();
        let alphas = l.put_u8s(alphas);
        l.ops.push(Op::BlendSpan { x, y, color, alphas });
    }
    fn blend_row(&self, x: i32, y: i32, colors: &[u16], alphas: &[u8]) {
        let mut l = self.list.borrow_mut();
        let colors = l.put_u16s(colors);
        let alphas = l.put_u8s(alphas);
        l.ops.push(Op::BlendRow { x, y, colors, alphas });
    }

    fn draw_line(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16) {
        self.push(Op::Line { x0, y0, x1, y1, color });
    }
    fn draw_line_thick(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16) {
        self.push(Op::LineThick { x0, y0, x1, y1, color, thickness });
    }
    fn draw_polyline(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        let mut l = self.list.borrow_mut();
        let points = l.put_pts(points);
        l.ops.push(Op::Polyline { points, color, closed, thickness, hard: false });
    }
    fn draw_polyline_hard(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        let mut l = self.list.borrow_mut();
        let points = l.put_pts(points);
        l.ops.push(Op::Polyline { points, color, closed, thickness, hard: true });
    }
    fn draw_wedge(&self, ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16) {
        self.push(Op::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color });
    }
    fn pen(&self, color: u16, thickness: u16) -> RecPen<'_> {
        self.push(Op::PenBegin { color, thickness });
        RecPen { rec: self }
    }

    fn fill_polygon(&self, points: &[Offset], color: u16) {
        let mut l = self.list.borrow_mut();
        let points = l.put_pts(points);
        l.ops.push(Op::FillPolygon { points, color });
    }
    fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        self.push(Op::Circle { cx, cy, r, color });
    }
    fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        self.push(Op::FillCircle { cx, cy, r, color });
    }
    fn draw_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        self.push(Op::Ellipse { cx, cy, rx, ry, color });
    }
    fn fill_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        self.push(Op::FillEllipse { cx, cy, rx, ry, color });
    }
    fn stroke_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16) {
        self.push(Op::StrokeEllipse { cx, cy, rx, ry, thickness, color });
    }
    fn draw_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16) {
        self.push(Op::Oval { x, y, w, h, t, color });
    }
    fn fill_arc(&self, cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16) {
        self.push(Op::Arc { cx, cy, r, ir, start, end, color });
    }
    fn draw_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        self.push(Op::RoundedRect { x, y, w, h, r, color });
    }
    fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        self.push(Op::FillRoundedRect { x, y, w, h, r, color });
    }
    fn fill_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8) {
        self.push(Op::FillRoundedRectAt { x, y, w, h, r, color, alpha });
    }
    fn fill_gradient_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8) {
        self.push(Op::GradientRoundedRectAt { x, y, w, h, r, c1, c2, dir });
    }
    fn stroke_rounded_rect(&self, x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16) {
        self.push(Op::StrokeRoundedRect { x, y, w, h, r, t, color });
    }
}
