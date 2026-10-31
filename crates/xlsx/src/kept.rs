//! WHIQ: a customer's file saved after an edit keeps what the edit did not touch exactly as the
//! file had it. Charts the edit left alone are written back from their own part (cached values
//! refreshed from the cells), with the style, colour and shape parts behind them and the drawing
//! anchor as it was while the chart has not moved. The theme part, the indexed colour palette and
//! `workbookProtection` are written back verbatim while the model's view of them is the one read
//! ([`kept_theme`], [`kept_colors`], [`kept_protection`]).

use std::fmt::Write as _;

use gridcraft_core::Value;
use gridcraft_model::{Chart, PreservedChart, Sheet, Workbook};
use quick_xml::Reader;
use quick_xml::events::Event;

use crate::passthrough::{anchor_fingerprint, chart_fingerprint, map_rel_ids, part_rels_xml, rels_part_name, write_parts};
use crate::write::Out;
use crate::xml::{self, esc, num};

/// The kept part of `ch` when the chart is the one read (not edited since).
pub(crate) fn kept_chart<'a>(sheet: &'a Sheet, ch: &Chart) -> Option<&'a PreservedChart> {
    let k = sheet.preserved.as_deref()?.charts.iter().find(|k| k.id == ch.id)?;
    (k.fingerprint == chart_fingerprint(ch)).then_some(k)
}

/// Writes a kept chart as `name` (with the parts behind it). False when its part can't be
/// written back (damaged XML): the caller writes the chart from the model instead.
pub(crate) fn write_kept_chart(out: &mut Out, k: &PreservedChart, wb: &Workbook, si: usize, name: &str) -> bool {
    let Some(data) = refresh_caches(&k.part.data, wb, si) else { return false };
    let names = write_parts(out, &k.parts);
    out.part(name, Some(&k.part.content_type), data);
    if !k.part.rels.is_empty() {
        out.part(&rels_part_name(name), None, part_rels_xml(&k.part.rels, &names).into_bytes());
    }
    true
}

/// The chart's anchor element as the file had it, naming the chart by `rid`, while the chart has
/// not been moved or resized.
pub(crate) fn kept_anchor(sheet: &Sheet, ch: &Chart, rid: &str) -> Option<String> {
    let k = sheet.preserved.as_deref()?.charts.iter().find(|k| k.id == ch.id)?;
    if k.anchor_fingerprint != anchor_fingerprint(&ch.anchor) {
        return None;
    }
    let x = k.anchor_xml.as_deref()?;
    map_rel_ids(x, &mut |old| (old == k.anchor_rid).then(|| rid.to_string()))
}

/// The anchor element with its drawing object's id (`cNvPr id`) set to `id`, unique in the
/// drawing GridCraft writes.
pub(crate) fn renumber_object(x: &str, id: usize) -> String {
    let Some(at) = x.find("cNvPr") else { return x.to_string() };
    let Some(close) = x.get(at..).and_then(|t| t.find('>')).map(|c| at + c) else { return x.to_string() };
    let tag = x.get(at..close).unwrap_or("");
    let Some(rel) = tag.find(" id=\"") else { return x.to_string() };
    let start = at + rel + 5;
    let Some(len) = x.get(start..close).and_then(|t| t.find('"')) else { return x.to_string() };
    format!("{}{id}{}", x.get(..start).unwrap_or(""), x.get(start + len..).unwrap_or(""))
}

/// The chart part with every `numCache` / `strCache` refilled from the cells its formula names
/// (the edit may have changed them); anything else byte for byte. `None` when the XML is damaged.
pub(crate) fn refresh_caches(data: &[u8], wb: &Workbook, si: usize) -> Option<Vec<u8>> {
    if !contains(data, b"Cache") {
        return Some(data.to_vec());
    }
    let utf8 = xml::to_utf8(data);
    let text = std::str::from_utf8(&utf8).ok()?;
    let mut reader = Reader::from_str(text);
    let mut out = String::with_capacity(text.len());
    let mut copied = 0usize;
    loop {
        let start = usize::try_from(reader.buffer_position()).ok()?;
        match reader.read_event().ok()? {
            Event::Eof => break,
            Event::Start(e) if matches!(e.local_name().as_ref(), b"numRef" | b"strRef") => {
                reader.read_to_end(e.name()).ok()?;
                let end = usize::try_from(reader.buffer_position()).ok()?;
                if let Some(new) = refresh_ref(text.get(start..end)?, wb, si) {
                    out.push_str(text.get(copied..start)?);
                    out.push_str(&new);
                    copied = end;
                }
            }
            _ => {}
        }
    }
    out.push_str(text.get(copied..)?);
    Some(out.into_bytes())
}

