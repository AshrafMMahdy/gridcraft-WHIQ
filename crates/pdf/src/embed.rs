//! WHIQ: embedded subset fonts (Type0, Identity-H).
//!
//! TrueType faces (CIDFontType2, `FontFile2`): content streams address glyphs by CID, and every
//! (glyph, text) pair gets its own CID, because shaped Arabic draws different letters with the
//! same glyph (a dotless base plus a dot mark): one CID per glyph would give `ToUnicode` one
//! letter for all of them. The `CIDToGIDMap` stream maps each CID to the glyph in the subset.
//!
//! CFF faces (`.otf`, the CJK fonts; CIDFontType0, `FontFile3 /OpenType`): the subsetter writes a
//! CID-keyed CFF whose CIDs are the new glyph ids, so a CID is the glyph's id in the subset,
//! allocated in first-use order exactly as the subsetter numbers them.
//!
//! A variable font's instance (Noto Sans Arabic bold) is instantiated at its weight while subset,
//! so the PDF holds a static font. Widths come from the face's metrics at that instance.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use gridcraft_text::Face;

/// Objects one embedded font takes: Type0, CIDFont, descriptor, font file, ToUnicode, CIDToGIDMap.
pub(crate) const OBJECTS: u32 = 6;

/// The CIDs of one face: CID → (glyph, text it shows). CID 0 is `.notdef`.
#[derive(Debug, Default)]
pub(crate) struct FaceCids {
    by_key: std::collections::HashMap<(u16, String), u16>,
    pub(crate) list: Vec<(u16, String)>,
    /// CFF faces: glyph → CID (the glyph's id in the subset).
    by_gid: std::collections::HashMap<u16, u16>,
    pub(crate) cff: bool,
}

/// The CIDs of every face a document embeds.
#[derive(Debug, Default)]
pub(crate) struct Cids {
    faces: BTreeMap<u8, FaceCids>,
}

impl Cids {
    /// The CID showing glyph `gid` of face `face` as `text` (allocated on first use).
    pub(crate) fn cid(&mut self, face: u8, gid: u16, text: &str, cff: bool) -> u16 {
        let f = self.faces.entry(face).or_default();
        if cff {
            f.cff = true;
            if gid == 0 {
                return 0;
            }
            if let Some(c) = f.by_gid.get(&gid) {
                return *c;
            }
            if f.list.len() >= 65_534 {
                return 0;
            }
            f.list.push((gid, text.to_string()));
            let c = f.list.len() as u16;
            f.by_gid.insert(gid, c);
            return c;
        }
        if let Some(c) = f.by_key.get(&(gid, text.to_string())) {
            return *c;
        }
        // CIDs run out after 65 534 distinct (glyph, text) pairs: reuse the glyph's first CID.
        if f.list.len() >= 65_534 {
            return f.list.iter().position(|(g, _)| *g == gid).map_or(0, |i| (i + 1) as u16);
        }
        f.list.push((gid, text.to_string()));
        let c = f.list.len() as u16;
        f.by_key.insert((gid, text.to_string()), c);
        c
    }

    pub(crate) fn faces(&self) -> impl Iterator<Item = (&u8, &FaceCids)> {
        self.faces.iter()
    }
}

fn stream(dict: &str, data: &[u8], compress: bool) -> Vec<u8> {
    let (data, filter) = if compress { (miniz_oxide::deflate::compress_to_vec_zlib(data, 6), " /Filter /FlateDecode") } else { (data.to_vec(), "") };
    let mut o = format!("<< {dict}{filter} /Length {} >>\nstream\n", data.len()).into_bytes();
    o.extend_from_slice(&data);
    o.extend_from_slice(b"\nendstream");
    o
}

