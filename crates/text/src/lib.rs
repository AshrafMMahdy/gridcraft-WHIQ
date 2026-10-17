//! WHIQ: text layout for GridCraft's exports (PDF and PNG).
//!
//! - **Fonts** come from the craft-fonts build input (`CRAFT_FONTS_DIR`, see `build.rs`), as in
//!   WordCraft and DeckCraft: the Latin presentation families (Carlito for Calibri, Caladea for
//!   Cambria, Liberation Sans / Serif / Mono for Arial, Times New Roman and Courier New, and
//!   others) with real bold, italic and bold-italic faces, Noto Sans Arabic (a variable font,
//!   instanced at regular and bold weights), and the CJK faces. A cell's font name picks its
//!   family ([`resolve_family`]): the family itself when embedded, else its open substitute, else
//!   Carlito. Without craft-fonts ([`available`] is false) exports use the PDF standard fonts.
//! - **Bidi**: the Unicode Bidirectional Algorithm orders each line; the paragraph direction is the
//!   one given, else the first strong character's ([`base_direction`]): an Arabic line is
//!   right-to-left.
//! - **Shaping**: HarfRust shapes each run with its script, direction and (variable fonts) its
//!   instance (Arabic joining, ligatures, marks, mirrored brackets in right-to-left runs).
//!   Characters pick a face by script (Arabic → the Arabic face), neutrals stay with their
//!   neighbours, and a character the face lacks goes to the first face that has it (CJK, Thai…).
//! - **Italics** use the family's italic face. Only a face with no italic at all (Arabic script has
//!   none) is slanted ([`Glyph::slant`]).
//! - **Outlines** ([`outline`]) for rasterising, **glyph text** for PDF `ToUnicode` maps.
//!
//! Positions are in the caller's units (points or pixels): `size` is the font size in them.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use std::sync::OnceLock;

use harfrust::{Direction, ShapeOptions, ShaperData, ShaperInstance, UnicodeBuffer};
use skrifa::MetadataProvider;
use skrifa::instance::{Location, LocationRef, Size};
use skrifa::outline::{DrawSettings, OutlinePen};
use skrifa::raw::TableProvider;
use unicode_script::{Script, UnicodeScript};

/// A font from the craft-fonts build input (empty unless built with `CRAFT_FONTS_DIR`).
pub struct CraftFont {
    pub family: &'static str,
    pub style: &'static str,
    /// ISO 15924 scripts the font is for, e.g. `"Arab"`.
    pub scripts: &'static [&'static str],
    pub bytes: &'static [u8],
}

include!(concat!(env!("OUT_DIR"), "/craft_fonts.rs"));

/// A face: one font file, or one instance (weight) of a variable font.
pub struct Face {
    /// PostScript-style name, unique among the faces (`Carlito-BoldItalic`).
    pub name: String,
    pub family: &'static str,
    pub data: &'static [u8],
    /// Weight 600 or more.
    pub bold: bool,
    pub weight: u16,
    pub italic: bool,
    /// For Arabic script.
    pub arabic: bool,
    pub scripts: &'static [&'static str],
    /// Variation settings of this instance (user space), empty for a static font.
    pub variations: Vec<([u8; 4], f32)>,
    pub upem: f32,
    /// Ascender / descender (positive up / positive down) in font units.
    pub ascent: f32,
    pub descent: f32,
    pub cap_height: f32,
    /// Degrees, counter-clockwise from vertical (negative for a right-leaning italic).
    pub italic_angle: f32,
    /// Font bounding box in font units: x_min, y_min, x_max, y_max.
    pub bbox: [f32; 4],
    location: Location,
    shaper: ShaperData,
    instance: Option<ShaperInstance>,
}

