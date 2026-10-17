//! WHIQ: table slicers in XLSX (Excel 2013 `x15` table slicer caches).
//!
//! A slicer is four things in the package, all written and read here:
//! - the slicer cache (`xl/slicerCaches/slicerCacheN.xml`, a workbook relationship and the
//!   `x15:slicerCaches` workbook extension) naming the table (`x15:tableSlicerCache tableId
//!   column`), with its defined name `Slicer_<column>` (`#N/A`);
//! - the slicer (`xl/slicers/slicerN.xml`, a sheet relationship and the sheet's `x14:slicerList`
//!   extension): name, caption, cache, button columns;
//! - its frame in the sheet's drawing (an `mc:AlternateContent` `sle:slicer` graphic frame);
//! - its selection, which is the table's AutoFilter on that column (modelled on the table).
//!
//! PivotTable slicers (`x14` caches over a PivotCache) are not modelled; reading one warns.

use std::fmt::Write as _;

use gridcraft_model::{Anchor, Slicer, Workbook};

use crate::package::Rel;
use crate::read::Ctx;
use crate::write::{Out, Rels, XML_DECL};
use crate::xml::{El, esc, esc_attr};

pub const CT_SLICER: &str = "application/vnd.ms-excel.slicer+xml";
pub const CT_SLICER_CACHE: &str = "application/vnd.ms-excel.slicerCache+xml";
pub const REL_SLICER: &str = "http://schemas.microsoft.com/office/2007/relationships/slicer";
pub const REL_SLICER_CACHE: &str = "http://schemas.microsoft.com/office/2007/relationships/slicerCache";
const NS_X14: &str = "http://schemas.microsoft.com/office/spreadsheetml/2009/9/main";
const NS_X15: &str = "http://schemas.microsoft.com/office/spreadsheetml/2010/11/main";
const NS_MAIN: &str = "http://schemas.openxmlformats.org/spreadsheetml/2006/main";
/// Sheet extension holding table slicers.
const EXT_SHEET_TABLE_SLICERS: &str = "{3A4CF648-6AED-40f4-86FF-DC5316D8AED3}";
/// Workbook extension holding table slicer caches.
const EXT_WB_TABLE_SLICER_CACHES: &str = "{46BE6895-7355-4a93-B00E-2C351335B9C9}";
/// Slicer cache extension naming its table.
const EXT_TABLE_SLICER_CACHE: &str = "{2F2917AC-EB37-4324-AD4E-5DD8C200BD13}";

/// One slicer to write.
#[derive(Clone, Debug)]
pub struct Planned {
    pub sheet: usize,
    pub slicer: u32,
    /// Unique slicer name (also its drawing frame's name).
    pub name: String,
    /// Unique cache name, also its defined name.
    pub cache: String,
    pub table: String,
    /// 1-based column of the table.
    pub column: u32,
    pub caption: String,
    pub columns: u32,
}

/// What the writer needs to know about slicers.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub slicers: Vec<Planned>,
    /// Written tables: model name → table id in the file.
    pub table_ids: Vec<(String, u32)>,
    pub parts: u32,
}

/// A defined name made of a column header: letters, digits and `_` (Excel's `Slicer_` names).
fn name_part(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if t.is_empty() { "Column".into() } else { t }
}

/// Slicers whose table and column exist get unique names; the rest are left out.
pub fn plan(wb: &Workbook) -> Plan {
    let mut out = Plan::default();
    let mut caches: Vec<String> = wb.names.iter().map(|n| n.name.to_ascii_lowercase()).collect();
    let mut names: Vec<String> = Vec::new();
    for (si, sh) in wb.sheets.iter().enumerate() {
        for s in &sh.slicers {
            let Some(t) = wb.sheets.iter().flat_map(|x| x.tables.iter()).find(|t| t.name.eq_ignore_ascii_case(&s.table)) else { continue };
            let Some(ci) = t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&s.column)) else { continue };
            let base = format!("Slicer_{}", name_part(&s.column));
            let mut cache = base.clone();
            let mut n = 1;
            while caches.contains(&cache.to_ascii_lowercase()) {
                n += 1;
                cache = format!("{base}{n}");
            }
            caches.push(cache.to_ascii_lowercase());
            let shown = if s.caption.is_empty() { s.column.clone() } else { s.caption.clone() };
            let mut name = shown.clone();
            let mut k = 1;
            while names.iter().any(|x| x.eq_ignore_ascii_case(&name)) {
                name = format!("{shown} {k}");
                k += 1;
            }
            names.push(name.clone());
            out.slicers.push(Planned {
                sheet: si,
                slicer: s.id,
                name,
                cache,
                table: t.name.clone(),
                column: ci as u32 + 1,
                caption: shown,
                columns: s.columns.clamp(1, 20000),
            });
        }
    }
    out
}

