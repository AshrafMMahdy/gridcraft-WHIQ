//! Read-only commands for agents: inspect the workbook, read ranges, cell details.

use gridcraft_core::{CellRef, RangeRef, Value};
use serde_json::{Value as Json, json};

use super::*;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(query "document.inspect", "Inspect Workbook", [], None, "{} → sheets, selection, names, tables, charts, dirty, history", has_doc, inspect),
        cmd!(query "sheet.read", "Read Range", [], None, "{range?: \"A1:D10\" (default: used range), sheet?, formulas?: bool, formatted?: bool} → rows of values", has_doc, read),
        cmd!(query "cell.get", "Get Cell", [], None, "{cell?, sheet?} → value, formula, display text, style, comment, link", has_doc, cell_get),
        cmd!(query "selection.stats", "Selection Statistics", [], None, "{} → count, sum, average, min, max", has_doc, sel_stats),
        cmd!(query "history.list", "Undo History", [], None, "{} → undo/redo labels", has_doc, history),
        cmd!(query "app.commands", "List Commands", [], None, "{search?} → ids, labels, params", always, list_commands),
    ]
}

pub fn value_json(v: &Value) -> Json {
    match v {
        Value::Empty => Json::Null,
        Value::Number(n) => json!(n),
        Value::Text(t) => json!(t.as_ref()),
        Value::Bool(b) => json!(b),
        Value::Error(e) => json!({"error": e.as_str()}),
        Value::Array(a) => {
            let rows: Vec<Json> =
                (0..a.rows).map(|r| Json::Array((0..a.cols).map(|c| value_json(&a.get(r, c).cloned().unwrap_or_default())).collect())).collect();
            Json::Array(rows)
        }
    }
}

fn inspect(s: &mut Session, _: &Json) -> Result<Json> {
    let d = s.doc()?;
    let wb = &d.wb;
    let sheets: Vec<Json> = wb
        .sheets
        .iter()
        .map(|sh| {
            json!({
                "name": sh.name,
                "visibility": format!("{:?}", sh.visibility),
                "usedRange": sh.used_range_hint().map(|r| r.a1()),
                "cells": sh.cells.len(),
                "freeze": sh.freeze,
                "merges": sh.merges.iter().map(|m| m.a1()).collect::<Vec<_>>(),
                "tables": sh.tables.iter().map(|t| json!({"name": t.name, "range": t.range.a1(), "style": t.style, "totals": t.totals_row, "columns": t.columns.iter().map(|c| c.name.clone()).collect::<Vec<_>>()})).collect::<Vec<_>>(),
                "charts": sh.charts.iter().map(chart_json).collect::<Vec<_>>(),
                "images": sh.images.len(),
                "shapes": sh.shapes.len(),
                "conditionalFormats": sh.cond_formats.len(),
                "validations": sh.validations.len(),
                "comments": sh.comments.len(),
                "autofilter": sh.autofilter.as_ref().map(|a| a.range.a1()),
                "protected": sh.protection.is_some(),
                "protection": protection_json(wb, sh),
                "codeName": sh.code_name,
                "tabColor": sh.tab_color.and_then(|c| c.hex(&wb.theme)),
                "zoom": sh.zoom,
            })
        })
        .collect();
    Ok(json!({
        "title": d.display_title(),
        "path": d.path,
        "dirty": d.is_dirty(),
        "activeSheet": wb.active_sheet,
        "sheets": sheets,
        "selection": d.selection.a1(),
        "activeCell": d.selection.active.a1(),
        "names": wb.names.iter().map(|n| json!({"name": n.name, "refersTo": format!("={}", n.formula), "scope": n.scope})).collect::<Vec<_>>(),
        "calcMode": format!("{:?}", wb.calc.mode),
        // WHIQ: a structure lock refuses sheet inserts, deletes, renames, moves, hides, colours.
        "structureProtected": wb.protected_structure,
        "codeName": wb.code_name,
        "macros": wb.vba_project.is_some(),
        "undo": d.undo.iter().rev().take(20).map(|e| e.label.clone()).collect::<Vec<_>>(),
        "redo": d.redo.iter().rev().take(20).map(|e| e.label.clone()).collect::<Vec<_>>(),
        "theme": wb.theme.name,
        "mode": format!("{:?}", s.mode),
    }))
}