impl Face {
    fn font(&self) -> Option<harfrust::FontRef<'_>> {
        harfrust::FontRef::new(self.data).ok()
    }

    /// The glyph for `c`, 0 when the face lacks it.
    pub fn glyph(&self, c: char) -> u16 {
        skrifa::FontRef::new(self.data).ok().and_then(|f| f.charmap().map(c)).map(|g| g.to_u32().min(u32::from(u16::MAX)) as u16).unwrap_or(0)
    }

    /// Advance of glyph `gid` in font units (at this instance).
    pub fn advance(&self, gid: u16) -> f32 {
        skrifa::FontRef::new(self.data)
            .ok()
            .and_then(|f| f.glyph_metrics(Size::unscaled(), LocationRef::from(&self.location)).advance_width(skrifa::GlyphId::new(u32::from(gid))))
            .unwrap_or(self.upem / 2.0)
    }

    /// Number of glyphs in the face.
    pub fn glyph_count(&self) -> u32 {
        skrifa::FontRef::new(self.data).ok().map(|f| u32::from(f.maxp().map(|m| m.num_glyphs()).unwrap_or(0))).unwrap_or(0)
    }

    /// Is this an instance of a variable font?
    pub fn is_variable(&self) -> bool {
        !self.variations.is_empty()
    }
}

/// (bold, italic, weight) of a manifest style name; `None` for a variable font's style.
fn static_style(style: &str) -> Option<(bool, u16)> {
    let s = style.to_ascii_lowercase();
    if s.contains("variable") {
        return None;
    }
    let italic = s.contains("italic") || s.contains("oblique");
    let weight = if s.contains("semibold") || s.contains("semi bold") || s.contains("demibold") {
        600
    } else if s.contains("extrabold") || s.contains("extra bold") {
        800
    } else if s.contains("bold") {
        700
    } else if s.contains("black") || s.contains("heavy") {
        900
    } else if s.contains("medium") {
        500
    } else if s.contains("light") {
        300
    } else {
        400
    };
    Some((italic, weight))
}

fn load(cf: &'static CraftFont, italic: bool, weight: u16, variable: bool) -> Option<Face> {
    let data = cf.bytes;
    let f = skrifa::FontRef::new(data).ok()?;
    let hb = harfrust::FontRef::new(data).ok()?;
    let mut variations = Vec::new();
    let location = if variable {
        let wght = f.axes().iter().find(|a| a.tag() == skrifa::Tag::new(b"wght"));
        let value = wght.as_ref().map(|a| f32::from(weight).clamp(a.min_value(), a.max_value())).unwrap_or(f32::from(weight));
        if wght.is_some() {
            variations.push((*b"wght", value));
        }
        f.axes().location(variations.iter().map(|(t, v)| (skrifa::Tag::new(t), *v)))
    } else {
        Location::default()
    };
    let m = f.metrics(Size::unscaled(), LocationRef::from(&location));
    let bbox = m.bounds.map(|b| [b.x_min, b.y_min, b.x_max, b.y_max]).unwrap_or([0.0, m.descent, f32::from(m.units_per_em), m.ascent]);
    let instance = (!variations.is_empty())
        .then(|| ShaperInstance::from_variations(&hb, variations.iter().map(|(t, v)| harfrust::Variation { tag: harfrust::Tag::new(t), value: *v })));
    let style = match (weight >= 600, italic) {
        (false, false) => "Regular",
        (true, false) => "Bold",
        (false, true) => "Italic",
        (true, true) => "BoldItalic",
    };
    let family_ps: String = cf.family.chars().filter(|c| c.is_ascii_alphanumeric()).collect();
    Some(Face {
        name: format!("{family_ps}-{style}"),
        family: cf.family,
        data,
        bold: weight >= 600,
        weight,
        italic,
        arabic: cf.scripts.contains(&"Arab"),
        scripts: cf.scripts,
        variations,
        upem: f32::from(m.units_per_em.max(1)),
        ascent: m.ascent,
        descent: -m.descent,
        cap_height: m.cap_height.unwrap_or(m.ascent * 0.7),
        italic_angle: m.italic_angle,
        bbox,
        location,
        shaper: ShaperData::new(&hb),
        instance,
    })
}

/// Every face, parsed once: the craft-fonts files, each variable font as a regular and a bold
/// instance. Empty without craft-fonts and on the web.
pub fn faces() -> &'static [Face] {
    static FACES: OnceLock<Vec<Face>> = OnceLock::new();
    FACES.get_or_init(|| {
        let mut out: Vec<Face> = Vec::new();
        for cf in CRAFT_FONTS {
            // A variable font (a weight axis), whatever its manifest style says.
            let variable = skrifa::FontRef::new(cf.bytes).is_ok_and(|f| f.axes().iter().any(|a| a.tag() == skrifa::Tag::new(b"wght")));
            match static_style(cf.style).filter(|_| !variable) {
                Some((italic, weight)) => out.extend(load(cf, italic, weight, false)),
                None => {
                    let italic = cf.style.to_ascii_lowercase().contains("italic");
                    out.extend(load(cf, italic, 400, true));
                    out.extend(load(cf, italic, 700, true));
                }
            }
        }
        // Unique names (a family listed twice, or a static face next to an instance).
        let mut seen: Vec<String> = Vec::new();
        for f in out.iter_mut() {
            let base = f.name.clone();
            let mut k = 2;
            while seen.contains(&f.name) {
                f.name = format!("{base}{k}");
                k += 1;
            }
            seen.push(f.name.clone());
        }
        out.truncate(250);
        out
    })
}