/// A `numRef` / `strRef` element with its cache refilled; `None` (keep it) when it has no cache or
/// its formula names no cells here.
fn refresh_ref(frag: &str, wb: &Workbook, si: usize) -> Option<String> {
    let el = xml::parse(frag.as_bytes()).ok()?;
    let formula = el.child("f")?.text.clone();
    let mut reader = Reader::from_str(frag);
    loop {
        let start = usize::try_from(reader.buffer_position()).ok()?;
        let (qname, numeric, end) = match reader.read_event().ok()? {
            Event::Eof => return None,
            Event::Start(e) if matches!(e.local_name().as_ref(), b"numCache" | b"strCache") => {
                let numeric = e.local_name().as_ref() == b"numCache";
                let qname = String::from_utf8(e.name().as_ref().to_vec()).ok()?;
                reader.read_to_end(e.name()).ok()?;
                (qname, numeric, usize::try_from(reader.buffer_position()).ok()?)
            }
            Event::Empty(e) if matches!(e.local_name().as_ref(), b"numCache" | b"strCache") => {
                let numeric = e.local_name().as_ref() == b"numCache";
                (String::from_utf8(e.name().as_ref().to_vec()).ok()?, numeric, usize::try_from(reader.buffer_position()).ok()?)
            }
            _ => continue,
        };
        let new = cache_xml(&qname, numeric, &formula, frag.get(start..end)?, wb, si)?;
        return Some(format!("{}{new}{}", frag.get(..start)?, frag.get(end..)?));
    }
}

fn contains(hay: &[u8], needle: &[u8]) -> bool {
    hay.windows(needle.len()).any(|w| w == needle)
}

/// A cache element named `qname` (`c:numCache`…) filled from the cells of `formula`, keeping the
/// old cache's number format. `None` (keep the old one) when the formula names no cells here.
fn cache_xml(qname: &str, numeric: bool, formula: &str, old: &str, wb: &Workbook, si: usize) -> Option<String> {
    let f = formula.trim();
    let f = f.strip_prefix('=').unwrap_or(f);
    let vals = crate::chart::ref_values(wb, si, f)?;
    let p = qname.rsplit_once(':').map(|(p, _)| format!("{p}:")).unwrap_or_default();
    let mut s = format!("<{qname}>");
    if numeric {
        let code = xml::parse(old.as_bytes()).ok().and_then(|e| e.child("formatCode").map(|c| c.text.clone())).unwrap_or_else(|| "General".into());
        let _ = write!(s, "<{p}formatCode>{}</{p}formatCode>", esc(&code));
    }
    let _ = write!(s, "<{p}ptCount val=\"{}\"/>", vals.len());
    for (i, v) in vals.iter().enumerate() {
        match v {
            Value::Number(n) if numeric => {
                let _ = write!(s, "<{p}pt idx=\"{i}\"><{p}v>{}</{p}v></{p}pt>", num(*n));
            }
            v if !numeric && !v.is_empty() => {
                let _ = write!(s, "<{p}pt idx=\"{i}\"><{p}v>{}</{p}v></{p}pt>", esc(&v.display()));
            }
            _ => {}
        }
    }
    let _ = write!(s, "</{qname}>");
    Some(s)
}

/// The theme part as the file had it, while the model's theme is the one read from it.
pub(crate) fn kept_theme(wb: &Workbook) -> Option<Vec<u8>> {
    let x = crate::passthrough::workbook_elements(wb, "theme");
    if x.is_empty() {
        return None;
    }
    let el = xml::parse(x.as_bytes()).ok()?;
    (crate::theme::read_theme(&el) == wb.theme).then(|| x.into_bytes())
}

/// The styles part's `<colors>` (indexed palette, recently used colours) as the file had it.
pub(crate) fn kept_colors(wb: &Workbook) -> String {
    crate::passthrough::workbook_elements(wb, "colors")
}

