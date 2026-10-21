//! WHIQ parity pass: the round-trip check WordCraft has (`roundtrip_check`), for any Open
//! Packaging Conventions file (.xlsx, .pptx…). It compares a package with the package the app
//! saves from it: the part list (a part saved under another name with identical bytes counts as
//! renamed), every part's relationships (by type and target), the content types, binary parts
//! byte for byte, and every XML part as an inventory of its elements by namespace and local name
//! (anything fewer after the save is a loss). The report has the keys WordCraft's has, so an
//! agent reads one shape in every craft app.

use std::collections::{BTreeMap, BTreeSet};
use std::io::Read;

use serde_json::{Value, json};

/// One part of a package: its unpacked size and CRC-32 from the zip's directory (binary parts are
/// compared by these, never inflated), and the bytes of an XML part.
#[derive(PartialEq)]
struct PartInfo {
    size: u64,
    crc: u32,
    xml: Option<Vec<u8>>,
}

/// WHIQ: parts by name (as stored). Binary parts (media, embeddings) are not inflated: the zip
/// directory's size and CRC-32 identify their bytes, so a 600 MB video costs nothing to compare.
/// XML parts are inflated, each within a share of the memory budget (a larger one is an error
/// that says so, never a silent pass).
fn parts<R: Read + std::io::Seek>(r: R) -> Result<BTreeMap<String, PartInfo>, String> {
    let mut z = zip::ZipArchive::new(r).map_err(|e| format!("not a package: {e}"))?;
    let per_part = (gridcraft_engine::core::memory::budget() / 4).max(96 << 20);
    let mut out = BTreeMap::new();
    for i in 0..z.len() {
        let (name, size, crc) = {
            let f = z.by_index_raw(i).map_err(|e| format!("unreadable part: {e}"))?;
            if f.is_dir() {
                continue;
            }
            (f.name().trim_start_matches('/').to_string(), f.size(), f.crc32())
        };
        let xml = if is_xml(&name) {
            if size > per_part {
                return Err(format!("XML part {name} ({} MB) is larger than this process compares in memory ({} MB)", size >> 20, per_part >> 20));
            }
            let mut f = z.by_index(i).map_err(|e| format!("unreadable part: {e}"))?;
            let mut b = Vec::with_capacity(size.min(16 << 20) as usize);
            f.by_ref().take(per_part.saturating_add(1)).read_to_end(&mut b).map_err(|e| format!("{name}: {e}"))?;
            Some(b)
        } else {
            None
        };
        out.insert(name, PartInfo { size, crc, xml });
    }
    Ok(out)
}

fn is_xml(name: &str) -> bool {
    let n = name.to_ascii_lowercase();
    n.ends_with(".xml") || n.ends_with(".rels") || n.ends_with(".vml")
}

/// The part a `_rels/x.rels` part belongs to (`""` for the package's own `_rels/.rels`).
fn rels_source(rels: &str) -> String {
    let (dir, file) = rels.rsplit_once('/').unwrap_or(("", rels));
    let dir = dir.strip_suffix("_rels").unwrap_or(dir).trim_end_matches('/');
    let file = file.strip_suffix(".rels").unwrap_or(file);
    if dir.is_empty() { file.to_string() } else { format!("{dir}/{file}") }
}

/// `target` of a relationship from `source`, as a part name (external targets as they are).
fn resolve(source: &str, target: &str, external: bool) -> String {
    if external {
        return target.to_string();
    }
    if let Some(abs) = target.strip_prefix('/') {
        return abs.to_string();
    }
    let mut segs: Vec<&str> = source.rsplit_once('/').map(|(d, _)| d.split('/').collect()).unwrap_or_default();
    for s in target.split('/') {
        match s {
            "." | "" => {}
            ".." => {
                segs.pop();
            }
            other => segs.push(other),
        }
    }
    segs.join("/")
}

