//! The GPU's drawing (`gpu_compose`): the Mali-400 repaints areas whole, straight into the
//! LCD shadow (or a scan buffer, a layer's surface). The board renders in `render::MODE_AREAS`
//! only: the frames CPU0 records are played by [`play`] (`compose/play.rs`), which turns their
//! calls into the pieces ([`Prim`]) and frames this module draws.
//!
//! **Pieces.** Solid quads; rounded boxes as anti-aliased triangles (`ferrite_core::lcd::tess`,
//! coverage per vertex; [`TESS`] off: the `ui_round` shader's corner squares); label text as
//! one quad per glyph from the glyph atlas ([`Atlas`], white × coverage, times the text
//! colour); raw RGB565 pictures sampled where they are in the flash shadow (`ui_flat`); ovals,
//! wedges and fills from the tessellator; anything else drawn by the CPU into a premultiplied
//! [`Sprite`] for this render only, and sampled; composed layers as one textured quad.
//!
//! **Frames.** The areas are first merged pairwise while one frame over both is estimated
//! cheaper (`merge_areas`: ~0.38 ms fixed per frame, ~11 µs per thousand pixels), and until none
//! overlap. A render that would still take more than `FREE_FRAMES` frames averaging under
//! `MIN_FRAME_KPX` thousand pixels is declined (the CPU replays the list). An area is cut into
//! frames of at most `gpu::MAX_TILES` tiles (full-width strips). A frame starts at the last
//! piece that paints the whole frame opaquely: nothing before it shows. Its pieces, clipped to
//! the frame, are batched while the program and texture stay the same; each batch is one mesh
//! of the frame and one draw.
//!
//! The GPU's anti-aliasing and rounding differ a little from the CPU's, so the pixels are not
//! identical to the core's drawing. (Mode 0's compositor of the widget tree, which made the
//! pieces from the widgets themselves, was removed with the other render modes, 2026-11-17.)

use alloc::vec::Vec;

use exynos_4412_rs::drivers::mali::Filter;
use exynos_4412_rs::gpu::{self, Area, Blend, Frame, Gpu, Target};
use ferrite_core::ctx::Ctx;
use ferrite_core::lcd::tess;
use ferrite_core::render::RoundPart;
use ferrite_core::types::{Color, Rect};
use ferrite_core::widget::{KIND_IMAGE, KIND_POLYGON};

use crate::gpu::Ui;
use crate::lcd;
use crate::platform::Exynos4412Platform as P;
use crate::renderer;

/// Sprite rows are padded to this many pixels and start on a 64-byte boundary (whole cache
/// lines for the clean).
const ROW_ALIGN: usize = 16;

/// What the CPU drew for the player (a call it has no pieces for: [`State::cpu`]):
/// premultiplied ARGB8888, sampled as a texture, kept until the frames are over.
pub(crate) struct Sprite {
    pub(crate) w: usize,
    pub(crate) h: usize,
    /// Pixels per row in `px` (`w` rounded up to `ROW_ALIGN`).
    pub(crate) stride: usize,
    /// The pixels start `start` words into `px` (64-byte aligned).
    px: Vec<u32>,
    start: usize,
}

impl Sprite {
    pub(crate) fn pixels(&self) -> *const u32 {
        self.px[self.start..].as_ptr()
    }

    /// What `f` draws inside `r` (screen coordinates, the primitives cut to `r`) in a new
    /// sprite covering `r`, premultiplied, cleaned for the GPU. None if it cannot get the
    /// memory or `r` is larger than the panel.
    pub(crate) fn draw_at(ctx: &Ctx<P>, r: &Rect, f: impl FnOnce()) -> Option<Self> {
        let (w, h) = (r.w as usize, r.h as usize);
        if w == 0 || h == 0 || w > lcd::PANEL.width || h > lcd::PANEL.height {
            return None;
        }
        let stride = w.next_multiple_of(ROW_ALIGN);
        let words = stride * h + ROW_ALIGN;
        let mut px = Vec::new();
        px.try_reserve_exact(words).ok()?;
        // Transparent: the drawing composites onto it.
        px.resize(words, 0);
        let start = ((px.as_ptr() as usize).next_multiple_of(ROW_ALIGN * 4) - px.as_ptr() as usize) / 4;
        let base = px[start..].as_mut_ptr();
        let t0 = crate::perf::cycles();
        let scissor = ctx.lcd.scissor();
        // SAFETY: `base` has stride * h words in `px`, and only the primitives touch them
        // until `end_sprite`; w, h are within the panel.
        unsafe { lcd::begin_sprite_at(base, w, h, stride, r.x as i32, r.y as i32) };
        ctx.lcd.set_scissor(Some(*r));
        f();
        ctx.lcd.set_scissor(scissor);
        lcd::end_sprite(base, stride * h);
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::SPRITE, crate::perf::cycles().wrapping_sub(t0));
        Some(Self { w, h, stride, px, start })
    }
}

pub mod shaders;
mod play;
pub use play::{PLAY_COUNTS, PLAY_HIST, PLAY_HIST_ON, PLAY_LAST_OP, PLAY_NAMES, play};

const TILE: i32 = 16;
/// Where a piece of a box away from its corners is against them: far inside.
const FAR: f32 = -256.0;

/// A rect by its edges, in screen pixels.
#[derive(Clone, Copy, PartialEq, Eq)]
struct R {
    x0: i32,
    y0: i32,
    x1: i32,
    y1: i32,
}

impl R {
    fn of(r: &Rect) -> Self {
        Self { x0: r.x as i32, y0: r.y as i32, x1: r.x as i32 + r.w as i32, y1: r.y as i32 + r.h as i32 }
    }

    fn is_empty(&self) -> bool {
        self.x0 >= self.x1 || self.y0 >= self.y1
    }

    fn rect(&self) -> Rect {
        Rect::new(self.x0 as i16, self.y0 as i16, self.w() as u16, self.h() as u16)
    }

    fn meet(&self, o: &R) -> R {
        R { x0: self.x0.max(o.x0), y0: self.y0.max(o.y0), x1: self.x1.min(o.x1), y1: self.y1.min(o.y1) }
    }

    fn meets(&self, o: &R) -> bool {
        !self.meet(o).is_empty()
    }

    fn contains(&self, o: &R) -> bool {
        o.x0 >= self.x0 && o.y0 >= self.y0 && o.x1 <= self.x1 && o.y1 <= self.y1
    }

    fn w(&self) -> i32 {
        self.x1 - self.x0
    }

    fn h(&self) -> i32 {
        self.y1 - self.y0
    }
}

/// A colour linear across the screen: `v + dx * x + dy * y` at (x, y), premultiplied.
#[derive(Clone, Copy)]
struct Lin {
    v: [f32; 4],
    dx: [f32; 4],
    dy: [f32; 4],
}

impl Lin {
    const CLEAR: Lin = Lin::flat([0.0; 4]);

    const fn flat(c: [f32; 4]) -> Self {
        Self { v: c, dx: [0.0; 4], dy: [0.0; 4] }
    }

    fn at(&self, x: f32, y: f32) -> [f32; 4] {
        core::array::from_fn(|i| self.v[i] + self.dx[i] * x + self.dy[i] * y)
    }

    /// `draw_widget`'s gradient over `g` from `c1` to `c2`: across (dir 1) or down (else),
    /// pixel centre `i` of `n + 1` at `c1 + (c2 - c1) * i / n` (`lerp_rgb565`).
    fn gradient(g: &R, c1: Color, c2: Color, dir: u8) -> Self {
        let (a, b) = (rgb(c1, 1.0), rgb(c2, 1.0));
        let (start, n) = if dir == 1 { (g.x0, g.w() - 1) } else { (g.y0, g.h() - 1) };
        if n <= 0 {
            return Self::flat(a);
        }
        let step: [f32; 4] = core::array::from_fn(|i| (b[i] - a[i]) / n as f32);
        // At the edge position p: the centre p + 0.5 is pixel p - start.
        let v = core::array::from_fn(|i| a[i] - step[i] * (start as f32 + 0.5));
        if dir == 1 { Self { v, dx: step, dy: [0.0; 4] } } else { Self { v, dx: [0.0; 4], dy: step } }
    }
}

/// RGB565 as premultiplied floats at `alpha` (0..1).
fn rgb(c: Color, alpha: f32) -> [f32; 4] {
    let r = ((c >> 11) & 0x1F) as f32 / 31.0;
    let g = ((c >> 5) & 0x3F) as f32 / 63.0;
    let b = (c & 0x1F) as f32 / 31.0;
    [r * alpha, g * alpha, b * alpha, alpha]
}

/// Texels the GPU samples where they are.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Fmt {
    Argb,
    Rgb565,
}

#[derive(Clone, Copy, PartialEq, Eq)]
struct Tex {
    /// 64-byte aligned.
    addr: usize,
    w: usize,
    h: usize,
    /// Bytes from a line to the next; `mali::TILED` (0) = block-interleaved.
    stride: usize,
    fmt: Fmt,
}

/// A piece of a widget's drawing, in screen pixels.
#[derive(Clone, Copy)]
enum Prim {
    /// `ui_round`: over `rect`, the pixel at x is `qx.0 + qx.1 * x` (and y likewise) from the
    /// nearest corner circle's centre, outwards positive; corner radius, border width; the
    /// fill (inside the border) and the border colour.
    Round { rect: R, qx: (f32, f32), qy: (f32, f32), radius: f32, bw: f32, fill: Lin, border: [f32; 4] },
    /// `ui_flat`: `tex` over `rect`, texel coordinates `u.0 + u.1 * x` (and v likewise),
    /// times `color` (solid colours: the white texel times it).
    Flat { rect: R, u: (f32, f32), v: (f32, f32), color: Lin, tex: Tex },
    /// `ui_cov`: triangles `start..start + len` of `State::tverts` (`ferrite_core::lcd::tess`,
    /// coverage per vertex as the tent of `a`) inside `bbox`, times `color`; `ui_band` for a
    /// circle's edge band (`tess::Chunk::band`: coverage `clamp(a − t²k)`). Not cut to a
    /// frame: the GPU clips them to it.
    Tris { bbox: R, start: u32, len: u32, color: Lin, band: bool },
}

