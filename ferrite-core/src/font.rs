/// Adafruit GFX compatible bitmap font renderer — sparse (UTF-8) variant
///
/// Two sources:
///   1. Flash: combined resource via load/load_by_id
///   2. Embedded: codepoints + glyph table + bitmap in ROM via from_embedded
///
/// Flash resource layouts:
///
///   V1 (1bpp monochrome, legacy):
///     [0..1]         num_glyphs: u16 LE
///     [2]            y_advance: u8
///     [3]            font_id: u8
///     [4..4+N*2]     codepoints[] (N × u16 LE, sorted ascending)
///     [4+N*2..4+N*9] glyphs[] (N × 7 bytes)
///     [4+N*9..]      bitmap data (1-bit packed, MSB first)
///
///   V2 (2bpp anti-aliased):
///     [0..1]         magic: 0x00, 0x02
///     [2..3]         num_glyphs: u16 LE
///     [4]            y_advance: u8
///     [5]            font_id: u8
///     [6]            bpp: u8  (2 = 4-level AA)
///     [7..7+N*2]     codepoints[] (N × u16 LE, sorted ascending)
///     [7+N*2..7+N*9] glyphs[] (N × 7 bytes)
///     [7+N*9..]      bitmap data (2bpp packed, MSB first; 4 pixels per byte;
///                    each pixel = 2 bits: 0=0%, 1=33%, 2=67%, 3=100%)
///
///   V3 (wide metrics, 1 or 2 bpp; what ferrite_build writes for ferrite-ui-xl devices):
///     [0..1]           magic: 0x00, 0x03
///     [2..3]           num_glyphs: u16 LE
///     [4..5]           y_advance: u16 LE
///     [6]              font_id: u8
///     [7]              bpp: u8 (1 or 2)
///     [8..10]          variants: bold, italic, bold italic font_id (0 = none; the
///                      project's font families, set by ferrite_build; rich text)
///     [11]             reserved (0)
///     [12..12+N*2]     codepoints[] (N × u16 LE, sorted ascending)
///     [12+N*2..12+N*16] glyphs[] (N × 14 bytes)
///     [12+N*16..]      bitmap data (as V1 / V2 for its bpp)
///
/// GfxGlyph on flash, V1/V2 (7 bytes, packed):
///   bitmapOffset: u16, width: u8, height: u8,
///   xAdvance: u8, xOffset: i8, yOffset: i8
/// V3 (14 bytes, LE): bitmapOffset: u32, width: u16, height: u16,
///   xAdvance: u16, xOffset: i16, yOffset: i16
extern crate alloc;

use crate::fs::{Fs, RES_FONT};
use alloc::sync::Arc;
use alloc::vec::Vec;
use crate::flash::{FlashBackend, FlashImpl};
use crate::canvas::Canvas;
use crate::lcd::lerp_rgb565;
use crate::types::Rect;

// Re-exported from ferrite-core — the framework's canonical glyph type.
pub use crate::gfx_glyph::GfxGlyph;

/// Glyph bitmap read buffer size
const BITMAP_BUF_SIZE: usize = 128;

/// Embedded font ID (reserved, user fonts cannot use this)
pub const EMBEDDED_FONT_ID: u8 = 0;

/// Glyph + codepoint storage — ROM reference (embedded) or heap-allocated (flash-loaded).
/// Never changed after loading, so copies share it (`Arc`: the render context's copy of
/// the VM's fonts, `FontList::sync_from`, costs no copy of the tables).
#[derive(Clone)]
enum GlyphData {
    Static {
        glyphs: &'static [GfxGlyph],
        codepoints: &'static [u16],
    },
    Heap {
        glyphs: Arc<[GfxGlyph]>,
        codepoints: Arc<[u16]>,
    },
}

impl GlyphData {
    fn codepoints(&self) -> &[u16] {
        match self {
            GlyphData::Static { codepoints, .. } => codepoints,
            GlyphData::Heap { codepoints, .. } => codepoints,
        }
    }

    fn glyph(&self, index: usize) -> &GfxGlyph {
        match self {
            GlyphData::Static { glyphs, .. } => &glyphs[index],
            GlyphData::Heap { glyphs, .. } => &glyphs[index],
        }
    }
}

/// Font — sparse format, binary search on sorted codepoints[].
#[derive(Clone)]
pub struct Font {
    pub font_id: u8,
    /// Bits per pixel: 1 = monochrome, 2 = 2bpp anti-aliased
    pub bpp: u8,
    y_advance: u16,
    /// Bold, italic, bold italic variants' font ids (0 = none): what rich text draws its
    /// `<b>` / `<i>` parts with (`variant`).
    variants: [u8; 3],
    glyphs: GlyphData,
    /// Absolute flash address of bitmap data
    bitmap_addr: u32,
    /// Bitmap data in memory: ROM (embedded font) or `bitmap_ram`. Null
    /// means bitmaps are read from flash.
    embedded_bitmap: *const u8,
    /// RAM copy of the bitmap data (`Platform::CACHE_FONT_BITMAPS`);
    /// `embedded_bitmap` points into it. None when not cached. Only held
    /// to own the buffer, never read directly. Never written after loading,
    /// so a copy of the font shares it: the copy's `embedded_bitmap` points
    /// into the same buffer, alive while any copy holds it.
    #[allow(dead_code)]
    bitmap_ram: Option<Arc<[u8]>>,
}

// Font only holds a const pointer to read-only data (ROM or the shared
// `bitmap_ram`), so it may move to and be read from another core (the render
// context's copy).
unsafe impl Send for Font {}
unsafe impl Sync for Font {}