/// The drawing-frame name of a sheet's slicer, when it is written.
pub fn frame_name<'a>(plan: &'a Plan, sheet: usize, slicer: &Slicer) -> Option<&'a str> {
    plan.slicers.iter().find(|p| p.sheet == sheet && p.slicer == slicer.id).map(|p| p.name.as_str())
}

/// Writes sheet `si`'s slicer part; returns the sheet extension that lists it.
pub fn write_sheet(out: &mut Out, si: usize, rels: &mut Rels) -> Option<String> {
    let mine: Vec<Planned> = out.slicer_plan.slicers.iter().filter(|p| p.sheet == si).cloned().collect();
    if mine.is_empty() {
        return None;
    }
    out.slicer_plan.parts += 1;
    let n = out.slicer_plan.parts;
    let mut x = format!(
        "{XML_DECL}<slicers xmlns=\"{NS_X14}\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"x\" xmlns:x=\"{NS_MAIN}\">"
    );
    for p in &mine {
        let _ = write!(
            x,
            "<slicer name=\"{}\" cache=\"{}\" caption=\"{}\"{} rowHeight=\"241300\"/>",
            esc_attr(&p.name),
            esc_attr(&p.cache),
            esc_attr(&p.caption),
            if p.columns > 1 { format!(" columnCount=\"{}\"", p.columns) } else { String::new() }
        );
    }
    x.push_str("</slicers>");
    out.part(&format!("xl/slicers/slicer{n}.xml"), Some(CT_SLICER), x.into_bytes());
    let rid = rels.add_uri(REL_SLICER, &format!("../slicers/slicer{n}.xml"));
    Some(format!(
        "<ext uri=\"{EXT_SHEET_TABLE_SLICERS}\" xmlns:x15=\"{NS_X15}\"><x14:slicerList xmlns:x14=\"{NS_X14}\"><x14:slicer r:id=\"{rid}\"/></x14:slicerList></ext>"
    ))
}

/// Writes the slicer caches (after the sheets: table ids are known). Returns the defined names
/// and the workbook extension.
pub fn write_workbook(out: &mut Out, wb_rels: &mut Rels) -> (String, String) {
    let plan = out.slicer_plan.clone();
    let mut names = String::new();
    let mut list = String::new();
    for (k, p) in plan.slicers.iter().enumerate() {
        let Some(id) = plan.table_ids.iter().find(|(t, _)| t.eq_ignore_ascii_case(&p.table)).map(|(_, id)| *id) else { continue };
        let n = k + 1;
        let x = format!(
            "{XML_DECL}<slicerCacheDefinition xmlns=\"{NS_X14}\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"x x15\" xmlns:x=\"{NS_MAIN}\" xmlns:x15=\"{NS_X15}\" name=\"{}\" sourceName=\"{}\"><extLst><x:ext uri=\"{EXT_TABLE_SLICER_CACHE}\" xmlns:x15=\"{NS_X15}\"><x15:tableSlicerCache tableId=\"{id}\" column=\"{}\"/></x:ext></extLst></slicerCacheDefinition>",
            esc_attr(&p.cache),
            esc_attr(&p.caption),
            p.column
        );
        out.part(&format!("xl/slicerCaches/slicerCache{n}.xml"), Some(CT_SLICER_CACHE), x.into_bytes());
        let rid = wb_rels.add_uri(REL_SLICER_CACHE, &format!("slicerCaches/slicerCache{n}.xml"));
        let _ = write!(list, "<x14:slicerCache r:id=\"{rid}\"/>");
        let _ = write!(names, "<definedName name=\"{}\">#N/A</definedName>", esc(&p.cache));
    }
    if list.is_empty() {
        return (String::new(), String::new());
    }
    (
        names,
        format!(
            "<ext uri=\"{EXT_WB_TABLE_SLICER_CACHES}\" xmlns:x15=\"{NS_X15}\"><x15:slicerCaches xmlns:x14=\"{NS_X14}\">{list}</x15:slicerCaches></ext>"
        ),
    )
}

// ---------------------------------------------------------------- reading

/// A slicer read from a sheet, waiting for its cache and frame.
#[derive(Clone, Debug)]
pub struct Pending {
    pub sheet: String,
    pub name: String,
    pub cache: String,
    pub caption: String,
    pub columns: u32,
}