/// WHIQ: a chart's type as `insert.chart` names it (`column`, `line`, `combo`…) and its subtype.
pub(crate) fn chart_type_name(k: gridcraft_model::ChartKind) -> (&'static str, Option<&'static str>) {
    use gridcraft_model::ChartKind as K;
    match k {
        K::ColumnClustered => ("column", None),
        K::ColumnStacked => ("column", Some("stacked")),
        K::ColumnStacked100 => ("column", Some("stacked100")),
        K::BarClustered => ("bar", None),
        K::BarStacked => ("bar", Some("stacked")),
        K::BarStacked100 => ("bar", Some("stacked100")),
        K::Line => ("line", None),
        K::LineMarkers => ("line", Some("markers")),
        K::LineStacked => ("line", Some("stacked")),
        K::Pie => ("pie", None),
        K::Doughnut => ("doughnut", None),
        K::Area => ("area", None),
        K::AreaStacked => ("area", Some("stacked")),
        K::Scatter => ("scatter", None),
        K::ScatterLines => ("scatter", Some("lines")),
        K::Bubble => ("bubble", None),
        K::Radar => ("radar", None),
        K::Histogram => ("histogram", None),
        K::Waterfall => ("waterfall", None),
        K::Funnel => ("funnel", None),
        K::Treemap => ("treemap", None),
        K::Sunburst => ("sunburst", None),
        K::BoxWhisker => ("boxWhisker", None),
        K::Stock => ("stock", None),
        K::Combo => ("combo", None),
    }
}

/// WHIQ: the kind a series is drawn as (a combo's series without one of their own: the first a
/// column, the others lines, as the writer and the painter do).
pub(crate) fn series_kind(ch: &gridcraft_model::Chart, i: usize) -> gridcraft_model::ChartKind {
    use gridcraft_model::ChartKind as K;
    let own = ch.series.get(i).and_then(|s| s.kind).filter(|k| *k != K::Combo);
    match ch.kind {
        K::Combo => own.unwrap_or(if i == 0 { K::ColumnClustered } else { K::Line }),
        k => own.unwrap_or(k),
    }
}

/// WHIQ: a chart as an agent reads it: title, anchor, type, axis titles and every series with its
/// kind, references and axis.
pub(crate) fn chart_json(c: &gridcraft_model::Chart) -> Json {
    let (t, sub) = chart_type_name(c.kind);
    let series: Vec<Json> = c
        .series
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let (k, ks) = chart_type_name(series_kind(c, i));
            json!({"index": i + 1, "name": s.name, "kind": k, "subtype": ks, "values": s.values, "categories": s.categories,
                   "axis": if s.secondary { "secondary" } else { "primary" }})
        })
        .collect();
    let a = &c.anchor;
    json!({
        "id": c.id,
        "kind": format!("{:?}", c.kind),
        "type": t,
        "subtype": sub,
        "title": c.title,
        "at": a.cell.a1(),
        "anchor": {"cell": a.cell.a1(), "dx": a.dx, "dy": a.dy, "width": a.width, "height": a.height, "mode": format!("{:?}", a.mode)},
        "x_title": c.x_title,
        "y_title": c.y_title,
        "y2_title": c.y2_title,
        "series_count": c.series.len(),
        "series": series,
        "pivot": c.pivot,
        "trendlines": c.series.iter().flat_map(|s| s.trendlines.iter().map(|t| format!("{:?}", t.kind))).collect::<Vec<_>>(),
    })
}