impl Prim {
    fn rect(&self) -> R {
        match *self {
            Prim::Round { rect, .. } | Prim::Flat { rect, .. } => rect,
            Prim::Tris { bbox, .. } => bbox,
        }
    }

    fn batch(&self) -> Batch {
        match *self {
            Prim::Round { .. } => Batch::Round,
            Prim::Flat { tex, .. } => Batch::Flat(tex),
            Prim::Tris { band: false, .. } => Batch::Cov,
            Prim::Tris { band: true, .. } => Batch::Band,
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Batch {
    Round,
    Cov,
    Band,
    Flat(Tex),
}

/// A frame's draw being gathered: the pieces of one batch (index in `prims`, rect inside the
/// frame) and the box around them.
struct Group {
    batch: Batch,
    bbox: R,
    items: Vec<(u32, R)>,
}

/// Where a widget's pieces start in `prims`, and the rect it paints opaquely whole, if any.
struct Item {
    start: usize,
    opaque: Option<R>,
}

/// The compositor's scratch, kept between renders so its vectors keep their room.
struct State {
    prims: Vec<Prim>,
    items: Vec<Item>,
    /// Sprites drawn for this render only (their appearance not seen before, or not complete).
    scratch: Vec<Sprite>,
    pos: Vec<[f32; 2]>,
    shape: Vec<[f32; 4]>,
    arc: Vec<[f32; 4]>,
    fill: Vec<[f32; 4]>,
    border: Vec<[f32; 4]>,
    uv: Vec<[f32; 2]>,
    color: Vec<[f32; 4]>,
    /// `ui_cov`'s `a` (the tessellator's vertex coverage).
    tri_a: Vec<[f32; 1]>,
    /// `ui_band`'s `cov` (a, t, k).
    tri_band: Vec<[f32; 3]>,
    /// Anti-aliased triangles of this render's `Prim::Tris` (`tess`), and the tessellator's
    /// chunks of the shape being added.
    tverts: Vec<tess::Vtx>,
    tchunks: Vec<tess::Chunk>,
    /// Round parts' triangles from earlier renders.
    meshes: MeshCache,
    /// Glyphs of labels, for `ui_flat` text.
    atlas: Atlas,
    /// One glyph's coverage while it is put in the atlas.
    cov: Vec<u8>,
    /// This render draws label text from the atlas ([`GLYPHS`] on, the atlas allocated).
    glyphs: bool,
    /// A graph's curve points, and triangles cut to a box (`tess_in`), kept for their room.
    gpts: Vec<(i32, i32)>,
    /// The areas being gathered (`collect`): a graph makes pieces only where they are.
    areas: Vec<R>,
    cov_tris: Vec<tess::Vtx>,
    /// Images that aren't raw opaque RGB565, decoded into textures.
    images: Images,
    /// Raw opaque RGB565 pictures in the PPs' block layout.
    tiled: TiledPics,
    /// The GPU reads RGB565 rows of any stride (`Ui::any_565_stride`), for this render.
    any_565: bool,
    /// A GPU error was printed: only the first is.
    told: bool,
    /// Drawing into a layer's surface ([`State::draw`]): cleared transparent, not black.
    on_layer: bool,
    /// The GPU player's screen frames may go straight to the back scan buffer
    /// (`lcd::direct_begin`; set by `play` around its screen group).
    may_direct: bool,
    /// The frame's draws while it is gathered (`frame`), kept for their room.
    groups: Vec<Group>,
    /// One opaque white texel (heap, 64-byte aligned, cleaned): solid pieces are `ui_flat`
    /// with it, times their colour. 0 until the first render.
    white: usize,
    /// [`OVERDRAW_ON`]: per cell of the frame, 1 + the last opaque piece covering it whole.
    od_cells: Vec<u32>,
    /// The frame's pieces from its first ([`CULL`]): left out.
    culled: Vec<bool>,
    /// The frame's opaque pieces that may hide others: index, rect in the frame.
    occluders: Vec<(u32, R)>,
}

static mut STATE: State = State {
    prims: Vec::new(),
    items: Vec::new(),
    scratch: Vec::new(),
    pos: Vec::new(),
    shape: Vec::new(),
    arc: Vec::new(),
    fill: Vec::new(),
    border: Vec::new(),
    uv: Vec::new(),
    color: Vec::new(),
    tri_a: Vec::new(),
    tri_band: Vec::new(),
    tverts: Vec::new(),
    tchunks: Vec::new(),
    meshes: MeshCache { entries: Vec::new(), clock: 0 },
    atlas: Atlas::new(),
    cov: Vec::new(),
    glyphs: false,
    gpts: Vec::new(),
    areas: Vec::new(),
    cov_tris: Vec::new(),
    images: Images { entries: Vec::new(), bytes: 0, render: 0 },
    tiled: TiledPics { entries: Vec::new(), bytes: 0 },
    any_565: false,
    told: false,
    on_layer: false,
    may_direct: false,
    groups: Vec::new(),
    white: 0,
    od_cells: Vec::new(),
    culled: Vec::new(),
    occluders: Vec::new(),
};

/// Triangles of round parts (`tess::oval`/`wedge`) by the part, colour aside: the clock
/// face, its dots and the gauges' tracks are tessellated once. Positions are absolute, so a
/// moved shape is a new entry; entries unused for [MESH_KEEP] lookups go once the cache
/// holds [MESH_MAX].
struct MeshCache {
    entries: Vec<MeshEntry>,
    /// Lookups so far: an entry's `used` is the clock at its last one.
    clock: u32,
}

struct MeshEntry {
    part: RoundPart,
    used: u32,
    verts: Vec<tess::Vtx>,
    chunks: Vec<tess::Chunk>,
}

const MESH_MAX: usize = 256;
const MESH_KEEP: u32 = 1024;

impl MeshCache {
    /// The part with its colour left out (the triangles don't depend on it).
    fn key(p: &RoundPart) -> RoundPart {
        let mut k = *p;
        match &mut k {
            RoundPart::Oval { color, .. }
            | RoundPart::Arc { color, .. }
            | RoundPart::Wedge { color, .. }
            | RoundPart::Rect { color, .. }
            | RoundPart::RoundRect { color, .. }
            | RoundPart::Path { color, .. } => *color = 0,
        }
        k
    }

    fn find(&mut self, p: &RoundPart) -> Option<usize> {
        self.clock = self.clock.wrapping_add(1);
        let k = Self::key(p);
        let at = self.entries.iter().position(|e| e.part == k)?;
        self.entries[at].used = self.clock;
        Some(at)
    }

    fn insert(&mut self, p: RoundPart, verts: Vec<tess::Vtx>, chunks: Vec<tess::Chunk>) -> usize {
        if self.entries.len() >= MESH_MAX {
            let now = self.clock;
            self.entries.retain(|e| now.wrapping_sub(e.used) < MESH_KEEP);
            if self.entries.len() >= MESH_MAX {
                self.entries.clear();
            }
        }
        self.entries.push(MeshEntry { part: Self::key(&p), used: self.clock, verts, chunks });
        self.entries.len() - 1
    }
}

/// Images that are not raw opaque RGB565 (compressed, indexed, with alpha, JPEG) decoded
/// once into premultiplied ARGB8888 textures (the image drawn into a sprite target), kept by
/// the image's pixel data address and the flash write generation (a new filesystem may put
/// another image there): every widget showing it samples the same texture, whatever its box.
/// Least recently used ones go past [`IMAGE_BYTES`], never one this render uses.
struct Images {
    entries: Vec<ImgTex>,
    bytes: usize,
    /// Renders so far: an entry's `used` is the render that last sampled it.
    render: u32,
}

struct ImgTex {
    data: u32,
    /// `flash::generation` when decoded.
    generation: u32,
    w: usize,
    h: usize,
    /// Pixels per row (`w` rounded up to 16), the pixels from `start` words into `px`.
    stride: usize,
    px: Vec<u32>,
    start: usize,
    used: u32,
}

const IMAGE_BYTES: usize = 32 << 20;
const IMAGE_ENTRIES: usize = 64;

impl Images {
    /// `img` as a texture (decoded now if not held); None without the memory, or larger
    /// than the panel.
    fn tex(&mut self, ctx: &Ctx<P>, img: &ferrite_core::image::Image) -> Option<Tex> {
        let (w, h) = (img.width as usize, img.height as usize);
        let generation = crate::flash::generation();
        let found = self
            .entries
            .iter()
            .position(|e| e.data == img.data_addr() && (e.w, e.h) == (w, h) && e.generation == generation);
        let at = match found {
            Some(at) => at,
            None => {
                if w == 0 || h == 0 || w > lcd::PANEL.width || h > lcd::PANEL.height {
                    return None;
                }
                let stride = w.next_multiple_of(16);
                let words = stride * h + 16;
                let render = self.render;
                // Decoded from an older flash generation: stale.
                self.entries.retain(|e| e.generation == generation || e.used == render);
                self.bytes = self.entries.iter().map(|e| e.px.len() * 4).sum();
                while self.bytes + words * 4 > IMAGE_BYTES || self.entries.len() >= IMAGE_ENTRIES {
                    let lru = (0..self.entries.len())
                        .filter(|&i| self.entries[i].used != render)
                        .max_by_key(|&i| render.wrapping_sub(self.entries[i].used))?;
                    let old = self.entries.swap_remove(lru);
                    self.bytes -= old.px.len() * 4;
                }
                let mut px = Vec::new();
                px.try_reserve_exact(words).ok()?;
                // Transparent: the image composites onto it.
                px.resize(words, 0u32);
                let start = ((px.as_ptr() as usize).next_multiple_of(64) - px.as_ptr() as usize) / 4;
                let base = px[start..].as_mut_ptr();
                let t = now_us();
                // SAFETY: `base` has stride * h words in `px`, only the primitives touch them
                // until `end_sprite`; w, h are within the panel.
                unsafe { lcd::begin_sprite(base, w, h, stride) };
                img.draw(&ctx.lcd, &ctx.flash, 0, 0);
                lcd::end_sprite(base, stride * h);
                sprite_time(KIND_IMAGE, now_us() - t);
                self.bytes += px.len() * 4;
                self.entries.push(ImgTex { data: img.data_addr(), generation, w, h, stride, px, start, used: render });
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[at];
        e.used = self.render;
        Some(Tex { addr: e.px[e.start..].as_ptr() as usize, w: e.w, h: e.h, stride: e.stride * 4, fmt: Fmt::Argb })
    }
}

/// Raw opaque RGB565 pictures copied into the PPs' block-interleaved layout
/// (`mali::TILED`): sampled linear where they are in the flash shadow, a full-screen
/// background cost the tablet home 1.7 of 5.0 ms of PP time per full repaint, 6.1 MB of bus
/// reads (3x its size) and nearly all texture cache misses (`?pp_cnt`, `?probe`). Copied the
/// first time a render shows one; a flash write (`flash::generation`) makes new copies.
/// Least recently used ones go past [`TILED_BYTES`], never one this render uses.
struct TiledPics {
    entries: Vec<TiledPic>,
    bytes: usize,
}

struct TiledPic {
    /// The picture's texels in the flash shadow, its size, the flash generation copied at.
    src: usize,
    w: usize,
    h: usize,
    generation: u32,
    /// Whole 16x16 blocks from `start` texels into `px` (64-byte aligned).
    px: Vec<u16>,
    start: usize,
    used: u32,
}

const TILED_BYTES: usize = 16 << 20;
const TILED_ENTRIES: usize = 16;

/// Block-interleaved copies of pictures ([`TiledPics`]); HTTP `POST /api/compose?tiled=0|1`.
pub static TILED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Time copying pictures into blocks since boot, us, and how many.
pub static TILED_US: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static TILED_COPIES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);

impl TiledPics {
    /// The picture of `w` x `h` RGB565 texels at `src` (2-byte aligned, CPU-readable) as a
    /// block-interleaved texture, copied now if not held; None without the memory.
    fn tex(&mut self, src: usize, w: usize, h: usize, render: u32) -> Option<Tex> {
        if w == 0 || h == 0 || w > 4096 || h > 4096 {
            return None;
        }
        let generation = crate::flash::generation();
        let found = self.entries.iter().position(|e| (e.src, e.w, e.h, e.generation) == (src, w, h, generation));
        let at = match found {
            Some(at) => at,
            None => {
                let (tw, th) = (w.next_multiple_of(16), h.next_multiple_of(16));
                let len = tw * th + 32;
                // Copies from an older flash generation go first.
                self.entries.retain(|e| e.generation == generation || e.used == render);
                self.bytes = self.entries.iter().map(|e| e.px.len() * 2).sum();
                while self.bytes + len * 2 > TILED_BYTES || self.entries.len() >= TILED_ENTRIES {
                    let lru = (0..self.entries.len())
                        .filter(|&i| self.entries[i].used != render)
                        .max_by_key(|&i| render.wrapping_sub(self.entries[i].used))?;
                    let old = self.entries.swap_remove(lru);
                    self.bytes -= old.px.len() * 2;
                }
                let mut px: Vec<u16> = Vec::new();
                px.try_reserve_exact(len).ok()?;
                px.resize(len, 0);
                let start = ((px.as_ptr() as usize).next_multiple_of(64) - px.as_ptr() as usize) / 2;
                let t = now_us();
                let index: [u8; 256] = core::array::from_fn(|i| exynos_4412_rs::drivers::mali::tiled_index(i as u32 % 16, i as u32 / 16) as u8);
                let blocks_across = tw / 16;
                // SAFETY: the picture's w * h texels in the flash shadow (`picture_addr`).
                let texels = unsafe { core::slice::from_raw_parts(src as *const u16, w * h) };
                let dst = &mut px[start..start + tw * th];
                for y in 0..h {
                    let row = &texels[y * w..(y + 1) * w];
                    let band = (y / 16) * blocks_across * 256;
                    let iy = (y % 16) * 16;
                    for (x, &t) in row.iter().enumerate() {
                        dst[band + (x / 16) * 256 + index[iy + x % 16] as usize] = t;
                    }
                }
                exynos_4412_rs::mmu::clean_dcache_range(dst.as_ptr() as usize, tw * th * 2);
                TILED_US.fetch_add((now_us() - t) as u32, core::sync::atomic::Ordering::Relaxed);
                TILED_COPIES.fetch_add(1, core::sync::atomic::Ordering::Relaxed);
                self.bytes += px.len() * 2;
                self.entries.push(TiledPic { src, w, h, generation, px, start, used: render });
                self.entries.len() - 1
            }
        };
        let e = &mut self.entries[at];
        e.used = render;
        Some(Tex { addr: e.px[e.start..].as_ptr() as usize, w: e.w, h: e.h, stride: exynos_4412_rs::drivers::mali::TILED as usize, fmt: Fmt::Rgb565 })
    }
}

/// Side of the glyph atlas in texels (ARGB8888: 4 MB). The GPU's mediump texture
/// coordinates were checked exact across 1280 texels (`gpu.rs` self-test).
const ATLAS: usize = 1024;

/// Label glyphs as premultiplied white × coverage (ARGB8888), packed in rows: a glyph is put
/// in the first time a render shows it and stays until the atlas is full; then it is emptied
/// at the start of the next render (the frames are synchronous: nothing samples it then). A
/// label whose glyph doesn't fit is drawn from a sprite meanwhile.
struct Atlas {
    /// Heap, 64-byte aligned, `ATLAS`² texels; 0 = not allocated yet, 1 = no memory.
    addr: usize,
    /// (font, codepoint) key → the glyph's top left in the atlas.
    slots: alloc::collections::BTreeMap<u64, (u16, u16)>,
    /// `flash::generation` the slots were filled at: a flash write (a new filesystem may put
    /// other glyphs at the same address) empties the atlas.
    generation: u32,
    /// Where the next glyph goes: the current row's top and next free column, its height.
    x: u16,
    y: u16,
    row_h: u16,
    full: bool,
}

impl Atlas {
    const fn new() -> Self {
        Self { addr: 0, slots: alloc::collections::BTreeMap::new(), generation: 0, x: 0, y: 0, row_h: 0, full: false }
    }

    fn tex(&self) -> Tex {
        Tex { addr: self.addr, w: ATLAS, h: ATLAS, stride: ATLAS * 4, fmt: Fmt::Argb }
    }

    /// Ready for a render: allocated (false if there is no memory), emptied if it filled up.
    fn begin(&mut self) -> bool {
        if self.addr == 0 {
            let layout = core::alloc::Layout::from_size_align(ATLAS * ATLAS * 4, 64).unwrap();
            // SAFETY: a non-zero size; the block is kept for good.
            let p = unsafe { alloc::alloc::alloc_zeroed(layout) };
            self.addr = if p.is_null() { 1 } else { p as usize };
            if !p.is_null() {
                exynos_4412_rs::mmu::clean_dcache_range(self.addr, ATLAS * ATLAS * 4);
            }
        }
        if self.addr == 1 {
            return false;
        }
        let generation = crate::flash::generation();
        if self.full || generation != self.generation {
            self.slots.clear();
            (self.x, self.y, self.row_h, self.full) = (0, 0, 0, false);
            self.generation = generation;
        }
        true
    }

    /// Glyph `g` of `font` (codepoint `cp`) in the atlas: its top left. None if it doesn't fit.
    fn slot(
        &mut self,
        flash: &ferrite_core::flash::FlashImpl<<P as ferrite_core::platform::Platform>::FlashB>,
        font: &ferrite_core::font::Font,
        cp: u16,
        g: &ferrite_core::font::GfxGlyph,
        cov: &mut Vec<u8>,
    ) -> Option<(u16, u16)> {
        let key = (font.bitmap_addr() as u64) << 32 | (font.bpp as u64) << 24 | (font.font_id as u64) << 16 | cp as u64;
        if let Some(&at) = self.slots.get(&key) {
            return Some(at);
        }
        if self.full {
            return None;
        }
        // One texel apart (none would be sampled across with exact coordinates; a margin
        // costs nothing).
        let (w, h) = (g.width as u16 + 1, g.height as u16 + 1);
        if self.x + w > ATLAS as u16 {
            (self.x, self.y, self.row_h) = (0, self.y + self.row_h, 0);
        }
        if self.y + h > ATLAS as u16 || w > ATLAS as u16 {
            self.full = true;
            return None;
        }
        let at = (self.x, self.y);
        self.x += w;
        self.row_h = self.row_h.max(h);
        let n = g.width as usize * g.height as usize;
        cov.clear();
        cov.resize(n, 0);
        font.glyph_coverage(flash, g, cov);
        let gw = g.width as usize;
        for row in 0..g.height as usize {
            let line = self.addr + ((at.1 as usize + row) * ATLAS + at.0 as usize) * 4;
            // SAFETY: inside the atlas block (the glyph fits where it is put).
            let px = unsafe { core::slice::from_raw_parts_mut(line as *mut u32, gw) };
            for (p, &c) in px.iter_mut().zip(&cov[row * gw..(row + 1) * gw]) {
                *p = c as u32 * 0x0101_0101;
            }
            exynos_4412_rs::mmu::clean_dcache_range(line, gw * 4);
        }
        self.slots.insert(key, at);
        Some(at)
    }
}

fn state() -> &'static mut State {
    // SAFETY: CPU0 only, from the render pass, never reentrantly.
    unsafe { &mut *(&raw mut STATE) }
}

/// Off = the passes draw everything. On at boot since 2026-11-10 (board, render per present,
/// compositor vs passes): Clock app 2.0 vs 8.4–10 ms, Monitor stress test 2.2 vs 2.7 ms
/// (after merging areas into fewer frames and `ui_round` for small corners), page slide
/// 9.8–10.2 vs 9.4–9.5 ms. HTTP `POST /api/compose?on=1|0` switches it, for A/B runs and
/// screenshots of the same scene both ways.
pub static ENABLED: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// A piece joins an earlier draw of its program + texture when no piece of the draws in
/// between touches it (the order only matters where pieces overlap), looking back at most
/// [`MERGE_BACK`] draws. Off = only runs of consecutive pieces share a draw, as before: the
/// Monitor stress test's tiles (flat pieces, `ui_round` corners, glyphs, each in turn) made
/// ~45 draws a frame. HTTP `POST /api/compose?merge=0|1` switches it for A/B runs.
pub static MERGE: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// A `ui_round` box (`rounded`) of at most this many pixels is four `ui_round` quarters, each
/// with its corner, not nine pieces (solids + corner squares): the same pixels, as `ui_round`
/// draws the straight parts like the solids, at under half the vertices, for a dearer shader
/// over the box's inside. For the many small boxes of the Monitor stress test's tiles. 0 =
/// always nine pieces; HTTP `POST /api/compose?round_px=N`.
/// The most tiles of one GPU frame (HTTP `?frame_tiles=N`, 512..=`gpu::MAX_FRAME_TILES`): up
/// to `gpu::MAX_TILES` 512 every tile has its own polygon list block, past it tiles share
/// blocks (HAL `mali::Blocking`). Board, Monitor stress, the once-a-second job over most of
/// the screen: 512 13.5 ms, 1024 10.5 ms, 2048 11.4 ms, 4096 11.3 ms (fewer frames, but the
/// PPs slow down as more tiles share a block).
pub static FRAME_TILES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(1024);
/// How far (1/100 px) a graph's curve may pass from a point it drops (`tess::polyline_within`);
/// HTTP `POST /api/compose?line_within=N`.
pub static LINE_WITHIN: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(50);
/// Triangle vertices put into frames, since boot.
pub static TRI_VERTS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
pub static ROUND_WHOLE_PX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2048);
const MERGE_BACK: usize = 12;
/// Rounded boxes as anti-aliased triangles (`ferrite_core::lcd::tess`: coverage per vertex,
/// `ui_flat` two instructions a pixel) rather than `ui_round` corner squares (the shader
/// works the coverage out per pixel, ~a dozen instructions). On by default; HTTP
/// `POST /api/compose?tess=0|1` switches it for A/B runs.
pub static TESS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// With [`TESS`], only corners of at least this radius are triangles; smaller ones stay
/// `ui_round` squares (HTTP `?tess_min=N`). The shader's cost grows with r², the triangles'
/// (CPU building, cache cleans, the GP) with √r: the Monitor stress test's ~120 r = 8 boxes a
/// render took 3.85 ms per present as triangles, 2.0 ms as `ui_round` (passes 2.7 ms).
pub static TESS_MIN_RADIUS: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(12);
/// Label text as glyph quads from the atlas ([`Atlas`]) rather than a sprite of the whole
/// label (drawn by the CPU, and again whenever the text changes). On by default; HTTP
/// `POST /api/compose?glyphs=0|1` switches it for A/B runs.
pub static GLYPHS: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Overdraw measurement (HTTP `POST /api/compose?overdraw=0|1`, off at boot; switching it on
/// zeroes [`OVERDRAW`]): each frame also works out, in 4×4-pixel cells, how much of what it
/// draws ends up under a later opaque piece — what drawing the opaque pieces first with a
/// depth test (Impeller's `DrawOrderResolver`) would leave unshaded. CPU time per frame.
pub static OVERDRAW_ON: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);
/// The names of [`OVERDRAW`]'s slots, pixels summed over frames (`/api/status`
/// `compositor.overdraw`): the frames' pixels; pixels the pieces drawn shade (triangles: their
/// area); of those, pixels of opaque pieces (solids at alpha 1, raw RGB565 pictures); pixels
/// under a later opaque piece (whole cells only, so a little low); pixels of the pieces the
/// frame skips (before the last widget covering it opaquely, `Item::opaque`); pixels of the
/// pieces [`CULL`] leaves out (not in the others).
pub const OVERDRAW_NAMES: [&str; 6] = ["frame_px", "shaded_px", "opaque_px", "hidden_px", "skipped_px", "culled_px"];
pub static OVERDRAW: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
/// `shaded_px` by kind of piece, as [`PROBE`]'s bits: pictures, solids, glyphs, other ARGB
/// textures, triangles, `ui_round`.
pub const OVERDRAW_KIND_NAMES: [&str; 6] = ["pictures", "solids", "glyphs", "argb", "tris", "round"];
pub static OVERDRAW_KIND: [core::sync::atomic::AtomicU64; 6] = [const { core::sync::atomic::AtomicU64::new(0) }; 6];
/// A piece wholly inside a later opaque piece (a solid at alpha 1 or a raw RGB565 picture, at
/// least [`CULL_MIN_PX`] pixels in the frame) is left out of the frame: "over" at alpha 1
/// leaves nothing of what is under it, so the pixels are the same. Board, tablet home: the
/// only such pieces were the background widget's fill under its own picture, a whole screen
/// of the ~2.1 the frames shaded (`?overdraw=1`), which is what drawing opaque pieces first
/// with a depth test would have saved too. HTTP `POST /api/compose?cull=0|1`.
pub static CULL: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Smaller opaque pieces are not checked as occluders (pieces × occluders a frame).
const CULL_MIN_PX: i32 = 4096;
/// The Mali-4xx FP events the PPs count during the compositor's frames (HTTP
/// `POST /api/compose?pp_cnt=A,B`, `off`; [`PP_CNT_OFF`] = that counter off), and their
/// values summed over the PPs and frames since they were set (`/api/status`
/// `compositor.pp_cnt`). Events: 0 active cycles, 2 / 3 bus reads / writes, 9 tile writeback
/// writes, 12 texture cache uncompressed reads, 29 fragments rasterized, 35 instructions
/// completed, 51 / 52 texture cache hits / misses (ARM's list; the board checks them).
pub static PP_CNT_SRC: [core::sync::atomic::AtomicU32; 2] = [const { core::sync::atomic::AtomicU32::new(PP_CNT_OFF) }; 2];
pub static PP_CNT: [core::sync::atomic::AtomicU64; 2] = [const { core::sync::atomic::AtomicU64::new(0) }; 2];
pub const PP_CNT_OFF: u32 = u32::MAX;
/// The CPU writes the GPU frames' memory through the uncached alias of DRAM, so no cache
/// clean precedes the hardware (HAL `Gpu::set_frame_uncached`). HTTP
/// `POST /api/compose?frame_nc=0|1`.
pub static FRAME_NC: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(true);
/// Where building the frames' draws goes (`frame`), µs summed since boot (`/api/status`
/// `compositor.build`): [`CULL`]'s check, putting the pieces into draws, their vertices
/// (`push`), the meshes and draws (`flush`: attribute copies into the frame's memory).
pub const BUILD_NAMES: [&str; 4] = ["cull_us", "group_us", "push_us", "flush_us"];
pub static BUILD: [core::sync::atomic::AtomicU64; 4] = [const { core::sync::atomic::AtomicU64::new(0) }; 4];
const B_CULL: usize = 0;
const B_GROUP: usize = 1;
const B_PUSH: usize = 2;
const B_FLUSH: usize = 3;

fn add_build(slot: usize, us: u64) {
    BUILD[slot].fetch_add(us, core::sync::atomic::Ordering::Relaxed);
}

/// The frames' cache cleans before the hardware (`Times::clean_us`/`clean_bytes`), summed
/// since boot (`/api/status` `compositor.clean_us`/`clean_bytes`).
pub static CLEAN_US: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
pub static CLEAN_BYTES: core::sync::atomic::AtomicU64 = core::sync::atomic::AtomicU64::new(0);
/// Measurement only (HTTP `?probe=MASK`, 0 at boot; the picture is wrong meanwhile): leaves
/// kinds of `ui_flat` pieces out of the frames, to see what the PPs' time goes to. 1 = raw
/// RGB565 pictures, 2 = solids (the white texel), 4 = glyphs (the atlas), 8 = other ARGB
/// textures (sprites, layers, captures, decoded images), 16 = `Prim::Tris`.
pub static PROBE: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(0);
/// Cell side of the overdraw measurement, pixels.
const OD_CELL: i32 = 4;
/// [`ENABLED`] changed: the whole screen is repainted the new way ([`service`]).
pub static REPAINT: core::sync::atomic::AtomicBool = core::sync::atomic::AtomicBool::new(false);

/// Frames a render may take before [`MIN_FRAME_KPX`] applies (HTTP `?free_frames=`).
pub static FREE_FRAMES: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(2);
/// Past [`FREE_FRAMES`] frames, the average frame must cover this many thousand pixels, or
/// the render is declined (HTTP `?min_kpx=`, 0 = never). A frame's fixed cost (~0.38 ms on the
/// board: resets, starting the units, cache cleans, the CPU setting it up) against the PPs'
/// ~11 µs per thousand pixels: the Monitor stress test's ~7.5 frames of ~9 Kpx a render took
/// 5.1 ms per present against the passes' 2.7 ms.
pub static MIN_FRAME_KPX: core::sync::atomic::AtomicU32 = core::sync::atomic::AtomicU32::new(32);

/// A frame's fixed cost and the PPs' cost per thousand pixels, in µs (board, 2026-11-10).
const FRAME_FIXED_US: i32 = 380;
const FRAME_KPX_US: i32 = 11;

/// GPU frames `a` (whole tiles) is cut into: full-width strips of at most `MAX_TILES` tiles.
fn frames_of(a: &R) -> i32 {
    let across = ((a.w() + TILE - 1) / TILE).max(1);
    let lines = ((FRAME_TILES.load(core::sync::atomic::Ordering::Relaxed) as i32 / across).max(1) * TILE).min(a.h().max(TILE));
    (a.h() + lines - 1) / lines
}

/// The estimated cost of `a`'s frames, µs.
fn cost_of(a: &R) -> i32 {
    frames_of(a) * FRAME_FIXED_US + a.w() * a.h() / 1000 * FRAME_KPX_US
}

/// `areas` merged pairwise while one frame over both costs less than theirs apart
/// ([`cost_of`]), and until none overlap.
fn merge_areas(areas: &[Rect]) -> Vec<R> {
    let mut out: Vec<R> = areas.iter().map(R::of).filter(|a| !a.is_empty()).collect();
    loop {
        let mut best: Option<(usize, usize, i32)> = None;
        for i in 0..out.len() {
            for j in i + 1..out.len() {
                let (a, b) = (out[i], out[j]);
                let u = R { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) };
                let gain = if a.meets(&b) { i32::MAX } else { cost_of(&a) + cost_of(&b) - cost_of(&u) };
                if gain > 0 && best.is_none_or(|(_, _, g)| gain > g) {
                    best = Some((i, j, gain));
                }
            }
        }
        let Some((i, j, _)) = best else { return out };
        let b = out.swap_remove(j);
        let a = &mut out[i];
        *a = R { x0: a.x0.min(b.x0), y0: a.y0.min(b.y0), x1: a.x1.max(b.x1), y1: a.y1.max(b.y1) };
    }
}

/// `MODE_AREAS` (`WidgetRenderer::play_areas`): the core's areas merged as [`compose`]
/// merges them, so the recording covers the frames the player will draw.
pub fn play_areas(areas: &[Rect]) -> Vec<Rect> {
    merge_areas(areas).iter().map(R::rect).collect()
}

/// Where the compositor's time goes, summed since boot (HTTP `/api/status`): the names of
/// [`TIMES`]' slots. GPU frames, their draws, pieces and pixels; the HAL's GP / PP / whole-frame
/// times (resets and L2 clear included in the last); CPU time building and running the frames,
/// and in whole `compose` calls (gathering the pieces and drawing sprites included).
/// Then: the pieces' gathering (`collect`, sprites included); sprites drawn by the CPU (cache
/// misses and this-render-only ones) and their time; the slowest such sprite's time and its
/// widget kind (`*_max`: the largest seen since boot). Then the CPU time of starting frames
/// (`Gpu::frame`) and of running them (`Frame::show`: cache cleans + the hardware time).
pub const TIME_NAMES: [&str; 16] = [
    "frames",
    "draws",
    "pieces",
    "kpx",
    "gp_us",
    "pp_us",
    "frame_hw_us",
    "frame_us",
    "compose_us",
    "collect_us",
    "sprites",
    "sprite_us",
    "sprite_max_us",
    "sprite_max_kind",
    "begin_us",
    "show_us",
];
const T_FRAMES: usize = 0;
const T_DRAWS: usize = 1;
const T_PIECES: usize = 2;
const T_KPX: usize = 3;
const T_GP: usize = 4;
const T_PP: usize = 5;
const T_HW: usize = 6;
const T_FRAME: usize = 7;
const T_COMPOSE: usize = 8;
const T_COLLECT: usize = 9;
const T_SPRITES: usize = 10;
const T_SPRITE_US: usize = 11;
const T_SPRITE_MAX: usize = 12;
const T_SPRITE_MAX_KIND: usize = 13;
const T_BEGIN: usize = 14;
const T_SHOW: usize = 15;
pub static TIMES: [core::sync::atomic::AtomicU64; 16] = [const { core::sync::atomic::AtomicU64::new(0) }; 16];

/// A sprite of a widget of `kind` took `us` to draw.
fn sprite_time(kind: u8, us: u64) {
    use core::sync::atomic::Ordering::Relaxed;
    add_time(T_SPRITES, 1);
    add_time(T_SPRITE_US, us);
    if us > TIMES[T_SPRITE_MAX].load(Relaxed) {
        TIMES[T_SPRITE_MAX].store(us, Relaxed);
        TIMES[T_SPRITE_MAX_KIND].store(kind as u64, Relaxed);
    }
}

fn add_time(slot: usize, n: u64) {
    TIMES[slot].fetch_add(n, core::sync::atomic::Ordering::Relaxed);
}

/// Microseconds on the MCT (24 MHz).
fn now_us() -> u64 {
    exynos_4412_rs::drivers::timer::Mct::new().count() / 24
}

/// CPU0, once per pass (`frame_end`): repaints everything after [`ENABLED`] changed.
pub fn service(ctx: &mut Ctx<P>) {
    use core::sync::atomic::Ordering;
    if REPAINT.swap(false, Ordering::AcqRel) {
        let root = ctx.objs.root;
        ctx.objs.mark_dirty(root);
    }
}

impl State {
    /// `img` as a texture: a raw opaque RGB565 picture where it is in the flash shadow (its
    /// first texel `shift` texels into the texture; opaque), else decoded ([`Images`]).
    fn image(&mut self, ctx: &Ctx<P>, img: &ferrite_core::image::Image) -> Option<(Tex, usize, bool)> {
        let (pw, ph) = (img.width as i32, img.height as i32);
        if img.is_raw_opaque()
            && let Some(addr) = renderer::picture_addr(img.data_addr(), (pw * ph * 2) as usize)
        {
            if TILED.load(core::sync::atomic::Ordering::Relaxed)
                && addr % 2 == 0
                && let Some(tex) = self.tiled.tex(addr, pw as usize, ph as usize, self.images.render)
            {
                return Some((tex, 0, true));
            }
            // From the 64-byte boundary before it: `shift` texels to the left.
            let shift = (addr % 64) / 2;
            let stride = pw as usize * 2;
            if addr % 2 == 0 && (self.any_565 || stride % 64 == 0) && pw as usize + shift <= 4096 {
                let tex = Tex { addr: addr - addr % 64, w: pw as usize + shift, h: ph as usize, stride, fmt: Fmt::Rgb565 };
                return Some((tex, shift, true));
            }
        }
        Some((self.images.tex(ctx, img)?, 0, false))
    }

