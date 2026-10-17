//! WHIQ: slicers in XLSX — table slicers (Excel 2013 `x15` table slicer caches) and PivotTable
//! slicers (Excel 2010 `x14` slicer caches over a PivotCache).
//!
//! A slicer is four things in the package, all written and read here:
//! - the slicer cache (`xl/slicerCaches/slicerCacheN.xml`, a workbook relationship and a
//!   workbook extension listing it) with its defined name `Slicer_<field>` (`#N/A`). A table
//!   slicer's cache names its table (`x15:tableSlicerCache tableId column`); a PivotTable
//!   slicer's names its PivotTables by sheet id and name (`pivotTables/pivotTable tabId name`), the
//!   PivotCache by the cache's `x14:pivotCacheDefinition pivotCacheId`, and its items with their
//!   selection (`data/tabular/items/i x s`);
//! - the slicer (`xl/slicers/slicerN.xml`, a sheet relationship and the sheet's `slicerList`
//!   extension, `x14` for PivotTable slicers and `x15` for table slicers): name, caption, cache,
//!   button columns;
//! - its frame in the sheet's drawing (an `mc:AlternateContent` `sle:slicer` graphic frame);
//! - its selection: a table slicer's is the table's filter on that column; a PivotTable slicer's
//!   is written as the hidden items of the field in every connected PivotTable and as the cache
//!   items' `s` flags.
//!
//! Every reference is regenerated on save from the model (sheet ids, PivotTable names, cache
//! ids, table ids), so renamed sheets, moved or deleted PivotTables never leave a dangling one:
//! a slicer whose PivotTables or table are gone is left out. PivotTables connected to one
//! slicer share one PivotCache (Excel requires it), see [`crate::pivot`].

use std::fmt::Write as _;

use gridcraft_model::{Anchor, PivotTable, Slicer, Workbook};

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
/// Sheet extension holding table slicers (x15).
const EXT_SHEET_TABLE_SLICERS: &str = "{3A4CF648-6AED-40f4-86FF-DC5316D8AED3}";
/// Sheet extension holding PivotTable slicers (x14).
const EXT_SHEET_PIVOT_SLICERS: &str = "{A8765BA9-456A-4dab-B4F3-ACF838C121DE}";
/// Workbook extension holding table slicer caches (x15).
const EXT_WB_TABLE_SLICER_CACHES: &str = "{46BE6895-7355-4a93-B00E-2C351335B9C9}";
/// Workbook extension holding PivotTable slicer caches (x14).
const EXT_WB_PIVOT_SLICER_CACHES: &str = "{BBE1A952-AA13-448e-AADC-164F8A28A991}";
/// Slicer cache extension naming its table.
const EXT_TABLE_SLICER_CACHE: &str = "{2F2917AC-EB37-4324-AD4E-5DD8C200BD13}";
/// Most items a slicer cache lists.
const MAX_ITEMS: usize = 1_048_576;

/// What a slicer filters.
#[derive(Clone, Debug)]
pub enum Target {
    /// A table, and its 1-based column.
    Table { table: String, column: u32 },
    /// PivotTables (sheet, index), and the cache field.
    Pivot { pivots: Vec<(usize, usize)>, field: String },
}

/// One slicer to write.
#[derive(Clone, Debug)]
pub struct Planned {
    pub sheet: usize,
    pub slicer: u32,
    /// Unique slicer name (also its drawing frame's name).
    pub name: String,
    /// Unique cache name, also its defined name.
    pub cache: String,
    pub caption: String,
    /// The field or column header (the cache's `sourceName`).
    pub source_name: String,
    pub columns: u32,
    pub target: Target,
    pub selected: Option<Vec<String>>,
}

/// What the writer needs to know about slicers.
#[derive(Clone, Debug, Default)]
pub struct Plan {
    pub slicers: Vec<Planned>,
    /// Written tables: model name → table id in the file.
    pub table_ids: Vec<(String, u32)>,
    pub parts: u32,
    /// `sheetId` of each model sheet.
    pub sheet_ids: Vec<u32>,
    /// Written PivotTables: (sheet, index), cache id, name in the file.
    pub written_pivots: Vec<((usize, usize), u32, String)>,
    /// Shared items of slicer fields: (cache id, field, item labels).
    pub cache_items: Vec<(u32, String, Vec<String>)>,
}

