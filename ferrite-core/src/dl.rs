//! Display lists (plan step 2b): a `Canvas` that records what widget drawing
//! does, and `replay` onto another canvas.
//!
//! A `List` is the calls in order, encoded in one stream of words (like the
//! VM's bytecode, but word-aligned for decoding speed): each call is a header
//! word, opcode in bits 0-7, a small argument in bits 8-15 (an alpha, a
//! gradient direction, flags), a 16-bit one in bits 16-31 (usually the
//! colour), then its other arguments, 16-bit ones two to a word (`lo | hi <<
//! 16`), `i32`s a word each, then its slice data (pixel runs, coverage rows,
//! points), padded with zeros to a word and read back in place as
//! `&[u16]` / `&[u8]` / `&[Offset]`. Recording a glyph row or a pixel run
//! costs no allocation once the buffer has grown. `List::ops` decodes it into
//! `Op`s (replay, dumps, tests). Replaying a list onto a canvas makes exactly
//! the calls the drawing code made, so the pixels are the same as drawing
//! directly; that includes the scissor changes and a streaming pen's segments
//! interleaved with other calls (a graph fills under each segment as it goes).
//!
//! The recorder answers the state queries (`width`, `has_alpha`, `smooth`,
//! `scissor`…) from the canvas it is about to replay onto (`Recorder::begin`)
//! and from the scissor changes recorded so far.
//!
//! With `ENABLED` on, the renderer keeps the lists in a `Cache` keyed by a
//! hash of everything a widget's drawing reads (`render::dl_key`): a widget
//! whose look was seen before is recorded once, against the whole screen,
//! and then replayed into each clip rect (`List::replay_clipped`). Lists
//! hold absolute screen coordinates, so the box is part of the key.

use core::cell::{Cell, RefCell, RefMut};
use core::sync::atomic::{AtomicBool, AtomicU32, Ordering};

use alloc::collections::{BTreeMap, BTreeSet};
use alloc::vec::Vec;

use crate::canvas::{Canvas, Pen};
use crate::types::{Offset, Rect};

/// Draw widgets through cached display lists (instead of drawing directly).
/// Off by default; the sim turns it on with `FERRITE_SIM_DL=1`, exynos with
/// `POST /api/dl?on=1`.
pub static ENABLED: AtomicBool = AtomicBool::new(false);

/// On a cache hit, record the widget again and panic if the cached list
/// differs: the key missed something the drawing reads (sim
/// `FERRITE_SIM_DL_VERIFY=1`, tests).
pub static VERIFY: AtomicBool = AtomicBool::new(false);

#[inline]
pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

#[inline]
pub fn verify() -> bool {
    VERIFY.load(Ordering::Relaxed)
}

/// Counters since boot (free-running; take differences): replays from the
/// cache, direct draws (a look not seen before), lists recorded, lists
/// evicted, `on_paint` lists recorded (`Vm::run_paint`); and the bytes held now.
pub static PAINTS: AtomicU32 = AtomicU32::new(0);
pub static HITS: AtomicU32 = AtomicU32::new(0);
pub static DIRECT: AtomicU32 = AtomicU32::new(0);
pub static STORES: AtomicU32 = AtomicU32::new(0);
pub static EVICTS: AtomicU32 = AtomicU32::new(0);
pub static BYTES: AtomicU32 = AtomicU32::new(0);

#[inline]
fn count(c: &AtomicU32) {
    c.fetch_add(1, Ordering::Relaxed);
}