    /// [`State::layer`]'s quad: layer `s`'s surface with its top-left at `r`'s, the part
    /// `shown` (inside `r`), times `alpha`. Also the GPU player's `Op::LayerCompose`.
    fn layer_quad(&mut self, any_565: bool, s: usize, r: &R, shown: &R, alpha: u8) -> bool {
        let shown = shown.meet(r);
        if shown.is_empty() || alpha == 0 {
            return true;
        }
        let (sx, sy) = ((shown.x0 - r.x0) as usize, (shown.y0 - r.y0) as usize);
        let (w, h) = (shown.w() as usize, shown.h() as usize);
        let Some((base, lw, lh)) = lcd::layer_texture(s, sx, sy, w, h) else { return false };
        let stride = lw * 4;
        if sx + w > lw || sy + h > lh || (!any_565 && stride % 64 != 0) {
            return false;
        }
        let addr = base + (sy * lw + sx) * 4;
        let shift = (addr % 64) / 4;
        let tex = Tex { addr: addr - addr % 64, w: w + shift, h, stride, fmt: Fmt::Argb };
        let (tw, th) = (tex.w as f32, tex.h as f32);
        let u = ((shift as f32 - shown.x0 as f32) / tw, 1.0 / tw);
        let v = (-(shown.y0 as f32) / th, 1.0 / th);
        let a = alpha as f32 / 255.0;
        self.prims.push(Prim::Flat { rect: shown, u, v, color: Lin::flat([a; 4]), tex });
        true
    }