impl Plan {
    /// The PivotTables sharing a cache with (sheet, index) because slicers connect them, the
    /// PivotTable itself first.
    pub fn pivot_group(&self, sheet: usize, idx: usize) -> Vec<(usize, usize)> {
        let mut group = vec![(sheet, idx)];
        loop {
            let mut grew = false;
            for p in &self.slicers {
                if let Target::Pivot { pivots, .. } = &p.target
                    && pivots.iter().any(|k| group.contains(k))
                {
                    for k in pivots {
                        if !group.contains(k) {
                            group.push(*k);
                            grew = true;
                        }
                    }
                }
            }
            if !grew {
                break;
            }
        }
        group
    }

    /// The fields slicers use over any of these PivotTables.
    pub fn slicer_fields(&self, group: &[(usize, usize)]) -> Vec<String> {
        let mut out: Vec<String> = Vec::new();
        for p in &self.slicers {
            if let Target::Pivot { pivots, field } = &p.target
                && pivots.iter().any(|k| group.contains(k))
                && !out.iter().any(|f| f.eq_ignore_ascii_case(field))
            {
                out.push(field.clone());
            }
        }
        out
    }
}

/// A defined name made of a column header: letters, digits and `_` (Excel's `Slicer_` names).
fn name_part(s: &str) -> String {
    let t: String = s.chars().map(|c| if c.is_alphanumeric() || c == '_' { c } else { '_' }).collect();
    if t.is_empty() || t.starts_with(|c: char| c.is_ascii_digit()) { format!("_{t}") } else { t }
}

/// (sheet, PivotTable) of a link `'My Sheet'!PivotTable1`.
fn parse_link(link: &str) -> Option<(String, String)> {
    let (sheet, name) = link.trim().rsplit_once('!')?;
    let sheet = sheet.trim();
    let sheet = sheet.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).map(|x| x.replace("''", "'")).unwrap_or_else(|| sheet.to_string());
    Some((sheet, name.trim().to_string()))
}

fn find_pivot(wb: &Workbook, link: &str) -> Option<(usize, usize)> {
    let (sheet, name) = parse_link(link)?;
    let si = wb.sheets.iter().position(|s| s.name.eq_ignore_ascii_case(&sheet))?;
    let pi = wb.sheet(si)?.pivots.iter().position(|p| p.name.eq_ignore_ascii_case(&name))?;
    Some((si, pi))
}

/// The selections of the slicers connected to a PivotTable: (field, items shown).
pub fn pivot_selections(wb: &Workbook, sheet: usize, pt: &PivotTable) -> Vec<(String, Vec<String>)> {
    let Some(sheet_name) = wb.sheet(sheet).map(|s| s.name.clone()) else { return vec![] };
    wb.sheets
        .iter()
        .flat_map(|sh| sh.slicers.iter())
        .filter(|sl| {
            sl.pivots.iter().any(|l| parse_link(l).is_some_and(|(s, n)| s.eq_ignore_ascii_case(&sheet_name) && n.eq_ignore_ascii_case(&pt.name)))
        })
        .filter_map(|sl| sl.selected.clone().map(|sel| (sl.column.clone(), sel)))
        .collect()
}

