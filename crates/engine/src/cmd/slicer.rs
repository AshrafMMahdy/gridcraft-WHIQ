//! Slicers: on-sheet widgets that filter one column of a table by clicking value tiles.
//!
//! A slicer is a thin front-end over the existing filter machinery: its selection is written as a
//! [`FilterCriterion::Values`] into the sheet's [`AutoFilter`] (over the table's range) and
//! [`super::data::apply_filter`] hides the rows. Creating a slicer (`insert.slicer` /
//! `table.insertSlicer`) selects everything, i.e. applies no filter.

use gridcraft_core::CellRef;
use gridcraft_model::*;
use serde_json::{Value as Json, json};

use super::*;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            "insert.slicer",
            "Slicer",
            ["Insert", "Filters"],
            None,
            "{table?, pivot?: \"PivotTable1\" | \"Sheet2!PivotTable1\" | [..] (a PivotTable slicer, filtering every PivotTable listed), column?: \"Region\", columns?: 1, at?: \"H2\", width?, height?}",
            has_doc,
            insert_slicer
        ),
        cmd!(
            "slicer.connections",
            "Report Connections",
            ["Slicer", "Slicer"],
            None,
            "{slicer?, pivots: [\"Sheet2!PivotTable1\", ..]} — the PivotTables a PivotTable slicer filters (they must share its source)",
            has_doc,
            slicer_connections
        ),
        cmd!(
            "table.insertSlicer",
            "Insert Slicer",
            ["Table", "Tools"],
            None,
            "{table?, column?: \"Region\", columns?: 1, at?, width?, height?}",
            has_doc,
            insert_slicer
        ),
        cmd!(query "slicer.list", "Slicers", [], None, "{sheet?} → the sheet's slicers: {id, caption, field, table | pivots, selected (null = all), items}", has_doc, slicer_list),
        cmd!("slicer.toggle", "Toggle Slicer Value", [], None, "{slicer?: id | caption, value: \"East\", multi?: false}", has_doc, slicer_toggle),
        cmd!("slicer.selectAll", "Clear Slicer Filter", [], None, "{slicer?: id | caption}", has_doc, slicer_select_all),
        cmd!("slicer.clear", "Clear Slicer Filter", [], None, "{slicer?: id}", has_doc, slicer_select_all),
        cmd!("slicer.delete", "Delete Slicer", [], None, "{slicer?: id}", has_doc, slicer_delete),
    ]
}

/// Distinct, non-blank display values of a table column, in first-seen order. Used to populate a
/// slicer's tiles and to detect "everything selected".
pub fn column_values(wb: &Workbook, sheet: usize, table: &str, column: &str) -> Vec<String> {
    let Some(sh) = wb.sheet(sheet) else { return vec![] };
    let Some(t) = sh.tables.iter().find(|t| t.name.eq_ignore_ascii_case(table)) else { return vec![] };
    let Some(ci) = t.column_index(column) else { return vec![] };
    let Some(dr) = t.data_range() else { return vec![] };
    let col = dr.start.col + ci as u32;
    let mut seen = std::collections::BTreeSet::new();
    let mut out = Vec::new();
    for row in dr.start.row..=dr.end.row {
        let text = crate::display::cell_text(wb, sh, CellRef::new(row, col));
        if text.is_empty() {
            continue;
        }
        if seen.insert(text.to_lowercase()) {
            out.push(text);
        }
    }
    out
}

/// The values a slicer offers: a table column's, or (a PivotTable slicer) its first connected
/// PivotTable's source field items.
pub fn slicer_values(wb: &Workbook, sheet: usize, sl: &Slicer) -> Vec<String> {
    if sl.pivots.is_empty() {
        return column_values(wb, sheet, &sl.table, &sl.column);
    }
    sl.pivots
        .iter()
        .find_map(|l| crate::pivot::find_linked(wb, l))
        .map(|(si, pi)| crate::pivot::field_items(wb, si, pi, &sl.column))
        .unwrap_or_default()
}

