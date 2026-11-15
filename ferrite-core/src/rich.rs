//! Rich label text: a small HTML subset, read as the text is laid out (`render::label_lines`
//! for widgets with `FLAG_RICH_TEXT`). Nothing is stored: the text stays one string object
//! (scene, commits and display-list keys see it as before) and every layout re-reads it.
//!
//! - `<b>` / `<strong>`, `<i>` / `<em>`, `<u>`: bold, italic, underline (nest; each close
//!   tag ends one open). Bold and italic draw with the label font's variants
//!   (`Font::variant`, from the project's font families); a font without one draws the
//!   text in itself.
//! - `<font color="…">` … `</font>`: text colour, `#RRGGBB`, `#RGB`, a colour name (`red`,
//!   `green`, `blue`, `white`, `black`, `yellow`, `cyan`, `magenta`, `gray`/`grey`,
//!   `orange`), or an RGB565 number (`63488`, `0xF800`: what `.fl` colour constants hold).
//!   Quotes are optional. Up to [`COLOR_DEPTH`] nest; deeper ones keep the outer colour.
//! - `<br>` (also `<br/>`): a line break (multi-line labels; a single-line label ignores
//!   line breaks, as it ignores `\n`).
//! - Entities: `&lt;` `&gt;` `&amp;` `&quot;` `&apos;` `&nbsp;` (U+00A0, drawn as a space
//!   by a font without it, never broken at), `&#NNN;`, `&#xHHHH;` (BMP).
//! - Any other tag is left out (its text shows). A `<` or `&` that doesn't start one shows
//!   as itself.
//!
//! Keep in sync with ferrite-ui `tools/ferrite_rich.py` (the Designer's preview).

/// Style bits of a character.
pub const BOLD: u8 = 1;
pub const ITALIC: u8 = 2;
pub const UNDERLINE: u8 = 4;

/// Nested `<font color>` levels kept.
pub const COLOR_DEPTH: usize = 8;

/// Longest tag read (`<font color="#RRGGBB">` with room to spare); a longer one is text.
const TAG_MAX: usize = 64;

/// How a character is drawn.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct Style {
    /// [`BOLD`] | [`ITALIC`] | [`UNDERLINE`].
    pub flags: u8,
    /// From the innermost `<font color>`; None = the widget's text colour.
    pub color: Option<u16>,
}

/// What [`Cursor::next`] reads.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Item {
    /// A BMP character (0xFFFF = invalid UTF-8, as `font::utf8_next`), its style, and where
    /// its encoding (or entity) starts in the text.
    Char { cp: u16, style: Style, at: usize },
    /// `\n` or `<br>`, starting at `at`.
    Break { at: usize },
}

/// Reads rich text a character at a time. Copying it saves the place and the open tags.
#[derive(Clone)]
pub struct Cursor<'a> {
    text: &'a [u8],
    pos: usize,
    bold: u8,
    italic: u8,
    underline: u8,
    /// Open `<font>` tags: `colors[..depth.min(COLOR_DEPTH)]` are their colours, outermost
    /// first; a tag without a colour (or past the depth) repeats the one around it.
    colors: [u16; COLOR_DEPTH],
    depth: u8,
    /// Whether `colors[i]` is a colour or "the widget's" (a `<font>` without one).
    set: u8,
}

impl<'a> Cursor<'a> {
    pub fn new(text: &'a [u8]) -> Self {
        Self { text, pos: 0, bold: 0, italic: 0, underline: 0, colors: [0; COLOR_DEPTH], depth: 0, set: 0 }
    }

    /// Byte position of the next thing to read.
    pub fn pos(&self) -> usize {
        self.pos
    }

    /// The style a character read now would have.
    pub fn style(&self) -> Style {
        let flags = (self.bold != 0) as u8 * BOLD
            | (self.italic != 0) as u8 * ITALIC
            | (self.underline != 0) as u8 * UNDERLINE;
        let top = (self.depth as usize).min(COLOR_DEPTH);
        let color = (top > 0 && self.set & (1 << (top - 1)) != 0).then(|| self.colors[top - 1]);
        Style { flags, color }
    }