/// Six capital letters naming the subset (PDF 9.6.4), stable for the same face and glyphs.
fn tag(face: &Face, used: &FaceCids) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in face.name.bytes().chain(used.list.iter().flat_map(|(g, _)| g.to_be_bytes())) {
        h ^= u64::from(b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    (0..6).map(|i| char::from(b'A' + ((h >> (i * 5)) % 26) as u8)).collect()
}

fn utf16_hex(s: &str) -> String {
    let mut out = String::new();
    for u in s.encode_utf16().take(64) {
        let _ = write!(out, "{u:04X}");
    }
    out
}

/// The objects for `face` with the CIDs in `used`, numbered from `id`.
pub(crate) fn font_objects(face: &Face, used: &FaceCids, id: u32) -> Vec<(u32, Vec<u8>)> {
    let (type0, cid, desc, file, tounicode, map) = (id, id + 1, id + 2, id + 3, id + 4, id + 5);
    let mut remap = subsetter::GlyphRemapper::new();
    for (g, _) in &used.list {
        remap.remap(*g);
    }
    // A variable font's instance is instantiated at its weight.
    let variations: Vec<(subsetter::Tag, f32)> = face.variations.iter().map(|(t, v)| (subsetter::Tag::new(t), *v)).collect();
    let subset_result = if variations.is_empty() {
        subsetter::subset(face.data, 0, &remap)
    } else {
        subsetter::subset_with_variations(face.data, 0, &variations, &remap)
    };
    // A face the subsetter can't handle is embedded whole: correct for TrueType (only bigger);
    // a whole CFF face keeps the file valid, though its glyph ids then differ from the CIDs.
    let (data, subset) = match subset_result {
        Ok(d) => (d, true),
        Err(_) => (face.data.to_vec(), false),
    };
    // A CFF font stays CFF unless it was instanced (the subsetter then writes TrueType outlines).
    let cff = used.cff && data.starts_with(b"OTTO");
    let name = format!("{}+{}", tag(face, used), face.name);
    let k = 1000.0 / face.upem.max(1.0);
    let u = |v: f32| (v * k).round() as i64;
    let mut out = Vec::with_capacity(OBJECTS as usize);
    out.push((
        type0,
        format!("<< /Type /Font /Subtype /Type0 /BaseFont /{name} /Encoding /Identity-H /DescendantFonts [{cid} 0 R] /ToUnicode {tounicode} 0 R >>")
            .into_bytes(),
    ));
    let mut widths = String::new();
    for (i, (g, _)) in used.list.iter().enumerate() {
        let _ = write!(widths, "{} [{}] ", i + 1, u(face.advance(*g)));
    }
    out.push((
        cid,
        format!(
            "<< /Type /Font /Subtype /{} /BaseFont /{name} /CIDSystemInfo << /Registry (Adobe) /Ordering (Identity) /Supplement 0 >> /FontDescriptor {desc} 0 R /DW {} /W [{widths}]{} >>",
            if cff { "CIDFontType0" } else { "CIDFontType2" },
            u(face.upem / 2.0),
            if cff { String::new() } else { format!(" /CIDToGIDMap {map} 0 R") }
        )
        .into_bytes(),
    ));
    let [x0, y0, x1, y1] = face.bbox;
    out.push((
        desc,
        format!(
            "<< /Type /FontDescriptor /FontName /{name} /Flags {} /FontBBox [{} {} {} {}] /ItalicAngle {} /Ascent {} /Descent {} /CapHeight {} /StemV {} /{} {file} 0 R >>",
            // Symbolic (4), plus Italic (64).
            if face.italic { 4 | 64 } else { 4 },
            u(x0),
            u(y0),
            u(x1),
            u(y1),
            (face.italic_angle * 100.0).round() / 100.0,
            u(face.ascent),
            -u(face.descent),
            u(face.cap_height),
            if face.bold { 140 } else { 80 },
            if cff { "FontFile3" } else { "FontFile2" }
        )
        .into_bytes(),
    ));
    let file_dict = if cff { "/Subtype /OpenType".to_string() } else { format!("/Length1 {}", data.len()) };
    out.push((file, stream(&file_dict, &data, true)));
    // ToUnicode.
    let mut cmap = String::from(
        "/CIDInit /ProcSet findresource begin\n12 dict begin\nbegincmap\n/CIDSystemInfo << /Registry (Adobe) /Ordering (UCS) /Supplement 0 >> def\n/CMapName /Adobe-Identity-UCS def\n/CMapType 2 def\n1 begincodespacerange\n<0000> <FFFF>\nendcodespacerange\n",
    );
    let mapped: Vec<(usize, &String)> = used.list.iter().enumerate().filter(|(_, (_, t))| !t.is_empty()).map(|(i, (_, t))| (i + 1, t)).collect();
    for chunk in mapped.chunks(100) {
        let _ = writeln!(cmap, "{} beginbfchar", chunk.len());
        for (c, t) in chunk {
            let _ = writeln!(cmap, "<{c:04X}> <{}>", utf16_hex(t));
        }
        cmap.push_str("endbfchar\n");
    }
    cmap.push_str("endcmap\nCMapName currentdict /CMapResource defineresource pop\nend\nend\n");
    out.push((tounicode, stream("", cmap.as_bytes(), false)));
    // CIDToGIDMap: CID → glyph id in the subset.
    let mut m = vec![0u8; (used.list.len() + 1) * 2];
    for (i, (g, _)) in used.list.iter().enumerate() {
        let new = if subset { remap.get(*g).unwrap_or(0) } else { *g };
        if let Some(slot) = m.get_mut((i + 1) * 2..(i + 2) * 2) {
            slot.copy_from_slice(&new.to_be_bytes());
        }
    }
    out.push((map, stream("", &m, true)));
    out
}