/// One recorded call (the `Canvas` method of the same name), as decoded from
/// a list (`List::ops`); slices point into the list.
#[derive(Clone, Copy, PartialEq, Debug)]
pub enum Op<'a> {
    SetScissor(Option<Rect>),
    FillRect { x: u16, y: u16, w: u16, h: u16, color: u16 },
    BlendRect { x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8 },
    GradientRect { x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16 },
    DrawRect { x: u16, y: u16, w: u16, h: u16, color: u16 },
    DrawPixel { x: u16, y: u16, color: u16 },
    BeginPixels { x: u16, y: u16, w: u16, h: u16 },
    /// `write_pixel` / `write_pixels` (consecutive ones merged).
    Pixels(&'a [u16]),
    Run { color: u16, n: usize },
    BlendSpan { x: i32, y: i32, color: u16, alphas: &'a [u8] },
    /// Colours and coverage of the same length.
    BlendRow { x: i32, y: i32, colors: &'a [u16], alphas: &'a [u8] },
    Line { x0: i16, y0: i16, x1: i16, y1: i16, color: u16 },
    LineThick { x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16 },
    Polyline { points: &'a [Offset], color: u16, closed: bool, thickness: u16, hard: bool },
    Wedge { ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16 },
    /// `pen(color, thickness)`; the pen's `to`/`finish`/drop follow.
    PenBegin { color: u16, thickness: u16 },
    PenTo { x: i16, y: i16 },
    PenFinish,
    PenEnd,
    FillPolygon { points: &'a [Offset], color: u16 },
    Circle { cx: i16, cy: i16, r: i16, color: u16 },
    FillCircle { cx: i16, cy: i16, r: i16, color: u16 },
    Ellipse { cx: i16, cy: i16, rx: i16, ry: i16, color: u16 },
    FillEllipse { cx: i16, cy: i16, rx: i16, ry: i16, color: u16 },
    StrokeEllipse { cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16 },
    Oval { x: i16, y: i16, w: u16, h: u16, t: u16, color: u16 },
    Arc { cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16 },
    ThinArc { cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16 },
    RoundedRect { x: u16, y: u16, w: u16, h: u16, r: u16, color: u16 },
    FillRoundedRect { x: u16, y: u16, w: u16, h: u16, r: u16, color: u16 },
    FillRoundedRectAt { x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8 },
    GradientRoundedRectAt { x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8 },
    StrokeRoundedRect { x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16 },
}

/// Opcodes (header bits 0-7). The words after the header, in order:
/// `[a]` = header bits 8-15, `{b}` = header bits 16-31, `x|y` = one word.
mod code {
    /// `[1 = some]`, then `x|y`, `w|h` if some.
    pub const SET_SCISSOR: u8 = 1;
    /// `{color}` `x|y` `w|h` (also DRAW_RECT).
    pub const FILL_RECT: u8 = 2;
    /// `[alpha]{color}` `x|y` `w|h`.
    pub const BLEND_RECT: u8 = 3;
    /// `[dir]{c1}` `x|y` `w|h` `c2|x_off` `y_off|full_w` `full_h`.
    pub const GRADIENT_RECT: u8 = 4;
    pub const DRAW_RECT: u8 = 5;
    /// `{color}` `x|y`.
    pub const DRAW_PIXEL: u8 = 6;
    /// `x|y` `w|h`.
    pub const BEGIN_PIXELS: u8 = 7;
    /// Header bits 8-31 = pixel count, then the pixels two to a word.
    pub const PIXELS: u8 = 8;
    /// `{color}` `n`.
    pub const RUN: u8 = 9;
    /// `[SHORT]{color}` `x|y` (SHORT) or `x` `y`, `n`, coverage four to a word.
    pub const BLEND_SPAN: u8 = 10;
    /// `[SHORT]` `x|y` or `x` `y`, `n`, colours two to a word, coverage four to a word.
    pub const BLEND_ROW: u8 = 11;
    /// `{color}` `x0|y0` `x1|y1`.
    pub const LINE: u8 = 12;
    /// `{color}` `x0|y0` `x1|y1` `thickness`.
    pub const LINE_THICK: u8 = 13;
    /// `[CLOSED | HARD]{color}` `thickness` `n`, points a word each.
    pub const POLYLINE: u8 = 14;
    /// `{color}` `ax|ay` `bx|by` `ra_q8` `rb_q8`.
    pub const WEDGE: u8 = 15;
    /// `{color}` `thickness`.
    pub const PEN_BEGIN: u8 = 16;
    /// `x|y`.
    pub const PEN_TO: u8 = 17;
    pub const PEN_FINISH: u8 = 18;
    pub const PEN_END: u8 = 19;
    /// `{color}` `n`, points a word each.
    pub const FILL_POLYGON: u8 = 20;
    /// `{color}` `cx|cy` `r` (also FILL_CIRCLE).
    pub const CIRCLE: u8 = 21;
    pub const FILL_CIRCLE: u8 = 22;
    /// `{color}` `cx|cy` `rx|ry` (also FILL_ELLIPSE).
    pub const ELLIPSE: u8 = 23;
    pub const FILL_ELLIPSE: u8 = 24;
    /// `{color}` `cx|cy` `rx|ry` `thickness`.
    pub const STROKE_ELLIPSE: u8 = 25;
    /// `{color}` `x|y` `w|h` `t`.
    pub const OVAL: u8 = 26;
    /// `{color}` `cx|cy` `r|ir` `start|end`.
    pub const ARC: u8 = 27;
    /// `{color}` `cx|cy` `r|start` `end`.
    pub const THIN_ARC: u8 = 28;
    /// `{color}` `x|y` `w|h` `r` (also FILL_ROUNDED_RECT).
    pub const ROUNDED_RECT: u8 = 29;
    pub const FILL_ROUNDED_RECT: u8 = 30;
    /// `[alpha]{color}` `x|y` `w|h` `r`.
    pub const FILL_ROUNDED_RECT_AT: u8 = 31;
    /// `[dir]{c1}` `x|y` `w|h` `r|c2`.
    pub const GRADIENT_ROUNDED_RECT_AT: u8 = 32;
    /// `{color}` `x|y` `w|h` `r|t`.
    pub const STROKE_ROUNDED_RECT: u8 = 33;

    /// BLEND_SPAN / BLEND_ROW: x and y fit an i16 and share a word.
    pub const SHORT: u8 = 1;
    /// POLYLINE flags.
    pub const CLOSED: u8 = 1;
    pub const HARD: u8 = 2;
}

#[inline]
fn pk(lo: u16, hi: u16) -> u32 {
    lo as u32 | (hi as u32) << 16
}
#[inline]
fn pki(lo: i16, hi: i16) -> u32 {
    pk(lo as u16, hi as u16)
}
#[inline]
fn lo(w: u32) -> u16 {
    w as u16
}
#[inline]
fn hi(w: u32) -> u16 {
    (w >> 16) as u16
}
#[inline]
fn header(op: u8, a: u8, b: u16) -> u32 {
    op as u32 | (a as u32) << 8 | (b as u32) << 16
}

// Slice data read and written in place. Sound: u16, u8 and `Offset`
// (`repr(C)`, two i16) have no invalid bit patterns and an alignment of at
// most a word's, and each view stays inside the words it is cut from.
#[inline]
fn view_u16(w: &[u32], n: usize) -> &[u16] {
    assert!(n <= w.len() * 2);
    unsafe { core::slice::from_raw_parts(w.as_ptr() as *const u16, n) }
}
#[inline]
fn view_u8(w: &[u32], n: usize) -> &[u8] {
    assert!(n <= w.len() * 4);
    unsafe { core::slice::from_raw_parts(w.as_ptr() as *const u8, n) }
}
#[inline]
fn view_pts(w: &[u32], n: usize) -> &[Offset] {
    const _: () = assert!(core::mem::size_of::<Offset>() == 4 && core::mem::align_of::<Offset>() <= 4);
    assert!(n <= w.len());
    unsafe { core::slice::from_raw_parts(w.as_ptr() as *const Offset, n) }
}
/// Most pixels one PIXELS call holds (its count is 24 bits).
const PIXELS_MAX: usize = (1 << 24) - 1;
/// `List::open_pixels`: none.
const NO_PIXELS: u32 = u32::MAX;

/// Recorded calls (see the module doc for the encoding).
pub struct List {
    words: Vec<u32>,
    /// Calls recorded (a merged pixel write is not a new one).
    calls: u32,
    /// Index of a PIXELS header that ends the list (later pixels extend it).
    open_pixels: u32,
    /// The last scissor set, if any (`end_scissor`).
    last_scissor: Option<Option<Rect>>,
}

impl Default for List {
    fn default() -> Self {
        Self::new()
    }
}

/// Same calls, same arguments.
impl PartialEq for List {
    fn eq(&self, o: &Self) -> bool {
        self.calls == o.calls && self.words == o.words
    }
}

impl List {
    pub const fn new() -> Self {
        Self { words: Vec::new(), calls: 0, open_pixels: NO_PIXELS, last_scissor: None }
    }

    /// Empty it, keeping the buffer.
    pub fn clear(&mut self) {
        self.words.clear();
        self.calls = 0;
        self.open_pixels = NO_PIXELS;
        self.last_scissor = None;
    }

    /// A copy holding just what it uses (the recorder's buffer grows).
    pub fn compact(&self) -> List {
        let mut words = Vec::with_capacity(self.words.len());
        words.extend_from_slice(&self.words);
        List { words, calls: self.calls, open_pixels: self.open_pixels, last_scissor: self.last_scissor }
    }

    /// Calls recorded.
    #[inline]
    pub fn len(&self) -> usize {
        self.calls as usize
    }

    #[inline]
    pub fn is_empty(&self) -> bool {
        self.calls == 0
    }

    /// The calls, decoded.
    pub fn ops(&self) -> Ops<'_> {
        Ops { w: &self.words, i: 0 }
    }

    /// The scissor the drawing left set, given the one it started with.
    pub fn end_scissor(&self, start: Option<Rect>) -> Option<Rect> {
        self.last_scissor.unwrap_or(start)
    }

    /// Bytes of its calls and their data (length, not capacity).
    pub fn used_bytes(&self) -> usize {
        self.words.len() * 4
    }

    /// Heap bytes held (capacity).
    pub fn heap_bytes(&self) -> usize {
        self.words.capacity() * 4
    }

    // --- recording ---

    /// Append `w` (a call's header and argument words; a literal array, so
    /// this inlines to one capacity check and a few stores).
    #[inline(always)]
    fn put(&mut self, w: &[u32]) {
        self.words.reserve(w.len());
        let len = self.words.len();
        // SAFETY: reserved; every new word is written before `set_len`.
        unsafe {
            core::ptr::copy_nonoverlapping(w.as_ptr(), self.words.as_mut_ptr().add(len), w.len());
            self.words.set_len(len + w.len());
        }
    }
    /// Start a call: its header word and argument words.
    #[inline(always)]
    fn call(&mut self, w: &[u32]) {
        self.put(w);
        self.calls += 1;
        self.open_pixels = NO_PIXELS;
    }

    /// Append `bytes` bytes from `src`, zero-padded to a word.
    #[inline(always)]
    fn put_bytes(&mut self, src: *const u8, bytes: usize) {
        let words = bytes.div_ceil(4);
        self.words.reserve(words);
        let len = self.words.len();
        // SAFETY: reserved; the last word is zeroed first, then the bytes
        // (which cover every other new word) copied over it.
        unsafe {
            let dst = self.words.as_mut_ptr().add(len);
            if words > 0 {
                dst.add(words - 1).write(0);
            }
            core::ptr::copy_nonoverlapping(src, dst as *mut u8, bytes);
            self.words.set_len(len + words);
        }
    }
    #[inline]
    fn data_u16(&mut self, d: &[u16]) {
        self.put_bytes(d.as_ptr() as *const u8, d.len() * 2);
    }
    #[inline]
    fn data_u8(&mut self, d: &[u8]) {
        self.put_bytes(d.as_ptr(), d.len());
    }
    #[inline]
    fn data_pts(&mut self, d: &[Offset]) {
        // `Offset` is `repr(C)` (x, y): the same bytes `view_pts` reads.
        self.put_bytes(d.as_ptr() as *const u8, d.len() * 4);
    }

    /// x and y: one word when both fit an i16 (the SHORT flag).
    #[inline]
    fn xy_short(x: i32, y: i32) -> bool {
        i16::try_from(x).is_ok() && i16::try_from(y).is_ok()
    }

    fn set_scissor(&mut self, r: Option<Rect>) {
        match r {
            Some(r) => self.call(&[header(code::SET_SCISSOR, 1, 0), pki(r.x, r.y), pk(r.w, r.h)]),
            None => self.call(&[header(code::SET_SCISSOR, 0, 0)]),
        }
        self.last_scissor = Some(r);
    }

    /// Append streamed pixels, extending a PIXELS call that ends the list.
    fn pixels(&mut self, mut colors: &[u16]) {
        while !colors.is_empty() {
            if self.open_pixels == NO_PIXELS || (self.words[self.open_pixels as usize] >> 8) as usize == PIXELS_MAX {
                let h = self.words.len();
                self.call(&[header(code::PIXELS, 0, 0)]);
                self.open_pixels = h as u32;
            }
            let h = self.open_pixels as usize;
            let had = (self.words[h] >> 8) as usize;
            let k = colors.len().min(PIXELS_MAX - had);
            let n = had + k;
            self.words[h] = header(code::PIXELS, 0, 0) | (n as u32) << 8;
            // The call ends the list: its data words are h + 1.., `had`
            // pixels written (the last word's upper half zero if odd).
            let end = h + 1 + n.div_ceil(2);
            let add = end - self.words.len();
            self.words.reserve(add);
            // SAFETY: reserved; the new last word is zeroed, then the
            // pixels copied from pixel `had` on (inside `end` words).
            unsafe {
                let base = self.words.as_mut_ptr();
                if add > 0 {
                    base.add(end - 1).write(0);
                }
                let dst = (base.add(h + 1) as *mut u16).add(had);
                core::ptr::copy_nonoverlapping(colors.as_ptr(), dst, k);
                self.words.set_len(end);
            }
            colors = &colors[k..];
        }
    }

    /// The list as text, one call a line (`GET /api/dl?id=`, sim `--dl-dump`):
    /// the call's fields, then the start of its slice data (`… +N` = N more).
    pub fn write_text(&self, out: &mut impl core::fmt::Write) -> core::fmt::Result {
        const SHOWN: usize = 12;
        fn some<T: core::fmt::Debug>(out: &mut impl core::fmt::Write, what: &str, d: &[T]) -> core::fmt::Result {
            write!(out, " {what}[")?;
            for (i, v) in d.iter().take(SHOWN).enumerate() {
                write!(out, "{}{v:?}", if i > 0 { " " } else { "" })?;
            }
            if d.len() > SHOWN {
                write!(out, " … +{}", d.len() - SHOWN)?;
            }
            write!(out, "]")
        }
        fn pts(out: &mut impl core::fmt::Write, p: &[Offset]) -> core::fmt::Result {
            write!(out, " pts[")?;
            for (i, o) in p.iter().take(SHOWN).enumerate() {
                write!(out, "{}{},{}", if i > 0 { " " } else { "" }, o.x, o.y)?;
            }
            if p.len() > SHOWN {
                write!(out, " … +{}", p.len() - SHOWN)?;
            }
            write!(out, "]")
        }
        for (i, op) in self.ops().enumerate() {
            write!(out, "{i:4} ")?;
            match op {
                Op::Pixels(px) => {
                    write!(out, "Pixels({})", px.len())?;
                    some(out, "px", px)?
                }
                Op::BlendSpan { x, y, color, alphas } => {
                    write!(out, "BlendSpan {{ x: {x}, y: {y}, color: {color}, n: {} }}", alphas.len())?;
                    some(out, "a", alphas)?
                }
                Op::BlendRow { x, y, colors, alphas } => {
                    write!(out, "BlendRow {{ x: {x}, y: {y}, n: {} }}", colors.len())?;
                    some(out, "px", colors)?;
                    some(out, "a", alphas)?;
                }
                Op::Polyline { points, color, closed, thickness, hard } => {
                    write!(
                        out,
                        "Polyline {{ color: {color}, closed: {closed}, thickness: {thickness}, hard: {hard}, n: {} }}",
                        points.len()
                    )?;
                    pts(out, points)?
                }
                Op::FillPolygon { points, color } => {
                    write!(out, "FillPolygon {{ color: {color}, n: {} }}", points.len())?;
                    pts(out, points)?
                }
                op => write!(out, "{op:?}")?,
            }
            writeln!(out)?;
        }
        Ok(())
    }

    /// Make the recorded calls on `c`.
    pub fn replay<C: Canvas>(&self, c: &C) {
        self.play(c, None, None);
    }

    /// Make the recorded calls on `c` as if drawn with the scissor always
    /// inside `clip`: every scissor the list sets is cut to it (the list
    /// starts with `start`, `None` = `clip` itself), and what ignores the
    /// scissor (pixel streams, gradient fills) is cut here. A list recorded
    /// against the whole screen and replayed into each rect of a clip region
    /// draws what the drawing would have drawn clipped to that region.
    /// Leaves the scissor at `clip`'s cut of the last one set.
    pub fn replay_clipped<C: Canvas>(&self, c: &C, clip: Rect, start: Option<Rect>) {
        self.play(c, Some(clip), start);
    }

    fn play<C: Canvas>(&self, c: &C, clip: Option<Rect>, start: Option<Rect>) {
        let cut = |s: Option<Rect>| -> Option<Rect> {
            let k = clip?;
            Some(match s {
                Some(r) => r.intersection(&k).unwrap_or(Rect::new(k.x, k.y, 0, 0)),
                None => k,
            })
        };
        // The scissor in force (clipped replay only).
        let mut cur = cut(start);
        if clip.is_some() {
            c.set_scissor(cur);
        }
        let mut stream = Stream::default();
        let mut pen: Option<C::Pen<'_>> = None;
        for op in self.ops() {
            match op {
                Op::SetScissor(r) => {
                    if clip.is_some() {
                        cur = cut(r);
                        c.set_scissor(cur);
                    } else {
                        c.set_scissor(r)
                    }
                }
                Op::FillRect { x, y, w, h, color } => c.fill_rect(x, y, w, h, color),
                Op::BlendRect { x, y, w, h, color, alpha } => c.blend_rect(x, y, w, h, color, alpha),
                Op::GradientRect { x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h } => match cur {
                    // Streamed: cut it, keeping its place in the gradient.
                    Some(k) => {
                        if let Some(v) = Rect::new(x as i16, y as i16, w, h).intersection(&k) {
                            let (dx, dy) = ((v.x as i32 - x as i32) as u16, (v.y as i32 - y as i32) as u16);
                            c.fill_gradient_rect(
                                v.x as u16, v.y as u16, v.w, v.h, c1, c2, dir, x_off + dx, y_off + dy, full_w, full_h,
                            )
                        }
                    }
                    None => c.fill_gradient_rect(x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h),
                },
                Op::DrawRect { x, y, w, h, color } => c.draw_rect(x, y, w, h, color),
                Op::DrawPixel { x, y, color } => c.draw_pixel(x, y, color),
                Op::BeginPixels { x, y, w, h } => match cur {
                    Some(k) => stream = Stream::open(c, Rect::new(x as i16, y as i16, w, h), k),
                    None => c.begin_pixels(x, y, w, h),
                },
                Op::Pixels(px) => {
                    if cur.is_some() {
                        stream.feed(px.len(), |i, n| c.write_pixels(&px[i..i + n]));
                    } else {
                        c.write_pixels(px)
                    }
                }
                Op::Run { color, n } => {
                    if cur.is_some() {
                        stream.feed(n, |_, k| c.write_run(color, k));
                    } else {
                        c.write_run(color, n)
                    }
                }
                Op::BlendSpan { x, y, color, alphas } => c.blend_span(x, y, color, alphas),
                Op::BlendRow { x, y, colors, alphas } => c.blend_row(x, y, colors, alphas),
                Op::Line { x0, y0, x1, y1, color } => c.draw_line(x0, y0, x1, y1, color),
                Op::LineThick { x0, y0, x1, y1, color, thickness } => {
                    c.draw_line_thick(x0, y0, x1, y1, color, thickness)
                }
                Op::Polyline { points, color, closed, thickness, hard } => {
                    if hard {
                        c.draw_polyline_hard(points, color, closed, thickness)
                    } else {
                        c.draw_polyline(points, color, closed, thickness)
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
                Op::FillPolygon { points, color } => c.fill_polygon(points, color),
                Op::Circle { cx, cy, r, color } => c.draw_circle(cx, cy, r, color),
                Op::FillCircle { cx, cy, r, color } => c.fill_circle(cx, cy, r, color),
                Op::Ellipse { cx, cy, rx, ry, color } => c.draw_ellipse(cx, cy, rx, ry, color),
                Op::FillEllipse { cx, cy, rx, ry, color } => c.fill_ellipse(cx, cy, rx, ry, color),
                Op::StrokeEllipse { cx, cy, rx, ry, thickness, color } => {
                    c.stroke_ellipse(cx, cy, rx, ry, thickness, color)
                }
                Op::Oval { x, y, w, h, t, color } => c.draw_oval(x, y, w, h, t, color),
                Op::Arc { cx, cy, r, ir, start, end, color } => c.fill_arc(cx, cy, r, ir, start, end, color),
                Op::ThinArc { cx, cy, r, start, end, color } => c.draw_arc(cx, cy, r, start, end, color),
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

/// Decodes a list's calls (`List::ops`).
pub struct Ops<'a> {
    w: &'a [u32],
    i: usize,
}

impl<'a> Ops<'a> {
    /// The next `N` argument words.
    #[inline(always)]
    fn args<const N: usize>(&mut self) -> [u32; N] {
        let a: [u32; N] = self.w[self.i..self.i + N].try_into().unwrap();
        self.i += N;
        a
    }
    /// The next `words` words (slice data).
    #[inline(always)]
    fn data(&mut self, words: usize) -> &'a [u32] {
        let d = &self.w[self.i..self.i + words];
        self.i += words;
        d
    }
    /// x and y as `put_xy` wrote them.
    #[inline(always)]
    fn xy(&mut self, flag: u8) -> (i32, i32) {
        if flag & code::SHORT != 0 {
            let [p] = self.args();
            (lo(p) as i16 as i32, hi(p) as i16 as i32)
        } else {
            let [x, y] = self.args();
            (x as i32, y as i32)
        }
    }
}

impl<'a> Iterator for Ops<'a> {
    type Item = Op<'a>;

    #[inline]
    fn next(&mut self) -> Option<Op<'a>> {
        let h = *self.w.get(self.i)?;
        self.i += 1;
        let (a, b) = ((h >> 8) as u8, hi(h));
        let s = |w: u32| (lo(w) as i16, hi(w) as i16);
        Some(match h as u8 {
            code::SET_SCISSOR => {
                if a == 0 {
                    Op::SetScissor(None)
                } else {
                    let [p, sz] = self.args();
                    Op::SetScissor(Some(Rect::new(lo(p) as i16, hi(p) as i16, lo(sz), hi(sz))))
                }
            }
            code::FILL_RECT | code::DRAW_RECT => {
                let [p, sz] = self.args();
                let (x, y, w, h_, color) = (lo(p), hi(p), lo(sz), hi(sz), b);
                if h as u8 == code::FILL_RECT {
                    Op::FillRect { x, y, w, h: h_, color }
                } else {
                    Op::DrawRect { x, y, w, h: h_, color }
                }
            }
            code::BLEND_RECT => {
                let [p, sz] = self.args();
                Op::BlendRect { x: lo(p), y: hi(p), w: lo(sz), h: hi(sz), color: b, alpha: a }
            }
            code::GRADIENT_RECT => {
                let [p, sz, c2o, yw, fh] = self.args();
                Op::GradientRect {
                    x: lo(p),
                    y: hi(p),
                    w: lo(sz),
                    h: hi(sz),
                    c1: b,
                    c2: lo(c2o),
                    dir: a,
                    x_off: hi(c2o),
                    y_off: lo(yw),
                    full_w: hi(yw),
                    full_h: lo(fh),
                }
            }
            code::DRAW_PIXEL => {
                let [p] = self.args();
                Op::DrawPixel { x: lo(p), y: hi(p), color: b }
            }
            code::BEGIN_PIXELS => {
                let [p, sz] = self.args();
                Op::BeginPixels { x: lo(p), y: hi(p), w: lo(sz), h: hi(sz) }
            }
            code::PIXELS => {
                let n = (h >> 8) as usize;
                Op::Pixels(view_u16(self.data(n.div_ceil(2)), n))
            }
            code::RUN => {
                let [n] = self.args();
                Op::Run { color: b, n: n as usize }
            }
            code::BLEND_SPAN => {
                let (x, y) = self.xy(a);
                let [n] = self.args();
                let n = n as usize;
                Op::BlendSpan { x, y, color: b, alphas: view_u8(self.data(n.div_ceil(4)), n) }
            }
            code::BLEND_ROW => {
                let (x, y) = self.xy(a);
                let [n] = self.args();
                let n = n as usize;
                let colors = view_u16(self.data(n.div_ceil(2)), n);
                let alphas = view_u8(self.data(n.div_ceil(4)), n);
                Op::BlendRow { x, y, colors, alphas }
            }
            code::LINE => {
                let [p0, p1] = self.args();
                let ((x0, y0), (x1, y1)) = (s(p0), s(p1));
                Op::Line { x0, y0, x1, y1, color: b }
            }
            code::LINE_THICK => {
                let [p0, p1, t] = self.args();
                let ((x0, y0), (x1, y1)) = (s(p0), s(p1));
                Op::LineThick { x0, y0, x1, y1, color: b, thickness: t as u16 }
            }
            code::POLYLINE => {
                let [t, n] = self.args();
                let points = view_pts(self.data(n as usize), n as usize);
                Op::Polyline {
                    points,
                    color: b,
                    closed: a & code::CLOSED != 0,
                    thickness: t as u16,
                    hard: a & code::HARD != 0,
                }
            }
            code::WEDGE => {
                let [pa, pb, ra, rb] = self.args();
                let ((ax, ay), (bx, by)) = (s(pa), s(pb));
                Op::Wedge { ax, ay, bx, by, ra_q8: ra as i32, rb_q8: rb as i32, color: b }
            }
            code::PEN_BEGIN => {
                let [t] = self.args();
                Op::PenBegin { color: b, thickness: t as u16 }
            }
            code::PEN_TO => {
                let [p] = self.args();
                let (x, y) = s(p);
                Op::PenTo { x, y }
            }
            code::PEN_FINISH => Op::PenFinish,
            code::PEN_END => Op::PenEnd,
            code::FILL_POLYGON => {
                let [n] = self.args();
                Op::FillPolygon { points: view_pts(self.data(n as usize), n as usize), color: b }
            }
            code::CIRCLE | code::FILL_CIRCLE => {
                let [p, r] = self.args();
                let ((cx, cy), r) = (s(p), r as i16);
                if h as u8 == code::CIRCLE {
                    Op::Circle { cx, cy, r, color: b }
                } else {
                    Op::FillCircle { cx, cy, r, color: b }
                }
            }
            code::ELLIPSE | code::FILL_ELLIPSE => {
                let [p, r] = self.args();
                let ((cx, cy), (rx, ry)) = (s(p), s(r));
                if h as u8 == code::ELLIPSE {
                    Op::Ellipse { cx, cy, rx, ry, color: b }
                } else {
                    Op::FillEllipse { cx, cy, rx, ry, color: b }
                }
            }
            code::STROKE_ELLIPSE => {
                let [p, r, t] = self.args();
                let ((cx, cy), (rx, ry)) = (s(p), s(r));
                Op::StrokeEllipse { cx, cy, rx, ry, thickness: t as i16, color: b }
            }
            code::OVAL => {
                let [p, sz, t] = self.args();
                Op::Oval { x: lo(p) as i16, y: hi(p) as i16, w: lo(sz), h: hi(sz), t: t as u16, color: b }
            }
            code::ARC => {
                let [p, r, se] = self.args();
                let ((cx, cy), (r, ir), (start, end)) = (s(p), s(r), s(se));
                Op::Arc { cx, cy, r, ir, start, end, color: b }
            }
            code::THIN_ARC => {
                let [p, rs, e] = self.args();
                let ((cx, cy), (r, start)) = (s(p), s(rs));
                Op::ThinArc { cx, cy, r, start, end: e as i16, color: b }
            }
            code::ROUNDED_RECT | code::FILL_ROUNDED_RECT => {
                let [p, sz, r] = self.args();
                let (x, y, w, h_, r) = (lo(p), hi(p), lo(sz), hi(sz), r as u16);
                if h as u8 == code::ROUNDED_RECT {
                    Op::RoundedRect { x, y, w, h: h_, r, color: b }
                } else {
                    Op::FillRoundedRect { x, y, w, h: h_, r, color: b }
                }
            }
            code::FILL_ROUNDED_RECT_AT => {
                let [p, sz, r] = self.args();
                Op::FillRoundedRectAt {
                    x: lo(p) as i16,
                    y: hi(p) as i16,
                    w: lo(sz),
                    h: hi(sz),
                    r: r as u16,
                    color: b,
                    alpha: a,
                }
            }
            code::GRADIENT_ROUNDED_RECT_AT => {
                let [p, sz, rc] = self.args();
                Op::GradientRoundedRectAt {
                    x: lo(p) as i16,
                    y: hi(p) as i16,
                    w: lo(sz),
                    h: hi(sz),
                    r: lo(rc),
                    c1: b,
                    c2: hi(rc),
                    dir: a,
                }
            }
            code::STROKE_ROUNDED_RECT => {
                let [p, sz, rt] = self.args();
                Op::StrokeRoundedRect {
                    x: lo(p) as i16,
                    y: hi(p) as i16,
                    w: lo(sz),
                    h: hi(sz),
                    r: lo(rt),
                    t: hi(rt),
                    color: b,
                }
            }
            op => panic!("dl: bad opcode {op} at word {}", self.i - 1),
        })
    }
}

/// A pixel stream cut to the scissor: the window's pixels arrive row-major,
/// the part inside `vis` goes on (as the stream of a window that is `vis`).
#[derive(Default)]
struct Stream {
    /// Window width, and the visible part relative to the window
    /// (x0, y0, x1, y1); empty when nothing shows.
    w: u32,
    vis: (u32, u32, u32, u32),
    /// Pixels of the window received.
    pos: u32,
}

impl Stream {
    fn open<C: Canvas>(c: &C, win: Rect, scissor: Rect) -> Stream {
        let mut s = Stream { w: win.w as u32, vis: (0, 0, 0, 0), pos: 0 };
        if let Some(v) = win.intersection(&scissor) {
            c.begin_pixels(v.x as u16, v.y as u16, v.w, v.h);
            let (x0, y0) = ((v.x - win.x) as u32, (v.y - win.y) as u32);
            s.vis = (x0, y0, x0 + v.w as u32, y0 + v.h as u32);
        }
        s
    }

    /// `n` more pixels: `emit(i, k)` passes on pixels `i..i + k` of them.
    fn feed(&mut self, n: usize, mut emit: impl FnMut(usize, usize)) {
        if self.w == 0 {
            return;
        }
        let (x0, y0, x1, y1) = self.vis;
        let mut i = 0usize;
        while i < n {
            let (col, row) = (self.pos % self.w, self.pos / self.w);
            let k = ((self.w - col) as usize).min(n - i);
            if row >= y0 && row < y1 {
                let a = col.max(x0);
                let b = (col + k as u32).min(x1);
                if b > a {
                    emit(i + (a - col) as usize, (b - a) as usize);
                }
            }
            self.pos += k as u32;
            i += k;
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

    /// Between `begin` and `end`.
    pub fn busy(&self) -> bool {
        self.busy.get()
    }

    /// End the recording (the list stays until the next `begin`).
    pub fn end(&self) {
        self.busy.set(false);
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
    fn l(&self) -> RefMut<'_, List> {
        self.list.borrow_mut()
    }
}

/// The recorder's pen: records `to`/`finish` and, when dropped, the end.
pub struct RecPen<'a> {
    rec: &'a Recorder,
}

impl Pen for RecPen<'_> {
    fn to(&mut self, x: i16, y: i16) {
        self.rec.l().call(&[header(code::PEN_TO, 0, 0), pki(x, y)]);
    }
    fn finish(&mut self) {
        self.rec.l().call(&[header(code::PEN_FINISH, 0, 0)]);
    }
}

impl Drop for RecPen<'_> {
    fn drop(&mut self) {
        self.rec.l().call(&[header(code::PEN_END, 0, 0)]);
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
        self.l().set_scissor(rect);
    }
    fn scissor(&self) -> Option<Rect> {
        self.scissor.get()
    }

    fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        self.l().call(&[header(code::FILL_RECT, 0, color), pk(x, y), pk(w, h)]);
    }
    fn blend_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16, alpha: u8) {
        self.l().call(&[header(code::BLEND_RECT, alpha, color), pk(x, y), pk(w, h)]);
    }
    fn fill_gradient_rect(&self, x: u16, y: u16, w: u16, h: u16, c1: u16, c2: u16, dir: u8, x_off: u16, y_off: u16, full_w: u16, full_h: u16) {
        let mut l = self.l();
        l.call(&[
            header(code::GRADIENT_RECT, dir, c1),
            pk(x, y),
            pk(w, h),
            pk(c2, x_off),
            pk(y_off, full_w),
            full_h as u32,
        ]);
    }
    fn draw_rect(&self, x: u16, y: u16, w: u16, h: u16, color: u16) {
        self.l().call(&[header(code::DRAW_RECT, 0, color), pk(x, y), pk(w, h)]);
    }
    fn draw_pixel(&self, x: u16, y: u16, color: u16) {
        self.l().call(&[header(code::DRAW_PIXEL, 0, color), pk(x, y)]);
    }
    fn begin_pixels(&self, x: u16, y: u16, w: u16, h: u16) {
        self.l().call(&[header(code::BEGIN_PIXELS, 0, 0), pk(x, y), pk(w, h)]);
    }
    fn write_pixel(&self, color: u16) {
        self.l().pixels(&[color]);
    }
    fn write_pixels(&self, colors: &[u16]) {
        self.l().pixels(colors);
    }
    fn write_run(&self, color: u16, n: usize) {
        self.l().call(&[header(code::RUN, 0, color), n as u32]);
    }
    fn blend_span(&self, x: i32, y: i32, color: u16, alphas: &[u8]) {
        let mut l = self.l();
        let n = alphas.len() as u32;
        if List::xy_short(x, y) {
            l.call(&[header(code::BLEND_SPAN, code::SHORT, color), pki(x as i16, y as i16), n]);
        } else {
            l.call(&[header(code::BLEND_SPAN, 0, color), x as u32, y as u32, n]);
        }
        l.data_u8(alphas);
    }
    fn blend_row(&self, x: i32, y: i32, colors: &[u16], alphas: &[u8]) {
        debug_assert_eq!(colors.len(), alphas.len());
        let n = colors.len().min(alphas.len());
        let mut l = self.l();
        if List::xy_short(x, y) {
            l.call(&[header(code::BLEND_ROW, code::SHORT, 0), pki(x as i16, y as i16), n as u32]);
        } else {
            l.call(&[header(code::BLEND_ROW, 0, 0), x as u32, y as u32, n as u32]);
        }
        l.data_u16(&colors[..n]);
        l.data_u8(&alphas[..n]);
    }

    fn draw_line(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16) {
        self.l().call(&[header(code::LINE, 0, color), pki(x0, y0), pki(x1, y1)]);
    }
    fn draw_line_thick(&self, x0: i16, y0: i16, x1: i16, y1: i16, color: u16, thickness: u16) {
        self.l().call(&[header(code::LINE_THICK, 0, color), pki(x0, y0), pki(x1, y1), thickness as u32]);
    }
    fn draw_polyline(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        let mut l = self.l();
        let f = if closed { code::CLOSED } else { 0 };
        l.call(&[header(code::POLYLINE, f, color), thickness as u32, points.len() as u32]);
        l.data_pts(points);
    }
    fn draw_polyline_hard(&self, points: &[Offset], color: u16, closed: bool, thickness: u16) {
        let mut l = self.l();
        let f = code::HARD | if closed { code::CLOSED } else { 0 };
        l.call(&[header(code::POLYLINE, f, color), thickness as u32, points.len() as u32]);
        l.data_pts(points);
    }
    fn draw_wedge(&self, ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16) {
        self.l().call(&[header(code::WEDGE, 0, color), pki(ax, ay), pki(bx, by), ra_q8 as u32, rb_q8 as u32]);
    }
    fn pen(&self, color: u16, thickness: u16) -> RecPen<'_> {
        self.l().call(&[header(code::PEN_BEGIN, 0, color), thickness as u32]);
        RecPen { rec: self }
    }

    fn fill_polygon(&self, points: &[Offset], color: u16) {
        let mut l = self.l();
        l.call(&[header(code::FILL_POLYGON, 0, color), points.len() as u32]);
        l.data_pts(points);
    }
    fn draw_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        self.l().call(&[header(code::CIRCLE, 0, color), pki(cx, cy), r as u16 as u32]);
    }
    fn fill_circle(&self, cx: i16, cy: i16, r: i16, color: u16) {
        self.l().call(&[header(code::FILL_CIRCLE, 0, color), pki(cx, cy), r as u16 as u32]);
    }
    fn draw_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        self.l().call(&[header(code::ELLIPSE, 0, color), pki(cx, cy), pki(rx, ry)]);
    }
    fn fill_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, color: u16) {
        self.l().call(&[header(code::FILL_ELLIPSE, 0, color), pki(cx, cy), pki(rx, ry)]);
    }
    fn stroke_ellipse(&self, cx: i16, cy: i16, rx: i16, ry: i16, thickness: i16, color: u16) {
        self.l().call(&[header(code::STROKE_ELLIPSE, 0, color), pki(cx, cy), pki(rx, ry), thickness as u16 as u32]);
    }
    fn draw_oval(&self, x: i16, y: i16, w: u16, h: u16, t: u16, color: u16) {
        self.l().call(&[header(code::OVAL, 0, color), pki(x, y), pk(w, h), t as u32]);
    }
    fn fill_arc(&self, cx: i16, cy: i16, r: i16, ir: i16, start: i16, end: i16, color: u16) {
        self.l().call(&[header(code::ARC, 0, color), pki(cx, cy), pki(r, ir), pki(start, end)]);
    }
    fn draw_arc(&self, cx: i16, cy: i16, r: i16, start: i16, end: i16, color: u16) {
        self.l().call(&[header(code::THIN_ARC, 0, color), pki(cx, cy), pki(r, start), end as u16 as u32]);
    }
    fn draw_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        self.l().call(&[header(code::ROUNDED_RECT, 0, color), pk(x, y), pk(w, h), r as u32]);
    }
    fn fill_rounded_rect(&self, x: u16, y: u16, w: u16, h: u16, r: u16, color: u16) {
        self.l().call(&[header(code::FILL_ROUNDED_RECT, 0, color), pk(x, y), pk(w, h), r as u32]);
    }
    fn fill_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, color: u16, alpha: u8) {
        self.l().call(&[header(code::FILL_ROUNDED_RECT_AT, alpha, color), pki(x, y), pk(w, h), r as u32]);
    }
    fn fill_gradient_rounded_rect_at(&self, x: i16, y: i16, w: u16, h: u16, r: u16, c1: u16, c2: u16, dir: u8) {
        self.l().call(&[header(code::GRADIENT_ROUNDED_RECT_AT, dir, c1), pki(x, y), pk(w, h), pk(r, c2)]);
    }
    fn stroke_rounded_rect(&self, x: i16, y: i16, w: u16, h: u16, r: u16, t: u16, color: u16) {
        self.l().call(&[header(code::STROKE_ROUNDED_RECT, 0, color), pki(x, y), pk(w, h), pk(r, t)]);
    }
}