/// Attributes of each element named `local` in an XML part (namespaces ignored).
fn elements_with_attrs(xml: &[u8], local: &str) -> Vec<BTreeMap<String, String>> {
    let mut r = quick_xml::Reader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out = Vec::new();
    loop {
        match r.read_event_into(&mut buf) {
            Ok(quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e)) if e.local_name().as_ref() == local.as_bytes() => {
                let mut m = BTreeMap::new();
                for a in e.attributes().flatten() {
                    let k = String::from_utf8_lossy(a.key.local_name().as_ref()).into_owned();
                    let raw = String::from_utf8_lossy(&a.value).into_owned();
                    let v = quick_xml::escape::unescape(&raw).map(|v| v.into_owned()).unwrap_or(raw);
                    m.insert(k, v);
                }
                out.push(m);
            }
            Ok(quick_xml::events::Event::Eof) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// `{namespace}local` → count, for every element of an XML part.
fn inventory(xml: &[u8]) -> BTreeMap<String, usize> {
    let mut r = quick_xml::NsReader::from_reader(xml);
    let mut buf = Vec::new();
    let mut out: BTreeMap<String, usize> = BTreeMap::new();
    loop {
        match r.read_resolved_event_into(&mut buf) {
            Ok((ns, quick_xml::events::Event::Start(e) | quick_xml::events::Event::Empty(e))) => {
                let uri = match ns {
                    quick_xml::name::ResolveResult::Bound(n) => String::from_utf8_lossy(n.as_ref()).into_owned(),
                    _ => String::new(),
                };
                let key = format!("{{{uri}}}{}", String::from_utf8_lossy(e.local_name().as_ref()));
                *out.entry(key).or_default() += 1;
            }
            Ok((_, quick_xml::events::Event::Eof)) | Err(_) => break,
            _ => {}
        }
        buf.clear();
    }
    out
}

/// Elements a reader can't see the loss of are left out of the comparison: the package's
/// bookkeeping (relationships and content types are compared on their own).
fn compared_xml(name: &str) -> bool {
    is_xml(name) && !name.ends_with(".rels") && name != "[Content_Types].xml"
}

/// Compare a package with the one saved from it. `Err` when either is not a package.
#[cfg(test)]
pub fn compare(before: &[u8], after: &[u8]) -> Result<Value, String> {
    compare_readers(std::io::Cursor::new(before), std::io::Cursor::new(after))
}

/// WHIQ: [`compare`] on open files (neither package is read whole into memory).
pub fn compare_readers<A: Read + std::io::Seek, B: Read + std::io::Seek>(before: A, after: B) -> Result<Value, String> {
    let a = parts(before)?;
    let b = parts(after)?;
    let lower = |m: &BTreeMap<String, PartInfo>| -> BTreeMap<String, String> { m.keys().map(|k| (k.to_ascii_lowercase(), k.clone())).collect() };
    let (la, lb) = (lower(&a), lower(&b));
    let mut missing: Vec<String> = la.iter().filter(|(k, _)| !lb.contains_key(*k)).map(|(_, v)| v.clone()).collect();
    let mut added: Vec<String> = lb.iter().filter(|(k, _)| !la.contains_key(*k)).map(|(_, v)| v.clone()).collect();
    // A part saved under another name with the same bytes (media renumbered) is renamed, not lost.
    let mut renamed: Vec<(String, String)> = Vec::new();
    missing.retain(|m| {
        let Some(bytes) = a.get(m) else { return true };
        match added.iter().position(|n| b.get(n) == Some(bytes)) {
            Some(i) => {
                renamed.push((m.clone(), added.remove(i)));
                false
            }
            None => true,
        }
    });
    let rename: BTreeMap<String, String> = renamed.iter().map(|(x, y)| (x.to_ascii_lowercase(), y.to_ascii_lowercase())).collect();
    let after_name = |n: &str| -> String {
        let l = n.to_ascii_lowercase();
        rename.get(&l).cloned().unwrap_or(l)
    };
    // Binary parts byte for byte.
    let mut changed_binary = Vec::new();
    for (k, name) in &la {
        if is_xml(name) {
            continue;
        }
        if let Some(other) = lb.get(k)
            && a.get(name) != b.get(other)
        {
            changed_binary.push(name.clone());
        }
    }
    // Relationships: (source, type, target) before that are not there after.
    let rels_of = |m: &BTreeMap<String, PartInfo>, map: &dyn Fn(&str) -> String| -> BTreeSet<(String, String, String)> {
        let mut out = BTreeSet::new();
        for (name, info) in m {
            let Some(bytes) = info.xml.as_deref() else { continue };
            if !name.to_ascii_lowercase().ends_with(".rels") {
                continue;
            }
            let source = rels_source(name);
            for r in elements_with_attrs(bytes, "Relationship") {
                let external = r.get("TargetMode").is_some_and(|t| t.eq_ignore_ascii_case("External"));
                let target = resolve(&source, r.get("Target").map(String::as_str).unwrap_or(""), external);
                let target = if external { target } else { map(&target) };
                let kind = r.get("Type").map(|t| t.rsplit('/').next().unwrap_or(t).to_string()).unwrap_or_default();
                out.insert((map(&source), kind, target));
            }
        }
        out
    };
    let ra = rels_of(&a, &after_name);
    let rb = rels_of(&b, &|n: &str| n.to_ascii_lowercase());
    let missing_rels: Vec<String> = ra.difference(&rb).map(|(s, t, x)| format!("{s} -> {x} ({t})")).collect();
    // Content types of parts kept.
    let types = |m: &BTreeMap<String, PartInfo>| -> (BTreeMap<String, String>, BTreeMap<String, String>) {
        let ct = m.get("[Content_Types].xml").and_then(|p| p.xml.as_deref()).unwrap_or(&[]);
        let defaults = elements_with_attrs(ct, "Default")
            .into_iter()
            .filter_map(|d| Some((d.get("Extension")?.to_ascii_lowercase(), d.get("ContentType")?.clone())))
            .collect();
        let overrides = elements_with_attrs(ct, "Override")
            .into_iter()
            .filter_map(|d| Some((d.get("PartName")?.trim_start_matches('/').to_ascii_lowercase(), d.get("ContentType")?.clone())))
            .collect();
        (defaults, overrides)
    };
    let (da, oa) = types(&a);
    let (db, ob) = types(&b);
    let type_of = |d: &BTreeMap<String, String>, o: &BTreeMap<String, String>, n: &str| -> Option<String> {
        o.get(n).cloned().or_else(|| n.rsplit_once('.').and_then(|(_, e)| d.get(e).cloned()))
    };
    let mut changed_types = Vec::new();
    for k in la.keys() {
        let k2 = after_name(k);
        if !lb.contains_key(&k2) || k.ends_with(".rels") || k == "[content_types].xml" {
            continue;
        }
        if let (Some(x), Some(y)) = (type_of(&da, &oa, k), type_of(&db, &ob, &k2))
            && x != y
        {
            changed_types.push(format!("{} ({x} -> {y})", la.get(k).cloned().unwrap_or_default()));
        }
    }
    // XML parts: an element inventory each.
    let mut stories = Vec::new();
    for (k, name) in &la {
        if !compared_xml(name) {
            continue;
        }
        let Some(other) = lb.get(&after_name(k)) else { continue };
        let (Some(x), Some(y)) = (a.get(name).and_then(|p| p.xml.as_deref()), b.get(other).and_then(|p| p.xml.as_deref())) else { continue };
        let (ix, iy) = (inventory(x), inventory(y));
        let losses: Vec<String> = ix
            .iter()
            .filter_map(|(el, n)| {
                let m = iy.get(el).copied().unwrap_or(0);
                (m < *n).then(|| format!("{el} {n}->{m}"))
            })
            .collect();
        stories.push(json!({"part": name, "identical": x == y, "losses": losses, "textChanges": 0, "firstDifference": null}));
    }
    let mut problems: Vec<String> = Vec::new();
    problems.extend(missing.iter().map(|p| format!("missing part {p}")));
    problems.extend(changed_binary.iter().map(|p| format!("binary part changed {p}")));
    problems.extend(missing_rels.iter().map(|r| format!("missing relationship {r}")));
    problems.extend(changed_types.iter().map(|t| format!("content type changed {t}")));
    for s in &stories {
        for l in s["losses"].as_array().into_iter().flatten().filter_map(Value::as_str) {
            problems.push(format!("{}: lost {l}", s["part"].as_str().unwrap_or("")));
        }
    }
    Ok(json!({
        "lossless": problems.is_empty(),
        "missingParts": missing,
        "addedParts": added,
        "changedBinaryParts": changed_binary,
        "missingRelationships": missing_rels,
        "changedContentTypes": changed_types,
        "renamed_parts": renamed.iter().map(|(x, y)| json!([x, y])).collect::<Vec<_>>(),
        "stories": stories,
        "problems": problems,
    }))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    fn package(parts: &[(&str, &[u8])]) -> Vec<u8> {
        let mut w = zip::ZipWriter::new(std::io::Cursor::new(Vec::new()));
        for (n, b) in parts {
            w.start_file(*n, zip::write::SimpleFileOptions::default()).unwrap();
            std::io::Write::write_all(&mut w, b).unwrap();
        }
        w.finish().unwrap().into_inner()
    }

    #[test]
    fn the_diff_reports_parts_relationships_and_elements() {
        let ct: &[u8] = br#"<Types xmlns="http://schemas.openxmlformats.org/package/2006/content-types"><Default Extension="xml" ContentType="application/xml"/></Types>"#;
        let rels: &[u8] = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://x/officeDocument" Target="doc/main.xml"/><Relationship Id="rId2" Type="http://x/thumb" Target="doc/pic.png"/></Relationships>"#;
        let main: &[u8] = br#"<m:root xmlns:m="urn:m"><m:a/><m:a/><m:b/></m:root>"#;
        let before =
            package(&[("[Content_Types].xml", ct), ("_rels/.rels", rels), ("doc/main.xml", main), ("doc/pic.png", b"PNG1"), ("doc/extra.bin", b"x")]);
        // Same content, other prefix and a renamed picture: lossless.
        let main2: &[u8] = br#"<root xmlns="urn:m"><a/><a/><b/></root>"#;
        let rels2: &[u8] = br#"<Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://x/officeDocument" Target="/doc/main.xml"/><Relationship Id="rId9" Type="http://x/thumb" Target="doc/image1.png"/></Relationships>"#;
        let same = package(&[
            ("[Content_Types].xml", ct),
            ("_rels/.rels", rels2),
            ("doc/main.xml", main2),
            ("doc/image1.png", b"PNG1"),
            ("doc/extra.bin", b"x"),
        ]);
        let r = compare(&before, &same).unwrap();
        assert_eq!(r["lossless"], true, "{r}");
        assert_eq!(r["renamed_parts"][0][0], "doc/pic.png");
        // A lost element, a lost part and a changed binary part.
        let main3: &[u8] = br#"<root xmlns="urn:m"><a/><b/></root>"#;
        let lossy = package(&[("[Content_Types].xml", ct), ("_rels/.rels", rels), ("doc/main.xml", main3), ("doc/pic.png", b"PNG2")]);
        let r = compare(&before, &lossy).unwrap();
        assert_eq!(r["lossless"], false);
        assert_eq!(r["missingParts"], json!(["doc/extra.bin"]));
        assert_eq!(r["changedBinaryParts"], json!(["doc/pic.png"]));
        assert!(r["problems"].to_string().contains("{urn:m}a 2->1"), "{r}");
        assert!(compare(b"not a zip", &lossy).is_err());
    }
}
