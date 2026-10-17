//! WHIQ: text layout for GridCraft's exports (PDF and PNG).
//!
//! - **Fonts always available**: Noto Sans (Latin, Greek, Cyrillic) and Noto Sans Arabic, regular
//!   and bold, are compiled into native builds (SIL OFL 1.1, `assets/fonts/whiq/`). Web builds
//!   carry none ([`faces`] is empty) and exports fall back to the PDF standard fonts.
//! - **Bidi**: the Unicode Bidirectional Algorithm orders each line; the paragraph direction is the
//!   one given, else the first strong character's ([`base_direction`]): an Arabic line is
//!   right-to-left.
//! - **Shaping**: HarfRust shapes each run with its script and direction (Arabic joining,
//!   ligatures, marks, mirrored brackets in right-to-left runs). Characters pick a face by script
//!   (Arabic → Noto Sans Arabic, else Noto Sans), neutrals stay with their neighbours, and a face
//!   that lacks a character hands it to the other one.
//! - **Outlines** ([`outline`]) for rasterising, **glyph text** for PDF `ToUnicode` maps.
//!
//! Positions are in the caller's units (points or pixels): `size` is the font size in them.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::sync::OnceLock;

use harfrust::{Direction, ShapeOptions, ShaperData, UnicodeBuffer};
use skrifa::MetadataProvider;
use skrifa::instance::{LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use unicode_script::{Script, UnicodeScript};

/// A bundled face.
pub struct Face {
    /// PostScript name (`NotoSans-Regular`).
    pub name: &'static str,
    pub data: &'static [u8],
    pub bold: bool,
    pub arabic: bool,
    pub upem: f32,
    /// Ascender / descender (positive up / positive down) in font units.
    pub ascent: f32,
    pub descent: f32,
    pub cap_height: f32,
    /// Font bounding box in font units: x_min, y_min, x_max, y_max.
    pub bbox: [f32; 4],
    shaper: ShaperData,
}

impl Face {
    fn font(&self) -> Option<harfrust::FontRef<'_>> {
        harfrust::FontRef::new(self.data).ok()
    }

    /// The glyph for `c`, 0 when the face lacks it.
    pub fn glyph(&self, c: char) -> u16 {
        skrifa::FontRef::new(self.data).ok().and_then(|f| f.charmap().map(c)).map(|g| g.to_u32().min(u32::from(u16::MAX)) as u16).unwrap_or(0)
    }

    /// Advance of glyph `gid` in font units.
    pub fn advance(&self, gid: u16) -> f32 {
        skrifa::FontRef::new(self.data)
            .ok()
            .and_then(|f| f.glyph_metrics(Size::unscaled(), LocationRef::default()).advance_width(skrifa::GlyphId::new(u32::from(gid))))
            .unwrap_or(self.upem / 2.0)
    }

    /// Number of glyphs in the face.
    pub fn glyph_count(&self) -> u32 {
        skrifa::FontRef::new(self.data).ok().map(|f| u32::from(f.maxp().map(|m| m.num_glyphs()).unwrap_or(0))).unwrap_or(0)
    }
}

use skrifa::raw::TableProvider;

/// The bundled fonts: (PostScript name, bytes, bold, Arabic).
#[cfg(not(target_arch = "wasm32"))]
const BUNDLED: &[(&str, &[u8], bool, bool)] = &[
    ("NotoSans-Regular", include_bytes!("../../../assets/fonts/whiq/NotoSans-Regular.ttf"), false, false),
    ("NotoSans-Bold", include_bytes!("../../../assets/fonts/whiq/NotoSans-Bold.ttf"), true, false),
    ("NotoSansArabic-Regular", include_bytes!("../../../assets/fonts/whiq/NotoSansArabic-Regular.ttf"), false, true),
    ("NotoSansArabic-Bold", include_bytes!("../../../assets/fonts/whiq/NotoSansArabic-Bold.ttf"), true, true),
];
#[cfg(target_arch = "wasm32")]
const BUNDLED: &[(&str, &[u8], bool, bool)] = &[];