impl Font {
    /// Where its glyph bitmaps are in flash: with `font_id` and `bpp`, what tells two loaded
    /// fonts apart (a board renderer keying cached drawings by appearance).
    pub fn bitmap_addr(&self) -> u32 {
        self.bitmap_addr
    }
}

/// Bytes of one glyph record on flash: V1/V2, V3.
const GLYPH_V2: usize = 7;
const GLYPH_V3: usize = 14;

/// A font resource's header, whichever version.
struct Header {
    num_glyphs: usize,
    y_advance: u16,
    font_id: u8,
    bpp: u8,
    /// Header bytes (where the codepoints start).
    len: u32,
    /// Bytes of a glyph record: [`GLYPH_V2`] or [`GLYPH_V3`].
    glyph_bytes: u8,
    /// V3: bold, italic, bold italic variants' font ids (0 = none).
    variants: [u8; 3],
}

impl Header {
    fn read<F: FlashBackend>(flash: &FlashImpl<F>, entry: &crate::fs::ResourceEntry) -> Option<Self> {
        if entry.size < 4 {
            return None;
        }
        let mut h = [0u8; 12];
        let n = 12.min(entry.size as usize);
        flash.read(entry.offset, &mut h[..n]);
        let u16_at = |i: usize| u16::from_le_bytes([h[i], h[i + 1]]);
        // A V1 font's glyph count is in the magic's place: 0x0200 / 0x0300 glyphs would read as
        // V2 / V3. V3 is told apart by its bpp (byte 7 is the high byte of a V1 font's second
        // codepoint, 0 below U+0100) and its tables fitting; V2 never was (128 KB boards).
        if h[0] == 0x00 && h[1] == 0x03 && entry.size >= 12 && matches!(h[7], 1 | 2) {
            let g = u16_at(2) as usize;
            if entry.size as usize >= 12 + g * (2 + GLYPH_V3) {
                return Some(Header {
                    num_glyphs: g,
                    y_advance: u16_at(4),
                    font_id: h[6],
                    bpp: h[7],
                    len: 12,
                    glyph_bytes: GLYPH_V3 as u8,
                    variants: [h[8], h[9], h[10]],
                });
            }
        }
        Some(if h[0] == 0x00 && h[1] == 0x02 && entry.size >= 7 {
            Header { num_glyphs: u16_at(2) as usize, y_advance: h[4] as u16, font_id: h[5], bpp: h[6], len: 7, glyph_bytes: GLYPH_V2 as u8, variants: [0; 3] }
        } else {
            Header { num_glyphs: u16_at(0) as usize, y_advance: h[2] as u16, font_id: h[3], bpp: 1, len: 4, glyph_bytes: GLYPH_V2 as u8, variants: [0; 3] }
        })
    }
}

impl Font {
    fn empty() -> Self {
        Font {
            font_id: 0xFF,
            bpp: 1,
            y_advance: 0,
            variants: [0; 3],
            glyphs: GlyphData::Heap {
                glyphs: Arc::from([]),
                codepoints: Arc::from([]),
            },
            bitmap_addr: 0,
            embedded_bitmap: core::ptr::null(),
            bitmap_ram: None,
        }
    }

    /// Load font from flash by resource name.
    pub fn load<F: FlashBackend>(fs: &Fs, flash: &FlashImpl<F>, name: &[u8]) -> Option<Self> {
        let entry = fs.find(flash, name)?;
        Self::load_from_entry(flash, &entry, false)
    }

    /// Load font from a ResourceEntry (detects V1/V2 format). With
    /// `cache_bitmap` the glyph bitmaps are copied into RAM too (skipped,
    /// falling back to flash reads, if the allocation fails).
    pub fn load_from_entry<F: FlashBackend>(
        flash: &FlashImpl<F>,
        entry: &crate::fs::ResourceEntry,
        cache_bitmap: bool,
    ) -> Option<Self> {
        let h = Header::read(flash, entry)?;
        let (num_glyphs, y_advance, font_id, bpp, hdr_len) = (h.num_glyphs, h.y_advance, h.font_id, h.bpp, h.len);
        let rec = h.glyph_bytes as usize;

        let cp_bytes = (num_glyphs * 2) as u32;
        let gl_bytes = (num_glyphs * rec) as u32;

        if entry.size < hdr_len + cp_bytes + gl_bytes {
            return None;
        }

        // Tables are read in chunks — one flash read per entry is one SPI
        // transaction each on SPI-NOR boards.
        const TABLE_CHUNK: usize = 10; // entries per read (140 B of V3 glyphs)

        let cp_base = entry.offset + hdr_len;
        let mut codepoints = Vec::with_capacity(num_glyphs);
        let mut buf = [0u8; TABLE_CHUNK * GLYPH_V3];
        let mut i = 0;
        while i < num_glyphs {
            let n = (num_glyphs - i).min(TABLE_CHUNK);
            flash.read(cp_base + i as u32 * 2, &mut buf[..n * 2]);
            for c in buf[..n * 2].chunks_exact(2) {
                codepoints.push(u16::from_le_bytes([c[0], c[1]]));
            }
            i += n;
        }

        let gl_base = cp_base + cp_bytes;
        let mut glyphs = Vec::with_capacity(num_glyphs);
        let mut i = 0;
        while i < num_glyphs {
            let n = (num_glyphs - i).min(TABLE_CHUNK);
            flash.read(gl_base + (i * rec) as u32, &mut buf[..n * rec]);
            for g in buf[..n * rec].chunks_exact(rec) {
                let u16_at = |i: usize| u16::from_le_bytes([g[i], g[i + 1]]);
                glyphs.push(if rec == GLYPH_V3 {
                    GfxGlyph {
                        bitmap_offset: u32::from_le_bytes([g[0], g[1], g[2], g[3]]),
                        width: u16_at(4),
                        height: u16_at(6),
                        x_advance: u16_at(8),
                        x_offset: u16_at(10) as i16,
                        y_offset: u16_at(12) as i16,
                    }
                } else {
                    GfxGlyph {
                        bitmap_offset: u16_at(0) as u32,
                        width: g[2] as u16,
                        height: g[3] as u16,
                        x_advance: g[4] as u16,
                        x_offset: g[5] as i8 as i16,
                        y_offset: g[6] as i8 as i16,
                    }
                });
            }
            i += n;
        }

        let bitmap_addr = gl_base + gl_bytes;

        let mut bitmap_ram: Option<Arc<[u8]>> = None;
        if cache_bitmap {
            let len = (entry.offset + entry.size - bitmap_addr) as usize;
            let mut v = Vec::new();
            if len != 0 && v.try_reserve_exact(len).is_ok() {
                v.resize(len, 0);
                flash.read(bitmap_addr, &mut v);
                bitmap_ram = Some(Arc::from(v));
            }
        }
        // The Arc's heap buffer does not move with the Font (or its copies),
        // so the pointer stays valid while one of them holds it.
        let embedded_bitmap = match &bitmap_ram {
            Some(b) => b.as_ptr(),
            None => core::ptr::null(),
        };

        Some(Font {
            font_id,
            bpp,
            y_advance,
            variants: h.variants,
            glyphs: GlyphData::Heap { glyphs: Arc::from(glyphs), codepoints: Arc::from(codepoints) },
            bitmap_addr,
            embedded_bitmap,
            bitmap_ram,
        })
    }