    /// `fill` over `rect`: the white texel times it (`ui_flat`, two instructions a pixel,
    /// against `ui_round`'s dozen).
    fn solid(&mut self, rect: R, fill: Lin) {
        if !rect.is_empty() {
            self.prims.push(solid_prim(rect, fill, self.white));
        }
    }

    /// [`tess`](Self::tess), cut to `clip` (as the drawing's scissor would): a chunk reaching
    /// past it is replaced by its clipped triangles (`tess::clip`).
    fn tess_in(&mut self, color: Lin, clip: Option<R>, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        self.tchunks.clear();
        f(&mut tess::Mesh { verts: &mut self.tverts, chunks: &mut self.tchunks });
        self.emit(color, clip);
    }

    /// A round part's triangles (`tess::oval`/`wedge`, `f` adds them), cut to `clip`, from
    /// [`MeshCache`] when the same part was tessellated before (a clock face and its dots stay
    /// put while the hands move: their trigonometry was most of the render's CPU time).
    fn tess_cached(&mut self, part: RoundPart, color: Lin, clip: R, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        self.tess_cached_at(part, (0, 0), color, Some(clip), f);
    }

    /// [`tess_cached`](Self::tess_cached) of a shape `f` adds at the origin, keyed by `part`
    /// (the shape at the origin), moved by `at` when copied: the same box anywhere (a card
    /// sliding with its page) hits the cache.
    fn tess_cached_at(&mut self, part: RoundPart, at: (i32, i32), color: Lin, clip: Option<R>, f: impl FnOnce(&mut tess::Mesh<'_>)) {
        let found = match self.meshes.find(&part) {
            Some(i) => i,
            None => {
                let (mut verts, mut chunks) = (Vec::new(), Vec::new());
                f(&mut tess::Mesh { verts: &mut verts, chunks: &mut chunks });
                self.meshes.insert(part, verts, chunks)
            }
        };
        let e = &self.meshes.entries[found];
        let base = self.tverts.len();
        let (dx, dy) = at;
        if (dx, dy) == (0, 0) {
            self.tverts.extend_from_slice(&e.verts);
        } else {
            let (fx, fy) = (dx as f32, dy as f32);
            self.tverts.extend(e.verts.iter().map(|v| tess::Vtx { x: v.x + fx, y: v.y + fy, ..*v }));
        }
        self.tchunks.clear();
        self.tchunks.extend(e.chunks.iter().map(|c| tess::Chunk {
            start: c.start + base,
            len: c.len,
            x0: c.x0 + dx,
            y0: c.y0 + dy,
            x1: c.x1 + dx,
            y1: c.y1 + dy,
            band: c.band,
        }));
        self.emit(color, clip);
    }

    /// `self.tchunks` (triangles in `self.tverts`) as `Prim::Tris`, cut to `clip`.
    fn emit(&mut self, color: Lin, clip: Option<R>) {
        for i in 0..self.tchunks.len() {
            let c = self.tchunks[i];
            let mut bbox = R { x0: c.x0, y0: c.y0, x1: c.x1, y1: c.y1 };
            let (mut start, mut len) = (c.start, c.len);
            if let Some(k) = clip {
                if !k.contains(&bbox) {
                    if !k.meets(&bbox) {
                        continue;
                    }
                    // The cut triangles go after everything; they refer only to themselves.
                    let at = self.tverts.len();
                    let mut cut = core::mem::take(&mut self.cov_tris);
                    cut.clear();
                    tess::clip(&self.tverts[start..start + len], k.x0 as f32, k.y0 as f32, k.x1 as f32, k.y1 as f32, &mut cut);
                    self.tverts.extend_from_slice(&cut);
                    self.cov_tris = cut;
                    (start, len) = (at, self.tverts.len() - at);
                    bbox = bbox.meet(&k);
                    if len == 0 {
                        continue;
                    }
                }
            }
            self.prims.push(Prim::Tris { bbox, start: start as u32, len: len as u32, color, band: c.band });
        }
    }

    /// One part (`render::RoundPart`) as triangles or a solid, cut to `r`.
    fn part(&mut self, p: RoundPart, r: R) {
        match p {
            RoundPart::Oval { x, y, w, h, t, color } => {
                let (a, b) = (w as f32 / 2.0, h as f32 / 2.0);
                let (cx, cy) = (x as f32 + a, y as f32 + b);
                // `smooth_oval`'s ring has a hole while its inner radii are over ½.
                let t = t as f32;
                let hole = (t > 0.0 && a - t > 0.5 && b - t > 0.5).then_some((a - t, b - t));
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::oval(m, cx, cy, a, b, hole, None));
            }
            RoundPart::Arc { cx, cy, r: rad, ir, start, end, color } => {
                let (fx, fy) = (cx as f32 + 0.5, cy as f32 + 0.5);
                let a = rad as f32 + 0.5;
                let hole = (ir > 1).then(|| (ir as f32 - 0.5, ir as f32 - 0.5));
                let arc = Some((start as f32, end as f32));
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::oval(m, fx, fy, a, a, hole, arc));
            }
            RoundPart::Wedge { ax, ay, bx, by, ra_q8, rb_q8, color } => {
                let (ra, rb) = (ra_q8.clamp(0, 150 << 8) as f32 / 256.0, rb_q8.clamp(0, 150 << 8) as f32 / 256.0);
                let (ax, ay, bx, by) = (ax as f32 + 0.5, ay as f32 + 0.5, bx as f32 + 0.5, by as f32 + 0.5);
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::wedge(m, ax, ay, bx, by, ra, rb));
            }
            RoundPart::Rect { x, y, w, h, color } => {
                let rect = R { x0: x as i32, y0: y as i32, x1: x as i32 + w as i32, y1: y as i32 + h as i32 };
                self.solid(rect.meet(&r), Lin::flat(rgb(color, 1.0)));
            }
            RoundPart::RoundRect { x, y, w, h, r: rad, t, color } => {
                let (x, y, w, h, rad, t) = (x as i32, y as i32, w as i32, h as i32, rad as i32, t as i32);
                let color = Lin::flat(rgb(color, 1.0));
                if t == 0 {
                    self.tess_cached(p, color, r, |m| tess::rounded_fill(m, x, y, w, h, rad));
                } else {
                    self.tess_cached(p, color, r, |m| tess::rounded_ring(m, x, y, w, h, rad, t));
                }
            }
            RoundPart::Path { pts, n, t, color } => {
                let mut f = [(0.0f32, 0.0f32); 3];
                for (q, &(px, py)) in f.iter_mut().zip(&pts[..n as usize]) {
                    *q = (px as f32 + 0.5, py as f32 + 0.5);
                }
                let rad = t as f32 / 2.0;
                self.tess_cached(p, Lin::flat(rgb(color, 1.0)), r, |m| tess::outline(m, &f[..n as usize], rad, false));
            }
        }
    }

    /// A rounded box: nine pieces, the corners `k` square (the radius, or the border if wider,
    /// at most half the box), each knowing where it is from its corner circle's centre. Only
    /// the corners need `ui_round`; the straight pieces' edges lie on pixel boundaries, so they
    /// are solids (border band + fill), which `ui_round` would draw the same, at a fraction of
    /// the shader cost. Solids first, then the corners: no piece overlaps another.
    fn rounded(&mut self, r: &R, radius: i32, bw: i32, fill: Lin, border: [f32; 4]) {
        let radius = radius.min(r.w() / 2).min(r.h() / 2).max(0);
        let kx = radius.max(bw).min(r.w() / 2);
        let ky = radius.max(bw).min(r.h() / 2);
        let cols = [r.x0, r.x0 + kx, r.x1 - kx, r.x1];
        let rows = [r.y0, r.y0 + ky, r.y1 - ky, r.y1];
        let rad = radius as f32;
        // Outwards from the left corners' centre (x0 + radius), the right ones' (x1 - radius).
        let qx = [(r.x0 as f32 + rad, -1.0), (FAR, 0.0), (-(r.x1 as f32 - rad), 1.0)];
        let qy = [(r.y0 as f32 + rad, -1.0), (FAR, 0.0), (-(r.y1 as f32 - rad), 1.0)];
        let bw = bw.max(0);
        let whole = ROUND_WHOLE_PX.load(core::sync::atomic::Ordering::Relaxed) as i32;
        if radius > 0 && r.w() * r.h() <= whole {
            // A small box: its four quarters, each `ui_round` with its corner's distances
            // (the straight parts come out of the shader as the solids below would).
            let (cx, cy) = (r.x0 + r.w() / 2, r.y0 + r.h() / 2);
            let (xs, ys) = ([r.x0, cx, r.x1], [r.y0, cy, r.y1]);
            for row in 0..2 {
                for col in 0..2 {
                    let rect = R { x0: xs[col], y0: ys[row], x1: xs[col + 1], y1: ys[row + 1] };
                    if !rect.is_empty() {
                        self.prims.push(Prim::Round { rect, qx: qx[col * 2], qy: qy[row * 2], radius: rad, bw: bw as f32, fill, border });
                    }
                }
            }
            return;
        }
        let band = Lin::flat(border);
        // Middle column (top, middle, bottom pieces) and middle row (left, right pieces).
        let (mx0, mx1) = (cols[1], cols[2]);
        let (my0, my1) = (rows[1], rows[2]);
        let top_band = (r.y0 + bw).min(my0);
        let bottom_band = (r.y1 - bw).max(my1);
        let left_band = (r.x0 + bw).min(mx0);
        let right_band = (r.x1 - bw).max(mx1);
        // Top piece: border band, then fill; bottom likewise upwards.
        self.solid(R { x0: mx0, y0: r.y0, x1: mx1, y1: top_band }, band);
        self.solid(R { x0: mx0, y0: top_band, x1: mx1, y1: my0 }, fill);
        self.solid(R { x0: mx0, y0: bottom_band, x1: mx1, y1: r.y1 }, band);
        self.solid(R { x0: mx0, y0: my1, x1: mx1, y1: bottom_band }, fill);
        // Left and right pieces, and the middle.
        self.solid(R { x0: r.x0, y0: my0, x1: left_band, y1: my1 }, band);
        self.solid(R { x0: left_band, y0: my0, x1: mx0, y1: my1 }, fill);
        self.solid(R { x0: right_band, y0: my0, x1: r.x1, y1: my1 }, band);
        self.solid(R { x0: mx1, y0: my0, x1: right_band, y1: my1 }, fill);
        self.solid(R { x0: mx0, y0: my0, x1: mx1, y1: my1 }, fill);
        for row in [0, 2] {
            for col in [0, 2] {
                let rect = R { x0: cols[col], y0: rows[row], x1: cols[col + 1], y1: rows[row + 1] };
                if !rect.is_empty() {
                    self.prims.push(Prim::Round { rect, qx: qx[col], qy: qy[row], radius: rad, bw: bw as f32, fill, border });
                }
            }
        }
    }

    /// `tex` over `rect`, its texel `shift` at screen (`x`, `y`) (the texture's first texel
    /// `shift` to the left of it).
    fn flat(&mut self, rect: R, x: i32, y: i32, tex: Tex, shift: usize) {
        let (tw, th) = (tex.w as f32, tex.h as f32);
        let u = ((shift as f32 - x as f32) / tw, 1.0 / tw);
        let v = (-(y as f32) / th, 1.0 / th);
        self.prims.push(Prim::Flat { rect, u, v, color: Lin::flat([1.0; 4]), tex });
    }

    /// Runs the frames of every area into the shadow. False if the GPU failed.
    fn draw(&mut self, ui: &Ui, gpu: &mut Gpu, areas: &[Rect], layer: Option<(usize, Target)>) -> bool {
        let direct = if layer.is_none() && self.may_direct { lcd::direct_begin(areas) } else { None };
        let target = match (layer, direct) {
            (Some((_, t)), _) => t,
            (None, Some((address, stride, width, height))) => Target { address, stride, width, height },
            (None, None) => {
                let Some((address, stride, width, height)) = lcd::gpu_target() else { return false };
                if !lcd::gpu_begin() {
                    return false;
                }
                Target { address, stride, width, height }
            }
        };
        let direct = direct.is_some();
        // SAFETY: the shadow (mapped at the GPU's start; between `gpu_begin` and `gpu_drew`
        // only the GPU writes it), or a layer's (`lcd::layer_gpu_target`: on the heap the GPU
        // maps, nothing else draws it during a render job).
        if unsafe { gpu.set_target(target) }.is_err() {
            if direct {
                lcd::direct_end(false);
            }
            return false;
        }
        self.on_layer = layer.is_some();
        for a in areas {
            let mut a = R::of(a);
            if layer.is_some() {
                // A layer's last rows: its target's rows go on to whole tiles.
                a.x1 = ((a.x1 + TILE - 1) / TILE * TILE).min(target.width as i32);
                a.y1 = ((a.y1 + TILE - 1) / TILE * TILE).min(target.height as i32);
            }
            // Full-width strips of at most MAX_TILES tiles.
            let across = (a.w() + TILE - 1) / TILE;
            let lines = ((FRAME_TILES.load(core::sync::atomic::Ordering::Relaxed) as i32 / across.max(1)).max(1) * TILE).min(a.h().max(TILE));
            let mut y = a.y0;
            while y < a.y1 {
                let c = R { x0: a.x0, y0: y, x1: a.x1, y1: (y + lines).min(a.y1) };
                if layer.is_none() && !direct {
                    lcd::gpu_fence(c.y0 as usize, c.y1 as usize);
                }
                if let Err(e) = self.frame(ui, gpu, &c) {
                    if !self.told {
                        self.told = true;
                        exynos_4412_rs::println!("ferrite: GPU compositor frame failed: {:?}", e);
                    }
                    if direct {
                        lcd::direct_end(false);
                    }
                    return false;
                }
                match layer {
                    None if direct => lcd::direct_drew(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize),
                    None => lcd::gpu_drew(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize),
                    Some((n, _)) => lcd::layer_gpu_drew(n, c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize),
                }
                y = c.y1;
            }
        }
        if direct {
            lcd::direct_end(true);
        }
        true
    }

    /// One frame over `c` (whole tiles, inside the shadow).
    fn frame(&mut self, ui: &Ui, gpu: &mut Gpu, c: &R) -> Result<(), gpu::Error> {
        let t0 = crate::perf::cycles();
        let started = now_us();
        let first = self.items.iter().rposition(|it| it.opaque.is_some_and(|o| o.contains(c))).unwrap_or(0);
        let from = self.items.get(first).map_or(self.prims.len(), |it| it.start);
        let culling = now_us();
        self.cull(from, c);
        add_build(B_CULL, now_us() - culling);
        if OVERDRAW_ON.load(core::sync::atomic::Ordering::Relaxed) {
            self.overdraw(from, c);
        }
        let src = |i: usize| Some(PP_CNT_SRC[i].load(core::sync::atomic::Ordering::Relaxed)).filter(|&s| s != PP_CNT_OFF);
        gpu.set_pp_counters([src(0), src(1)]);
        let nc = FRAME_NC.load(core::sync::atomic::Ordering::Relaxed);
        if gpu.frame_uncached() != nc {
            gpu.set_frame_uncached(nc);
        }
        let mut frame = gpu.frame(Area::new(c.x0 as usize, c.y0 as usize, c.w() as usize, c.h() as usize), if self.on_layer { 0 } else { 0xFF00_0000 });
        add_time(T_BEGIN, now_us() - started);
        let scale = [2.0 / c.w() as f32, 2.0 / c.h() as f32];
        // The pieces into draws: a piece joins the latest draw of its batch unless a piece of
        // a draw after that one touches it (painter's order where they overlap).
        let probe = PROBE.load(core::sync::atomic::Ordering::Relaxed);
        let grouping = now_us();
        let back = if MERGE.load(core::sync::atomic::Ordering::Relaxed) { MERGE_BACK } else { 1 };
        let mut groups = core::mem::take(&mut self.groups);
        let mut used = 0usize;
        for at in from..self.prims.len() {
            let p = self.prims[at];
            let rect = p.rect().meet(c);
            if rect.is_empty() || self.culled[at - from] || (probe != 0 && probe & self.probe_kind(&p) != 0) {
                continue;
            }
            let b = p.batch();
            let mut to = None;
            for g in (used.saturating_sub(back)..used).rev() {
                let group = &groups[g];
                if group.batch == b {
                    to = Some(g);
                    break;
                }
                if group.bbox.meets(&rect) && group.items.iter().any(|(_, r)| r.meets(&rect)) {
                    break;
                }
            }
            let g = to.unwrap_or_else(|| {
                if used == groups.len() {
                    groups.push(Group { batch: b, bbox: rect, items: Vec::new() });
                } else {
                    let group = &mut groups[used];
                    group.batch = b;
                    group.bbox = rect;
                    group.items.clear();
                }
                used += 1;
                used - 1
            });
            let group = &mut groups[g];
            group.bbox = R {
                x0: group.bbox.x0.min(rect.x0),
                y0: group.bbox.y0.min(rect.y0),
                x1: group.bbox.x1.max(rect.x1),
                y1: group.bbox.y1.max(rect.y1),
            };
            group.items.push((at as u32, rect));
            add_time(T_PIECES, 1);
        }
        let pushing = now_us();
        add_build(B_GROUP, pushing - grouping);
        let (mut push_us, mut flush_us) = (0, 0);
        let mut built = Ok(());
        for group in &groups[..used] {
            let t = now_us();
            for &(at, rect) in &group.items {
                let p = self.prims[at as usize];
                self.push(&p, &rect, c);
            }
            let t2 = now_us();
            built = self.flush(ui, &mut frame, group.batch, scale);
            let t3 = now_us();
            push_us += t2 - t;
            flush_us += t3 - t2;
            if built.is_err() {
                break;
            }
        }
        add_build(B_PUSH, push_us);
        add_build(B_FLUSH, flush_us);
        self.groups = groups;
        built?;
        let drew = used != 0;
        if !drew {
            // Nothing there: black (on a layer transparent), as an empty frame would leave the
            // area as it was.
            let a = if self.on_layer { 0.0 } else { 1.0 };
            let black = solid_prim(*c, Lin::flat([0.0, 0.0, 0.0, a]), self.white);
            self.push(&black, c, c);
            self.flush(ui, &mut frame, black.batch(), scale)?;
        }
        let showing = now_us();
        let shown = frame.show();
        add_time(T_SHOW, now_us() - showing);
        add_time(T_FRAMES, 1);
        add_time(T_KPX, (c.w() * c.h()) as u64 / 1000);
        if let Ok(t) = &shown {
            add_time(T_GP, t.gp_us);
            add_time(T_PP, t.pps_us);
            add_time(T_HW, t.total_us);
            CLEAN_US.fetch_add(t.clean_us, core::sync::atomic::Ordering::Relaxed);
            CLEAN_BYTES.fetch_add(t.clean_bytes, core::sync::atomic::Ordering::Relaxed);
            for (sum, n) in PP_CNT.iter().zip(t.pp_counts) {
                sum.fetch_add(n, core::sync::atomic::Ordering::Relaxed);
            }
        }
        add_time(T_FRAME, now_us() - started);
        ferrite_core::perf::add_sub(ferrite_core::perf::phase::G2D, crate::perf::cycles().wrapping_sub(t0));
        shown.map(|_| ())
    }

    /// [`PROBE`]'s bit for `p`.
    fn probe_kind(&self, p: &Prim) -> u32 {
        match *p {
            Prim::Flat { tex, .. } if tex.fmt == Fmt::Rgb565 => 1,
            Prim::Flat { tex, .. } if tex.addr == self.white => 2,
            Prim::Flat { tex, .. } if tex.addr == self.atlas.tex().addr => 4,
            Prim::Flat { .. } => 8,
            Prim::Tris { .. } => 16,
            Prim::Round { .. } => 32,
        }
    }

    /// Whether `p` paints every pixel of its rect opaquely: a solid at alpha 1, or a raw RGB565
    /// picture not faded.
    fn is_opaque(&self, p: &Prim) -> bool {
        match *p {
            Prim::Flat { color, tex, .. } => {
                color.dx[3] == 0.0 && color.dy[3] == 0.0 && color.v[3] >= 1.0 && (tex.addr == self.white || tex.fmt == Fmt::Rgb565)
            }
            _ => false,
        }
    }

    /// [`CULL`]: marks the pieces of frame `c` from `from` that a later opaque piece hides
    /// whole (`culled`).
    fn cull(&mut self, from: usize, c: &R) {
        self.culled.clear();
        self.culled.resize(self.prims.len() - from, false);
        if !CULL.load(core::sync::atomic::Ordering::Relaxed) {
            return;
        }
        let mut occ = core::mem::take(&mut self.occluders);
        occ.clear();
        for at in from..self.prims.len() {
            let p = self.prims[at];
            let r = p.rect().meet(c);
            if !r.is_empty() && r.w() * r.h() >= CULL_MIN_PX && self.is_opaque(&p) {
                occ.push((at as u32, r));
            }
        }
        if !occ.is_empty() {
            for at in from..self.prims.len() {
                let r = self.prims[at].rect().meet(c);
                if r.is_empty() {
                    continue;
                }
                // Occluders after this piece, latest first.
                self.culled[at - from] = occ.iter().rev().take_while(|(i, _)| *i as usize > at).any(|(_, o)| o.contains(&r));
            }
        }
        self.occluders = occ;
    }

    /// [`OVERDRAW_ON`]: the frame `c`'s overdraw into [`OVERDRAW`]; its pieces start at `from`.
    fn overdraw(&mut self, from: usize, c: &R) {
        use core::sync::atomic::Ordering::Relaxed;
        let area = |r: &R| if r.is_empty() { 0u64 } else { (r.w() * r.h()) as u64 };
        let skipped: u64 = self.prims[..from].iter().map(|p| area(&p.rect().meet(c))).sum();
        let cols = ((c.w() + OD_CELL - 1) / OD_CELL) as usize;
        let rows = ((c.h() + OD_CELL - 1) / OD_CELL) as usize;
        let mut cells = core::mem::take(&mut self.od_cells);
        cells.clear();
        cells.resize(cols * rows, 0);
        // The last opaque piece covering each cell whole.
        for at in from..self.prims.len() {
            let p = self.prims[at];
            let r = p.rect().meet(c);
            if r.is_empty() || self.culled[at - from] || !self.is_opaque(&p) {
                continue;
            }
            let x0 = (r.x0 - c.x0 + OD_CELL - 1) / OD_CELL;
            let x1 = (r.x1 - c.x0) / OD_CELL;
            let y0 = (r.y0 - c.y0 + OD_CELL - 1) / OD_CELL;
            let y1 = (r.y1 - c.y0) / OD_CELL;
            for y in y0..y1 {
                let row = y as usize * cols;
                cells[row + x0 as usize..row + x1.max(x0) as usize].fill(at as u32 + 1);
            }
        }
        let (mut shaded, mut opaque, mut hidden, mut culled) = (0u64, 0u64, 0u64, 0u64);
        for at in from..self.prims.len() {
            let p = self.prims[at];
            let r = p.rect().meet(c);
            if r.is_empty() {
                continue;
            }
            if self.culled[at - from] {
                culled += area(&r);
                continue;
            }
            // Pixels of `r` in cells a later opaque piece covers.
            let mut under = 0u64;
            let (x0, x1) = ((r.x0 - c.x0) / OD_CELL, (r.x1 - c.x0 + OD_CELL - 1) / OD_CELL);
            let (y0, y1) = ((r.y0 - c.y0) / OD_CELL, (r.y1 - c.y0 + OD_CELL - 1) / OD_CELL);
            for y in y0..y1 {
                let cy0 = (c.y0 + y * OD_CELL).max(r.y0);
                let cy1 = (c.y0 + (y + 1) * OD_CELL).min(r.y1);
                let row = y as usize * cols;
                for x in x0..x1 {
                    if cells[row + x as usize] > at as u32 + 1 {
                        let cx0 = (c.x0 + x * OD_CELL).max(r.x0);
                        let cx1 = (c.x0 + (x + 1) * OD_CELL).min(r.x1);
                        under += ((cx1 - cx0) * (cy1 - cy0)) as u64;
                    }
                }
            }
            let px = area(&r);
            match p {
                Prim::Tris { bbox, start, len, .. } => {
                    // The triangles' area, the part of the box in the frame of it.
                    let t = &self.tverts[start as usize..(start + len) as usize];
                    let tri: f32 = t.chunks_exact(3).map(|v| ((v[1].x - v[0].x) * (v[2].y - v[0].y) - (v[2].x - v[0].x) * (v[1].y - v[0].y)).abs() * 0.5).sum();
                    let fill = (tri / area(&bbox).max(1) as f32).min(1.0);
                    shaded += (px as f32 * fill) as u64;
                    hidden += (under as f32 * fill) as u64;
                    OVERDRAW_KIND[4].fetch_add((px as f32 * fill) as u64, Relaxed);
                }
                _ => {
                    shaded += px;
                    hidden += under;
                    OVERDRAW_KIND[self.probe_kind(&p).trailing_zeros() as usize].fetch_add(px, Relaxed);
                    if self.is_opaque(&p) {
                        opaque += px;
                    }
                }
            }
        }
        self.od_cells = cells;
        for (slot, n) in [area(c), shaded, opaque, hidden, skipped, culled].into_iter().enumerate() {
            OVERDRAW[slot].fetch_add(n, Relaxed);
        }
    }

    /// Two triangles of `p` over `rect` (inside the frame `c`) into the batch's vertices; a
    /// `Prim::Tris` whole (the GPU clips it to the frame).
    fn push(&mut self, p: &Prim, rect: &R, c: &R) {
        if let Prim::Tris { start, len, color, band, .. } = *p {
            let (start, len) = (start as usize, len as usize);
            TRI_VERTS.fetch_add(len as u32, core::sync::atomic::Ordering::Relaxed);
            let (ox, oy) = (c.x0 as f32, c.y0 as f32);
            for v in &self.tverts[start..start + len] {
                self.pos.push([v.x - ox, v.y - oy]);
                self.color.push(color.at(v.x, v.y));
                if band {
                    self.tri_band.push([v.a, v.t, v.k]);
                } else {
                    self.tri_a.push([v.a]);
                }
            }
            return;
        }
        let corners = [(rect.x0, rect.y0), (rect.x1, rect.y0), (rect.x0, rect.y1), (rect.x1, rect.y0), (rect.x1, rect.y1), (rect.x0, rect.y1)];
        let piece_arc = match *p {
            Prim::Round { radius, bw, .. } => arc(radius, bw),
            _ => [0.0; 4],
        };
        for (x, y) in corners {
            let (fx, fy) = (x as f32, y as f32);
            self.pos.push([(x - c.x0) as f32, (y - c.y0) as f32]);
            match *p {
                Prim::Round { qx, qy, radius, bw, fill, border, .. } => {
                    self.shape.push([qx.0 + qx.1 * fx, qy.0 + qy.1 * fy, radius, bw]);
                    self.arc.push(piece_arc);
                    self.fill.push(fill.at(fx, fy));
                    self.border.push(border);
                }
                Prim::Flat { u, v, color, .. } => {
                    self.uv.push([u.0 + u.1 * fx, v.0 + v.1 * fy]);
                    self.color.push(color.at(fx, fy));
                }
                Prim::Tris { .. } => unreachable!("pushed above"),
            }
        }
    }

    /// The batch's vertices as one mesh of the frame, and its draw.
    fn flush(&mut self, ui: &Ui, frame: &mut Frame<'_>, batch: Batch, scale: [f32; 2]) -> Result<(), gpu::Error> {
        add_time(T_DRAWS, 1);
        let result = match batch {
            Batch::Round => frame
                .mesh(&ui.round)
                .attribute("pos", &self.pos)
                .attribute("shape", &self.shape)
                .attribute("arc", &self.arc)
                .attribute("fill", &self.fill)
                .attribute("border", &self.border)
                .build()
                .map(|mesh| {
                    frame.draw(&mesh).uniform("scale", &scale).blend(Blend::Premultiplied);
                }),
            Batch::Cov => frame
                .mesh(&ui.cov)
                .attribute("pos", &self.pos)
                .attribute("color", &self.color)
                .attribute("a", &self.tri_a)
                .build()
                .map(|mesh| {
                    frame.draw(&mesh).uniform("scale", &scale).blend(Blend::Premultiplied);
                }),
            Batch::Band => frame
                .mesh(&ui.band)
                .attribute("pos", &self.pos)
                .attribute("color", &self.color)
                .attribute("cov", &self.tri_band)
                .build()
                .map(|mesh| {
                    frame.draw(&mesh).uniform("scale", &scale).blend(Blend::Premultiplied);
                }),
            Batch::Flat(t) => {
                let format = match t.fmt {
                    Fmt::Argb => ui.argb,
                    Fmt::Rgb565 => ui.rgb565,
                };
                // SAFETY: sprites and model buffers are heap memory the GPU maps, cleaned when
                // drawn and kept until the frames are over (`scratch`, the cache's `keep_after`);
                // pictures are in the flash shadow, on the heap, cleaned by `picture_addr`.
                unsafe { frame.texture_at(t.addr, t.w, t.h, t.stride, format, Filter::Nearest) }.and_then(|texture| {
                    frame
                        .mesh(&ui.flat)
                        .attribute("pos", &self.pos)
                        .attribute("uv", &self.uv)
                        .attribute("color", &self.color)
                        .build()
                        .map(|mesh| {
                            frame.draw(&mesh).uniform("scale", &scale).texture("t", &texture).blend(Blend::Premultiplied);
                        })
                })
            }
        };
        self.pos.clear();
        self.shape.clear();
        self.arc.clear();
        self.fill.clear();
        self.border.clear();
        self.uv.clear();
        self.color.clear();
        self.tri_a.clear();
        self.tri_band.clear();
        result
    }
}