fn load(name: &'static str, data: &'static [u8], bold: bool, arabic: bool) -> Option<Face> {
    let f = skrifa::FontRef::new(data).ok()?;
    let m = f.metrics(Size::unscaled(), LocationRef::default());
    let hb = harfrust::FontRef::new(data).ok()?;
    let bbox = m.bounds.map(|b| [b.x_min, b.y_min, b.x_max, b.y_max]).unwrap_or([0.0, m.descent, f32::from(m.units_per_em), m.ascent]);
    Some(Face {
        name,
        data,
        bold,
        arabic,
        upem: f32::from(m.units_per_em.max(1)),
        ascent: m.ascent,
        descent: -m.descent,
        cap_height: m.cap_height.unwrap_or(m.ascent * 0.7),
        bbox,
        shaper: ShaperData::new(&hb),
    })
}

/// The bundled faces, parsed once. Empty on the web.
pub fn faces() -> &'static [Face] {
    static FACES: OnceLock<Vec<Face>> = OnceLock::new();
    FACES.get_or_init(|| BUNDLED.iter().filter_map(|(n, d, b, a)| load(n, d, *b, *a)).collect())
}

/// Are fonts available to shape and embed (native builds)?
pub fn available() -> bool {
    faces().len() >= 2
}

/// Index of the face for (bold, arabic), falling back to any face.
fn face_index(bold: bool, arabic: bool) -> u8 {
    let f = faces();
    f.iter()
        .position(|x| x.bold == bold && x.arabic == arabic)
        .or_else(|| f.iter().position(|x| x.arabic == arabic))
        .or_else(|| (!f.is_empty()).then_some(0))
        .unwrap_or(0) as u8
}

/// The face at `i`.
pub fn face(i: u8) -> Option<&'static Face> {
    faces().get(usize::from(i))
}

// ---------------------------------------------------------------- direction

/// Paragraph or run direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Dir {
    Ltr,
    Rtl,
}

/// The direction of a strong character, `None` for neutrals (digits, spaces, punctuation).
pub fn strong_dir(c: char) -> Option<Dir> {
    use unicode_bidi::BidiClass::{AL, L, R};
    match unicode_bidi::bidi_class(c) {
        R | AL => Some(Dir::Rtl),
        L => Some(Dir::Ltr),
        _ => None,
    }
}

/// The direction of the first strong character (the paragraph direction when none is set).
pub fn base_direction(text: &str) -> Option<Dir> {
    text.chars().find_map(strong_dir)
}

/// Does `text` need shaping or bidi (anything beyond simple left-to-right text)?
pub fn is_complex(text: &str) -> bool {
    text.chars().any(|c| {
        strong_dir(c) == Some(Dir::Rtl)
            || matches!(c.script(), Script::Arabic | Script::Hebrew | Script::Syriac | Script::Thaana | Script::Nko)
            || matches!(c as u32, 0x0300..=0x036F | 0x200C..=0x200F | 0x202A..=0x202E | 0x2066..=0x2069)
    })
}

// ---------------------------------------------------------------- shaping

/// One positioned glyph of a shaped line.
#[derive(Clone, Debug, PartialEq)]
pub struct Glyph {
    /// Index of the face ([`face`]).
    pub face: u8,
    pub gid: u16,
    /// Pen position of the glyph's origin from the line start (caller units).
    pub x: f32,
    /// Vertical offset, y-down (caller units).
    pub dy: f32,
    pub advance: f32,
    /// The characters this glyph shows (first glyph of a cluster), for text extraction.
    pub text: Option<String>,
}

/// A line laid out in visual order, left to right.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Line {
    pub glyphs: Vec<Glyph>,
    pub width: f32,
    /// The paragraph direction used.
    pub rtl: bool,
    /// The text laid out, in logical order (PDF `ActualText`).
    pub text: String,
}

/// Does `face` want `c` by script?
fn arabic_script(c: char) -> bool {
    matches!(c.script(), Script::Arabic)
        || matches!(c as u32, 0x0600..=0x06FF | 0x0750..=0x077F | 0x08A0..=0x08FF | 0xFB50..=0xFDFF | 0xFE70..=0xFEFF)
}

fn neutral(c: char) -> bool {
    matches!(c.script(), Script::Common | Script::Inherited | Script::Unknown)
}