    /// Search flash for a font with the given font_id and load it.
    /// Returns None if font_id is 0 (embedded) or not found in flash.
    pub fn load_by_id<F: FlashBackend>(
        fs: &Fs,
        flash: &FlashImpl<F>,
        font_id: u8,
        cache_bitmap: bool,
    ) -> Option<Self> {
        if font_id == EMBEDDED_FONT_ID {
            return None;
        }
        let count = fs.count_by_kind(flash, RES_FONT);
        for i in 0..count {
            if let Some(entry) = fs.find_nth_by_kind(flash, RES_FONT, i) {
                if Header::read(flash, &entry).is_some_and(|h| h.font_id == font_id) {
                    return Self::load_from_entry(flash, &entry, cache_bitmap);
                }
            }
        }
        None
    }

    /// Create an embedded (ROM) font. Always gets font_id = 0.
    /// Glyphs and codepoints reference ROM directly — zero RAM cost.
    #[inline(never)]
    pub fn from_embedded(
        glyphs: &'static [GfxGlyph],
        codepoints: &'static [u16],
        bitmap: &'static [u8],
        y_advance: u8,
        bpp: u8,
    ) -> Self {
        Font {
            font_id: EMBEDDED_FONT_ID,
            bpp,
            y_advance: y_advance as u16,
            variants: [0; 3],
            glyphs: GlyphData::Static { glyphs, codepoints },
            bitmap_addr: 0,
            embedded_bitmap: bitmap.as_ptr(),
            bitmap_ram: None,
        }
    }

    /// The font id of the variant drawing `style` (`rich::BOLD` | `rich::ITALIC`; other bits
    /// ignored), None for the font itself (plain style, or no such variant). Bold italic
    /// without its own variant takes the bold one, then the italic one.
    pub fn variant(&self, style: u8) -> Option<u8> {
        let [b, i, bi] = self.variants;
        let pick = |id: u8| (id != 0).then_some(id);
        match style & (crate::rich::BOLD | crate::rich::ITALIC) {
            crate::rich::BOLD => pick(b),
            crate::rich::ITALIC => pick(i),
            0 => None,
            _ => pick(bi).or(pick(b)).or(pick(i)),
        }
    }

    /// Line height in pixels
    pub fn line_height(&self) -> u16 {
        self.y_advance
    }

    /// Binary search for glyph by Unicode BMP codepoint.
    fn find_glyph(&self, cp: u16) -> Option<&GfxGlyph> {
        match self.glyphs.codepoints().binary_search(&cp) {
            Ok(idx) => Some(self.glyphs.glyph(idx)),
            Err(_) => None,
        }
    }

    /// The glyph of a BMP codepoint, if the font has it.
    pub fn glyph(&self, cp: u16) -> Option<GfxGlyph> {
        self.find_glyph(cp).copied()
    }

    /// Where `draw_str` puts each glyph of `text` at pen (`x`, baseline `y`): `f(cp, glyph,
    /// left, top)` for every glyph with pixels (no culling). Returns the pen's x after the text.
    pub fn for_each_glyph(&self, text: &[u8], mut x: i16, y: i16, mut f: impl FnMut(u16, &GfxGlyph, i16, i16)) -> i16 {
        let mut pos = 0;
        while let Some(cp) = utf8_next(text, &mut pos) {
            if cp == 0xFFFF {
                continue;
            }
            let Some(g) = self.find_glyph(cp) else { continue };
            if g.width != 0 && g.height != 0 {
                f(cp, g, x + g.x_offset as i16, y + g.y_offset as i16);
            }
            x += g.x_advance as i16;
        }
        x
    }