/// FNV-1a over what a widget's drawing reads (`render::dl_key`).
pub struct Hasher(u64);

impl Default for Hasher {
    fn default() -> Self {
        Self::new()
    }
}

impl Hasher {
    pub const fn new() -> Self {
        Hasher(0xcbf2_9ce4_8422_2325)
    }
    #[inline]
    pub fn bytes(&mut self, b: &[u8]) {
        for &x in b {
            self.0 = (self.0 ^ x as u64).wrapping_mul(0x0000_0100_0000_01b3);
        }
    }
    #[inline]
    pub fn u8(&mut self, v: u8) {
        self.bytes(&[v]);
    }
    #[inline]
    pub fn u16(&mut self, v: u16) {
        self.bytes(&v.to_le_bytes());
    }
    #[inline]
    pub fn u32(&mut self, v: u32) {
        self.bytes(&v.to_le_bytes());
    }
    #[inline]
    pub fn i32(&mut self, v: i32) {
        self.bytes(&v.to_le_bytes());
    }
    pub fn rect(&mut self, r: Rect) {
        self.i32(r.x as i32);
        self.i32(r.y as i32);
        self.u16(r.w);
        self.u16(r.h);
    }
    pub fn finish(&self) -> u64 {
        self.0
    }
}

/// Bytes the cache may hold, and the largest list it keeps.
pub const CACHE_BYTES: usize = 32 << 20;
pub const ENTRY_BYTES: usize = 4 << 20;
/// Lists kept at most; looks remembered as seen (forgotten when full).
pub const CACHE_ENTRIES: usize = 4096;
pub const SEEN_MAX: usize = 8192;