/// WHIQ: the cells a protected sheet still lets anyone edit: unlocked cells (and unlocked whole
/// rows and columns) as ranges, at most `cap`, and how many more there are.
pub(crate) fn unlocked_ranges(wb: &gridcraft_model::Workbook, sh: &gridcraft_model::Sheet, cap: usize) -> (Vec<String>, usize) {
    let unlocked = |st: gridcraft_model::StyleId| !wb.styles.get(st).protection.locked;
    let mut out: Vec<String> = vec![];
    for (c, info) in &sh.cols {
        if info.style.is_some_and(unlocked) {
            let l = gridcraft_core::col_to_letters(*c);
            out.push(format!("{l}:{l}"));
        }
    }
    for (r, info) in &sh.rows {
        if info.style.is_some_and(unlocked) {
            out.push(format!("{0}:{0}", r + 1));
        }
    }
    // Cells by column, rows in order; runs down a column, then side-by-side equal runs joined.
    let mut cells: Vec<CellRef> =
        if sh.cells.is_resident() { sh.cells.iter().filter(|(_, x)| unlocked(x.style)).map(|(c, _)| c).take(200_000).collect() } else { vec![] };
    cells.sort_by_key(|c| (c.col, c.row));
    let mut runs: Vec<RangeRef> = vec![];
    for c in cells {
        match runs.last_mut() {
            Some(r) if r.end.col == c.col && r.start.col == c.col && r.end.row + 1 == c.row => r.end.row = c.row,
            _ => runs.push(RangeRef::cell(c)),
        }
    }
    let mut blocks: Vec<RangeRef> = vec![];
    for r in runs {
        if let Some(b) = blocks.iter_mut().find(|b| b.end.col + 1 == r.start.col && b.start.row == r.start.row && b.end.row == r.end.row) {
            b.end.col = r.start.col;
        } else {
            blocks.push(r);
        }
    }
    out.extend(blocks.iter().map(|b| b.a1()));
    let more = out.len().saturating_sub(cap);
    out.truncate(cap);
    (out, more)
}

/// WHIQ: what a sheet's protection allows and what can still be edited (`null` unprotected).
pub(crate) fn protection_json(wb: &gridcraft_model::Workbook, sh: &gridcraft_model::Sheet) -> Json {
    let Some(p) = &sh.protection else { return Json::Null };
    let (cells, more) = unlocked_ranges(wb, sh, 20);
    let allowed: Vec<&str> = [
        ("formatCells", p.format_cells),
        ("formatColumns", p.format_columns),
        ("formatRows", p.format_rows),
        ("insertColumns", p.insert_columns),
        ("insertRows", p.insert_rows),
        ("insertHyperlinks", p.insert_hyperlinks),
        ("deleteColumns", p.delete_columns),
        ("deleteRows", p.delete_rows),
        ("sort", p.sort),
        ("autoFilter", p.autofilter),
        ("pivotTables", p.pivot_tables),
        ("editObjects", p.edit_objects),
        ("editScenarios", p.edit_scenarios),
    ]
    .into_iter()
    .filter(|(_, on)| *on)
    .map(|(k, _)| k)
    .collect();
    json!({
        "password": p.password.is_some() || p.legacy_password_hash.is_some(),
        "unlockedCells": cells,
        "unlockedMore": more,
        "editRanges": sh.edit_ranges.iter().map(|r| json!({"title": r.title, "ranges": r.ranges.iter().map(|x| x.a1()).collect::<Vec<_>>(), "password": r.password.is_some()})).collect::<Vec<_>>(),
        "allowed": allowed,
    })
}

/// WHIQ: one line saying what a protected sheet still lets anyone edit, for agents.
pub(crate) fn protection_summary(wb: &gridcraft_model::Workbook, sh: &gridcraft_model::Sheet) -> Option<String> {
    sh.protection.as_ref()?;
    let (cells, more) = unlocked_ranges(wb, sh, 20);
    let mut parts = vec![];
    if !cells.is_empty() {
        parts.push(format!("its unlocked cells {}{}", cells.join(", "), if more > 0 { format!(" and {more} more") } else { String::new() }));
    }
    let ranges: Vec<String> = sh
        .edit_ranges
        .iter()
        .take(10)
        .map(|r| {
            format!(
                "\"{}\" ({}{})",
                r.title,
                r.ranges.iter().map(|x| x.a1()).collect::<Vec<_>>().join(","),
                if r.password.is_some() { ", needs its password" } else { "" }
            )
        })
        .collect();
    if !ranges.is_empty() {
        parts.push(format!("its Allow Edit Ranges {}", ranges.join(", ")));
    }
    Some(if parts.is_empty() {
        format!("sheet \"{}\" is protected and has no unlocked cells", sh.name)
    } else {
        format!("sheet \"{}\" is protected: only {} can be edited", sh.name, parts.join(" and "))
    })
}