    /// The next character or line break; None at the end.
    pub fn next(&mut self) -> Option<Item> {
        loop {
            let at = self.pos;
            let &b = self.text.get(at)?;
            match b {
                b'<' => {
                    if let Some(item) = self.tag() {
                        match item {
                            Some(item) => return Some(item),
                            None => continue,
                        }
                    }
                    self.pos += 1;
                    return Some(Item::Char { cp: b'<' as u16, style: self.style(), at });
                }
                b'&' => {
                    let cp = match self.entity() {
                        Some(cp) => cp,
                        None => {
                            self.pos += 1;
                            b'&' as u16
                        }
                    };
                    return Some(Item::Char { cp, style: self.style(), at });
                }
                b'\n' => {
                    self.pos += 1;
                    return Some(Item::Break { at });
                }
                _ => {
                    let cp = crate::font::utf8_next(self.text, &mut self.pos)?;
                    return Some(Item::Char { cp, style: self.style(), at });
                }
            }
        }
    }

    /// A tag at `pos` (`<`): None if it isn't one (the `<` is text); Some(None) = read
    /// (`pos` past it), nothing to return; Some(Some(break)) for `<br>`.
    fn tag(&mut self) -> Option<Option<Item>> {
        let at = self.pos;
        let rest = &self.text[at + 1..];
        let end = rest.iter().take(TAG_MAX).position(|&c| c == b'>')?;
        let body = &rest[..end];
        let (close, body) = match body.first() {
            Some(b'/') => (true, &body[1..]),
            _ => (false, body),
        };
        let name_len = body.iter().position(|c| !c.is_ascii_alphabetic()).unwrap_or(body.len());
        if name_len == 0 {
            return None;
        }
        let name = &body[..name_len];
        let attrs = &body[name_len..];
        // After the name: nothing, attributes after a space, or a `/` (`<br/>`).
        if !matches!(attrs.first(), None | Some(b' ' | b'\t' | b'/')) {
            return None;
        }
        self.pos = at + 2 + end;
        let is = |s: &[u8]| name.eq_ignore_ascii_case(s);
        let open = |n: &mut u8| {
            if close {
                *n = n.saturating_sub(1)
            } else {
                *n = n.saturating_add(1)
            }
        };
        if is(b"b") || is(b"strong") {
            open(&mut self.bold);
        } else if is(b"i") || is(b"em") {
            open(&mut self.italic);
        } else if is(b"u") {
            open(&mut self.underline);
        } else if is(b"br") {
            return Some(Some(Item::Break { at }));
        } else if is(b"font") {
            if close {
                self.depth = self.depth.saturating_sub(1);
            } else if (self.depth as usize) < 255 {
                let d = self.depth as usize;
                if d < COLOR_DEPTH {
                    match attr(attrs, b"color").and_then(parse_color) {
                        Some(c) => {
                            self.colors[d] = c;
                            self.set |= 1 << d;
                        }
                        // No (readable) colour: the one around it.
                        None if d > 0 => {
                            self.colors[d] = self.colors[d - 1];
                            self.set = self.set & !(1 << d) | (self.set >> (d - 1) & 1) << d;
                        }
                        None => self.set &= !1,
                    }
                }
                self.depth += 1;
            }
        }
        Some(None)
    }

    /// An entity at `pos` (`&`): its character, `pos` past it; None if it isn't one.
    fn entity(&mut self) -> Option<u16> {
        let rest = &self.text[self.pos + 1..];
        let end = rest.iter().take(10).position(|&c| c == b';')?;
        let name = &rest[..end];
        let cp = match name {
            b"lt" => b'<' as u16,
            b"gt" => b'>' as u16,
            b"amp" => b'&' as u16,
            b"quot" => b'"' as u16,
            b"apos" => b'\'' as u16,
            b"nbsp" => 0xA0,
            [b'#', b'x' | b'X', hex @ ..] => number(hex, 16)?,
            [b'#', dec @ ..] => number(dec, 10)?,
            _ => return None,
        };
        self.pos += end + 2;
        Some(cp)
    }
}

