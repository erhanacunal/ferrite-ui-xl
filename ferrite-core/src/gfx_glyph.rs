/// Per-glyph metrics. On flash a V1/V2 font stores them in 7 bytes (Adafruit GFX layout:
/// offset u16, w/h/advance u8, offsets i8), a V3 font in 14 (offset u32, w/h/advance u16,
/// offsets i16: glyphs over 255 px, bitmaps over 64 KB); loaded, both are this.
/// Shared between font renderer and embedded font data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct GfxGlyph {
    pub bitmap_offset: u32,
    pub width: u16,
    pub height: u16,
    pub x_advance: u16,
    pub x_offset: i16,
    pub y_offset: i16,
}

impl GfxGlyph {
    pub const fn empty() -> Self {
        GfxGlyph {
            bitmap_offset: 0,
            width: 0,
            height: 0,
            x_advance: 0,
            x_offset: 0,
            y_offset: 0,
        }
    }

    pub const fn new(
        bitmap_offset: u32,
        width: u16,
        height: u16,
        x_advance: u16,
        x_offset: i16,
        y_offset: i16,
    ) -> Self {
        GfxGlyph {
            bitmap_offset,
            width,
            height,
            x_advance,
            x_offset,
            y_offset,
        }
    }
}