/// Are fonts available to shape and embed (a native build with craft-fonts)?
pub fn available() -> bool {
    faces().iter().any(|f| f.scripts.contains(&"Latn"))
}

/// The face at `i`.
pub fn face(i: u8) -> Option<&'static Face> {
    faces().get(usize::from(i))
}

/// Open substitutes for the fonts spreadsheets ask for (metric-compatible where one exists).
const ALIASES: &[(&str, &str)] = &[
    ("calibri", "Carlito"),
    ("calibri light", "Carlito"),
    ("aptos", "Carlito"),
    ("aptos narrow", "Carlito"),
    ("aptos display", "Carlito"),
    ("segoe ui", "Carlito"),
    ("candara", "Carlito"),
    ("corbel", "Carlito"),
    ("cambria", "Caladea"),
    ("cambria math", "Caladea"),
    ("constantia", "Caladea"),
    ("arial", "Liberation Sans"),
    ("arial narrow", "Liberation Sans"),
    ("arial unicode ms", "Liberation Sans"),
    ("helvetica", "Liberation Sans"),
    ("helvetica neue", "Liberation Sans"),
    ("tahoma", "Liberation Sans"),
    ("verdana", "Liberation Sans"),
    ("trebuchet ms", "Liberation Sans"),
    ("century gothic", "Liberation Sans"),
    ("sans-serif", "Liberation Sans"),
    ("times new roman", "Liberation Serif"),
    ("times", "Liberation Serif"),
    ("georgia", "Liberation Serif"),
    ("garamond", "Liberation Serif"),
    ("book antiqua", "Liberation Serif"),
    ("palatino linotype", "Liberation Serif"),
    ("traditional arabic", "Liberation Serif"),
    ("simplified arabic", "Liberation Serif"),
    ("serif", "Liberation Serif"),
    ("courier new", "Liberation Mono"),
    ("courier", "Liberation Mono"),
    ("consolas", "Liberation Mono"),
    ("lucida console", "Liberation Mono"),
    ("monospace", "Liberation Mono"),
    ("source sans pro", "Source Sans 3"),
    ("source serif pro", "Source Serif 4"),
];

/// The default family (Excel's default font is Calibri).
const DEFAULT_FAMILIES: &[&str] = &["Carlito", "Liberation Sans", "Source Sans 3", "Inter"];

/// The embedded family a font name is drawn in: the family itself, its open substitute, or the
/// default (Carlito). `None` without fonts.
pub fn resolve_family(name: &str) -> Option<&'static str> {
    let fs = faces();
    let has = |n: &str| fs.iter().find(|f| f.family.eq_ignore_ascii_case(n)).map(|f| f.family);
    let n = name.trim().trim_matches('"').trim_matches('\'');
    if !n.is_empty()
        && let Some(f) = has(n)
    {
        return Some(f);
    }
    let lower = n.to_ascii_lowercase();
    if let Some((_, sub)) = ALIASES.iter().find(|(a, _)| *a == lower)
        && let Some(f) = has(sub)
    {
        return Some(f);
    }
    DEFAULT_FAMILIES.iter().find_map(|d| has(d)).or_else(|| fs.iter().find(|f| f.scripts.contains(&"Latn")).map(|f| f.family))
}

/// The face of `family` closest to (bold, italic).
fn styled(family: &str, bold: bool, italic: bool) -> Option<u8> {
    let target: i32 = if bold { 700 } else { 400 };
    faces()
        .iter()
        .enumerate()
        .filter(|(_, f)| f.family == family)
        .min_by_key(|(_, f)| (if f.italic == italic { 0 } else { 1000 }) + (i32::from(f.weight) - target).abs() + i32::from(f.is_variable()))
        .map(|(i, _)| i as u8)
}