/// Recomputes the PivotTables a pivot slicer filters (after its selection or connections changed).
fn refresh_connected(wb: &mut Workbook, links: &[String]) {
    for l in links {
        if let Some((si, pi)) = crate::pivot::find_linked(wb, l) {
            let _ = crate::pivot::refresh(wb, si, pi, true);
        }
    }
}

/// A slicer's tiles as `(value, selected)`, for the UI. `selected` is true for every tile when the
/// slicer has no filter (`selected == None`).
pub fn slicer_items(wb: &Workbook, sheet: usize, sl: &Slicer) -> Vec<(String, bool)> {
    slicer_values(wb, sheet, sl)
        .into_iter()
        .map(|v| {
            let on = match &sl.selected {
                None => true,
                Some(set) => set.iter().any(|x| x.eq_ignore_ascii_case(&v)),
            };
            (v, on)
        })
        .collect()
}

/// Writes the slicer's current selection into the sheet filter and re-applies it (a PivotTable
/// slicer: recomputes its PivotTables).
fn reapply_slicer(wb: &mut Workbook, sheet: usize, id: u32) {
    let links = wb.sheet(sheet).and_then(|sh| sh.slicers.iter().find(|s| s.id == id)).map(|s| s.pivots.clone()).unwrap_or_default();
    if !links.is_empty() {
        refresh_connected(wb, &links);
        return;
    }
    let info = wb.sheet(sheet).and_then(|sh| {
        let sl = sh.slicers.iter().find(|s| s.id == id)?;
        let t = sh.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&sl.table))?;
        let ci = t.column_index(&sl.column)?;
        Some((t.range, t.range.start.col + ci as u32, sl.selected.clone()))
    });
    let Some((table_range, abs_col, selected)) = info else { return };
    {
        let Some(sh) = wb.sheet_mut(sheet) else { return };
        let af = sh.autofilter.get_or_insert_with(|| AutoFilter { range: table_range, criteria: vec![] });
        let off = abs_col.saturating_sub(af.range.start.col);
        af.criteria.retain(|(o, _)| *o != off);
        if let Some(values) = selected {
            af.criteria.push((off, FilterCriterion::Values { values, blanks: false }));
        }
    }
    super::data::apply_filter(wb, sheet);
}

/// `pivot` as a list of links, each resolved to an existing PivotTable (a bare name is looked
/// up on `sheet`, then anywhere).
fn pivot_links(s: &Session, p: &Json, sheet: usize, cmd: &str) -> Result<Vec<String>> {
    let d = s.doc()?;
    let names: Vec<String> = match p.get("pivot").or(p.get("pivots")) {
        Some(Json::String(t)) => vec![t.clone()],
        Some(Json::Array(v)) => v.iter().filter_map(|x| x.as_str().map(str::to_string)).collect(),
        _ => vec![],
    };
    let mut out = Vec::new();
    for n in names {
        let found = if n.contains('!') {
            crate::pivot::find_linked(&d.wb, &n)
        } else {
            let here = d.wb.sheet(sheet).and_then(|sh| sh.pivots.iter().position(|pt| pt.name.eq_ignore_ascii_case(&n))).map(|pi| (sheet, pi));
            here.or_else(|| {
                d.wb.sheets
                    .iter()
                    .enumerate()
                    .find_map(|(si, sh)| sh.pivots.iter().position(|pt| pt.name.eq_ignore_ascii_case(&n)).map(|pi| (si, pi)))
            })
        };
        let (si, pi) = found.ok_or_else(|| bad(cmd, format!("there is no PivotTable `{n}`")))?;
        let (Some(sh), Some(pt)) = (d.wb.sheet(si), d.wb.sheet(si).and_then(|sh| sh.pivots.get(pi))) else { continue };
        let link = crate::pivot::chart_link(&sh.name, &pt.name);
        if !out.contains(&link) {
            out.push(link);
        }
    }
    // A slicer filters PivotTables of one source (Excel: one PivotCache).
    let sources: Vec<String> = out
        .iter()
        .filter_map(|l| crate::pivot::find_linked(&d.wb, l))
        .filter_map(|(si, pi)| {
            crate::pivot::read_source(&d.wb, &d.wb.sheet(si)?.pivots.get(pi)?.source, si).ok().map(|src| format!("{}:{}", src.sheet, src.range.a1()))
        })
        .collect();
    if sources.windows(2).any(|w| w[0] != w[1]) {
        return Err(bad(cmd, "a slicer can only connect PivotTables built from the same source data"));
    }
    Ok(out)
}