/// Face per character of `text` (logical order).
fn assign_faces(text: &str, bold: bool) -> Vec<u8> {
    let latin = face_index(bold, false);
    let arabic = face_index(bold, true);
    let has = |f: u8, c: char| face(f).is_some_and(|x| x.glyph(c) != 0);
    let chars: Vec<char> = text.chars().collect();
    let strong: Vec<Option<u8>> = chars
        .iter()
        .map(|&c| {
            if neutral(c) {
                None
            } else if arabic_script(c) {
                Some(if has(arabic, c) || !has(latin, c) { arabic } else { latin })
            } else if has(latin, c) || !has(arabic, c) {
                Some(latin)
            } else {
                Some(arabic)
            }
        })
        .collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut prev: Option<u8> = None;
    for (i, &c) in chars.iter().enumerate() {
        let f = match strong.get(i).copied().flatten() {
            Some(f) => f,
            None => {
                // A neutral stays with the run it is in: the previous strong face, else the next.
                let near = prev.or_else(|| strong.iter().skip(i + 1).flatten().next().copied()).unwrap_or(latin);
                if has(near, c) || !has(if near == latin { arabic } else { latin }, c) {
                    near
                } else if near == latin {
                    arabic
                } else {
                    latin
                }
            }
        };
        if strong.get(i).copied().flatten().is_some() {
            prev = Some(f);
        }
        out.push(f);
    }
    out
}

/// Shapes one run of one face and direction; returns glyphs (visual order) with `x` relative
/// to the run start and the run's advance.
fn shape_run(text: &str, offset: usize, full: &str, f: u8, rtl: bool, scale: f32) -> (Vec<Glyph>, f32) {
    let Some(fc) = face(f) else { return (vec![], 0.0) };
    let Some(font) = fc.font() else { return (vec![], 0.0) };
    let shaper = fc.shaper.shaper(&font).instance(None).build();
    let mut buf = UnicodeBuffer::new();
    for (i, c) in text.char_indices() {
        buf.add(c, (offset + i) as u32);
    }
    buf.guess_segment_properties();
    buf.set_direction(if rtl { Direction::RightToLeft } else { Direction::LeftToRight });
    let gb = shaper.shape(buf, ShapeOptions::new());
    let infos = gb.glyph_infos();
    let positions = gb.glyph_positions();
    // Cluster boundaries for the text each cluster shows.
    let mut starts: Vec<usize> = infos.iter().map(|i| i.cluster as usize).collect();
    starts.sort_unstable();
    starts.dedup();
    let end = offset + text.len();
    // The cluster's text goes on its widest glyph (the base, not a dot or mark).
    let mut carrier: Vec<(usize, usize, i32)> = Vec::new();
    for (k, (info, pos)) in infos.iter().zip(positions).enumerate() {
        let c = info.cluster as usize;
        match carrier.iter_mut().find(|(cl, _, _)| *cl == c) {
            Some(entry) if pos.x_advance > entry.2 => *entry = (c, k, pos.x_advance),
            Some(_) => {}
            None => carrier.push((c, k, pos.x_advance)),
        }
    }
    let mut out = Vec::with_capacity(infos.len());
    let mut pen = 0.0f32;
    for (k, (info, pos)) in infos.iter().zip(positions).enumerate() {
        let c = info.cluster as usize;
        let text = if carrier.iter().any(|(cl, at, _)| *cl == c && *at == k) {
            let next = starts.iter().find(|s| **s > c).copied().unwrap_or(end);
            full.get(c..next).map(str::to_string)
        } else {
            None
        };
        out.push(Glyph {
            face: f,
            gid: info.glyph_id.min(u32::from(u16::MAX)) as u16,
            x: pen + pos.x_offset as f32 * scale,
            dy: -(pos.y_offset as f32) * scale,
            advance: pos.x_advance as f32 * scale,
            text,
        });
        pen += pos.x_advance as f32 * scale;
    }
    (out, pen)
}