fn read(s: &mut Session, p: &Json) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let d = s.doc()?;
    let sh = d.wb.sheet(sheet).ok_or(EngineError::NoDocument)?;
    let r = match str_param(p, "range") {
        Some(_) => target_range(s, p)?,
        None => match sh.used_range() {
            Some(r) => r,
            None => return Ok(json!({"range": null, "values": []})),
        },
    };
    let r = match sh.used_range() {
        Some(u) if r.is_full_cols() || r.is_full_rows() => r.intersection(&u).unwrap_or(RangeRef::cell(r.start)),
        _ => r,
    };
    if r.count() > 1_000_000 {
        return Err(bad("sheet.read", "range larger than 1,000,000 cells; read it in parts"));
    }
    let formulas = bool_param(p, "formulas").unwrap_or(false);
    let formatted = bool_param(p, "formatted").unwrap_or(false);
    let mut rows = Vec::with_capacity(r.height() as usize);
    for row in r.start.row..=r.end.row {
        let mut cols = Vec::with_capacity(r.width() as usize);
        for col in r.start.col..=r.end.col {
            let c = CellRef::new(row, col);
            let v = if formulas && let Some(f) = sh.cell(c).and_then(|x| x.formula.as_ref()) {
                json!(format!("={}", f.text))
            } else if formatted {
                json!(crate::display::cell_text(&d.wb, sh, c))
            } else {
                value_json(&sh.value(c))
            };
            cols.push(v);
        }
        rows.push(Json::Array(cols));
    }
    Ok(json!({"range": r.a1(), "sheet": sh.name, "values": rows}))
}

fn cell_get(s: &mut Session, p: &Json) -> Result<Json> {
    let sheet = target_sheet(s, p)?;
    let d = s.doc()?;
    let sh = d.wb.sheet(sheet).ok_or(EngineError::NoDocument)?;
    let c = cell_param_on(s, p, "cell", sheet)?.unwrap_or(d.selection.active);
    let cell = sh.cell(c);
    let style = d.wb.styles.get(sh.style_id(c));
    Ok(json!({
        "cell": c.a1(),
        "value": value_json(&sh.value(c)),
        "type": if sh.cell_pictures.contains_key(&c) { "picture" } else { match sh.value(c) { Value::Empty => "empty", Value::Number(_) => "number", Value::Text(_) => "text", Value::Bool(_) => "boolean", Value::Error(_) => "error", Value::Array(_) => "array" } },
        "formula": cell.and_then(|x| x.formula.as_ref()).map(|f| format!("={}", f.text)),
        "text": crate::display::cell_text(&d.wb, sh, c),
        "input": sh.input_text(c),
        "picture": sh.cell_pictures.get(&c).map(|picture| json!({"mime": picture.mime, "alt": picture.alt, "bytes": picture.data.len()})),
        "spilledFrom": sh.spill_ranges.iter().find(|(a, r)| **a != c && r.contains(c)).map(|(a, _)| a.a1()),
        "spill": sh.spill_ranges.get(&c).map(|r| r.a1()),
        "style": style,
        "merge": sh.merge_at(c).map(|m| m.a1()),
        "comment": sh.comments.get(&c),
        "hyperlink": sh.hyperlinks.get(&c),
    }))
}

fn sel_stats(s: &mut Session, _: &Json) -> Result<Json> {
    let d = s.doc()?;
    let sh = d.wb.active().ok_or(EngineError::NoDocument)?;
    let st = crate::display::stats(sh, &d.selection.ranges);
    serde_json::to_value(st).map_err(|e| EngineError::Other(e.to_string()))
}

fn history(s: &mut Session, _: &Json) -> Result<Json> {
    let d = s.doc()?;
    Ok(
        json!({"undo": d.undo.iter().rev().map(|e| e.label.clone()).collect::<Vec<_>>(), "redo": d.redo.iter().rev().map(|e| e.label.clone()).collect::<Vec<_>>()}),
    )
}

fn list_commands(s: &mut Session, p: &Json) -> Result<Json> {
    let q = str_param(p, "search").map(str::to_ascii_lowercase);
    let v: Vec<Json> = s
        .commands()
        .into_iter()
        .filter(|c| q.as_ref().is_none_or(|q| c.id.to_ascii_lowercase().contains(q.as_str()) || c.label.to_ascii_lowercase().contains(q.as_str())))
        .map(|c| serde_json::to_value(c).unwrap_or_default())
        .collect();
    Ok(Json::Array(v))
}