/// A BMP number in `radix` (at least one digit, nothing else).
fn number(digits: &[u8], radix: u32) -> Option<u16> {
    if digits.is_empty() {
        return None;
    }
    let mut v: u32 = 0;
    for &d in digits {
        v = v * radix + (d as char).to_digit(radix)?;
        if v > 0xFFFF {
            return None;
        }
    }
    Some(v as u16)
}

/// The value of attribute `name` in `attrs` (` color="#F00"`, quotes optional).
fn attr<'t>(attrs: &'t [u8], name: &[u8]) -> Option<&'t [u8]> {
    let mut i = 0;
    while i < attrs.len() {
        while i < attrs.len() && matches!(attrs[i], b' ' | b'\t' | b'/') {
            i += 1;
        }
        let k0 = i;
        while i < attrs.len() && (attrs[i].is_ascii_alphanumeric() || attrs[i] == b'-') {
            i += 1;
        }
        let key = &attrs[k0..i];
        if key.is_empty() {
            return None;
        }
        let mut value: &[u8] = &[];
        if attrs.get(i) == Some(&b'=') {
            i += 1;
            match attrs.get(i) {
                Some(&q @ (b'"' | b'\'')) => {
                    let v0 = i + 1;
                    let len = attrs[v0..].iter().position(|&c| c == q).unwrap_or(attrs.len() - v0);
                    value = &attrs[v0..v0 + len];
                    i = v0 + len + 1;
                }
                _ => {
                    let v0 = i;
                    while i < attrs.len() && !matches!(attrs[i], b' ' | b'\t' | b'/') {
                        i += 1;
                    }
                    value = &attrs[v0..i];
                }
            }
        }
        if key.eq_ignore_ascii_case(name) {
            return Some(value);
        }
    }
    None
}

/// 8-bit channels to RGB565.
const fn rgb(r: u32, g: u32, b: u32) -> u16 {
    ((r >> 3) << 11 | (g >> 2) << 5 | b >> 3) as u16
}