fn insert_pivot_slicer(s: &mut Session, p: &Json, sheet: usize, links: Vec<String>) -> Result<Json> {
    let d = s.doc()?;
    let (psi, ppi) = links.first().and_then(|l| crate::pivot::find_linked(&d.wb, l)).ok_or_else(|| bad("insert.slicer", "no such PivotTable"))?;
    let pt = d.wb.sheet(psi).and_then(|sh| sh.pivots.get(ppi)).ok_or_else(|| bad("insert.slicer", "no such PivotTable"))?;
    let src = crate::pivot::read_source(&d.wb, &pt.source, psi).map_err(|e| bad("insert.slicer", e))?;
    let column = match str_param(p, "column") {
        Some(c) => src
            .headers
            .iter()
            .find(|h| h.eq_ignore_ascii_case(c))
            .cloned()
            .ok_or_else(|| bad("insert.slicer", format!("the PivotTable's source has no field `{c}` (fields: {})", src.headers.join(", "))))?,
        None => src.headers.first().cloned().ok_or_else(|| bad("insert.slicer", "the PivotTable's source has no fields"))?,
    };
    let at = match cell_param(s, p, "at")? {
        Some(c) => c,
        None => {
            let a = pt.last_range.map(|r| CellRef::new(r.start.row, r.end.col.saturating_add(2))).unwrap_or(pt.anchor);
            CellRef::new(a.row, a.col.min(gridcraft_core::MAX_COLS - 1))
        }
    };
    let columns = u32_param(p, "columns").unwrap_or(1).clamp(1, 10);
    let width = f64_param(p, "width").unwrap_or(168.0).clamp(60.0, 10000.0) as f32;
    let height = f64_param(p, "height").unwrap_or(200.0).clamp(60.0, 10000.0) as f32;
    let id = d.wb.next_object_id();
    let slicer = Slicer {
        id,
        table: String::new(),
        column: column.clone(),
        caption: column,
        anchor: Anchor { cell: at, dx: 4.0, dy: 4.0, width, height, mode: AnchorMode::MoveOnly },
        selected: None,
        columns,
        pivots: links,
    };
    edit(s, |cx| {
        cx.sheet_mut(sheet)?.slicers.push(slicer);
        Ok(json!({"slicer": id}))
    })
}

fn slicer_connections(s: &mut Session, p: &Json) -> Result<Json> {
    let (idx, id) = find_slicer(s, p)?;
    let sheet = s.doc()?.wb.active_sheet;
    let is_pivot = s.doc()?.wb.sheet(sheet).and_then(|sh| sh.slicers.get(idx)).is_some_and(|sl| !sl.pivots.is_empty());
    if !is_pivot {
        return Err(bad("slicer.connections", "this is a table slicer: it filters its table"));
    }
    let links = pivot_links(s, &json!({"pivot": p.get("pivots").cloned().unwrap_or(Json::Null)}), sheet, "slicer.connections")?;
    if links.is_empty() {
        return Err(bad("slicer.connections", "give at least one PivotTable in `pivots`"));
    }
    edit(s, |cx| {
        let old = cx.wb.sheet(sheet).and_then(|sh| sh.slicers.get(idx)).map(|sl| sl.pivots.clone()).unwrap_or_default();
        if let Some(sl) = cx.sheet_mut(sheet)?.slicers.get_mut(idx) {
            sl.pivots = links.clone();
        }
        let mut all = old;
        all.extend(links.iter().cloned());
        refresh_connected(&mut cx.wb, &all);
        let _ = id;
        cx.structural = true;
        Ok(json!({"pivots": links}))
    })
}