/// Lays out one line of `text` (newlines are treated as spaces) at `size`, in `bold` or
/// regular, with paragraph direction `base` (`None`: the first strong character's).
pub fn shape(text: &str, size: f32, bold: bool, base: Option<Dir>) -> Line {
    if !available() || text.is_empty() || !size.is_finite() || size <= 0.0 {
        return Line::default();
    }
    // Hostile input: one line never needs more than this.
    let text: String = text.chars().take(20_000).map(|c| if c == '\n' || c == '\r' || c == '\u{2029}' { ' ' } else { c }).collect();
    let base = base.or_else(|| base_direction(&text)).unwrap_or(Dir::Ltr);
    let level = if base == Dir::Rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::BidiInfo::new(&text, Some(level));
    let faces_of = assign_faces(&text, bold);
    let byte_face: Vec<(usize, u8)> = text.char_indices().map(|(i, _)| i).zip(faces_of.iter().copied()).collect();
    let face_at = |b: usize| byte_face.iter().find(|(i, _)| *i == b).map(|(_, f)| *f).unwrap_or(0);
    let mut line = Line { rtl: base == Dir::Rtl, text: text.clone(), ..Line::default() };
    let mut pen = 0.0f32;
    for para in &info.paragraphs {
        let (levels, runs) = info.visual_runs(para, para.range.clone());
        for run in runs {
            let rtl = levels.get(run.start).is_some_and(|l| l.is_rtl());
            let Some(sub) = text.get(run.clone()) else { continue };
            // Face runs in logical order.
            let mut pieces: Vec<(usize, usize, u8)> = Vec::new();
            for (i, _) in sub.char_indices() {
                let b = run.start + i;
                let f = face_at(b);
                match pieces.last_mut() {
                    Some(last) if last.2 == f => last.1 = b,
                    _ => pieces.push((b, b, f)),
                }
            }
            // End offsets: up to the next piece (or the run end).
            let ends: Vec<usize> = (0..pieces.len()).map(|k| pieces.get(k + 1).map_or(run.end, |p| p.0)).collect();
            let mut order: Vec<usize> = (0..pieces.len()).collect();
            if rtl {
                order.reverse();
            }
            for k in order {
                let (Some(&(s, _, f)), Some(&e)) = (pieces.get(k), ends.get(k)) else { continue };
                let Some(piece) = text.get(s..e) else { continue };
                let scale = face(f).map_or(0.0, |x| size / x.upem);
                let (glyphs, adv) = shape_run(piece, s, &text, f, rtl, scale);
                for mut g in glyphs {
                    g.x += pen;
                    line.glyphs.push(g);
                }
                pen += adv;
            }
        }
    }
    line.width = pen;
    line
}

/// Width of `text` laid out at `size`.
pub fn width(text: &str, size: f32, bold: bool) -> f32 {
    shape(text, size, bold, None).width
}

// ---------------------------------------------------------------- outlines

/// Receives a glyph outline in caller units, y-down, relative to the glyph origin on the baseline.
pub trait Pen {
    fn move_to(&mut self, x: f32, y: f32);
    fn line_to(&mut self, x: f32, y: f32);
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32);
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32);
    fn close(&mut self);
}

struct Flip<'a>(&'a mut dyn Pen);

impl OutlinePen for Flip<'_> {
    fn move_to(&mut self, x: f32, y: f32) {
        self.0.move_to(x, -y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        self.0.line_to(x, -y);
    }
    fn quad_to(&mut self, cx0: f32, cy0: f32, x: f32, y: f32) {
        self.0.quad_to(cx0, -cy0, x, -y);
    }
    fn curve_to(&mut self, cx0: f32, cy0: f32, cx1: f32, cy1: f32, x: f32, y: f32) {
        self.0.curve_to(cx0, -cy0, cx1, -cy1, x, -y);
    }
    fn close(&mut self) {
        self.0.close();
    }
}