struct Entry {
    list: List,
    bytes: usize,
    used: u32,
}

/// Display lists by look (`render::dl_key`). A look is recorded on its
/// second sighting (`seen`): one drawn once (an animation's frames) costs
/// no recording. Least recently used lists go first past the caps.
#[derive(Default)]
pub struct Cache {
    map: BTreeMap<u64, Entry>,
    seen: BTreeSet<u64>,
    bytes: usize,
    clock: u32,
}

impl Cache {
    pub const fn new() -> Self {
        Cache { map: BTreeMap::new(), seen: BTreeSet::new(), bytes: 0, clock: 0 }
    }

    pub fn clear(&mut self) {
        self.map.clear();
        self.seen.clear();
        self.bytes = 0;
        BYTES.store(0, Ordering::Relaxed);
    }

    /// The list for `key`, marked used.
    pub fn get(&mut self, key: u64) -> Option<&List> {
        self.clock = self.clock.wrapping_add(1);
        let clock = self.clock;
        let e = self.map.get_mut(&key)?;
        e.used = clock;
        count(&HITS);
        Some(&e.list)
    }

    /// The list for `key`, as is (no use mark, not counted: dumps).
    pub fn peek(&self, key: u64) -> Option<&List> {
        self.map.get(&key).map(|e| &e.list)
    }

