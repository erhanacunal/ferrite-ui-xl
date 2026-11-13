//! What widget drawing draws on (plan step 2a).
//!
//! The renderer's widget bodies, text and images draw through `Canvas`
//! instead of `LcdImpl` directly. `LcdImpl` is the immediate canvas (pixels
//! go to the backend now); a recording canvas (display list, step 2b) will
//! implement the same trait, so the drawing code does not care which one it
//! got. Frame control, layers, portrait mapping and the like stay on
//! `LcdImpl`: they are not drawing.
//!
//! The methods mirror `LcdImpl`'s, with the same arguments and semantics
//! (see `lcd.rs` / `lcd/smooth.rs`).

use crate::flash::{FlashBackend, FlashImpl};
use crate::font::Font;
use crate::image::Image;
use crate::lcd::{LcdBackend, LcdImpl, LinePen};
use crate::types::{Offset, Rect};

/// A streaming polyline (`Canvas::pen`): points in order, each pixel drawn
/// once by the segment covering it most. Dropping it finishes the line.
pub trait Pen {
    fn to(&mut self, x: i16, y: i16);
    fn finish(&mut self);
}

impl<B: LcdBackend> Pen for LinePen<'_, B> {
    #[inline]
    fn to(&mut self, x: i16, y: i16) {
        LinePen::to(self, x, y)
    }
    #[inline]
    fn finish(&mut self) {
        LinePen::finish(self)
    }
}

pub trait Canvas {
    type Pen<'a>: Pen
    where
        Self: 'a;

    // --- State ---
    /// Logical screen size (`LcdImpl::width` / `height`).
    fn width(&self) -> u16;
    fn height(&self) -> u16;
    /// Blending reads the pixels below (`LcdBackend::HAS_ALPHA`).
    fn has_alpha(&self) -> bool;
    /// Shapes are anti-aliased right now.
    fn smooth(&self) -> bool;
    fn set_scissor(&self, rect: Option<Rect>);
    fn scissor(&self) -> Option<Rect>;

    // --- Rectangles and pixels ---
    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16);
    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8);
    /// A window at (`x_off`, `y_off`) into a `full_w` x `full_h` gradient.
    fn fill_gradient_rect(&self, x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16);
    fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16);
    fn draw_pixel(&self, x: u16, y: u16, color: u16);
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16);
    fn write_pixel(&self, color: u16);
    fn write_pixels(&self, colors: &[u16]);
    fn write_run(&self, color: u16, n: usize);
    fn blend_span(&self, x: i32, y: i32, color: u16, alphas: &[u8]);
    fn blend_row(&self, x: i32, y: i32, colors: &[u16], alphas: &[u8]);

    // --- Lines ---
    fn draw_line(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16);
    fn draw_line_thick(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16);
    fn draw_polyline(&self, points: &[Offset], color: u16, closed: bool, thickness: u16);
    fn draw_polyline_hard(&self, points: &[Offset], color: u16, closed: bool, thickness: u16);
    fn draw_wedge(&self, ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16);
    fn pen(&self, color: u16, thickness: u16) -> Self::Pen<'_>;

    // --- Shapes ---
    fn fill_polygon(&self, points: &[Offset], color: u16);
    fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16);
    fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16);
    fn draw_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16);
    fn fill_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16);
    fn stroke_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16);
    fn draw_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16);
    fn fill_arc(&self, cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16);
    /// A 1 px arc (the VM's `arc` builtin).
    fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16);
    fn draw_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16);
    fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16);
    fn fill_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8);
    fn fill_gradient_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8);
    fn stroke_rounded_rect(&self, x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16);

    // --- Images ---
    /// `img.draw(self, flash, x, y)`: the image decoded into this canvas's
    /// primitives now. A recording canvas keeps a reference instead (the
    /// image's flash address), which the replay resolves (`dl::Images`).
    fn draw_image<F: FlashBackend>(&self, img: &Image, flash: &FlashImpl<F>, x: i16, y: i16)
    where
        Self: Sized,
    {
        img.draw(self, flash, x, y)
    }

    // --- Text ---
    /// `font.draw_str(self, flash, text, x, y, fg, bg)`: the glyphs drawn now;
    /// returns the pen's x after the text. A recording canvas keeps the font
    /// by reference and the text instead.
    #[allow(clippy::too_many_arguments)]
    fn draw_text<F: FlashBackend>(
        &self,
        font: &Font,
        flash: &FlashImpl<F>,
        text: &[u8],
        x: i16,
        y: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> i16
    where
        Self: Sized,
    {
        font.draw_str(self, flash, text, x, y, fg, bg)
    }
}

/// The immediate canvas: everything goes straight to the backend.
impl<B: LcdBackend> Canvas for LcdImpl<B> {
    type Pen<'a>
        = LinePen<'a, B>
    where
        B: 'a;