    /// Glyph `g`'s coverage, one byte a pixel (0 = none … 255 = full, as the blending draws
    /// weigh it), row by row into `out[..width * height]`.
    pub fn glyph_coverage<F: FlashBackend>(&self, flash: &FlashImpl<F>, g: &GfxGlyph, out: &mut [u8]) {
        const LEVELS: [u8; 4] = [0, 85, 170, 255];
        let n = g.width as usize * g.height as usize;
        let per_byte = if self.bpp == 1 { 8 } else { 4 };
        let total = n.div_ceil(per_byte);
        let mut i = 0;
        let mut read = 0;
        while read < total {
            let chunk = (total - read).min(BITMAP_BUF_SIZE);
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, g.bitmap_offset, read as u32, &mut buf[..chunk]);
            for &byte in &buf[..chunk] {
                if self.bpp == 1 {
                    for bit in (0..8).rev() {
                        if i >= n {
                            return;
                        }
                        out[i] = if byte & (1 << bit) != 0 { 255 } else { 0 };
                        i += 1;
                    }
                } else {
                    for shift in [6u8, 4, 2, 0] {
                        if i >= n {
                            return;
                        }
                        out[i] = LEVELS[((byte >> shift) & 3) as usize];
                        i += 1;
                    }
                }
            }
            read += chunk;
        }
    }

    /// Advance width for a BMP codepoint (0 if not in font).
    pub fn char_width_cp(&self, cp: u16) -> u16 {
        self.find_glyph(cp).map(|g| g.x_advance).unwrap_or(0)
    }

    /// Advance width for a char (0 if out of BMP or not in font).
    pub fn char_width(&self, ch: char) -> u16 {
        let cp = ch as u32;
        if cp > 0xFFFF {
            return 0;
        }
        self.char_width_cp(cp as u16)
    }

    /// Calculate string width in pixels (text is UTF-8 encoded).
    pub fn text_width(&self, text: &[u8]) -> u16 {
        let mut w: u16 = 0;
        let mut pos = 0;
        while let Some(cp) = utf8_next(text, &mut pos) {
            if cp != 0xFFFF {
                w = w.saturating_add(self.char_width_cp(cp));
            }
        }
        w
    }

    /// Word wrap: the line of `text` that starts at byte `start` and fits in
    /// `max_w` pixels. Returns `(end, next)`: the line is `text[start..end]`
    /// and the following one starts at `next`. A line ends at a `'\n'`
    /// (skipped), after the last space that fits (skipped), or inside a word
    /// too long for a line (at least one character per line). At the end of
    /// the text `next` is `text.len() + 1`; after a final `'\n'` there is one
    /// more, empty, line.
    pub fn wrap_line(&self, text: &[u8], start: usize, max_w: u16) -> (usize, usize) {
        let mut pos = start;
        let mut w: u16 = 0;
        let mut space: Option<usize> = None;
        while pos < text.len() {
            if text[pos] == b'\n' {
                return (pos, pos + 1);
            }
            let at = pos;
            let cp = match utf8_next(text, &mut pos) {
                Some(cp) => cp,
                None => break,
            };
            let cw = if cp == 0xFFFF { 0 } else { self.char_width_cp(cp) };
            if cp == b' ' as u16 {
                // A space may hang past the edge: the line breaks there.
                space = Some(at);
            } else if w.saturating_add(cw) > max_w && at > start {
                return match space {
                    Some(s) => (s, s + 1),
                    None => (at, at),
                };
            }
            w = w.saturating_add(cw);
        }
        (text.len(), text.len() + 1)
    }

    /// Draw a single Unicode character.
    pub fn draw_char<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        ch: char,
        cx: i16,
        cy: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> u16 {
        let cp = ch as u32;
        if cp > 0xFFFF {
            return 0;
        }
        self.draw_cp(lcd, flash, cp as u16, cx, cy, fg, bg)
    }

    /// Draw a UTF-8 encoded string. Returns the X position after the last glyph.
    pub fn draw_str<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        text: &[u8],
        mut x: i16,
        y: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> i16 {
        let mut pos = 0;
        while let Some(cp) = utf8_next(text, &mut pos) {
            if cp != 0xFFFF {
                x += self.draw_cp(lcd, flash, cp, x, y, fg, bg) as i16;
            }
        }
        x
    }

    // --- Internal: draw one BMP codepoint ---

    fn draw_cp<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        cp: u16,
        cx: i16,
        cy: i16,
        fg: u16,
        bg: Option<u16>,
    ) -> u16 {
        let glyph = match self.find_glyph(cp) {
            Some(g) => g,
            None => return 0,
        };

        if glyph.width == 0 || glyph.height == 0 {
            return glyph.x_advance;
        }

        let draw_x = cx + glyph.x_offset as i16;
        let draw_y = cy + glyph.y_offset as i16;

        // Cull glyphs that fall outside the panel. Use the real screen bounds
        // (not a fixed large constant): the opaque text path streams via
        // `begin_pixels`, which some backends (e.g. the Nextion FPGA bus) do
        // NOT clamp — so a glyph drawn past the right/bottom edge wraps onto
        // the visible area. This is what makes an off-screen "parked" panel's
        // label bleed onto the page that is actually on screen.
        if draw_x + glyph.width as i16 <= 0
            || draw_y + glyph.height as i16 <= 0
            || draw_x >= lcd.width() as i16
            || draw_y >= lcd.height() as i16
        {
            return glyph.x_advance;
        }
        // Also cull against the scissor: labels on readback backends are drawn
        // once per clip rect, so most glyphs of each pass fall outside it —
        // skip them before their bitmap is read (an SPI transfer on some
        // boards) and walked pixel by pixel.
        if let Some(c) = lcd.scissor() {
            let glyph_rect = Rect::new(draw_x, draw_y, glyph.width as u16, glyph.height as u16);
            if !glyph_rect.intersects(&c) {
                return glyph.x_advance;
            }
        }

        let bitmap_offset = glyph.bitmap_offset;
        let width = glyph.width;
        let height = glyph.height;
        let x_advance = glyph.x_advance;

        if self.bpp == 1 {
            // 1bpp: traditional opaque / transparent paths
            if let Some(bg_color) = bg {
                let total_bits = width as u32 * height as u32;
                let total_bytes = (total_bits + 7) / 8;
                self.draw_opaque(
                    lcd,
                    flash,
                    total_bytes,
                    bitmap_offset,
                    width,
                    height,
                    draw_x,
                    draw_y,
                    fg,
                    bg_color,
                );
            } else {
                let total_bits = width as u32 * height as u32;
                let total_bytes = (total_bits + 7) / 8;
                self.draw_transparent(
                    lcd,
                    flash,
                    total_bytes,
                    bitmap_offset,
                    width,
                    height,
                    draw_x,
                    draw_y,
                    fg,
                );
            }
        } else if bg.is_none() && lcd.has_alpha() {
            // 2bpp, transparent: blend each pixel's coverage into the
            // existing framebuffer content (needs readback).
            let total_pixels = width as u32 * height as u32;
            let total_bytes = (total_pixels + 3) / 4;
            self.draw_2bpp_blend(
                lcd,
                flash,
                total_bytes,
                bitmap_offset,
                width,
                height,
                draw_x,
                draw_y,
                fg,
            );
        } else {
            // 2bpp: always blend fg with bg via begin_pixels stream.
            // bg=None without readback falls back to black — callers should
            // pass a known background for correct anti-aliased rendering.
            let bg_color = bg.unwrap_or(0);
            let total_pixels = width as u32 * height as u32;
            let total_bytes = (total_pixels + 3) / 4;
            self.draw_2bpp(
                lcd,
                flash,
                total_bytes,
                bitmap_offset,
                width,
                height,
                draw_x,
                draw_y,
                fg,
                bg_color,
            );
        }

        x_advance
    }

    // --- Bitmap reading (flash or ROM) ---

    fn read_bitmap<F: FlashBackend>(
        &self,
        flash: &FlashImpl<F>,
        glyph_offset: u32,
        byte_offset: u32,
        buf: &mut [u8],
    ) {
        let addr = glyph_offset + byte_offset;
        if !self.embedded_bitmap.is_null() {
            unsafe {
                let src = self.embedded_bitmap.add(addr as usize);
                for i in 0..buf.len() {
                    buf[i] = *src.add(i);
                }
            }
        } else {
            flash.read(self.bitmap_addr + addr, buf);
        }
    }

    // --- 1bpp draw implementations (unchanged) ---

    fn draw_opaque<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u32,
        width: u16,
        height: u16,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
        bg: u16,
    ) {
        lcd.begin_pixels(draw_x as u16, draw_y as u16, width as u16, height as u16);

        let total_bits = width as u32 * height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                for bit in (0..8).rev() {
                    if bit_idx >= total_bits {
                        return;
                    }
                    if byte & (1 << bit) != 0 {
                        lcd.write_pixel(fg);
                    } else {
                        lcd.write_pixel(bg);
                    }
                    bit_idx += 1;
                }
            }

            bytes_read += chunk as u32;
        }
    }

    fn draw_transparent<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u32,
        width: u16,
        height: u16,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
    ) {
        let w = width as u32;
        let total_bits = w * height as u32;
        let mut bit_idx: u32 = 0;
        let mut bytes_read: u32 = 0;
        // Row/column counters instead of `bit_idx % w` / `bit_idx / w`:
        // ARMv5 has no divide instruction (each one is a library call).
        let mut col: i16 = 0;
        let mut py = draw_y;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                for bit in (0..8).rev() {
                    if bit_idx >= total_bits {
                        return;
                    }
                    if byte & (1 << bit) != 0 {
                        let px = draw_x + col;
                        if px >= 0 && py >= 0 {
                            lcd.draw_pixel(px as u16, py as u16, fg);
                        }
                    }
                    bit_idx += 1;
                    col += 1;
                    if col == width as i16 {
                        col = 0;
                        py += 1;
                    }
                }
            }

            bytes_read += chunk as u32;
        }
    }

    // --- 2bpp anti-aliased draw ---

    fn draw_2bpp<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u32,
        width: u16,
        height: u16,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
        bg: u16,
    ) {
        let total_pixels = width as u32 * height as u32;
        // The 4 coverage levels, blended once per glyph instead of per pixel
        // (`lerp_rgb565` divides — a library call on ARMv5).
        let palette = [
            bg,
            lerp_rgb565(bg, fg, 1, 3),
            lerp_rgb565(bg, fg, 2, 3),
            fg,
        ];

        lcd.begin_pixels(draw_x as u16, draw_y as u16, width as u16, height as u16);

        let mut pixel_idx: u32 = 0;
        let mut bytes_read: u32 = 0;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                // 4 pixels per byte, MSB first: bits [7:6]=p0, [5:4]=p1, [3:2]=p2, [1:0]=p3
                for shift in [6u8, 4, 2, 0] {
                    if pixel_idx >= total_pixels {
                        return;
                    }
                    lcd.write_pixel(palette[((byte >> shift) & 0x03) as usize]);
                    pixel_idx += 1;
                }
            }

            bytes_read += chunk as u32;
        }
    }

    /// 2bpp glyph blended over the existing pixels (`LcdBackend::HAS_ALPHA`
    /// backends only); zero-coverage pixels are left untouched.
    fn draw_2bpp_blend<L: Canvas, F: FlashBackend>(
        &self,
        lcd: &L,
        flash: &FlashImpl<F>,
        total_bytes: u32,
        bitmap_offset: u32,
        width: u16,
        height: u16,
        draw_x: i16,
        draw_y: i16,
        fg: u16,
    ) {
        const ALPHA: [u8; 4] = [0, 85, 170, 255];
        let w = width as u32;
        let total_pixels = w * height as u32;
        let mut pixel_idx: u32 = 0;
        let mut bytes_read: u32 = 0;
        // Row/column counters — no per-pixel divide (see draw_transparent).
        let mut col: i16 = 0;
        let mut py = draw_y;
        // One glyph row's coverage, handed over as a span (`blend_span`:
        // one call per row instead of one per pixel). Wider glyphs go in
        // pieces of `SPAN`.
        const SPAN: usize = 64;
        let mut row = [0u8; SPAN];
        let mut n = 0usize;
        let mut span_x = draw_x;

        while bytes_read < total_bytes {
            let chunk = (total_bytes - bytes_read).min(BITMAP_BUF_SIZE as u32) as usize;
            let mut buf = [0u8; BITMAP_BUF_SIZE];
            self.read_bitmap(flash, bitmap_offset, bytes_read, &mut buf[..chunk]);

            for i in 0..chunk {
                let byte = buf[i];
                for shift in [6u8, 4, 2, 0] {
                    if pixel_idx >= total_pixels {
                        return;
                    }
                    row[n] = ALPHA[((byte >> shift) & 0x03) as usize];
                    n += 1;
                    pixel_idx += 1;
                    col += 1;
                    let row_end = col == width as i16;
                    if row_end || n == SPAN {
                        blend_coverage(lcd, span_x, py, fg, &row[..n]);
                        n = 0;
                        span_x = draw_x + col;
                    }
                    if row_end {
                        col = 0;
                        py += 1;
                        span_x = draw_x;
                    }
                }
            }

            bytes_read += chunk as u32;
        }
    }
}