/// `<workbookProtection>` as the file had it, while the model's structure protection is the one
/// read from it.
pub(crate) fn kept_protection(wb: &Workbook) -> Option<String> {
    let x = crate::passthrough::workbook_elements(wb, "workbookProtection");
    if x.is_empty() {
        return None;
    }
    let el = xml::parse(x.as_bytes()).ok()?;
    let locked = el.flag("lockStructure", false);
    let password = if locked { crate::sheet_read::read_password(&el, "workbook") } else { None };
    (locked == wb.protected_structure && password == wb.structure_password).then_some(x)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use gridcraft_core::{CellRef, Value};
    use gridcraft_model::{Sheet, Workbook};

    use super::*;

    fn book() -> Workbook {
        let mut wb = Workbook::new();
        wb.sheets.clear();
        let mut sh = Sheet::new("R&D");
        sh.set_value(CellRef::new(0, 1), Value::number(5.0));
        sh.set_value(CellRef::new(1, 1), Value::number(7.5));
        sh.set_value(CellRef::new(0, 0), Value::Text("North".into()));
        wb.sheets.push(Arc::new(sh));
        wb
    }

    #[test]
    fn caches_are_refilled_and_everything_else_stays() {
        let wb = book();
        let xml = r#"<c:chartSpace xmlns:c="http://schemas.openxmlformats.org/drawingml/2006/chart"><c:x a="1"/><c:val><c:numRef><c:f>'R&amp;D'!$B$1:$B$2</c:f><c:numCache><c:formatCode>0.0</c:formatCode><c:ptCount val="2"/><c:pt idx="0"><c:v>1</c:v></c:pt></c:numCache></c:numRef></c:val><c:tx><c:strRef><c:f>'R&amp;D'!$A$1</c:f><c:strCache><c:ptCount val="1"/><c:pt idx="0"><c:v>old</c:v></c:pt></c:strCache></c:strRef></c:tx><c:cat><c:numRef><c:f>Elsewhere!A1:A3</c:f><c:numCache><c:ptCount val="0"/></c:numCache></c:numRef></c:cat></c:chartSpace>"#;
        let out = String::from_utf8(refresh_caches(xml.as_bytes(), &wb, 0).unwrap()).unwrap();
        assert!(out.contains("<c:numCache><c:formatCode>0.0</c:formatCode><c:ptCount val=\"2\"/><c:pt idx=\"0\"><c:v>5</c:v></c:pt><c:pt idx=\"1\"><c:v>7.5</c:v></c:pt></c:numCache>"), "{out}");
        assert!(out.contains("<c:strCache><c:ptCount val=\"1\"/><c:pt idx=\"0\"><c:v>North</c:v></c:pt></c:strCache>"), "{out}");
        // A formula naming no sheet here keeps its cache; the rest is byte for byte.
        assert!(out.contains("<c:f>Elsewhere!A1:A3</c:f><c:numCache><c:ptCount val=\"0\"/></c:numCache>"), "{out}");
        assert!(out.starts_with("<c:chartSpace xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:x a=\"1\"/>"));
        // A part without caches is returned as it is; damaged XML is refused.
        assert_eq!(refresh_caches(b"<chartSpace><f>x</f></chartSpace>", &wb, 0).unwrap(), b"<chartSpace><f>x</f></chartSpace>");
        assert!(refresh_caches(b"<chartSpace><numRef><numCache></numRef>", &wb, 0).is_none());
    }

    #[test]
    fn a_kept_anchor_gets_a_fresh_object_id_and_its_namespaces() {
        let x = r#"<wsDr xmlns="urn:sd" xmlns:r="http://schemas.openxmlformats.org/officeDocument/2006/relationships"><oneCellAnchor><from/><graphicFrame><nvGraphicFramePr><cNvPr id="1" name="Chart 1"/></nvGraphicFramePr><c:chart xmlns:c="urn:c" r:id="rId1"/></graphicFrame></oneCellAnchor><absoluteAnchor/></wsDr>"#;
        let frags = crate::passthrough::top_level_fragments(x.as_bytes()).unwrap();
        assert_eq!(frags.len(), 2);
        assert!(
            frags[0].starts_with("<oneCellAnchor xmlns=\"urn:sd\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\">"),
            "{}",
            frags[0]
        );
        assert_eq!(frags[1], "<absoluteAnchor xmlns=\"urn:sd\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\"/>");
        let mapped = crate::passthrough::map_rel_ids(&frags[0], &mut |id| (id == "rId1").then(|| "rId7".to_string())).unwrap();
        assert!(mapped.contains("r:id=\"rId7\""), "{mapped}");
        let renumbered = renumber_object(&mapped, 12);
        assert!(renumbered.contains("<cNvPr id=\"12\" name=\"Chart 1\"/>"), "{renumbered}");
    }
}
