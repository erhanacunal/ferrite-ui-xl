//! The GPU player (`render::MODE_AREAS`): a frame recorded on the VM's core in compositor
//! order (each changed area after its `Op::Area` marker, every widget touching it back to
//! front, the drawing cut by the scissor) played by the compositor: each call becomes the
//! pieces the compositor would make of it ([`Prim`]: solids, anti-aliased triangles from
//! `tess`, glyph quads from the atlas, picture / image textures), cut to the scissor in force
//! (itself cut to the area), and the areas are drawn as frames like [`compose`]'s.
//!
//! What maps: rects (fill, blend, gradient, outline, pixel), lines / thick lines / wedges /
//! polylines / pens (graph curves), convex polygon fills, circles, ovals, arcs, rounded rect
//! fills and rings, images, text. Anything else (pixel streams, coverage rows, concave
//! polygons, the other ellipse calls), drawing outside an area, or a text glyph the atlas
//! can't take: declined, nothing drawn; the CPU replays the list (`render::run_job`).
//!
//! The pixels are the compositor's, not the CPU's (anti-aliasing within a few 1/255).

use super::*;
use core::sync::atomic::{AtomicU32, Ordering::Relaxed};
use ferrite_core::dl::{List, Op};

/// Frames offered ([`play`]): played, declined for a call it can't draw (the last one's
/// opcode: [`PLAY_LAST_OP`]), for drawing outside the areas, as small areas (`SMALL`), for
/// another reason (off, portrait, keyboard, no GPU or shaders), a GPU error.
pub static PLAY_COUNTS: [AtomicU32; 6] = [const { AtomicU32::new(0) }; 6];
pub const PLAY_NAMES: [&str; 6] = ["played", "unsupported", "outside", "small", "other", "gpu_error"];
const PLAYED: usize = 0;
const UNSUPPORTED: usize = 1;
const OUTSIDE: usize = 2;
const SMALL_AREAS: usize = 3;
const OTHER: usize = 4;
const FAILED: usize = 5;
/// The opcode of the last call declined (`dl::OP_NAMES`).
pub static PLAY_LAST_OP: AtomicU32 = AtomicU32::new(0);

fn verdict(slot: usize) -> bool {
    PLAY_COUNTS[slot].fetch_add(1, Relaxed);
    slot == PLAYED
}

/// `x, y, w, h` as an `R`.
fn rect(x: i32, y: i32, w: i32, h: i32) -> R {
    R { x0: x, y0: y, x1: x + w, y1: y + h }
}

/// Plays `list` on the GPU; false (nothing drawn) to have it replayed on the CPU.
pub fn play(ctx: &Ctx<P>, list: &List) -> bool {
    if !ENABLED.load(Relaxed) || ctx.lcd.is_portrait() {
        return verdict(OTHER);
    }
    // The areas, and nothing drawn outside them.
    let mut areas: Vec<Rect> = Vec::new();
    let mut open = false;
    for op in list.ops() {
        match op {
            Op::Area(r) if r.is_empty() => open = false,
            Op::Area(r) => {
                areas.push(r);
                open = true;
            }
            Op::SetScissor(_) => {}
            _ if !open => return verdict(OUTSIDE),
            _ => {}
        }
    }
    if areas.is_empty() {
        return verdict(OUTSIDE);
    }
    let rs: Vec<R> = areas.iter().map(R::of).collect();
    let frames: i32 = rs.iter().map(frames_of).sum();
    let kpx: i32 = rs.iter().map(|a| a.w() * a.h() / 1000).sum();
    if frames > FREE_FRAMES.load(Relaxed) as i32 && kpx < MIN_FRAME_KPX.load(Relaxed) as i32 * frames {
        return verdict(SMALL_AREAS);
    }
    if let Some(o) = ctx.objs.overlay
        && areas.iter().any(|a| a.intersects(&o))
    {
        return verdict(OTHER);
    }
    let Some(shared) = crate::gpu::get() else { return verdict(OTHER) };
    let Some(ui) = shared.ui else { return verdict(OTHER) };
    let gpu = &mut shared.gpu;
    let t0 = now_us();
    let s = state();
    if s.white == 0 {
        match white_texel() {
            Some(addr) => s.white = addr,
            None => return verdict(OTHER),
        }
    }
    s.prims.clear();
    s.items.clear();
    s.scratch.clear();
    s.tverts.clear();
    s.glyphs = GLYPHS.load(Relaxed) && s.atlas.begin();
    s.images.render = s.images.render.wrapping_add(1);
    s.any_565 = ui.any_565_stride;
    s.areas.clear();
    s.areas.extend_from_slice(&rs);
    let built = s.calls(ctx, list);
    add_time(T_COLLECT, now_us() - t0);
    let result = match built {
        Err(op) => {
            PLAY_LAST_OP.store(op as u32, Relaxed);
            UNSUPPORTED
        }
        Ok(()) if s.draw(&ui, gpu, &areas) => PLAYED,
        Ok(()) => FAILED,
    };
    s.scratch.clear();
    add_time(T_COMPOSE, now_us() - t0);
    verdict(result)
}