/// Blend a row of glyph coverage at (x, y), without its uncovered ends.
#[inline]
fn blend_coverage<L: Canvas>(lcd: &L, x: i16, y: i16, fg: u16, row: &[u8]) {
    let Some(first) = row.iter().position(|&a| a != 0) else {
        return;
    };
    let last = row.iter().rposition(|&a| a != 0).unwrap_or(first);
    lcd.blend_span(x as i32 + first as i32, y as i32, fg, &row[first..=last]);
}

// ============================================================
// UTF-8 decoder
// ============================================================

/// Decode one UTF-8 codepoint from `bytes` starting at `*pos`.
///
/// Advances `*pos` past the consumed bytes and returns `Some(codepoint)`.
/// Returns `Some(0xFFFF)` for invalid sequences (pos advanced past the bad byte).
/// Returns `None` when `pos >= bytes.len()` (end of input).
///
/// Covers U+0000..U+FFFF (BMP). 4-byte sequences (U+10000+) return 0xFFFF.
pub fn utf8_next(bytes: &[u8], pos: &mut usize) -> Option<u16> {
    if *pos >= bytes.len() {
        return None;
    }
    let b0 = bytes[*pos];
    *pos += 1;

    // 1-byte ASCII
    if b0 & 0x80 == 0 {
        return Some(b0 as u16);
    }

    // 2-byte: 110xxxxx 10xxxxxx
    if b0 & 0xE0 == 0xC0 {
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b1 = bytes[*pos];
        *pos += 1;
        return Some(((b0 as u16 & 0x1F) << 6) | (b1 as u16 & 0x3F));
    }

    // 3-byte: 1110xxxx 10xxxxxx 10xxxxxx
    if b0 & 0xF0 == 0xE0 {
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b1 = bytes[*pos];
        *pos += 1;
        if *pos >= bytes.len() || bytes[*pos] & 0xC0 != 0x80 {
            return Some(0xFFFF);
        }
        let b2 = bytes[*pos];
        *pos += 1;
        return Some(((b0 as u16 & 0x0F) << 12) | ((b1 as u16 & 0x3F) << 6) | (b2 as u16 & 0x3F));
    }

    // 4-byte (out of BMP): consume continuation bytes, return sentinel
    if b0 & 0xF8 == 0xF0 {
        while *pos < bytes.len() && bytes[*pos] & 0xC0 == 0x80 {
            *pos += 1;
        }
    }

    Some(0xFFFF)
}