/// The face for text in font `family` (any name), bold and italic as asked.
pub fn face_for(family: &str, bold: bool, italic: bool) -> Option<u8> {
    styled(resolve_family(family)?, bold, italic)
}

/// The Arabic face at this weight.
fn arabic_face(bold: bool) -> Option<u8> {
    let fam = faces().iter().find(|f| f.arabic)?.family;
    styled(fam, bold, false)
}

/// ISO 15924 tags a character's script is covered by.
fn script_tags(c: char) -> &'static [&'static str] {
    match c.script() {
        Script::Han => &["Hans", "Jpan", "Hant", "Kore"],
        Script::Hiragana | Script::Katakana => &["Jpan"],
        Script::Hangul => &["Kore"],
        Script::Thai => &["Thai"],
        Script::Arabic => &["Arab"],
        Script::Hebrew => &["Hebr"],
        Script::Devanagari => &["Deva"],
        _ => &[],
    }
}

/// A face that has `c`, styled like the text (first one whose script covers it, else any).
fn fallback(c: char, bold: bool, italic: bool) -> Option<u8> {
    let fs = faces();
    let tags = script_tags(c);
    let mut families: Vec<&'static str> = Vec::new();
    // By script tag in order, sans-serif families (Gothic, Sans) before the others.
    let sans = |f: &str| f.contains("Gothic") || f.contains("Sans");
    for tag in tags {
        let mut here: Vec<&'static str> = Vec::new();
        for f in fs.iter().filter(|f| f.scripts.contains(tag)) {
            if !families.contains(&f.family) && !here.contains(&f.family) {
                here.push(f.family);
            }
        }
        here.sort_by_key(|f| !sans(f));
        families.extend(here);
    }
    for f in fs {
        if !families.contains(&f.family) {
            families.push(f.family);
        }
    }
    families.into_iter().filter_map(|fam| styled(fam, bold, italic)).find(|&i| face(i).is_some_and(|f| f.glyph(c) != 0))
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
    /// Italic text in a face with no italic (Arabic): drawn slanted.
    pub slant: bool,
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

/// How a line is set: font family (any name, see [`resolve_family`]), bold, italic.
#[derive(Clone, Copy, Debug, Default)]
pub struct TextStyle<'a> {
    pub family: &'a str,
    pub bold: bool,
    pub italic: bool,
}

/// Face per character of `text` (logical order).
fn assign_faces(text: &str, st: &TextStyle<'_>) -> Vec<u8> {
    let latin = face_for(st.family, st.bold, st.italic).unwrap_or(0);
    let arabic = arabic_face(st.bold).unwrap_or(latin);
    let has = |f: u8, c: char| face(f).is_some_and(|x| x.glyph(c) != 0);
    let pick = |preferred: u8, c: char| -> u8 {
        if has(preferred, c) {
            preferred
        } else if preferred != latin && has(latin, c) {
            latin
        } else if preferred != arabic && has(arabic, c) {
            arabic
        } else {
            fallback(c, st.bold, st.italic).unwrap_or(preferred)
        }
    };
    let chars: Vec<char> = text.chars().collect();
    let strong: Vec<Option<u8>> = chars
        .iter()
        .map(|&c| {
            if neutral(c) {
                None
            } else if arabic_script(c) {
                Some(pick(arabic, c))
            } else {
                Some(pick(latin, c))
            }
        })
        .collect();
    let mut out = Vec::with_capacity(chars.len());
    let mut prev: Option<u8> = None;
    for (i, &c) in chars.iter().enumerate() {
        let f = match strong.get(i).copied().flatten() {
            Some(f) => f,
            // A neutral stays with the run it is in: the previous strong face, else the next.
            None => pick(prev.or_else(|| strong.iter().skip(i + 1).flatten().next().copied()).unwrap_or(latin), c),
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
fn shape_run(text: &str, offset: usize, full: &str, f: u8, rtl: bool, scale: f32, italic: bool) -> (Vec<Glyph>, f32) {
    let Some(fc) = face(f) else { return (vec![], 0.0) };
    let Some(font) = fc.font() else { return (vec![], 0.0) };
    let shaper = fc.shaper.shaper(&font).instance(fc.instance.as_ref()).build();
    let slant = italic && !fc.italic;
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
            slant,
        });
        pen += pos.x_advance as f32 * scale;
    }
    (out, pen)
}