/// Slicers whose table or PivotTables and field exist get unique names; the rest are left out.
pub fn plan(wb: &Workbook) -> Plan {
    let mut out = Plan::default();
    let mut caches: Vec<String> = wb.names.iter().map(|n| n.name.to_ascii_lowercase()).collect();
    let mut names: Vec<String> = Vec::new();
    for (si, sh) in wb.sheets.iter().enumerate() {
        for s in &sh.slicers {
            let (target, source_name) = if s.pivots.is_empty() {
                let Some(t) = wb.sheets.iter().flat_map(|x| x.tables.iter()).find(|t| t.name.eq_ignore_ascii_case(&s.table)) else { continue };
                let Some(ci) = t.columns.iter().position(|c| c.name.eq_ignore_ascii_case(&s.column)) else { continue };
                let header = t.columns.get(ci).map(|c| c.name.clone()).unwrap_or_default();
                (Target::Table { table: t.name.clone(), column: ci as u32 + 1 }, header)
            } else {
                // Connected PivotTables that exist and can be written, all over one source.
                let mut pivots: Vec<(usize, usize)> = Vec::new();
                let mut key = None;
                for l in &s.pivots {
                    let Some((psi, ppi)) = find_pivot(wb, l) else { continue };
                    let Some(pt) = wb.sheet(psi).and_then(|x| x.pivots.get(ppi)) else { continue };
                    let Some(k) = crate::pivot::source_key(wb, pt, psi) else { continue };
                    if key.is_none() {
                        key = Some(k);
                    }
                    if key == Some(k) && !pivots.contains(&(psi, ppi)) {
                        pivots.push((psi, ppi));
                    }
                }
                let Some(&(psi, ppi)) = pivots.first() else { continue };
                let Some(pt) = wb.sheet(psi).and_then(|x| x.pivots.get(ppi)) else { continue };
                let Some(field) =
                    crate::pivot::cache_field_names(wb, pt, psi).and_then(|f| f.into_iter().find(|f| f.eq_ignore_ascii_case(&s.column)))
                else {
                    continue;
                };
                (Target::Pivot { pivots, field: field.clone() }, field)
            };
            let base = format!("Slicer_{}", name_part(&source_name));
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
                caption: shown,
                source_name,
                columns: s.columns.clamp(1, 20000),
                target,
                selected: s.selected.clone(),
            });
        }
    }
    out
}

/// The drawing-frame name of a sheet's slicer, when it is written.
pub fn frame_name<'a>(plan: &'a Plan, sheet: usize, slicer: &Slicer) -> Option<&'a str> {
    plan.slicers.iter().find(|p| p.sheet == sheet && p.slicer == slicer.id).map(|p| p.name.as_str())
}