// ============================================================
// FontList — application-wide font registry
// ============================================================

/// Application-wide font list. Heap-allocated, grows on demand.
pub struct FontList {
    fonts: Vec<Font>,
}

impl FontList {
    pub fn new() -> Self {
        Self { fonts: Vec::new() }
    }

    /// Add a font to the list.
    pub fn add(&mut self, font: Font) -> bool {
        self.fonts.push(font);
        true
    }

    /// Find a loaded font by font_id.
    pub fn find(&self, font_id: u8) -> Option<&Font> {
        self.fonts.iter().find(|f| f.font_id == font_id)
    }

    /// Find a font by font_id. If not already loaded, search flash and load it.
    pub fn find_or_load<F: FlashBackend>(
        &mut self,
        font_id: u8,
        fs: &Fs,
        flash: &FlashImpl<F>,
        cache_bitmap: bool,
    ) -> bool {
        if self.find(font_id).is_some() {
            return true;
        }
        if let Some(font) = Font::load_by_id(fs, flash, font_id, cache_bitmap) {
            // Its family's variants too: rich text picks them while laying out, where
            // nothing loads (the render context only gets fonts loaded here).
            let variants = font.variants;
            self.add(font);
            for v in variants {
                if v != 0 && v != EMBEDDED_FONT_ID {
                    self.find_or_load(v, fs, flash, cache_bitmap);
                }
            }
            return true;
        }
        false
    }