/// The `arc` attribute of `ui_round` for a corner of `radius` with a border `bw` wide:
/// 1/r, r/2, (r/ri)^2, ri/2 with ri = r - bw; zeros where that circle is under a pixel (the
/// corner, or the border's inner corner, is then square).
pub fn arc(radius: f32, bw: f32) -> [f32; 4] {
    if radius < 1.0 {
        return [0.0; 4];
    }
    let ri = radius - bw;
    if ri < 1.0 {
        return [1.0 / radius, radius / 2.0, 0.0, 0.0];
    }
    let k = radius / ri;
    [1.0 / radius, radius / 2.0, k * k, ri / 2.0]
}

/// `fill` over `rect` as the white texel at `white` times it.
fn solid_prim(rect: R, fill: Lin, white: usize) -> Prim {
    let tex = Tex { addr: white, w: 1, h: 1, stride: 64, fmt: Fmt::Argb };
    Prim::Flat { rect, u: (0.5, 0.0), v: (0.5, 0.0), color: fill, tex }
}

/// One opaque white texel for [`solid_prim`]: a 64-byte heap block, kept for good, cleaned out
/// of the caches for the GPU. None if the heap has no room.
fn white_texel() -> Option<usize> {
    let layout = core::alloc::Layout::from_size_align(64, 64).ok()?;
    // SAFETY: a non-zero size; the block is never freed.
    let p = unsafe { alloc::alloc::alloc(layout) } as *mut u32;
    if p.is_null() {
        return None;
    }
    // SAFETY: 16 words in the block.
    unsafe { core::slice::from_raw_parts_mut(p, 16).fill(0xFFFF_FFFF) };
    exynos_4412_rs::mmu::clean_dcache_range(p as usize, 64);
    Some(p as usize)
}