fn insert_slicer(s: &mut Session, p: &Json) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let links = pivot_links(s, p, sheet, "insert.slicer")?;
    if !links.is_empty() {
        return insert_pivot_slicer(s, p, sheet, links);
    }
    let d = s.doc()?;
    let sh = d.wb.sheet(sheet).ok_or(EngineError::NoDocument)?;
    let t = match str_param(p, "table") {
        Some(name) => sh.tables.iter().find(|t| t.name.eq_ignore_ascii_case(name)),
        None => sh.table_at(d.selection.active),
    }
    .ok_or_else(|| EngineError::Other("Select a cell inside a table first, or pass `table`.".into()))?;
    let column = match str_param(p, "column") {
        Some(c) => t
            .columns
            .iter()
            .find(|tc| tc.name.eq_ignore_ascii_case(c))
            .map(|tc| tc.name.clone())
            .ok_or_else(|| bad("insert.slicer", format!("table has no column `{c}`")))?,
        None => {
            // Default to the active cell's column within the table, else the first column.
            let idx = d.selection.active.col.checked_sub(t.range.start.col).map(|o| o as usize).filter(|o| *o < t.columns.len()).unwrap_or(0);
            t.columns.get(idx).map(|tc| tc.name.clone()).ok_or_else(|| bad("insert.slicer", "the table has no columns"))?
        }
    };
    let table = t.name.clone();
    let at = cell_param(s, p, "at")?
        .unwrap_or_else(|| CellRef::new(t.range.start.row, t.range.end.col.saturating_add(2).min(gridcraft_core::MAX_COLS - 1)));
    let columns = u32_param(p, "columns").unwrap_or(1).clamp(1, 10);
    let width = f64_param(p, "width").unwrap_or(168.0).clamp(60.0, 10000.0) as f32;
    let height = f64_param(p, "height").unwrap_or(200.0).clamp(60.0, 10000.0) as f32;
    let id = d.wb.next_object_id();
    let slicer = Slicer {
        id,
        table,
        column: column.clone(),
        caption: column,
        anchor: Anchor { cell: at, dx: 4.0, dy: 4.0, width, height, mode: AnchorMode::MoveOnly },
        selected: None,
        columns,
        pivots: vec![],
    };
    edit(s, |cx| {
        cx.sheet_mut(sheet)?.slicers.push(slicer);
        Ok(json!({"slicer": id}))
    })
}

/// The target slicer: by `slicer` id, else the last one on the active sheet.
fn find_slicer(s: &Session, p: &Json) -> Result<(usize, u32)> {
    let d = s.doc()?;
    let sh = d.wb.active().ok_or(EngineError::NoDocument)?;
    let idx = match (u32_param(p, "slicer"), str_param(p, "slicer")) {
        (Some(id), _) => sh.slicers.iter().position(|x| x.id == id),
        // WHIQ: a slicer by its caption or field.
        (None, Some(name)) => sh.slicers.iter().position(|x| x.caption.eq_ignore_ascii_case(name) || x.column.eq_ignore_ascii_case(name)),
        (None, None) => sh.slicers.len().checked_sub(1),
    };
    idx.and_then(|i| sh.slicers.get(i).map(|x| (i, x.id))).ok_or_else(|| bad("slicer", "no slicer on this sheet"))
}

fn slicer_list(s: &mut Session, p: &Json) -> Result<Json> {
    let si = target_sheet(s, p)?;
    let d = s.doc()?;
    let sh = d.wb.sheet(si).ok_or(EngineError::NoDocument)?;
    let list: Vec<Json> = sh
        .slicers
        .iter()
        .map(|sl| {
            let mut o = json!({"id": sl.id, "caption": sl.caption, "field": sl.column, "selected": sl.selected, "items": slicer_values(&d.wb, si, sl), "at": sl.anchor.cell.a1()});
            if sl.pivots.is_empty() {
                o["table"] = json!(sl.table);
            } else {
                o["pivots"] = json!(sl.pivots);
            }
            o
        })
        .collect();
    Ok(json!({"sheet": sh.name, "slicers": list}))
}