    /// Whether `key` was met (dumps; nothing remembered).
    pub fn was_seen(&self, key: u64) -> bool {
        self.seen.contains(&key)
    }

    /// Whether `key` was met before (and remember it). A miss counts as a
    /// direct draw.
    pub fn seen(&mut self, key: u64) -> bool {
        if self.seen.contains(&key) {
            return true;
        }
        if self.seen.len() >= SEEN_MAX {
            self.seen.clear();
        }
        self.seen.insert(key);
        count(&DIRECT);
        false
    }

    /// Keep `list` for `key`, evicting the least recently used past the caps.
    pub fn insert(&mut self, key: u64, list: List) {
        let bytes = list.heap_bytes() + core::mem::size_of::<Entry>() + 32;
        if bytes > ENTRY_BYTES {
            return;
        }
        while !self.map.is_empty() && (self.bytes + bytes > CACHE_BYTES || self.map.len() >= CACHE_ENTRIES) {
            let old = *self.map.iter().min_by_key(|(_, e)| e.used).unwrap().0;
            if let Some(e) = self.map.remove(&old) {
                self.bytes -= e.bytes;
            }
            count(&EVICTS);
        }
        self.clock = self.clock.wrapping_add(1);
        if let Some(e) = self.map.insert(key, Entry { list, bytes, used: self.clock }) {
            self.bytes -= e.bytes;
        }
        self.bytes += bytes;
        count(&STORES);
        BYTES.store(self.bytes as u32, Ordering::Relaxed);
    }