    /// Make this list hold the fonts `other` holds (the render context's copy
    /// of the VM's list): copies the ones missing (their glyph tables and
    /// cached bitmaps shared, not copied); starts over if it holds one
    /// `other` doesn't.
    pub fn sync_from(&mut self, other: &FontList) {
        if self.fonts.len() == other.fonts.len()
            && self.fonts.iter().zip(&other.fonts).all(|(a, b)| a.font_id == b.font_id)
        {
            return;
        }
        if self.fonts.iter().any(|f| other.find(f.font_id).is_none()) {
            self.fonts.clear();
        }
        for f in &other.fonts {
            if self.find(f.font_id).is_none() {
                self.fonts.push(f.clone());
            }
        }
    }

    /// Get the embedded font (font_id = 0). Panics if not added.
    pub fn embedded(&self) -> &Font {
        &self.fonts[0]
    }

    /// Resolve a font_id: try exact match, fall back to embedded (id=0).
    pub fn resolve(&self, font_id: u8) -> Option<&Font> {
        self.find(font_id).or_else(|| self.find(EMBEDDED_FONT_ID))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rom() -> Font {
        Font::from_embedded(
            &crate::embedded_font::GLYPHS,
            &crate::embedded_font::CODEPOINTS,
            &crate::embedded_font::BITMAP,
            crate::embedded_font::Y_ADVANCE,
            crate::embedded_font::BPP,
        )
    }

    /// A copy of a loaded font (the render context's, `FontList::sync_from`)
    /// shares its glyph tables and cached bitmap instead of copying them, and
    /// keeps reading the bitmap after the original is gone.
    #[test]
    fn copies_share_the_loaded_tables_and_bitmap() {
        // V1 font, one glyph 'A' (8×2, advance 9, top 8 px above the baseline).
        let mut img = alloc::vec![1, 0, 10, 5];
        img.extend_from_slice(&65u16.to_le_bytes());
        img.extend_from_slice(&[0, 0, 8, 2, 9, 0, 0xF8]);
        img.extend_from_slice(&[0xFF, 0x81]);
        crate::mock::set_flash_image(img.clone());
        let flash = FlashImpl::with_backend(crate::mock::MockFlash);
        let entry = crate::fs::ResourceEntry { kind: RES_FONT, flags: 0, offset: 0, size: img.len() as u32 };
        let f = Font::load_from_entry(&flash, &entry, true).unwrap();
        let g = f.clone();
        assert!(!f.embedded_bitmap.is_null());
        assert_eq!(f.embedded_bitmap, g.embedded_bitmap);
        match (&f.glyphs, &g.glyphs) {
            (GlyphData::Heap { glyphs: a, codepoints: c }, GlyphData::Heap { glyphs: b, codepoints: d }) => {
                assert!(Arc::ptr_eq(a, b) && Arc::ptr_eq(c, d));
            }
            _ => panic!("flash fonts keep their tables on the heap"),
        }
        drop(f);
        assert_eq!(unsafe { *g.embedded_bitmap }, 0xFF);
        assert_eq!(g.glyph(65).map(|x| x.x_advance), Some(9));
    }

    /// A V3 font keeps what V1/V2 can't: glyphs over 255 px, offsets below −128, a line
    /// height over 255 and bitmaps past 64 KB.
    #[test]
    fn v3_fonts_hold_wide_metrics() {
        let wide = 300u16; // 1bpp, one row: 38 bytes, 0xFF … then the last 4 bits set
        let at = 70_000u32;
        let mut img = alloc::vec![0x00, 0x03];
        img.extend_from_slice(&2u16.to_le_bytes());
        img.extend_from_slice(&310u16.to_le_bytes());
        img.extend_from_slice(&[7, 1, 8, 0, 9, 0]);
        for cp in [b' ' as u16, b'W' as u16] {
            img.extend_from_slice(&cp.to_le_bytes());
        }
        for (off, w, h, adv, xo, yo) in [(0u32, 0u16, 0u16, 60u16, 0i16, 0i16), (at, wide, 1, 320, -3, -200)] {
            img.extend_from_slice(&off.to_le_bytes());
            for v in [w, h, adv, xo as u16, yo as u16] {
                img.extend_from_slice(&v.to_le_bytes());
            }
        }
        let bitmap_at = img.len();
        img.resize(bitmap_at + at as usize, 0);
        img.extend_from_slice(&[0xFF; 37]);
        img.push(0xF0);
        crate::mock::set_flash_image(img.clone());
        let flash = FlashImpl::with_backend(crate::mock::MockFlash);
        let entry = crate::fs::ResourceEntry { kind: RES_FONT, flags: 0, offset: 0, size: img.len() as u32 };
        for cache in [false, true] {
            let f = Font::load_from_entry(&flash, &entry, cache).unwrap();
            assert_eq!((f.font_id, f.bpp, f.line_height()), (7, 1, 310));
            use crate::rich::{BOLD, ITALIC, UNDERLINE};
            assert_eq!([0, BOLD, ITALIC, BOLD | ITALIC, UNDERLINE].map(|s| f.variant(s)), [None, Some(8), None, Some(9), None]);
            assert_eq!(f.bitmap_addr() as usize, bitmap_at);
            assert_eq!(f.glyph(b'W' as u16), Some(GfxGlyph::new(at, wide, 1, 320, -3, -200)));
            assert_eq!(f.text_width(b"W W"), 700);
            let mut cov = alloc::vec![0u8; wide as usize];
            f.glyph_coverage(&flash, &f.glyph(b'W' as u16).unwrap(), &mut cov);
            assert!(cov.iter().all(|&c| c == 255), "cache {cache}");
            let mut tops = alloc::vec::Vec::new();
            f.for_each_glyph(b"W", 10, 250, |_, _, x, y| tops.push((x, y)));
            assert_eq!(tops, [(7, 50)]);
        }
    }

    /// All lines of `text` wrapped to the width of `fit` in the ROM font.
    fn lines(text: &str, fit: &str) -> alloc::vec::Vec<alloc::string::String> {
        let f = rom();
        let w = f.text_width(fit.as_bytes());
        let b = text.as_bytes();
        let mut out = alloc::vec::Vec::new();
        let mut i = 0;
        while i <= b.len() {
            let (end, next) = f.wrap_line(b, i, w);
            out.push(alloc::string::String::from_utf8(b[i..end].to_vec()).unwrap());
            assert!(next > i, "no progress at {i}");
            i = next;
        }
        out
    }

    #[test]
    fn wrap_line_breaks_at_spaces_newlines_and_long_words() {
        assert_eq!(lines("one two three", "one two"), ["one two", "three"]);
        // The space that would overflow ends the line and is dropped.
        assert_eq!(lines("abc abc", "abc"), ["abc", "abc"]);
        assert_eq!(lines("ab\ncd", "abcdefgh"), ["ab", "cd"]);
        // A final newline leaves an empty last line (for the cursor).
        assert_eq!(lines("ab\n", "abcdefgh"), ["ab", ""]);
        // A word longer than a line is cut.
        assert_eq!(lines("aaaaaaa", "aaa"), ["aaa", "aaa", "a"]);
        assert_eq!(lines("", "aaa"), [""]);
    }

    /// A readback panel that keeps each pixel's last coverage (fills = 255).
    struct Cover(core::cell::RefCell<alloc::vec::Vec<u8>>);
    const CW: usize = 96;
    impl crate::lcd::LcdBackend for Cover {
        const WIDTH: u16 = CW as u16;
        const HEIGHT: u16 = CW as u16;
        const HAS_ALPHA: bool = true;
        fn begin_frame(&mut self) {}
        fn end_frame(&mut self) {}
        fn back_buf(&self) -> u8 {
            0
        }
        fn fill_rect(&self, x: u16, y: u16, w: u16, h: u16, _: u16) {
            for yy in y as usize..y as usize + h as usize {
                for xx in x as usize..x as usize + w as usize {
                    self.0.borrow_mut()[yy * CW + xx] = 255;
                }
            }
        }
        fn blend_pixel(&self, x: u16, y: u16, _: u16, alpha: u8) {
            self.0.borrow_mut()[y as usize * CW + x as usize] = alpha;
        }
        fn begin_pixels(&self, _: u16, _: u16, _: u16, _: u16) {}
        fn write_pixel(&self, _: u16) {}
        fn send_command(&self, _: u16) {}
        fn send_data(&self, _: u16) {}
    }

    /// A 2bpp font: 'A' 3×2 (coverage 3 2 1 / 0 1 3), 'B' without pixels.
    static AA_GLYPHS: [GfxGlyph; 2] = [GfxGlyph::new(0, 3, 2, 4, 1, -2), GfxGlyph::new(0, 0, 0, 2, 0, 0)];
    static AA_CODEPOINTS: [u16; 2] = [b'A' as u16, b'B' as u16];
    static AA_BITMAP: [u8; 2] = [0b11_10_01_00, 0b01_11_0000];

    /// `for_each_glyph` + `glyph_coverage` (what a GPU compositor draws text from) put the
    /// same coverage where `draw_str` blends it, for 1bpp and 2bpp fonts, and end at its pen.
    #[test]
    fn glyph_walk_matches_draw_str() {
        let aa = Font::from_embedded(&AA_GLYPHS, &AA_CODEPOINTS, &AA_BITMAP, 4, 2);
        let flash = crate::flash::FlashImpl::with_backend(crate::mock::MockFlash);
        for (font, text) in [(rom(), "Hi, gy!"), (aa, "ABxAA")] {
            let lcd = crate::lcd::LcdImpl::with_backend(Cover(core::cell::RefCell::new(alloc::vec![0; CW * CW])));
            let end = font.draw_str(&lcd, &flash, text.as_bytes(), 5, 30, 0xFFFF, None);
            let mut want = alloc::vec![0u8; CW * CW];
            let mut cov = alloc::vec::Vec::new();
            let walked = font.for_each_glyph(text.as_bytes(), 5, 30, |_, g, x, y| {
                cov.resize(g.width as usize * g.height as usize, 0);
                font.glyph_coverage(&flash, g, &mut cov);
                for row in 0..g.height as usize {
                    for col in 0..g.width as usize {
                        let c = cov[row * g.width as usize + col];
                        if c != 0 {
                            want[(y as usize + row) * CW + x as usize + col] = c;
                        }
                    }
                }
            });
            assert_eq!(walked, end, "pen of {text:?}");
            assert!(want.iter().any(|&c| c != 0));
            assert!(*lcd.backend().0.borrow() == want, "pixels of {text:?}");
        }
    }
}