fn slicer_toggle(s: &mut Session, p: &Json) -> Result<Json> {
    let (idx, id) = find_slicer(s, p)?;
    let value = str_param(p, "value").ok_or_else(|| bad("slicer.toggle", "missing `value`"))?.to_string();
    let multi = bool_param(p, "multi").unwrap_or(false);
    let sheet = s.doc()?.wb.active_sheet;
    let (all, current) = {
        let d = s.doc()?;
        let sl = d.wb.active().and_then(|sh| sh.slicers.get(idx)).ok_or_else(|| bad("slicer.toggle", "no slicer"))?;
        (slicer_values(&d.wb, sheet, sl), sl.selected.clone())
    };
    if !all.iter().any(|v| v.eq_ignore_ascii_case(&value)) {
        return Err(bad("slicer.toggle", "value is not in the column"));
    }
    let new_selected: Option<Vec<String>> = if multi {
        let mut set: Vec<String> = current.unwrap_or_else(|| all.clone());
        match set.iter().position(|x| x.eq_ignore_ascii_case(&value)) {
            Some(pos) => {
                set.remove(pos);
            }
            None => set.push(value.clone()),
        }
        // Everything selected is the same as no filter.
        if set.len() == all.len() && all.iter().all(|a| set.iter().any(|x| x.eq_ignore_ascii_case(a))) { None } else { Some(set) }
    } else {
        // Single click: show only this value; clicking the sole selected value clears the filter.
        match &current {
            Some(set) if set.len() == 1 && set.iter().any(|x| x.eq_ignore_ascii_case(&value)) => None,
            _ => Some(vec![value.clone()]),
        }
    };
    edit(s, |cx| {
        if let Some(sl) = cx.sheet_mut(sheet)?.slicers.get_mut(idx) {
            sl.selected = new_selected;
        }
        reapply_slicer(&mut cx.wb, sheet, id);
        cx.structural = true;
        Ok(Json::Null)
    })
}

fn slicer_select_all(s: &mut Session, p: &Json) -> Result<Json> {
    let (idx, id) = find_slicer(s, p)?;
    let sheet = s.doc()?.wb.active_sheet;
    edit(s, |cx| {
        if let Some(sl) = cx.sheet_mut(sheet)?.slicers.get_mut(idx) {
            sl.selected = None;
        }
        reapply_slicer(&mut cx.wb, sheet, id);
        cx.structural = true;
        Ok(Json::Null)
    })
}