/// A `color` value as RGB565.
pub fn parse_color(v: &[u8]) -> Option<u16> {
    let hex = |d: &[u8]| -> Option<u32> {
        let mut x = 0u32;
        for &c in d {
            x = x << 4 | (c as char).to_digit(16)?;
        }
        Some(x)
    };
    match v {
        [b'#', d @ ..] if d.len() == 6 => {
            let x = hex(d)?;
            Some(rgb(x >> 16, x >> 8 & 0xFF, x & 0xFF))
        }
        [b'#', d @ ..] if d.len() == 3 => {
            let x = hex(d)?;
            let (r, g, b) = (x >> 8, x >> 4 & 0xF, x & 0xF);
            Some(rgb(r * 17, g * 17, b * 17))
        }
        [b'0', b'x' | b'X', d @ ..] if !d.is_empty() && d.len() <= 4 => hex(d).map(|x| x as u16),
        [b'0'..=b'9', ..] => number(v, 10),
        _ => {
            const NAMES: [(&[u8], u16); 12] = [
                (b"black", 0x0000),
                (b"white", 0xFFFF),
                (b"red", 0xF800),
                (b"green", 0x07E0),
                (b"blue", 0x001F),
                (b"yellow", 0xFFE0),
                (b"cyan", 0x07FF),
                (b"magenta", 0xF81F),
                (b"gray", rgb(128, 128, 128)),
                (b"grey", rgb(128, 128, 128)),
                (b"orange", rgb(255, 165, 0)),
                (b"purple", rgb(128, 0, 128)),
            ];
            NAMES.iter().find(|(n, _)| v.eq_ignore_ascii_case(n)).map(|&(_, c)| c)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use alloc::vec::Vec;

    /// (char, flags, colour) of each character; `|` for a break.
    fn read(text: &str) -> Vec<(char, u8, Option<u16>)> {
        let mut c = Cursor::new(text.as_bytes());
        let mut out = Vec::new();
        while let Some(item) = c.next() {
            out.push(match item {
                Item::Char { cp, style, .. } => (char::from_u32(cp as u32).unwrap(), style.flags, style.color),
                Item::Break { .. } => ('|', 0, None),
            });
        }
        out
    }

    fn plain(text: &str) -> alloc::string::String {
        read(text).iter().map(|c| c.0).collect()
    }

    #[test]
    fn tags_set_styles_and_nest() {
        let r = read("a<b>b<i>c</b>d</i>e<u>f</u>");
        let flags: Vec<u8> = r.iter().map(|c| c.1).collect();
        assert_eq!(plain("a<b>b<i>c</b>d</i>e<u>f</u>"), "abcdef");
        assert_eq!(flags, [0, BOLD, BOLD | ITALIC, ITALIC, 0, UNDERLINE]);
        assert_eq!(read("<STRONG>x</strong><em>y</EM>").iter().map(|c| c.1).collect::<Vec<_>>(), [BOLD, ITALIC]);
        // A stray close tag changes nothing.
        assert_eq!(read("</b>x")[0].1, 0);
    }

    #[test]
    fn font_colors_nest_and_parse() {
        let r = read("a<font color=\"#FF0000\">b<font color=blue>c</font>d<font>e</font></font>f");
        let colors: Vec<_> = r.iter().map(|c| c.2).collect();
        assert_eq!(colors, [None, Some(0xF800), Some(0x001F), Some(0xF800), Some(0xF800), None]);
        assert_eq!(parse_color(b"#0f0"), Some(0x07E0));
        assert_eq!(parse_color(b"0xF800"), Some(0xF800));
        assert_eq!(parse_color(b"2016"), Some(2016));
        assert_eq!(parse_color(b"Orange"), Some(0xFD20));
        assert_eq!(parse_color(b"#12345"), None);
        // An unreadable colour keeps the one around it; '' quotes and spaces work.
        assert_eq!(read("<font color='white'><font color=nope>x")[0].2, Some(0xFFFF));
        assert_eq!(read("<font  size=3  color = red>x")[0].2, None, "no spaces around '='");
        assert_eq!(read("<font size=3 color=red>x")[0].2, Some(0xF800));
    }

    #[test]
    fn deep_font_tags_keep_the_innermost_kept_colour() {
        let mut s = alloc::string::String::new();
        for i in 0..COLOR_DEPTH + 2 {
            s.push_str(&alloc::format!("<font color={}>", i + 1));
        }
        s.push('x');
        for _ in 0..COLOR_DEPTH + 2 {
            s.push_str("</font>");
        }
        s.push('y');
        let r = read(&s);
        assert_eq!(r[0].2, Some(COLOR_DEPTH as u16));
        assert_eq!(r[1].2, None);
    }

    #[test]
    fn breaks_entities_and_text_that_only_looks_like_markup() {
        assert_eq!(plain("a<br>b<BR/>c\nd"), "a|b|c|d");
        assert_eq!(plain("&lt;b&gt; &amp; &quot;&apos; &#65;&#x42;"), "<b> & \"' AB");
        assert_eq!(read("&nbsp;")[0].0, '\u{a0}');
        assert_eq!(plain("a < b, a<3, x & y, &bogus; &#xZZ;"), "a < b, a<3, x & y, &bogus; &#xZZ;");
        assert_eq!(plain("<unknown attr=1>text</unknown><p>"), "text");
        assert_eq!(plain("Ğü<b>ş</b>"), "Ğüş");
        // An unclosed tag is text.
        assert_eq!(plain("<b"), "<b");
    }

    #[test]
    fn positions_point_at_the_characters() {
        let mut c = Cursor::new(b"<b>ab</b>&lt;");
        let mut at = Vec::new();
        while let Some(Item::Char { at: a, .. }) = c.next() {
            at.push(a);
        }
        assert_eq!(at, [3, 4, 9]);
    }
}