/// Writes sheet `si`'s slicer parts (one for PivotTable slicers, one for table slicers); returns
/// the sheet extensions that list them.
pub fn write_sheet(out: &mut Out, si: usize, rels: &mut Rels) -> Option<String> {
    let mine: Vec<Planned> = out.slicer_plan.slicers.iter().filter(|p| p.sheet == si).cloned().collect();
    if mine.is_empty() {
        return None;
    }
    let mut ext = String::new();
    for pivot in [true, false] {
        let list: Vec<&Planned> = mine.iter().filter(|p| matches!(p.target, Target::Pivot { .. }) == pivot).collect();
        if list.is_empty() {
            continue;
        }
        out.slicer_plan.parts += 1;
        let n = out.slicer_plan.parts;
        let mut x = format!(
            "{XML_DECL}<slicers xmlns=\"{NS_X14}\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"x xr10\" xmlns:x=\"{NS_MAIN}\" xmlns:xr10=\"http://schemas.microsoft.com/office/spreadsheetml/2016/revision10\">"
        );
        for p in list {
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
        if pivot {
            let _ = write!(
                ext,
                "<ext uri=\"{EXT_SHEET_PIVOT_SLICERS}\" xmlns:x14=\"{NS_X14}\"><x14:slicerList><x14:slicer r:id=\"{rid}\"/></x14:slicerList></ext>"
            );
        } else {
            let _ = write!(
                ext,
                "<ext uri=\"{EXT_SHEET_TABLE_SLICERS}\" xmlns:x15=\"{NS_X15}\"><x14:slicerList xmlns:x14=\"{NS_X14}\"><x14:slicer r:id=\"{rid}\"/></x14:slicerList></ext>"
            );
        }
    }
    Some(ext)
}

/// Writes the slicer caches (after the sheets: table ids, sheet ids and PivotCaches are known).
/// Returns the defined names and the workbook extensions.
pub fn write_workbook(out: &mut Out, wb_rels: &mut Rels) -> (String, String) {
    let plan = out.slicer_plan.clone();
    let mut names = String::new();
    let mut table_list = String::new();
    let mut pivot_list = String::new();
    for (k, p) in plan.slicers.iter().enumerate() {
        let n = k + 1;
        let x = match &p.target {
            Target::Table { table, column } => {
                let Some(id) = plan.table_ids.iter().find(|(t, _)| t.eq_ignore_ascii_case(table)).map(|(_, id)| *id) else { continue };
                format!(
                    "{XML_DECL}<slicerCacheDefinition xmlns=\"{NS_X14}\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"x x15\" xmlns:x=\"{NS_MAIN}\" xmlns:x15=\"{NS_X15}\" name=\"{}\" sourceName=\"{}\"><extLst><x:ext uri=\"{EXT_TABLE_SLICER_CACHE}\" xmlns:x15=\"{NS_X15}\"><x15:tableSlicerCache tableId=\"{id}\" column=\"{column}\"/></x:ext></extLst></slicerCacheDefinition>",
                    esc_attr(&p.cache),
                    esc_attr(&p.source_name),
                )
            }
            Target::Pivot { pivots, field } => {
                // The connected PivotTables as written, all on one cache.
                let written: Vec<&((usize, usize), u32, String)> =
                    pivots.iter().filter_map(|k| plan.written_pivots.iter().find(|w| w.0 == *k)).collect();
                let Some(cache_id) = written.first().map(|w| w.1) else { continue };
                let mut tables = String::new();
                for w in written.iter().filter(|w| w.1 == cache_id) {
                    let Some(tab) = plan.sheet_ids.get(w.0.0) else { continue };
                    let _ = write!(tables, "<pivotTable tabId=\"{tab}\" name=\"{}\"/>", esc_attr(&w.2));
                }
                let labels = plan
                    .cache_items
                    .iter()
                    .find(|(c, f, _)| *c == cache_id && f.eq_ignore_ascii_case(field))
                    .map(|(_, _, l)| l.clone())
                    .unwrap_or_default();
                let mut items = String::new();
                for (x, l) in labels.iter().enumerate().take(MAX_ITEMS) {
                    let on = p.selected.as_ref().is_none_or(|sel| sel.iter().any(|s| s.eq_ignore_ascii_case(l)));
                    let _ = write!(items, "<i x=\"{x}\"{}/>", if on { " s=\"1\"" } else { "" });
                }
                format!(
                    "{XML_DECL}<slicerCacheDefinition xmlns=\"{NS_X14}\" xmlns:mc=\"http://schemas.openxmlformats.org/markup-compatibility/2006\" mc:Ignorable=\"x\" xmlns:x=\"{NS_MAIN}\" name=\"{}\" sourceName=\"{}\"><pivotTables>{tables}</pivotTables><data><tabular pivotCacheId=\"{cache_id}\"><items count=\"{}\">{items}</items></tabular></data></slicerCacheDefinition>",
                    esc_attr(&p.cache),
                    esc_attr(&p.source_name),
                    labels.len().min(MAX_ITEMS)
                )
            }
        };
        out.part(&format!("xl/slicerCaches/slicerCache{n}.xml"), Some(CT_SLICER_CACHE), x.into_bytes());
        let rid = wb_rels.add_uri(REL_SLICER_CACHE, &format!("slicerCaches/slicerCache{n}.xml"));
        let list = if matches!(p.target, Target::Pivot { .. }) { &mut pivot_list } else { &mut table_list };
        let _ = write!(list, "<x14:slicerCache r:id=\"{rid}\"/>");
        let _ = write!(names, "<definedName name=\"{}\">#N/A</definedName>", esc(&p.cache));
    }
    let mut ext = String::new();
    if !pivot_list.is_empty() {
        let _ =
            write!(ext, "<ext uri=\"{EXT_WB_PIVOT_SLICER_CACHES}\" xmlns:x14=\"{NS_X14}\"><x14:slicerCaches>{pivot_list}</x14:slicerCaches></ext>");
    }
    if !table_list.is_empty() {
        let _ = write!(
            ext,
            "<ext uri=\"{EXT_WB_TABLE_SLICER_CACHES}\" xmlns:x15=\"{NS_X15}\"><x15:slicerCaches xmlns:x14=\"{NS_X14}\">{table_list}</x15:slicerCaches></ext>"
        );
    }
    (names, ext)
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

/// The sheet's `slicerList` extensions (x14 and x15): its slicers.
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

/// A slicer cache read from the workbook.
enum CacheIn {
    Table {
        table_id: u32,
        column: u32,
    },
    /// (sheet id, PivotTable name)s, the PivotCache id, item (x, selected) flags.
    Pivot {
        tables: Vec<(u32, String)>,
        cache_id: Option<u32>,
        source: String,
        items: Vec<(u32, bool)>,
    },
}

/// Item labels of field `field` of the PivotCache with `x14 pivotCacheId` `id`.
fn pivot_cache_labels(cx: &mut Ctx<'_>, wb_xml: &El, wb_rels: &[Rel], id: u32, field: &str) -> Vec<String> {
    let rids: Vec<String> =
        wb_xml.child("pivotCaches").into_iter().flat_map(|p| p.kids("pivotCache")).filter_map(|c| c.attr("id").map(str::to_string)).collect();
    for rid in rids.iter().take(10_000) {
        let Some(rel) = wb_rels.iter().find(|r| &r.id == rid) else { continue };
        let target = rel.target.clone();
        let Ok(Some(x)) = cx.optional_xml(&target) else { continue };
        let this = x
            .child("extLst")
            .into_iter()
            .flat_map(|l| l.kids("ext"))
            .filter_map(|e| e.child("pivotCacheDefinition"))
            .any(|d| d.attr_u32("pivotCacheId") == Some(id));
        if !this {
            continue;
        }
        let sys = cx.date_system;
        return x
            .child("cacheFields")
            .into_iter()
            .flat_map(|c| c.kids("cacheField"))
            .find(|f| f.attr("name").is_some_and(|n| n.eq_ignore_ascii_case(field)))
            .and_then(|f| f.child("sharedItems"))
            .map(|si| si.children.iter().take(MAX_ITEMS).map(|e| crate::pivot::item_label_el(e, sys)).collect())
            .unwrap_or_default();
    }
    Vec::new()
}

/// Ties the slicers read to their caches (tables, PivotTables, fields) and frames; returns the
/// cache names (their `#N/A` defined names are dropped: the writer makes them).
pub fn resolve(cx: &mut Ctx<'_>, wb_xml: &El, wb_rels: &[Rel], wb: &mut Workbook) -> Vec<String> {
    let pending = std::mem::take(&mut cx.slicers);
    let mut caches: Vec<(String, Option<CacheIn>)> = Vec::new();
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
            .and_then(|t| Some(CacheIn::Table { table_id: t.attr_u32("tableId")?, column: t.attr_u32("column")? }));
        let pivot = x.child("pivotTables").map(|pts| {
            let tables = pts.kids("pivotTable").filter_map(|p| Some((p.attr_u32("tabId")?, p.attr("name")?.to_string()))).collect();
            let tab = x.path(&["data", "tabular"]);
            let items = tab
                .and_then(|t| t.child("items"))
                .map(|it| it.kids("i").filter_map(|i| Some((i.attr_u32("x")?, i.flag("s", false)))).collect())
                .unwrap_or_default();
            CacheIn::Pivot {
                tables,
                cache_id: tab.and_then(|t| t.attr_u32("pivotCacheId")),
                source: x.attr("sourceName").unwrap_or("").to_string(),
                items,
            }
        });
        caches.push((name.to_string(), table.or(pivot)));
    }
    let mut lost = 0;
    for p in pending {
        let Some((_, Some(cache))) = caches.iter().find(|(n, _)| n.eq_ignore_ascii_case(&p.cache)) else {
            lost += 1;
            continue;
        };
        let anchor = cx
            .slicer_frames
            .iter()
            .find(|(s, n, _)| s == &p.sheet && n.eq_ignore_ascii_case(&p.name))
            .map(|(_, _, a)| *a)
            .unwrap_or(Anchor { width: 144.0, height: 192.0, ..Anchor::default() });
        let id = cx.next_object_id();
        let slicer = match cache {
            CacheIn::Table { table_id, column: col } => {
                let Some(table) = cx.table_file_ids.iter().find(|(id, _)| id == table_id).map(|(_, n)| n.clone()) else {
                    lost += 1;
                    continue;
                };
                let Some(table_ref) = wb.sheets.iter().flat_map(|s| s.tables.iter()).find(|t| t.name == table) else { continue };
                let Some(column) = table_ref.columns.get(col.saturating_sub(1) as usize).map(|c| c.name.clone()) else { continue };
                // The selection is the table's filter on that column.
                let selected = table_ref.filter.iter().find(|(o, _)| *o + 1 == *col).and_then(|(_, f)| match f {
                    gridcraft_model::FilterCriterion::Values { values, .. } => Some(values.clone()),
                    _ => None,
                });
                Slicer { id, table, column, caption: p.caption, anchor, selected, columns: p.columns, pivots: vec![] }
            }
            CacheIn::Pivot { tables, cache_id, source, items } => {
                let mut links = Vec::new();
                for (tab, name) in tables {
                    let Some(sheet) = cx.sheet_ids.iter().find(|(sid, _)| sid == tab).map(|(_, n)| n.clone()) else { continue };
                    let Some(si) = wb.sheet_index(&sheet) else { continue };
                    let Some(pt) = wb.sheet(si).and_then(|sh| sh.pivots.iter().find(|pt| pt.name.eq_ignore_ascii_case(name))) else { continue };
                    links.push(format!("{}!{}", gridcraft_formula::quote_sheet(&sheet), pt.name));
                }
                if links.is_empty() {
                    lost += 1;
                    continue;
                }
                let labels = match cache_id {
                    Some(cid) => pivot_cache_labels(cx, wb_xml, wb_rels, *cid, source),
                    None => Vec::new(),
                };
                let shown: Vec<String> = items.iter().filter(|(_, s)| *s).filter_map(|(x, _)| labels.get(*x as usize).cloned()).collect();
                let selected = if labels.is_empty() || items.is_empty() || items.iter().all(|(_, s)| *s) { None } else { Some(shown) };
                // The slicer owns the field's filter in its PivotTables: a hidden-items filter the
                // reader derived from it would keep filtering after the slicer is cleared.
                for l in &links {
                    let Some((sheet, name)) = parse_link(l) else { continue };
                    let Some(si) = wb.sheet_index(&sheet) else { continue };
                    if let Some(sh) = wb.sheet_mut(si)
                        && let Some(pt) = sh.pivots.iter_mut().find(|pt| pt.name.eq_ignore_ascii_case(&name))
                    {
                        let on_axis = pt.rows.iter().chain(pt.columns.iter()).any(|f| f.source_col.eq_ignore_ascii_case(source));
                        for f in pt.filters.iter_mut().filter(|f| f.source_col.eq_ignore_ascii_case(source)) {
                            f.selected = None;
                        }
                        if on_axis {
                            pt.filters.retain(|f| !f.source_col.eq_ignore_ascii_case(source));
                        }
                    }
                }
                Slicer { id, table: String::new(), column: source.clone(), caption: p.caption, anchor, selected, columns: p.columns, pivots: links }
            }
        };
        let Some(si) = wb.sheet_index(&p.sheet) else { continue };
        if let Some(sh) = wb.sheet_mut(si) {
            sh.slicers.push(slicer);
        }
    }
    if lost > 0 {
        cx.warn(format!("{lost} slicer(s) refer to a table, PivotTable or cache that isn't in the file and were dropped"));
    }
    caches.into_iter().map(|(n, _)| n).collect()
}