    pub fn len(&self) -> usize {
        self.map.len()
    }

    pub fn is_empty(&self) -> bool {
        self.map.is_empty()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every kind of call, recorded and decoded back, with the arguments'
    /// extremes (negative coordinates, i32 positions past i16, odd slice
    /// lengths, merged and split pixel writes).
    #[test]
    fn every_call_decodes_to_what_was_recorded() {
        let r = Recorder::new();
        let pts = [Offset { x: -3, y: 4 }, Offset { x: 1200, y: -800 }, Offset { x: 7, y: 7 }];
        r.set_scissor(Some(Rect::new(-5, 6, 70, 80)));
        r.fill_rect(1, 2, 3, 4, 0xF800);
        r.blend_rect(5, 6, 7, 8, 0x07E0, 200);
        r.fill_gradient_rect(1, 2, 3, 4, 5, 6, 1, 7, 8, 9, 10);
        r.draw_rect(9, 8, 7, 6, 0x1234);
        r.draw_pixel(65535, 0, 0xFFFF);
        r.begin_pixels(10, 20, 3, 2);
        r.write_pixel(1);
        r.write_pixels(&[2, 3, 4]);
        r.write_pixel(5);
        r.write_run(0xABCD, 100_000);
        r.write_pixels(&[6, 7]);
        r.blend_span(-4, 3, 0x0F0F, &[1, 2, 3, 4, 5]);
        r.blend_span(70_000, -40_000, 0x0F0F, &[9]);
        r.blend_row(-1, -2, &[10, 11, 12], &[13, 14, 15]);
        r.blend_row(i32::MIN, i32::MAX, &[], &[]);
        r.draw_line(-1, -2, 3, 4, 5);
        r.draw_line_thick(-1, -2, 3, 4, 5, 6);
        r.draw_polyline(&pts, 9, true, 2);
        r.draw_polyline_hard(&pts[..2], 9, false, 1);
        r.draw_wedge(-1, 2, -3, 4, -500, 70_000, 6);
        {
            let mut p = r.pen(0x1111, 3);
            p.to(-7, 8);
            p.finish();
        }
        r.fill_polygon(&pts, 0x2222);
        r.draw_circle(-1, 2, 3, 4);
        r.fill_circle(-1, 2, 3, 4);
        r.draw_ellipse(-1, 2, 3, 4, 5);
        r.fill_ellipse(-1, 2, 3, 4, 5);
        r.stroke_ellipse(-1, 2, 3, 4, -5, 6);
        r.draw_oval(-1, -2, 3, 4, 5, 6);
        r.fill_arc(-1, 2, 3, 4, -90, 270, 7);
        r.draw_arc(-1, 2, 3, -90, 270, 7);
        r.draw_rounded_rect(1, 2, 3, 4, 5, 6);
        r.fill_rounded_rect(1, 2, 3, 4, 5, 6);
        r.fill_rounded_rect_at(-1, -2, 3, 4, 5, 6, 7);
        r.fill_gradient_rounded_rect_at(-1, -2, 3, 4, 5, 6, 7, 1);
        r.stroke_rounded_rect(-1, -2, 3, 4, 5, 6, 7);
        r.set_scissor(None);

        let want = [
            Op::SetScissor(Some(Rect::new(-5, 6, 70, 80))),
            Op::FillRect { x: 1, y: 2, w: 3, h: 4, color: 0xF800 },
            Op::BlendRect { x: 5, y: 6, w: 7, h: 8, color: 0x07E0, alpha: 200 },
            Op::GradientRect { x: 1, y: 2, w: 3, h: 4, c1: 5, c2: 6, dir: 1, x_off: 7, y_off: 8, full_w: 9, full_h: 10 },
            Op::DrawRect { x: 9, y: 8, w: 7, h: 6, color: 0x1234 },
            Op::DrawPixel { x: 65535, y: 0, color: 0xFFFF },
            Op::BeginPixels { x: 10, y: 20, w: 3, h: 2 },
            Op::Pixels(&[1, 2, 3, 4, 5]),
            Op::Run { color: 0xABCD, n: 100_000 },
            Op::Pixels(&[6, 7]),
            Op::BlendSpan { x: -4, y: 3, color: 0x0F0F, alphas: &[1, 2, 3, 4, 5] },
            Op::BlendSpan { x: 70_000, y: -40_000, color: 0x0F0F, alphas: &[9] },
            Op::BlendRow { x: -1, y: -2, colors: &[10, 11, 12], alphas: &[13, 14, 15] },
            Op::BlendRow { x: i32::MIN, y: i32::MAX, colors: &[], alphas: &[] },
            Op::Line { x0: -1, y0: -2, x1: 3, y1: 4, color: 5 },
            Op::LineThick { x0: -1, y0: -2, x1: 3, y1: 4, color: 5, thickness: 6 },
            Op::Polyline { points: &pts, color: 9, closed: true, thickness: 2, hard: false },
            Op::Polyline { points: &pts[..2], color: 9, closed: false, thickness: 1, hard: true },
            Op::Wedge { ax: -1, ay: 2, bx: -3, by: 4, ra_q8: -500, rb_q8: 70_000, color: 6 },
            Op::PenBegin { color: 0x1111, thickness: 3 },
            Op::PenTo { x: -7, y: 8 },
            Op::PenFinish,
            Op::PenEnd,
            Op::FillPolygon { points: &pts, color: 0x2222 },
            Op::Circle { cx: -1, cy: 2, r: 3, color: 4 },
            Op::FillCircle { cx: -1, cy: 2, r: 3, color: 4 },
            Op::Ellipse { cx: -1, cy: 2, rx: 3, ry: 4, color: 5 },
            Op::FillEllipse { cx: -1, cy: 2, rx: 3, ry: 4, color: 5 },
            Op::StrokeEllipse { cx: -1, cy: 2, rx: 3, ry: 4, thickness: -5, color: 6 },
            Op::Oval { x: -1, y: -2, w: 3, h: 4, t: 5, color: 6 },
            Op::Arc { cx: -1, cy: 2, r: 3, ir: 4, start: -90, end: 270, color: 7 },
            Op::ThinArc { cx: -1, cy: 2, r: 3, start: -90, end: 270, color: 7 },
            Op::RoundedRect { x: 1, y: 2, w: 3, h: 4, r: 5, color: 6 },
            Op::FillRoundedRect { x: 1, y: 2, w: 3, h: 4, r: 5, color: 6 },
            Op::FillRoundedRectAt { x: -1, y: -2, w: 3, h: 4, r: 5, color: 6, alpha: 7 },
            Op::GradientRoundedRectAt { x: -1, y: -2, w: 3, h: 4, r: 5, c1: 6, c2: 7, dir: 1 },
            Op::StrokeRoundedRect { x: -1, y: -2, w: 3, h: 4, r: 5, t: 6, color: 7 },
            Op::SetScissor(None),
        ];
        let l = r.list();
        let got: Vec<Op<'_>> = l.ops().collect();
        assert_eq!(got.len(), want.len());
        for (g, w) in got.iter().zip(want.iter()) {
            assert_eq!(g, w);
        }
        assert_eq!(l.len(), want.len());
        assert_eq!(l.end_scissor(Some(Rect::new(0, 0, 1, 1))), None);
        assert!(l.compact() == *l);
        // FillRect: a header and two words, not a padded enum.
        assert_eq!(&l.words[3..6], &[header(code::FILL_RECT, 0, 0xF800), pk(1, 2), pk(3, 4)]);
    }
}