impl State {
    /// The list's calls as pieces. Err(the opcode) for a call it can't draw.
    fn calls(&mut self, ctx: &Ctx<P>, list: &List) -> Result<(), u8> {
        let mut area = R { x0: 0, y0: 0, x1: 0, y1: 0 };
        let mut clip = area;
        // The pen being drawn (colour, thickness) and its points.
        let mut pen: Option<(u16, u16)> = None;
        let mut pts = core::mem::take(&mut self.gpts);
        pts.clear();
        let mut result = Ok(());
        for op in list.ops() {
            let ok = match op {
                Op::Area(r) => {
                    area = R::of(&r);
                    clip = area;
                    self.items.push(Item { start: self.prims.len(), opaque: None });
                    true
                }
                Op::SetScissor(r) => {
                    clip = match r {
                        Some(r) => R::of(&r).meet(&area),
                        None => area,
                    };
                    true
                }
                Op::FillRect { x, y, w, h, color } => {
                    self.solid(rect(x as i32, y as i32, w as i32, h as i32).meet(&clip), Lin::flat(rgb(color, 1.0)));
                    true
                }
                Op::DrawPixel { x, y, color } => {
                    self.solid(rect(x as i32, y as i32, 1, 1).meet(&clip), Lin::flat(rgb(color, 1.0)));
                    true
                }
                Op::BlendRect { x, y, w, h, color, alpha } => {
                    let fill = Lin::flat(rgb(color, alpha as f32 / 255.0));
                    self.solid(rect(x as i32, y as i32, w as i32, h as i32).meet(&clip), fill);
                    true
                }
                Op::GradientRect { x, y, w, h, c1, c2, dir, x_off, y_off, full_w, full_h } => {
                    // A window at (x_off, y_off) into the whole gradient.
                    let g = rect(x as i32 - x_off as i32, y as i32 - y_off as i32, full_w as i32, full_h as i32);
                    let fill = Lin::gradient(&g, c1, c2, dir);
                    self.solid(rect(x as i32, y as i32, w as i32, h as i32).meet(&clip), fill);
                    true
                }
                Op::DrawRect { x, y, w, h, color } => {
                    self.frame_rect(rect(x as i32, y as i32, w as i32, h as i32), 1, color, &clip);
                    true
                }
                Op::Line { x0, y0, x1, y1, color } => {
                    self.stroke(x0, y0, x1, y1, 128, 128, color, &clip);
                    true
                }
                Op::LineThick { x0, y0, x1, y1, color, thickness } => {
                    let hw = thickness.max(1) as i32 * 128;
                    self.stroke(x0, y0, x1, y1, hw, hw, color, &clip);
                    true
                }
                Op::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color } => {
                    self.stroke(ax, ay, bx, by, ra_q8, rb_q8, color, &clip);
                    true
                }
                Op::Polyline { points, color, closed, thickness, .. } => {
                    if points.len() >= 2 {
                        let f: Vec<(f32, f32)> = points.iter().map(|p| (p.x as f32 + 0.5, p.y as f32 + 0.5)).collect();
                        let r = thickness.max(1) as f32 / 2.0;
                        let closed = closed && f.len() > 2;
                        self.tess_in(Lin::flat(rgb(color, 1.0)), Some(clip), |m| tess::outline(m, &f, r, closed));
                    }
                    true
                }
                Op::PenBegin { color, thickness } => {
                    pen = Some((color, thickness));
                    pts.clear();
                    true
                }
                Op::PenTo { x, y } => {
                    pts.push((x as i32, y as i32));
                    true
                }
                Op::PenFinish => true,
                Op::PenEnd => {
                    if let Some((color, t)) = pen.take()
                        && pts.len() >= 2
                    {
                        let color = Lin::flat(rgb(color, 1.0));
                        if t <= 1 {
                            let within = LINE_WITHIN.load(Relaxed) as f32 / 100.0;
                            self.tess_in(color, Some(clip), |m| tess::polyline_within(m, &pts, within));
                        } else {
                            let f: Vec<(f32, f32)> = pts.iter().map(|&(x, y)| (x as f32 + 0.5, y as f32 + 0.5)).collect();
                            self.tess_in(color, Some(clip), |m| tess::outline(m, &f, t as f32 / 2.0, false));
                        }
                    }
                    pts.clear();
                    true
                }
                Op::FillPolygon { points, color } => {
                    points.len() < 3 || {
                        let f: Vec<(f32, f32)> = points.iter().map(|p| (p.x as f32 + 0.5, p.y as f32 + 0.5)).collect();
                        let tverts = self.tverts.len();
                        self.tchunks.clear();
                        if tess::convex_fill(&mut tess::Mesh { verts: &mut self.tverts, chunks: &mut self.tchunks }, &f) {
                            self.emit(Lin::flat(rgb(color, 1.0)), Some(clip));
                            true
                        } else {
                            // Not convex (a battery's bolt): the CPU draws it into a sprite.
                            self.tverts.truncate(tverts);
                            let (mut b, mut any) = (R { x0: i32::MAX, y0: i32::MAX, x1: i32::MIN, y1: i32::MIN }, false);
                            for p in points {
                                (b.x0, b.y0) = (b.x0.min(p.x as i32 - 1), b.y0.min(p.y as i32 - 1));
                                (b.x1, b.y1) = (b.x1.max(p.x as i32 + 2), b.y1.max(p.y as i32 + 2));
                                any = true;
                            }
                            any && self.cpu(ctx, b.meet(&clip), || ctx.lcd.fill_polygon(points, color))
                        }
                    }
                }
                // `smooth_ring` without a sweep: an `Arc` part over the whole turn.
                Op::Circle { cx, cy, r, color } => {
                    self.ring(cx, cy, r, r, None, color, &clip);
                    true
                }
                Op::FillCircle { cx, cy, r, color } => {
                    self.ring(cx, cy, r, 0, None, color, &clip);
                    true
                }
                Op::Arc { cx, cy, r, ir, start, end, color } => {
                    self.ring(cx, cy, r, ir, Some((start, end)), color, &clip);
                    true
                }
                Op::Oval { x, y, w, h, t, color } => {
                    if w > 0 && h > 0 {
                        self.part(RoundPart::Oval { x, y, w, h, t, color }, clip);
                    }
                    true
                }
                Op::FillRoundedRectAt { x, y, w, h, r, color, alpha } => {
                    let box_ = rect(x as i32, y as i32, w as i32, h as i32);
                    let fill = Lin::flat(rgb(color, alpha as f32 / 255.0));
                    if r == 0 {
                        self.solid(box_.meet(&clip), fill);
                    } else if TESS.load(Relaxed) && r as u32 >= TESS_MIN_RADIUS.load(Relaxed) {
                        let key = RoundPart::RoundRect { x: 0, y: 0, w, h, r, t: 0, color: 0 };
                        let (w, h, r) = (w as i32, h as i32, r as i32);
                        self.tess_cached_at(key, (x as i32, y as i32), fill, Some(clip), |m| tess::rounded_fill(m, 0, 0, w, h, r));
                    } else {
                        let start = self.prims.len();
                        self.rounded(&box_, r as i32, 0, fill, [0.0; 4]);
                        self.clip_from(start, &clip);
                    }
                    true
                }
                Op::StrokeRoundedRect { x, y, w, h, r, t, color } => {
                    let box_ = rect(x as i32, y as i32, w as i32, h as i32);
                    if t == 0 || w == 0 || h == 0 {
                    } else if r == 0 {
                        self.frame_rect(box_, t as i32, color, &clip);
                    } else if TESS.load(Relaxed) && r as u32 >= TESS_MIN_RADIUS.load(Relaxed) {
                        let key = RoundPart::RoundRect { x: 0, y: 0, w, h, r, t, color: 0 };
                        let (w, h, r, t) = (w as i32, h as i32, r as i32, t as i32);
                        let color = Lin::flat(rgb(color, 1.0));
                        self.tess_cached_at(key, (x as i32, y as i32), color, Some(clip), |m| tess::rounded_ring(m, 0, 0, w, h, r, t));
                    } else {
                        let start = self.prims.len();
                        self.rounded(&box_, r as i32, t as i32, Lin::CLEAR, rgb(color, 1.0));
                        self.clip_from(start, &clip);
                    }
                    true
                }
                Op::Image { addr, x, y } => match ctx.images.find_addr(addr) {
                    Some(img) => match self.image(ctx, img) {
                        Some((tex, shift, _)) => {
                            let at = rect(x as i32, y as i32, img.width as i32, img.height as i32);
                            let shown = at.meet(&clip);
                            if !shown.is_empty() {
                                self.flat(shown, x as i32, y as i32, tex, shift);
                            }
                            true
                        }
                        None => false,
                    },
                    // Not loaded here: nothing (the CPU replay counts it).
                    None => true,
                },
                Op::Text { font_id, bitmaps, x, y, fg, bg, text } => match ctx.fonts.find(font_id) {
                    Some(font) if font.bitmap_addr() == bitmaps => self.glyphs && self.text(ctx, font, text, x, y, fg, bg, &clip),
                    _ => true,
                },
                _ => false,
            };
            if !ok {
                result = Err(op.code());
                break;
            }
        }
        self.gpts = pts;
        result
    }

    /// What `f` draws on the CPU inside `r`, from a sprite drawn for this frame only. False
    /// without the memory.
    fn cpu(&mut self, ctx: &Ctx<P>, r: R, f: impl FnOnce()) -> bool {
        if r.is_empty() {
            return true;
        }
        let t = now_us();
        let Some(s) = Sprite::draw_at(ctx, &r.rect(), f) else { return false };
        sprite_time(KIND_POLYGON, now_us() - t);
        let tex = Tex { addr: s.pixels() as usize, w: s.w, h: s.h, stride: s.stride * 4, fmt: Fmt::Argb };
        self.scratch.push(s);
        self.flat(r, r.x0, r.y0, tex, 0);
        true
    }

    /// The pieces from `start` on cut to `clip` (`rounded` lays a box out whole). Solids and
    /// `ui_round` squares are affine in the screen position: cutting their rect is enough.
    fn clip_from(&mut self, start: usize, clip: &R) {
        let mut keep = start;
        for i in start..self.prims.len() {
            let mut p = self.prims[i];
            let shown = match &mut p {
                Prim::Round { rect, .. } | Prim::Flat { rect, .. } => {
                    *rect = rect.meet(clip);
                    !rect.is_empty()
                }
                // `rounded` makes none.
                Prim::Tris { .. } => true,
            };
            if shown {
                self.prims[keep] = p;
                keep += 1;
            }
        }
        self.prims.truncate(keep);
    }

    /// `draw_rect` / a square-cornered `stroke_rounded_rect`: the box's border `t` wide.
    fn frame_rect(&mut self, r: R, t: i32, color: u16, clip: &R) {
        if r.is_empty() {
            return;
        }
        let fill = Lin::flat(rgb(color, 1.0));
        let strips = [
            R { x0: r.x0, y0: r.y0, x1: r.x1, y1: (r.y0 + t).min(r.y1) },
            R { x0: r.x0, y0: (r.y1 - t).max(r.y0 + t), x1: r.x1, y1: r.y1 },
            R { x0: r.x0, y0: r.y0 + t, x1: (r.x0 + t).min(r.x1), y1: r.y1 - t },
            R { x0: (r.x1 - t).max(r.x0 + t), y0: r.y0 + t, x1: r.x1, y1: r.y1 - t },
        ];
        for s in strips {
            self.solid(s.meet(clip), fill);
        }
    }

    /// `draw_wedge` (`draw_line` = radii ½ px, `draw_line_thick(t)` = t/2).
    #[allow(clippy::too_many_arguments)]
    fn stroke(&mut self, ax: i16, ay: i16, bx: i16, by: i16, ra_q8: i32, rb_q8: i32, color: u16, clip: &R) {
        self.part(RoundPart::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color }, *clip);
    }

    /// `smooth_ring(cx, cy, r, r, ir, ir, arc)`: `fill_arc`, or with no sweep a whole circle
    /// (`fill_circle`: ir 0; `draw_circle`: ir = r).
    #[allow(clippy::too_many_arguments)]
    fn ring(&mut self, cx: i16, cy: i16, r: i16, ir: i16, arc: Option<(i16, i16)>, color: u16, clip: &R) {
        if r <= 0 {
            return;
        }
        match arc {
            Some((start, end)) => self.part(RoundPart::Arc { cx, cy, r, ir, start, end, color }, *clip),
            None => {
                // Keyed as an arc no call makes (start past end by a turn and more).
                let part = RoundPart::Arc { cx, cy, r, ir, start: i16::MIN, end: i16::MIN, color };
                let (fx, fy) = (cx as f32 + 0.5, cy as f32 + 0.5);
                let a = r as f32 + 0.5;
                let hole = (ir > 1).then(|| (ir as f32 - 0.5, ir as f32 - 0.5));
                self.tess_cached(part, Lin::flat(rgb(color, 1.0)), *clip, |m| tess::oval(m, fx, fy, a, a, hole, None));
            }
        }
    }

    /// `draw_text`: a quad per glyph from the atlas (over an opaque cell of `bg`), cut to
    /// `clip`. False if a glyph doesn't fit in the atlas.
    #[allow(clippy::too_many_arguments)]
    fn text(&mut self, ctx: &Ctx<P>, font: &ferrite_core::font::Font, text: &[u8], x: i16, y: i16, fg: u16, bg: Option<u16>, clip: &R) -> bool {
        let color = Lin::flat(rgb(fg, 1.0));
        let tex = self.atlas.tex();
        let a = ATLAS as f32;
        let mut ok = true;
        font.for_each_glyph(text, x, y, |cp, g, gx, gy| {
            let (gx, gy) = (gx as i32, gy as i32);
            let at = R { x0: gx, y0: gy, x1: gx + g.width as i32, y1: gy + g.height as i32 };
            let shown = at.meet(clip);
            if !ok || shown.is_empty() {
                return;
            }
            let Some((sx, sy)) = self.atlas.slot(&ctx.flash, font, cp, g, &mut self.cov) else {
                ok = false;
                return;
            };
            if let Some(bg) = bg {
                self.solid(shown, Lin::flat(rgb(bg, 1.0)));
            }
            let u = ((sx as i32 - gx) as f32 / a, 1.0 / a);
            let v = ((sy as i32 - gy) as f32 / a, 1.0 / a);
            self.prims.push(Prim::Flat { rect: shown, u, v, color, tex });
        });
        ok
    }
}