/// Draws glyph `gid` of face `f` at `size` into `pen`. `false` when there is no outline.
pub fn outline(f: u8, gid: u16, size: f32, pen: &mut dyn Pen) -> bool {
    let Some(fc) = face(f) else { return false };
    let Ok(font) = skrifa::FontRef::new(fc.data) else { return false };
    let glyphs = font.outline_glyphs();
    let Some(g) = glyphs.get(skrifa::GlyphId::new(u32::from(gid))) else { return false };
    g.draw(DrawSettings::unhinted(Size::new(size), LocationRef::default()), &mut Flip(pen)).is_ok()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn fonts_are_bundled() {
        assert!(available());
        assert_eq!(faces().len(), 4);
        let latin = face(face_index(false, false)).unwrap();
        let arabic = face(face_index(false, true)).unwrap();
        assert_ne!(latin.glyph('A'), 0);
        assert_ne!(arabic.glyph('م'), 0);
        assert_eq!(latin.glyph('م'), 0, "Noto Sans has no Arabic: the fallback is needed");
    }

    #[test]
    fn latin_shapes_left_to_right() {
        let l = shape("Hello", 10.0, false, None);
        assert_eq!(l.glyphs.len(), 5);
        assert!(!l.rtl);
        assert!(l.width > 15.0 && l.width < 40.0, "{}", l.width);
        assert!(l.glyphs.windows(2).all(|w| w[0].x < w[1].x));
        assert_eq!(l.glyphs[0].text.as_deref(), Some("H"));
        assert!((width("Hello", 20.0, false) - 2.0 * l.width).abs() < 0.01);
        assert!(width("Hello", 10.0, true) > l.width, "bold is wider");
    }

    #[test]
    fn arabic_is_joined_and_right_to_left() {
        let text = "مرحبا";
        let l = shape(text, 12.0, false, None);
        assert!(l.rtl, "an Arabic line is right-to-left by default");
        let arabic = face_index(false, true);
        assert!(l.glyphs.iter().all(|g| g.face == arabic && g.gid != 0), "{l:?}");
        // Joined forms differ from the isolated (cmap) glyphs: shaping happened.
        let f = face(arabic).unwrap();
        let isolated: Vec<u16> = text.chars().map(|c| f.glyph(c)).collect();
        let shaped: Vec<u16> = l.glyphs.iter().map(|g| g.gid).collect();
        assert_ne!(shaped, isolated, "contextual forms are used");
        // Visual order: the first letter (logical) is drawn rightmost.
        let first = l.glyphs.iter().find(|g| g.text.as_deref().is_some_and(|t| t.starts_with('م'))).unwrap();
        assert!(first.x >= l.glyphs.iter().map(|g| g.x).fold(0.0, f32::max) - 0.01, "{l:?}");
    }

    #[test]
    fn mixed_lines_keep_each_script_in_its_face() {
        let l = shape("Total: 1,250 ريال", 10.0, false, None);
        assert!(!l.rtl, "the first strong character is Latin");
        let latin = face_index(false, false);
        let arabic = face_index(false, true);
        assert!(l.glyphs.iter().any(|g| g.face == latin));
        assert!(l.glyphs.iter().any(|g| g.face == arabic));
        assert!(l.glyphs.iter().all(|g| g.gid != 0), "no missing glyphs: {l:?}");
        let r = shape("المجموع 1250 SAR", 10.0, false, None);
        assert!(r.rtl);
        // In a right-to-left line, the Latin "SAR" (logically last) is drawn leftmost.
        let sar = r.glyphs.iter().position(|g| g.text.as_deref() == Some("S")).unwrap();
        let alif = r.glyphs.iter().position(|g| g.text.as_deref().is_some_and(|t| t.starts_with('ا'))).unwrap();
        assert!(r.glyphs[sar].x < r.glyphs[alif].x);
    }

    #[test]
    fn outlines_and_directions() {
        struct Count(usize);
        impl Pen for Count {
            fn move_to(&mut self, _: f32, _: f32) {
                self.0 += 1;
            }
            fn line_to(&mut self, _: f32, _: f32) {}
            fn quad_to(&mut self, _: f32, _: f32, _: f32, _: f32) {}
            fn curve_to(&mut self, _: f32, _: f32, _: f32, _: f32, _: f32, _: f32) {}
            fn close(&mut self) {}
        }
        let l = shape("ب", 20.0, false, None);
        let mut c = Count(0);
        assert!(outline(l.glyphs[0].face, l.glyphs[0].gid, 20.0, &mut c));
        assert!(c.0 >= 1);
        assert_eq!(base_direction("123 abc"), Some(Dir::Ltr));
        assert_eq!(base_direction("  ١٢ مرحبا"), Some(Dir::Rtl));
        assert_eq!(base_direction("123"), None);
        assert!(is_complex("مرحبا") && !is_complex("Hello, world"));
        // An explicit direction wins.
        assert!(shape("abc", 10.0, false, Some(Dir::Rtl)).rtl);
        assert!(shape("", 10.0, false, None).glyphs.is_empty());
        assert!(shape("x", f32::NAN, false, None).glyphs.is_empty());
    }
}