    #[inline]
    fn width(&self) -> u16 {
        LcdImpl::width(self)
    }
    #[inline]
    fn height(&self) -> u16 {
        LcdImpl::height(self)
    }
    #[inline]
    fn has_alpha(&self) -> bool {
        LcdImpl::has_alpha(self)
    }
    #[inline]
    fn smooth(&self) -> bool {
        LcdImpl::smooth(self)
    }
    #[inline]
    fn set_scissor(&self, rect: Option<Rect>) {
        LcdImpl::set_scissor(self, rect)
    }
    #[inline]
    fn scissor(&self) -> Option<Rect> {
        LcdImpl::scissor(self)
    }

    #[inline]
    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        LcdImpl::fill_rect(self, x, y, w, h, color)
    }
    #[inline]
    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        LcdImpl::blend_rect(self, x, y, w, h, color, alpha)
    }
    #[inline]
    fn fill_gradient_rect(&self, x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16) {
        LcdImpl::fill_gradient_rect(self, x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h)
    }
    #[inline]
    fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        LcdImpl::draw_rect(self, x, y, w, h, color)
    }
    #[inline]
    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        LcdImpl::draw_pixel(self, x, y, color)
    }
    #[inline]
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        LcdImpl::begin_pixels(self, x, y, w, h)
    }
    #[inline]
    fn write_pixel(&self, color: u16) {
        LcdImpl::write_pixel(self, color)
    }
    #[inline]
    fn write_pixels(&self, colors: &[u16]) {
        LcdImpl::write_pixels(self, colors)
    }
    #[inline]
    fn write_run(&self, color: u16, n: usize) {
        LcdImpl::write_run(self, color, n)
    }
    #[inline]
    fn blend_span(&self, x: i32, y: i32, color: u16, alphas: &[u8]) {
        LcdImpl::blend_span(self, x, y, color, alphas)
    }
    #[inline]
    fn blend_row(&self, x: i32, y: i32, colors: &[u16], alphas: &[u8]) {
        LcdImpl::blend_row(self, x, y, colors, alphas)
    }

    #[inline]
    fn draw_line(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16) {
        LcdImpl::draw_line(self, x0, y0, x1, y1, color)
    }
    #[inline]
    fn draw_line_thick(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16) {
        LcdImpl::draw_line_thick(self, x0, y0, x1, y1, color, thickness)
    }
    #[inline]
    fn draw_polyline(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        LcdImpl::draw_polyline(self, points, color, closed, thickness)
    }
    #[inline]
    fn draw_polyline_hard(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        LcdImpl::draw_polyline_hard(self, points, color, closed, thickness)
    }
    #[inline]
    fn draw_wedge(&self, ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16) {
        LcdImpl::draw_wedge(self, ax, ay, bx, by, ra_q8, rb_q8, color)
    }
    #[inline]
    fn pen(&self, color: u16, thickness: u16) -> LinePen<'_, B> {
        LcdImpl::pen(self, color, thickness)
    }

    #[inline]
    fn fill_polygon(&self, points: &[Offset], color: u16) {
        LcdImpl::fill_polygon(self, points, color)
    }
    #[inline]
    fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        LcdImpl::draw_circle(self, cx, cy, r, color)
    }
    #[inline]
    fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        LcdImpl::fill_circle(self, cx, cy, r, color)
    }
    #[inline]
    fn draw_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        LcdImpl::draw_ellipse(self, cx, cy, rx, ry, color)
    }
    #[inline]
    fn fill_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        LcdImpl::fill_ellipse(self, cx, cy, rx, ry, color)
    }
    #[inline]
    fn stroke_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16) {
        LcdImpl::stroke_ellipse(self, cx, cy, rx, ry, thickness, color)
    }
    #[inline]
    fn draw_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16) {
        LcdImpl::draw_oval(self, x, y, w, h, t, color)
    }
    #[inline]
    fn fill_arc(&self, cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16) {
        LcdImpl::fill_arc(self, cx, cy, r, ir, start, end, color)
    }
    #[inline]
    fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16) {
        LcdImpl::draw_arc(self, cx, cy, r, start, end, color)
    }
    #[inline]
    fn draw_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        LcdImpl::draw_rounded_rect(self, x, y, w, h, r, color)
    }
    #[inline]
    fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        LcdImpl::fill_rounded_rect(self, x, y, w, h, r, color)
    }
    #[inline]
    fn fill_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8) {
        LcdImpl::fill_rounded_rect_at(self, x, y, w, h, r, color, alpha)
    }
    #[inline]
    fn fill_gradient_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8) {
        LcdImpl::fill_gradient_rounded_rect_at(self, x, y, w, h, r, c1, c2, dir)
    }
    #[inline]
    fn stroke_rounded_rect(&self, x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16) {
        LcdImpl::stroke_rounded_rect(self, x, y, w, h, r, t, color)
    }
}