/// Lays out one line of `text` (newlines are treated as spaces) at `size` in the default family,
/// with paragraph direction `base` (`None`: the first strong character's).
pub fn shape(text: &str, size: f32, bold: bool, base: Option<Dir>) -> Line {
    shape_styled(text, size, &TextStyle { family: "", bold, italic: false }, base)
}

/// Lays out one line of `text` (newlines are treated as spaces) at `size` in `style`, with
/// paragraph direction `base` (`None`: the first strong character's).
pub fn shape_styled(text: &str, size: f32, style: &TextStyle<'_>, base: Option<Dir>) -> Line {
    if !available() || text.is_empty() || !size.is_finite() || size <= 0.0 {
        return Line::default();
    }
    // Hostile input: one line never needs more than this.
    let text: String = text.chars().take(20_000).map(|c| if c == '\n' || c == '\r' || c == '\u{2029}' { ' ' } else { c }).collect();
    let base = base.or_else(|| base_direction(&text)).unwrap_or(Dir::Ltr);
    let level = if base == Dir::Rtl { unicode_bidi::Level::rtl() } else { unicode_bidi::Level::ltr() };
    let info = unicode_bidi::BidiInfo::new(&text, Some(level));
    let faces_of = assign_faces(&text, style);
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
                let (glyphs, adv) = shape_run(piece, s, &text, f, rtl, scale, style.italic);
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

/// Width of `text` laid out at `size` in the default family.
pub fn width(text: &str, size: f32, bold: bool) -> f32 {
    shape(text, size, bold, None).width
}

/// Width of `text` laid out at `size` in `style`.
pub fn width_styled(text: &str, size: f32, style: &TextStyle<'_>) -> f32 {
    shape_styled(text, size, style, None).width
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
    g.draw(DrawSettings::unhinted(Size::new(size), LocationRef::from(&fc.location)), &mut Flip(pen)).is_ok()
}

/// For tests: are fonts there? Without craft-fonts a test that needs them is skipped, unless
/// `CRAFT_FONTS_REQUIRED` is set (CI, releases), where that is a failure.
pub fn fonts_for_tests() -> bool {
    if available() {
        return true;
    }
    assert!(std::env::var_os("CRAFT_FONTS_REQUIRED").is_none(), "CRAFT_FONTS_REQUIRED is set but no craft-fonts are embedded (CRAFT_FONTS_DIR)");
    eprintln!("skipped: built without craft-fonts (set CRAFT_FONTS_DIR)");
    false
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn latin() -> u8 {
        face_for("", false, false).unwrap()
    }
    fn arabic() -> u8 {
        arabic_face(false).unwrap()
    }

    #[test]
    fn craft_fonts_are_embedded() {
        if !fonts_for_tests() {
            return;
        }
        let l = face(latin()).unwrap();
        let a = face(arabic()).unwrap();
        assert_eq!(l.family, "Carlito", "Calibri's metric-compatible substitute is the default");
        assert_ne!(l.glyph('A'), 0);
        assert_ne!(a.glyph('م'), 0);
        assert_eq!(l.glyph('م'), 0, "Carlito has no Arabic: the fallback is needed");
        // Real italic and bold-italic faces.
        for (b, i) in [(false, true), (true, true), (true, false)] {
            let f = face(face_for("Calibri", b, i).unwrap()).unwrap();
            assert_eq!((f.family, f.bold, f.italic), ("Carlito", b, i));
            assert!(!f.is_variable());
        }
        assert!(face(face_for("Calibri", false, true).unwrap()).unwrap().italic_angle < 0.0);
        // Office fonts map to their substitutes; unknown names to the default.
        assert_eq!(resolve_family("Arial"), Some("Liberation Sans"));
        assert_eq!(resolve_family("Times New Roman"), Some("Liberation Serif"));
        assert_eq!(resolve_family("Courier New"), Some("Liberation Mono"));
        assert_eq!(resolve_family("Cambria"), Some("Caladea"));
        assert_eq!(resolve_family("Lato"), Some("Lato"));
        assert_eq!(resolve_family("No Such Font"), Some("Carlito"));
        // The variable Arabic font gives a regular and a real bold instance.
        let ab = face(arabic_face(true).unwrap()).unwrap();
        assert!(ab.bold && ab.arabic && ab.is_variable(), "{}", ab.name);
        assert!(faces().iter().map(|f| &f.name).collect::<std::collections::HashSet<_>>().len() == faces().len());
    }

    #[test]
    fn italic_uses_the_italic_face_and_arabic_is_slanted() {
        if !fonts_for_tests() {
            return;
        }
        let st = TextStyle { family: "Calibri", bold: false, italic: true };
        let l = shape_styled("Total مجموع", 12.0, &st, None);
        let it = face_for("Calibri", false, true).unwrap();
        assert!(l.glyphs.iter().filter(|g| g.face == it).all(|g| !g.slant));
        assert!(l.glyphs.iter().any(|g| g.face == it));
        assert!(l.glyphs.iter().filter(|g| face(g.face).unwrap().arabic).all(|g| g.slant), "no Arabic italic exists");
        let upright = shape_styled("Total", 12.0, &TextStyle { family: "Calibri", ..TextStyle::default() }, None);
        let italic = shape_styled("Total", 12.0, &st, None);
        assert_ne!(upright.glyphs[0].face, italic.glyphs[0].face, "the italic face, not a slanted copy");
        assert!(face(italic.glyphs[0].face).unwrap().italic);
    }

    #[test]
    fn latin_shapes_left_to_right() {
        if !fonts_for_tests() {
            return;
        }
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
        if !fonts_for_tests() {
            return;
        }
        let text = "مرحبا";
        let l = shape(text, 12.0, false, None);
        assert!(l.rtl, "an Arabic line is right-to-left by default");
        let arabic = arabic();
        assert!(l.glyphs.iter().all(|g| g.face == arabic && g.gid != 0), "{l:?}");
        let f = face(arabic).unwrap();
        let isolated: Vec<u16> = text.chars().map(|c| f.glyph(c)).collect();
        let shaped: Vec<u16> = l.glyphs.iter().map(|g| g.gid).collect();
        assert_ne!(shaped, isolated, "contextual forms are used");
        let first = l.glyphs.iter().find(|g| g.text.as_deref().is_some_and(|t| t.starts_with('م'))).unwrap();
        assert!(first.x >= l.glyphs.iter().map(|g| g.x).fold(0.0, f32::max) - 0.01, "{l:?}");
        // Bold Arabic is the bold instance, and wider.
        assert!(width(text, 12.0, true) > l.width);
    }

    #[test]
    fn mixed_lines_keep_each_script_in_its_face() {
        if !fonts_for_tests() {
            return;
        }
        let l = shape("Total: 1,250 ريال", 10.0, false, None);
        assert!(!l.rtl, "the first strong character is Latin");
        assert!(l.glyphs.iter().any(|g| g.face == latin()));
        assert!(l.glyphs.iter().any(|g| g.face == arabic()));
        assert!(l.glyphs.iter().all(|g| g.gid != 0), "no missing glyphs: {l:?}");
        let r = shape("المجموع 1250 SAR", 10.0, false, None);
        assert!(r.rtl);
        let sar = r.glyphs.iter().position(|g| g.text.as_deref() == Some("S")).unwrap();
        let alif = r.glyphs.iter().position(|g| g.text.as_deref().is_some_and(|t| t.starts_with('ا'))).unwrap();
        assert!(r.glyphs[sar].x < r.glyphs[alif].x);
        // CJK goes to a CJK face.
        let c = shape("合計 Total", 10.0, false, None);
        assert!(c.glyphs.iter().all(|g| g.gid != 0), "{c:?}");
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
        assert_eq!(base_direction("123 abc"), Some(Dir::Ltr));
        assert_eq!(base_direction("  ١٢ مرحبا"), Some(Dir::Rtl));
        assert_eq!(base_direction("123"), None);
        assert!(is_complex("مرحبا") && !is_complex("Hello, world"));
        if !fonts_for_tests() {
            return;
        }
        let l = shape("ب", 20.0, true, None);
        let mut c = Count(0);
        assert!(outline(l.glyphs[0].face, l.glyphs[0].gid, 20.0, &mut c));
        assert!(c.0 >= 1);
        assert!(shape("abc", 10.0, false, Some(Dir::Rtl)).rtl);
        assert!(shape("", 10.0, false, None).glyphs.is_empty());
        assert!(shape("x", f32::NAN, false, None).glyphs.is_empty());
    }
}