fn slicer_delete(s: &mut Session, p: &Json) -> Result<Json> {
    let (idx, id) = find_slicer(s, p)?;
    let sheet = s.doc()?.wb.active_sheet;
    // Absolute column of the slicer, so its filter criterion can be removed too.
    let abs_col = s.doc().ok().and_then(|d| {
        let sh = d.wb.active()?;
        let sl = sh.slicers.get(idx)?;
        let t = sh.tables.iter().find(|t| t.name.eq_ignore_ascii_case(&sl.table))?;
        let ci = t.column_index(&sl.column)?;
        Some(t.range.start.col + ci as u32)
    });
    let links = s.doc()?.wb.sheet(sheet).and_then(|sh| sh.slicers.get(idx)).map(|sl| sl.pivots.clone()).unwrap_or_default();
    edit(s, |cx| {
        let sh = cx.sheet_mut(sheet)?;
        sh.slicers.retain(|x| x.id != id);
        if !links.is_empty() {
            refresh_connected(&mut cx.wb, &links);
            cx.structural = true;
            return Ok(Json::Null);
        }
        if let (Some(abs), Some(af)) = (abs_col, sh.autofilter.as_mut()) {
            let off = abs.saturating_sub(af.range.start.col);
            af.criteria.retain(|(o, _)| *o != off);
        }
        super::data::apply_filter(&mut cx.wb, sheet);
        cx.structural = true;
        Ok(Json::Null)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[u8] = include_bytes!("../../../../tests/mcp-contract/fixtures/pivot-slicer.xlsx");

    fn report(s: &Session) -> Vec<(String, f64)> {
        let d = s.doc().unwrap();
        let si = d.wb.sheet_index("Report").or_else(|| d.wb.sheet_index("Pivot")).unwrap();
        let sh = d.wb.sheet(si).unwrap();
        let r = sh.pivots[0].last_range.unwrap();
        (r.start.row + 1..=r.end.row)
            .filter_map(|row| {
                let label = crate::display::cell_text(&d.wb, sh, CellRef::new(row, r.start.col));
                sh.value(CellRef::new(row, r.start.col + 1)).as_f64().map(|v| (label, v))
            })
            .collect()
    }

    #[test]
    fn pivot_slicers_filter_their_pivot_tables() {
        let mut s = Session::new();
        s.execute("file.open", json!({"base64": crate::io::base64_encode(FIXTURE), "name": "pivot-slicer.xlsx"})).unwrap();
        s.execute("sheet.activate", json!({"sheet": "Pivot"})).unwrap();
        s.execute("pivot.refresh", json!({"replace": true})).unwrap();
        let rows = report(&s);
        assert_eq!(rows.last(), Some(&("Grand Total".to_string(), 26.0)), "only Bolts (the Product slicer): {rows:?}");
        // Select Nuts as well: everything.
        let product = s.doc().unwrap().wb.active().unwrap().slicers.iter().find(|x| x.column == "Product").unwrap().id;
        s.execute("slicer.toggle", json!({"slicer": product, "value": "Nuts", "multi": true})).unwrap();
        assert_eq!(report(&s).last().map(|r| r.1), Some(59.0));
        // The Region slicer (a row field): one region.
        let region = s.doc().unwrap().wb.active().unwrap().slicers.iter().find(|x| x.column == "Region").unwrap().id;
        s.execute("slicer.toggle", json!({"slicer": region, "value": "South"})).unwrap();
        assert_eq!(report(&s), vec![("South".to_string(), 23.0), ("Grand Total".to_string(), 23.0)]);
        // Renaming the sheet keeps the connection; the saved file and its reopening agree.
        s.execute("sheet.rename", json!({"name": "Report"})).unwrap();
        let d = s.doc().unwrap();
        assert!(d.wb.active().unwrap().slicers.iter().all(|x| x.pivots == vec!["Report!PivotTable1".to_string()]));
        let bytes = crate::io::save_bytes(&d.wb, "out.xlsx").unwrap();
        let (back, _) = crate::io::open_bytes("out.xlsx", &bytes).unwrap();
        let sh = back.sheet(back.sheet_index("Report").unwrap()).unwrap();
        let r = sh.slicers.iter().find(|x| x.column == "Region").unwrap();
        assert_eq!(r.selected, Some(vec!["South".to_string()]));
        assert_eq!(r.pivots, vec!["Report!PivotTable1".to_string()]);
        // Deleting the PivotTable takes its slicers.
        s.execute("pivot.delete", json!({})).unwrap();
        assert!(s.doc().unwrap().wb.active().unwrap().slicers.is_empty());
    }

    #[test]
    fn insert_pivot_slicer() {
        let mut s = Session::new();
        s.new_workbook();
        s.execute("range.setValues", json!({"range": "A1", "values": [["Region", "Units"], ["North", 1], ["South", 2], ["North", 3]]})).unwrap();
        s.execute("insert.pivotTable", json!({"source": "Sheet1!$A$1:$B$4", "destination": "Sheet1!E3", "rows": ["Region"], "values": ["Units"]}))
            .unwrap();
        let r = s.execute("insert.slicer", json!({"pivot": "PivotTable1", "column": "Region", "at": "H2"})).unwrap();
        s.execute("slicer.toggle", json!({"slicer": r["slicer"], "value": "North"})).unwrap();
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        assert_eq!(sh.slicers[0].pivots, vec!["Sheet1!PivotTable1".to_string()]);
        let total = (0..20).find_map(|row| {
            (crate::display::cell_text(&d.wb, sh, CellRef::new(row, 4)) == "Grand Total").then(|| sh.value(CellRef::new(row, 5)).as_f64())
        });
        assert_eq!(total, Some(Some(4.0)));
        assert!(s.execute("insert.slicer", json!({"pivot": "Nope"})).is_err());
        assert!(s.execute("insert.slicer", json!({"pivot": "PivotTable1", "column": "Nope"})).is_err());
    }
}