/// The sheet's `slicerList` extensions: its slicers.
pub fn read_sheet_ext(cx: &mut Ctx<'_>, ext_lst: &El, rels: &[Rel], sheet: &str) {
    for ext in ext_lst.kids("ext") {
        for list in ext.kids("slicerList") {
            for s in list.kids("slicer") {
                let Some(rel) = s.attr("id").and_then(|id| rels.iter().find(|r| r.id == id)) else { continue };
                let target = rel.target.clone();
                let Ok(Some(x)) = cx.optional_xml(&target) else { continue };
                for sl in x.kids("slicer").take(1000) {
                    let (Some(name), Some(cache)) = (sl.attr("name"), sl.attr("cache")) else { continue };
                    cx.slicers.push(Pending {
                        sheet: sheet.to_string(),
                        name: name.to_string(),
                        cache: cache.to_string(),
                        caption: sl.attr("caption").unwrap_or(name).to_string(),
                        columns: sl.attr_u32("columnCount").unwrap_or(1).clamp(1, 20000),
                    });
                }
            }
        }
    }
}

/// Ties the slicers read to their caches (tables, columns) and frames; returns the cache names
/// (their `#N/A` defined names are dropped: the writer makes them).
pub fn resolve(cx: &mut Ctx<'_>, wb_xml: &El, wb_rels: &[Rel], wb: &mut Workbook) -> Vec<String> {
    let pending = std::mem::take(&mut cx.slicers);
    // Every slicer cache the workbook lists: name → (table file id, 1-based column) or pivot.
    let mut caches: Vec<(String, Option<(u32, u32)>)> = Vec::new();
    let rids: Vec<String> = wb_xml
        .child("extLst")
        .into_iter()
        .flat_map(|l| l.kids("ext"))
        .flat_map(|e| e.kids("slicerCaches"))
        .flat_map(|c| c.kids("slicerCache"))
        .filter_map(|c| c.attr("id").map(str::to_string))
        .collect();
    for rid in rids.iter().take(1000) {
        let Some(rel) = wb_rels.iter().find(|r| &r.id == rid) else { continue };
        let target = rel.target.clone();
        let Ok(Some(x)) = cx.optional_xml(&target) else { continue };
        let Some(name) = x.attr("name") else { continue };
        let table = x
            .child("extLst")
            .into_iter()
            .flat_map(|l| l.kids("ext"))
            .find_map(|e| e.child("tableSlicerCache"))
            .and_then(|t| Some((t.attr_u32("tableId")?, t.attr_u32("column")?)));
        caches.push((name.to_string(), table));
    }
    let mut pivot_slicers = 0;
    for p in pending {
        let Some((_, cache)) = caches.iter().find(|(n, _)| n.eq_ignore_ascii_case(&p.cache)) else { continue };
        let Some((table_id, col)) = *cache else {
            pivot_slicers += 1;
            continue;
        };
        let Some(table) = cx.table_file_ids.iter().find(|(id, _)| *id == table_id).map(|(_, n)| n.clone()) else { continue };
        let anchor = cx
            .slicer_frames
            .iter()
            .find(|(s, n, _)| s == &p.sheet && n.eq_ignore_ascii_case(&p.name))
            .map(|(_, _, a)| *a)
            .unwrap_or(Anchor { width: 144.0, height: 192.0, ..Anchor::default() });
        let Some(table_ref) = wb.sheets.iter().flat_map(|s| s.tables.iter()).find(|t| t.name == table) else { continue };
        let Some(column) = table_ref.columns.get(col.saturating_sub(1) as usize).map(|c| c.name.clone()) else { continue };
        // The selection is the table's filter on that column.
        let selected = table_ref.filter.iter().find(|(o, _)| *o + 1 == col).and_then(|(_, f)| match f {
            gridcraft_model::FilterCriterion::Values { values, .. } => Some(values.clone()),
            _ => None,
        });
        let id = cx.next_object_id();
        let Some(si) = wb.sheet_index(&p.sheet) else { continue };
        if let Some(sh) = wb.sheet_mut(si) {
            sh.slicers.push(Slicer { id, table, column, caption: p.caption, anchor, selected, columns: p.columns });
        }
    }
    if pivot_slicers > 0 {
        cx.warn(format!("{pivot_slicers} PivotTable slicer(s) are not supported and were dropped (table slicers are kept)"));
    }
    caches.into_iter().map(|(n, _)| n).collect()
}
