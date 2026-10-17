//! WHIQ: `file.build` — a whole workbook from one JSON spec (the platform's report writer),
//! and `formulas.recalculate`.
//!
//! The spec is applied with the engine's own commands (typed input and formulas through
//! `range.setValues`, number formats, widths, freeze panes, AutoFilter, tables, conditional
//! formats, validation), so a built workbook is exactly what the same edits by hand make. Charts
//! are native charts with series by range (bar, column, line, pie, doughnut, area, scatter, combo
//! with a secondary axis). Formulas are recalculated before the workbook is returned or saved, so
//! the file carries correct cached values.

use gridcraft_core::{CellRef, RangeRef, col_to_letters};
use gridcraft_model::{Anchor, Chart, ChartKind, LegendPos, Series};
use serde_json::{Map, Value as Json, json};

use super::*;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            noundo "file.build",
            "Build Workbook",
            [],
            None,
            "{sheets: [{name, header?: [..], rows: [[..]], start?: \"A1\", columns?: [{header?, width?, format?}], number_formats?: {\"B\": \"#,##0\", \"C2:D9\": \"0%\"}, column_widths?: {\"A\": 30} | [30, 12] | \"auto\", autofit?: bool (default true: columns without a width fit their contents), freeze_header?: bool, autofilter?: bool, table?: bool | \"TableStyleMedium2\" | {name?, style?}, header_style?: {bold?, fill?, font_color?}, merges?: [\"A1:D1\"], rtl?: bool, charts?: [{type: bar|column|line|pie|doughnut|area|scatter|combo, title?, data?: \"A1:C9\", series?: [{name?, values, categories?, type?, secondary?, color?}], x_title?, y_title?, legend?, data_labels?, stacked?, at?, width?, height?}], conditional_formats?: [{range, rule}], validations?: [{range, type, …}]}], title?, author?, company?, path?} → {sheets, charts, path?}",
            always,
            build
        ),
        cmd!(
            noundo "formulas.recalculate",
            "Recalculate Workbook",
            [],
            None,
            "{} → {formulas, cells, errors: [{cell, error}] (first 100), errorCount} — a full recalculation of every formula (cached values are what the file saves)",
            has_doc,
            recalculate
        ),
    ]
}

const CMD: &str = "file.build";
/// Largest workbook a spec may describe (cells written).
const MAX_CELLS: u64 = 5_000_000;

fn fail(step: &str, e: EngineError) -> EngineError {
    match e {
        EngineError::BadParams { msg, .. } => bad(CMD, format!("{step}: {msg}")),
        other => bad(CMD, format!("{step}: {other}")),
    }
}

fn run(s: &mut Session, step: &str, id: &str, p: Json) -> Result<Json> {
    s.execute(id, p).map_err(|e| fail(step, e))
}

/// `'My Sheet'!` for references.
fn q(name: &str) -> String {
    format!("{}!", gridcraft_formula::quote_sheet(name))
}

/// Columns `B`, `B:D`, a cell or a range → a range inside the data block (header excluded for
/// whole columns).
fn target(key: &str, first_data_row: u32, last_row: u32) -> Option<RangeRef> {
    let k = key.trim().trim_matches('$');
    if let Some(r) = RangeRef::parse(k).filter(|r| !r.is_full_cols()) {
        return Some(r);
    }
    let (a, b) = k.split_once(':').unwrap_or((k, k));
    let c0 = gridcraft_core::letters_to_col(a.trim())?;
    let c1 = gridcraft_core::letters_to_col(b.trim())?;
    Some(RangeRef::new(CellRef::new(first_data_row, c0.min(c1)), CellRef::new(last_row.max(first_data_row), c0.max(c1))))
}

fn build(s: &mut Session, p: &Json) -> Result<Json> {
    let sheets = p.get("sheets").and_then(Json::as_array).filter(|a| !a.is_empty()).ok_or_else(|| {
        bad(CMD, "missing `sheets`: give at least one sheet, e.g. {\"sheets\": [{\"name\": \"Data\", \"header\": [\"Item\", \"Qty\"], \"rows\": [[\"Bolts\", 12]]}]}")
    })?;
    let mut total: u64 = 0;
    let mut names: Vec<String> = Vec::new();
    for (i, sh) in sheets.iter().enumerate() {
        let name = sh.get("name").and_then(Json::as_str).map(str::to_string).unwrap_or_else(|| format!("Sheet{}", i + 1));
        if names.iter().any(|n| n.eq_ignore_ascii_case(&name)) {
            return Err(bad(CMD, format!("two sheets are named `{name}`")));
        }
        names.push(name);
        let rows = sh.get("rows").and_then(Json::as_array).map_or(0, Vec::len) as u64 + u64::from(sh.get("header").is_some());
        let cols = sh.get("rows").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_array).map(Vec::len).max().unwrap_or(0) as u64;
        total = total.saturating_add(rows.saturating_mul(cols.max(1)));
    }
    if total > MAX_CELLS {
        return Err(bad(CMD, format!("the spec holds {total} cells, more than {MAX_CELLS}: build it in parts (write_range more rows after)")));
    }
    let before = s.active_index();
    let had = s.documents().len();
    s.execute("file.new", json!({})).map_err(|e| fail("new workbook", e))?;
    let result = fill(s, p, sheets, &names);
    match result {
        Ok(v) => Ok(v),
        Err(e) => {
            // Leave no half-built workbook behind: close it and go back.
            if s.documents().len() > had {
                let i = s.active_index();
                s.close_document(i);
                if before < s.documents().len() {
                    s.set_active(before);
                }
            }
            Err(e)
        }
    }
}

fn fill(s: &mut Session, p: &Json, sheets: &[Json], names: &[String]) -> Result<Json> {
    let mut out_sheets = Vec::new();
    let mut charts = 0usize;
    for (i, (spec, name)) in sheets.iter().zip(names).enumerate() {
        if i == 0 {
            run(s, &format!("sheet {name}"), "sheet.rename", json!({"sheet": 0, "name": name}))?;
        } else {
            run(s, &format!("sheet {name}"), "home.insertSheet", json!({"name": name, "before": i}))?;
        }
        run(s, &format!("sheet {name}"), "sheet.activate", json!({"sheet": name}))?;
        out_sheets.push(fill_sheet(s, spec, name, &mut charts)?);
    }
    // Charts listed at the top level name their sheet (the platform's chart specs).
    for (k, c) in p.get("charts").and_then(Json::as_array).into_iter().flatten().enumerate() {
        let sheet = str_param(c, "sheet").map(str::to_string).or_else(|| names.first().cloned()).unwrap_or_default();
        let Some(i) = names.iter().position(|n| n.eq_ignore_ascii_case(&sheet)) else {
            return Err(bad(CMD, format!("chart {}: there is no sheet `{sheet}` (sheets: {})", k + 1, names.join(", "))));
        };
        let name = names.get(i).cloned().unwrap_or_default();
        add_chart(s, c, &name, None, false).map_err(|e| fail(&format!("chart {}", k + 1), e))?;
        charts += 1;
    }
    run(s, "activate the first sheet", "sheet.activate", json!({"sheet": 0}))?;
    let mut props = Map::new();
    for (k, key) in [("title", "title"), ("author", "author"), ("company", "company"), ("subject", "subject"), ("last_modified_by", "lastModifiedBy")]
    {
        if let Some(v) = p.get(k).and_then(Json::as_str) {
            props.insert(key.into(), json!(v));
        }
    }
    if !props.is_empty() {
        run(s, "properties", "file.properties", Json::Object(props))?;
    }
    // Every formula computed now: the saved file carries the right cached values.
    let calc = run(s, "recalculate", "formulas.calculateNow", json!({}))?;
    // The built workbook starts its history here.
    if let Ok(d) = s.doc_mut() {
        d.undo.clear();
        d.redo.clear();
    }
    let mut out = json!({"sheets": out_sheets, "charts": charts, "recalculated": calc["cells"]});
    if let Some(path) = str_param(p, "path").filter(|x| !x.is_empty()) {
        let r = run(s, "save", "file.save", json!({"path": path}))?;
        out["path"] = r["path"].clone();
        out["bytes"] = r["bytes"].clone();
    }
    Ok(out)
}

fn fill_sheet(s: &mut Session, spec: &Json, name: &str, charts: &mut usize) -> Result<Json> {
    let step = |what: &str| format!("sheet {name}, {what}");
    let start = match str_param(spec, "start") {
        Some(a) => CellRef::parse(a.trim()).ok_or_else(|| bad(CMD, step(&format!("`start` is not a cell: `{a}`"))))?,
        None => CellRef::new(0, 0),
    };
    let columns: Vec<Json> = spec.get("columns").and_then(Json::as_array).cloned().unwrap_or_default();
    let header: Option<Vec<Json>> = spec.get("header").and_then(Json::as_array).cloned().or_else(|| {
        let h: Vec<Json> = columns.iter().map(|c| c.get("header").cloned().unwrap_or(Json::Null)).collect();
        (!h.is_empty() && h.iter().any(|x| !x.is_null())).then_some(h)
    });
    let rows: Vec<Json> = spec.get("rows").and_then(Json::as_array).cloned().unwrap_or_default();
    for (i, r) in rows.iter().enumerate() {
        if !r.is_array() {
            return Err(bad(CMD, step(&format!("row {} is not a list of cells", i + 1))));
        }
    }
    let width = rows.iter().filter_map(Json::as_array).map(Vec::len).chain(header.as_ref().map(Vec::len)).max().unwrap_or(0) as u32;
    let mut values: Vec<Json> = Vec::with_capacity(rows.len() + 1);
    if let Some(h) = &header {
        values.push(Json::Array(h.clone()));
    }
    values.extend(rows.iter().cloned());
    let q = q(name);
    let first_data = start.row + u32::from(header.is_some());
    let last_row = start.row + (values.len() as u32).saturating_sub(1);
    let block = (width > 0 && !values.is_empty()).then(|| RangeRef::new(start, CellRef::new(last_row, start.col + width.saturating_sub(1))));
    if !values.is_empty() {
        run(s, &step("values"), "range.setValues", json!({"range": format!("{q}{}", start.a1()), "values": values}))?;
    }
    let header_range = header.as_ref().and(block).map(|b| RangeRef::new(b.start, CellRef::new(b.start.row, b.end.col)));
    // Header look (bold by default).
    if let Some(h) = header_range {
        let hs = spec.get("header_style").cloned().unwrap_or_else(|| json!({}));
        if hs.get("bold").and_then(Json::as_bool).unwrap_or(true) {
            run(s, &step("header style"), "home.bold", json!({"range": format!("{q}{}", h.a1()), "on": true}))?;
        }
        if let Some(c) = hs.get("fill").and_then(Json::as_str) {
            run(s, &step("header fill"), "home.fillColor", json!({"range": format!("{q}{}", h.a1()), "color": c}))?;
        }
        if let Some(c) = hs.get("font_color").or(hs.get("color")).and_then(Json::as_str) {
            run(s, &step("header font colour"), "home.fontColor", json!({"range": format!("{q}{}", h.a1()), "color": c}))?;
        }
    }
    // Per-column formats and widths.
    for (ci, c) in columns.iter().enumerate() {
        let col = start.col + ci as u32;
        let letter = col_to_letters(col);
        if let Some(f) = c.get("format").and_then(Json::as_str)
            && last_row >= first_data
        {
            let r = RangeRef::new(CellRef::new(first_data, col), CellRef::new(last_row, col));
            run(s, &step(&format!("format of column {letter}")), "home.numberFormat", json!({"range": format!("{q}{}", r.a1()), "format": f}))?;
        }
        if let Some(w) = c.get("width").and_then(Json::as_f64) {
            run(s, &step(&format!("width of column {letter}")), "home.columnWidth", json!({"cols": format!("{letter}:{letter}"), "chars": w}))?;
        }
    }
    match spec.get("number_formats") {
        Some(Json::Object(m)) => {
            for (k, f) in m {
                let Some(f) = f.as_str() else { continue };
                let r =
                    target(k, first_data, last_row).ok_or_else(|| bad(CMD, step(&format!("number format target `{k}` is not a column or range"))))?;
                run(s, &step(&format!("number format {k}")), "home.numberFormat", json!({"range": format!("{q}{}", r.a1()), "format": f}))?;
            }
        }
        Some(Json::Array(list)) => {
            for item in list {
                let (Some(k), Some(f)) = (item.get("range").and_then(Json::as_str), item.get("format").and_then(Json::as_str)) else { continue };
                let r =
                    target(k, first_data, last_row).ok_or_else(|| bad(CMD, step(&format!("number format target `{k}` is not a column or range"))))?;
                run(s, &step(&format!("number format {k}")), "home.numberFormat", json!({"range": format!("{q}{}", r.a1()), "format": f}))?;
            }
        }
        _ => {}
    }
    match spec.get("column_widths") {
        Some(Json::Object(m)) => {
            for (k, w) in m {
                let Some(w) = w.as_f64() else { continue };
                let cols = if k.contains(':') { k.clone() } else { format!("{k}:{k}") };
                run(s, &step(&format!("width of {k}")), "home.columnWidth", json!({"cols": cols, "chars": w}))?;
            }
        }
        Some(Json::Array(list)) => {
            for (i, w) in list.iter().enumerate() {
                let Some(w) = w.as_f64() else { continue };
                let l = col_to_letters(start.col + i as u32);
                run(s, &step(&format!("width of {l}")), "home.columnWidth", json!({"cols": format!("{l}:{l}"), "chars": w}))?;
            }
        }
        Some(Json::String(t)) if t == "auto" => {
            if let Some(b) = block {
                let cols = format!("{}:{}", col_to_letters(b.start.col), col_to_letters(b.end.col));
                run(s, &step("autofit widths"), "home.autofitColumnWidth", json!({"cols": cols}))?;
            }
        }
        _ => {}
    }
    // Columns given no width fit their contents (numbers never show as ####), unless
    // `autofit: false`.
    if spec.get("autofit").and_then(Json::as_bool) != Some(false)
        && let Some(b) = block
    {
        let explicit = |col: u32| -> bool {
            let i = (col - start.col) as usize;
            let letter = col_to_letters(col);
            columns.get(i).is_some_and(|c| c.get("width").is_some())
                || match spec.get("column_widths") {
                    Some(Json::Object(m)) => m.keys().any(|k| {
                        let (a, z) = k.split_once(':').unwrap_or((k, k));
                        match (gridcraft_core::letters_to_col(a.trim()), gridcraft_core::letters_to_col(z.trim())) {
                            (Some(a), Some(z)) => (a.min(z)..=a.max(z)).contains(&col),
                            _ => k.eq_ignore_ascii_case(&letter),
                        }
                    }),
                    Some(Json::Array(l)) => l.get(i).is_some_and(|w| w.is_number()),
                    Some(Json::String(_)) => true,
                    _ => false,
                }
        };
        for col in b.start.col..=b.end.col {
            if !explicit(col) {
                let l = col_to_letters(col);
                run(s, &step(&format!("autofit column {l}")), "home.autofitColumnWidth", json!({"cols": format!("{l}:{l}")}))?;
            }
        }
    }
    for m in spec.get("merges").and_then(Json::as_array).into_iter().flatten().filter_map(Json::as_str) {
        run(s, &step(&format!("merge {m}")), "home.mergeCells", json!({"range": format!("{q}{m}")}))?;
    }
    // Table, or a plain AutoFilter.
    let table = spec.get("table").filter(|t| !matches!(t, Json::Bool(false) | Json::Null));
    if let (Some(t), Some(b)) = (table, block) {
        let mut tp = json!({"range": format!("{q}{}", b.a1()), "header": header.is_some()});
        match t {
            Json::String(style) => tp["style"] = json!(style),
            Json::Object(o) => {
                if let Some(st) = o.get("style") {
                    tp["style"] = st.clone();
                }
                if let Some(n) = o.get("name") {
                    tp["name"] = n.clone();
                }
            }
            _ => {}
        }
        run(s, &step("table"), "insert.table", tp)?;
    } else if spec.get("autofilter").and_then(Json::as_bool) == Some(true)
        && let Some(b) = block
    {
        run(s, &step("autofilter"), "selection.set", json!({"range": format!("{q}{}", b.a1())}))?;
        run(s, &step("autofilter"), "data.filter", json!({"range": format!("{q}{}", b.a1())}))?;
    }
    if spec.get("freeze_header").and_then(Json::as_bool) == Some(true) {
        let at = CellRef::new(first_data.max(start.row + 1), 0);
        run(s, &step("freeze header"), "view.freezePanes", json!({"cell": at.a1()}))?;
    }
    for cf in spec.get("conditional_formats").and_then(Json::as_array).into_iter().flatten() {
        let mut p = cf.clone();
        if let Some(r) = cf.get("range").and_then(Json::as_str) {
            p["range"] = json!(format!("{q}{r}"));
        }
        run(s, &step("conditional format"), "home.conditionalFormat", p)?;
    }
    for v in spec.get("validations").and_then(Json::as_array).into_iter().flatten() {
        let mut p = v.clone();
        if let Some(r) = v.get("range").and_then(Json::as_str) {
            p["range"] = json!(format!("{q}{r}"));
        }
        run(s, &step("validation"), "data.validation", p)?;
    }
    if spec.get("rtl").and_then(Json::as_bool) == Some(true) {
        run(s, &step("right-to-left"), "view.rightToLeft", json!({"on": true}))?;
    }
    for (k, c) in spec.get("charts").and_then(Json::as_array).into_iter().flatten().enumerate() {
        add_chart(s, c, name, block, header.is_some()).map_err(|e| fail(&step(&format!("chart {}", k + 1)), e))?;
        *charts += 1;
    }
    run(s, &step("selection"), "selection.set", json!({"range": format!("{q}{}", start.a1())}))?;
    Ok(json!({"name": name, "range": block.map(|b| b.a1()), "rows": rows.len(), "columns": width}))
}

fn chart_type(t: &str, stacked: bool) -> Option<ChartKind> {
    Some(match (t.to_ascii_lowercase().as_str(), stacked) {
        ("bar", false) => ChartKind::BarClustered,
        ("bar", true) => ChartKind::BarStacked,
        ("column" | "col" | "vbar", false) => ChartKind::ColumnClustered,
        ("column" | "col" | "vbar", true) => ChartKind::ColumnStacked,
        ("line", false) => ChartKind::Line,
        ("line", true) => ChartKind::LineStacked,
        ("linemarkers" | "line_markers", _) => ChartKind::LineMarkers,
        ("pie", _) => ChartKind::Pie,
        ("doughnut" | "donut", _) => ChartKind::Doughnut,
        ("area", false) => ChartKind::Area,
        ("area", true) => ChartKind::AreaStacked,
        ("scatter" | "xy", _) => ChartKind::Scatter,
        ("scatterlines" | "scatter_lines", _) => ChartKind::ScatterLines,
        ("combo", _) => ChartKind::Combo,
        ("radar", _) => ChartKind::Radar,
        _ => return None,
    })
}

/// A reference for a series part: `B2:B9` on this sheet becomes `'Data'!$B$2:$B$9`; a reference
/// with its own sheet is kept.
fn abs_ref(r: &str, sheet: &str) -> Result<String> {
    let (sh, body) = split_sheet(r);
    let range = RangeRef::parse(body.trim()).ok_or_else(|| bad("chart", format!("not a range: `{r}`")))?;
    let sh = sh.unwrap_or_else(|| sheet.to_string());
    let a = |c: CellRef| format!("${}${}", col_to_letters(c.col), c.row + 1);
    let body = if range.is_single() { a(range.start) } else { format!("{}:{}", a(range.start), a(range.end)) };
    Ok(format!("{}{body}", q(&sh)))
}

/// The platform's chart spec (`kind: col|line|scatter|combo`, `valRange` / `catRange` / `xRange`,
/// `lineSeries`, `hideLegend`, a cell-box `anchor`) read as this module's.
fn platform_chart(s: &Session, c: &Json, sheet: &str) -> Json {
    let mut c = c.clone();
    if c.get("type").is_none()
        && let Some(k) = c.get("kind").cloned()
    {
        c["type"] = match k.as_str() {
            Some("col") => json!("column"),
            _ => k,
        };
    }
    let conv = |sp: &Json, line: bool| -> Json {
        let mut o = sp.clone();
        if o.get("values").is_none()
            && let Some(v) = sp.get("valRange")
        {
            o["values"] = v.clone();
        }
        if o.get("categories").is_none()
            && let Some(v) = sp.get("xRange").or(sp.get("catRange"))
        {
            o["categories"] = v.clone();
        }
        if line && o.get("type").is_none() {
            o["type"] = json!("line");
        }
        o
    };
    if let Some(list) = c.get("series").and_then(Json::as_array).cloned() {
        let mut all: Vec<Json> = list.iter().map(|x| conv(x, false)).collect();
        if let Some(lines) = c.get("lineSeries").and_then(Json::as_array) {
            if c["type"] == "combo" {
                for x in all.iter_mut() {
                    if x.get("type").is_none() {
                        x["type"] = json!("column");
                    }
                }
            }
            all.extend(lines.iter().map(|x| conv(x, true)));
        }
        c["series"] = Json::Array(all);
    }
    if c.get("legend").is_none() && c.get("hideLegend").and_then(Json::as_bool) == Some(true) {
        c["legend"] = json!("none");
    }
    // anchor {fromCol, fromRow, toCol, toRow} (0-based cells) → at + size.
    if let Some(a) = c.get("anchor").filter(|a| a.is_object()).cloned() {
        let num = |k: &str| a.get(k).and_then(Json::as_u64).map(|v| v.min(u64::from(gridcraft_core::MAX_COLS.max(gridcraft_core::MAX_ROWS))) as u32);
        if let (Some(fc), Some(fr)) = (num("fromCol"), num("fromRow")) {
            c["at"] = json!(CellRef::new(fr.min(gridcraft_core::MAX_ROWS - 1), fc.min(gridcraft_core::MAX_COLS - 1)).a1());
            if let (Some(tc), Some(tr), Some(sh)) =
                (num("toCol"), num("toRow"), s.doc().ok().and_then(|d| d.wb.sheet_index(sheet).and_then(|i| d.wb.sheet(i).cloned())))
            {
                let w = sh.col_left(tc.min(gridcraft_core::MAX_COLS - 1)) - sh.col_left(fc.min(gridcraft_core::MAX_COLS - 1));
                let h = sh.row_top(tr.min(gridcraft_core::MAX_ROWS - 1)) - sh.row_top(fr.min(gridcraft_core::MAX_ROWS - 1));
                if w > 20.0 && h > 20.0 {
                    c["width"] = json!(w);
                    c["height"] = json!(h);
                }
            }
        }
    }
    c
}

fn add_chart(s: &mut Session, c: &Json, sheet: &str, block: Option<RangeRef>, has_header: bool) -> Result<()> {
    let c = &platform_chart(s, c, sheet);
    let t = str_param(c, "type").unwrap_or("column");
    let stacked = bool_param(c, "stacked").unwrap_or(false);
    let kind = chart_type(t, stacked)
        .ok_or_else(|| bad("chart", format!("unknown chart type `{t}` (bar, column, line, pie, doughnut, area, scatter, combo)")))?;
    let explicit = c.get("series").and_then(Json::as_array).filter(|a| !a.is_empty());
    let mut chart = match explicit {
        None => {
            // From a data range (default: the sheet's whole block), like Insert › Chart.
            let data = match str_param(c, "data").or(str_param(c, "range")) {
                Some(d) => d.to_string(),
                None => block.map(|b| b.a1()).ok_or_else(|| bad("chart", "give `data` (a range) or `series`"))?,
            };
            let mut p = json!({"range": format!("{}{}", q(sheet), split_sheet(&data).1), "type": "column"});
            if let Some(at) = str_param(c, "at") {
                p["at"] = json!(at);
            }
            let (_, mut chart) = super::insert::build_chart(s, &p)?;
            chart.kind = kind;
            chart
        }
        Some(list) => {
            let mut series = Vec::with_capacity(list.len());
            for (i, sp) in list.iter().enumerate() {
                let values = str_param(sp, "values").ok_or_else(|| bad("chart", format!("series {} needs `values` (a range)", i + 1)))?;
                let name = match sp.get("name") {
                    Some(Json::String(n)) if RangeRef::parse(split_sheet(n).1).is_some() && !n.contains(' ') => Some(abs_ref(n, sheet)?),
                    Some(Json::String(n)) => Some(format!("\"{}\"", n.replace('"', "\"\""))),
                    _ => None,
                };
                let kind = str_param(sp, "type").and_then(|t| chart_type(t, false));
                series.push(Series {
                    name,
                    categories: str_param(sp, "categories").or(str_param(c, "categories")).map(|r| abs_ref(r, sheet)).transpose()?,
                    values: abs_ref(values, sheet)?,
                    bubble_sizes: None,
                    color: str_param(sp, "color").and_then(gridcraft_model::Color::from_hex),
                    secondary: bool_param(sp, "secondary").unwrap_or(false),
                    kind,
                    smooth: bool_param(sp, "smooth").unwrap_or(false),
                    trendlines: vec![],
                });
            }
            let d = s.doc()?;
            let at = match str_param(c, "at") {
                Some(a) => CellRef::parse(a).ok_or_else(|| bad("chart", format!("`at` is not a cell: `{a}`")))?,
                None => block.map(|b| CellRef::new(b.start.row, b.end.col + 2)).unwrap_or_default(),
            };
            Chart {
                id: d.wb.next_object_id(),
                kind,
                anchor: Anchor { cell: at, dx: 0.0, dy: 0.0, width: 480.0, height: 288.0, mode: Default::default() },
                title: None,
                series,
                legend: LegendPos::Bottom,
                data_labels: false,
                gridlines: !matches!(kind, ChartKind::Pie | ChartKind::Doughnut),
                style: 1,
                x_title: None,
                y_title: None,
                source: None,
                by_rows: false,
                pivot: None,
            }
        }
    };
    let _ = has_header;
    // Combo: the first series as columns, the rest as lines unless they say otherwise.
    if kind == ChartKind::Combo {
        for (i, se) in chart.series.iter_mut().enumerate() {
            if se.kind.is_none() {
                se.kind = Some(if i == 0 { ChartKind::ColumnClustered } else { ChartKind::Line });
            }
        }
    }
    if let Some(t) = c.get("title") {
        chart.title = t.as_str().map(str::to_string);
    }
    chart.x_title = str_param(c, "x_title").or(str_param(c, "xTitle")).map(str::to_string);
    chart.y_title = str_param(c, "y_title").or(str_param(c, "yTitle")).map(str::to_string);
    if let Some(l) = str_param(c, "legend") {
        chart.legend = match l {
            "none" => LegendPos::None,
            "top" => LegendPos::Top,
            "left" => LegendPos::Left,
            "right" => LegendPos::Right,
            _ => LegendPos::Bottom,
        };
    } else if matches!(kind, ChartKind::Pie | ChartKind::Doughnut) {
        chart.legend = LegendPos::Right;
    }
    chart.data_labels = bool_param(c, "data_labels").or(bool_param(c, "dataLabels")).unwrap_or(chart.data_labels);
    if let Some(w) = f64_param(c, "width").filter(|w| w.is_finite()) {
        chart.anchor.width = w.clamp(20.0, 10_000.0) as f32;
    }
    if let Some(h) = f64_param(c, "height").filter(|h| h.is_finite()) {
        chart.anchor.height = h.clamp(20.0, 10_000.0) as f32;
    }
    if let Some(colors) = c.get("colors").and_then(Json::as_array) {
        for (se, col) in chart.series.iter_mut().zip(colors.iter().filter_map(Json::as_str)) {
            se.color = gridcraft_model::Color::from_hex(col).or(se.color);
        }
    }
    let si = s.doc()?.wb.sheet_index(sheet).ok_or_else(|| bad("chart", format!("no sheet `{sheet}`")))?;
    edit(s, |cx| {
        cx.sheet_mut(si)?.charts.push(chart);
        Ok(Json::Null)
    })?;
    Ok(())
}

/// `formulas.recalculate`: everything recomputed, the errors listed.
fn recalculate(s: &mut Session, _: &Json) -> Result<Json> {
    let was_clean = !s.doc()?.is_dirty();
    let r = s.execute("formulas.calculateNow", json!({}))?;
    // Recomputing changes no input: a saved workbook stays saved.
    if was_clean && let Ok(d) = s.doc_mut() {
        d.saved = d.wb.clone();
    }
    let d = s.doc()?;
    let mut formulas = 0u64;
    let mut errors = Vec::new();
    let mut error_count = 0u64;
    for sh in &d.wb.sheets {
        for (c, cell) in sh.cells.iter() {
            if cell.formula.is_none() {
                continue;
            }
            formulas += 1;
            if let gridcraft_core::Value::Error(e) = sh.value(c) {
                error_count += 1;
                if errors.len() < 100 {
                    errors.push(json!({"cell": format!("{}{}", q(&sh.name), c.a1()), "error": e.as_str()}));
                }
            }
        }
    }
    Ok(json!({"formulas": formulas, "cells": r["cells"], "errors": errors, "errorCount": error_count}))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s() -> Session {
        Session::new()
    }

    #[test]
    fn builds_sheets_formats_tables_and_charts() {
        let mut s = s();
        let r = s
            .execute(
                "file.build",
                json!({
                    "title": "Q3",
                    "sheets": [
                        {
                            "name": "Sales",
                            "header": ["Region", "Units", "Price", "Revenue"],
                            "rows": [["North", 10, 2.5, "=B2*C2"], ["South", 20, 3, "=B3*C3"], ["Total", "=SUM(B2:B3)", null, "=SUM(D2:D3)"]],
                            "columns": [{"width": 18}, {"format": "#,##0"}, {"format": "0.00"}, {"format": "#,##0.00"}],
                            "freeze_header": true,
                            "table": "TableStyleMedium2",
                            "charts": [
                                {"type": "bar", "title": "Units", "series": [{"name": "Units", "values": "B2:B3", "categories": "A2:A3"}], "at": "F2"},
                                {"type": "combo", "title": "Units and revenue", "series": [{"name": "Units", "values": "B2:B3", "categories": "A2:A3"}, {"name": "Revenue", "values": "D2:D3", "secondary": true}]},
                                {"type": "pie", "data": "A1:B3"},
                                {"type": "line", "data": "A1:B3", "x_title": "Region", "y_title": "Units"},
                                {"type": "scatter", "series": [{"values": "C2:C3", "categories": "B2:B3"}]}
                            ]
                        },
                        {"name": "Notes", "rows": [["مرحبا"]], "rtl": true, "autofilter": false}
                    ]
                }),
            )
            .unwrap();
        assert_eq!(r["charts"], 5, "{r}");
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        assert_eq!(sh.name, "Sales");
        assert_eq!(sh.value(CellRef::new(2, 3)).as_f64(), Some(60.0), "formulas computed");
        assert_eq!(sh.value(CellRef::new(3, 3)).as_f64(), Some(85.0));
        assert_eq!(sh.tables.len(), 1);
        assert_eq!(sh.charts.len(), 5);
        assert_eq!(sh.charts[0].kind, ChartKind::BarClustered);
        assert!(sh.charts[1].series[1].secondary);
        assert_eq!(sh.charts[1].series[1].kind, Some(ChartKind::Line));
        assert_eq!(sh.charts[0].series[0].values, "Sales!$B$2:$B$3");
        assert!(d.wb.sheet(1).unwrap().right_to_left);
        assert_eq!(d.wb.props.title, "Q3");
        assert!(d.undo.is_empty());
        // The saved file carries cached values Excel shows without recalculating.
        let bytes = crate::io::save_bytes(&d.wb, "b.xlsx").unwrap();
        let (back, _) = gridcraft_xlsx::read_xlsx(&bytes).unwrap();
        assert_eq!(back.sheet(0).unwrap().value(CellRef::new(3, 3)).as_f64(), Some(85.0));
        assert_eq!(back.sheet(0).unwrap().charts.len(), 5);
    }

    #[test]
    fn bad_specs_fail_cleanly_and_leave_nothing_behind() {
        let mut s = s();
        s.new_workbook();
        assert!(s.execute("file.build", json!({})).is_err());
        let e = s.execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1]], "charts": [{"type": "nope"}]}]})).unwrap_err();
        assert!(e.to_string().contains("chart 1"), "{e}");
        assert_eq!(s.documents().len(), 1, "the half-built workbook is closed");
        assert!(s.execute("file.build", json!({"sheets": [{"name": "A"}, {"name": "a"}]})).is_err());
        assert!(s.execute("file.build", json!({"sheets": [{"name": "A", "rows": [1, 2]}]})).is_err());
    }

    /// The platform's chart specs (`kind`, `valRange`, `lineSeries`, cell-box anchors) build the
    /// same native charts its injector made.
    #[test]
    fn platform_chart_specs_are_understood() {
        let mut s = s();
        s.execute(
            "file.build",
            json!({
                "sheets": [{"name": "Cash Flow", "header": ["Month", "In", "Cum"], "rows": [["Jan", 5, 5], ["Feb", 7, "=C2+B3"], ["Mar", 4, "=C3+B4"]]}],
                "charts": [
                    {"sheet": "Cash Flow", "kind": "combo", "title": "S-curve", "xTitle": "Month", "yTitle": "SAR",
                     "anchor": {"fromCol": 5, "fromRow": 1, "toCol": 12, "toRow": 18},
                     "series": [{"name": "Monthly", "catRange": "$A$2:$A$4", "catText": true, "valRange": "$B$2:$B$4", "color": "3B82F6"}],
                     "lineSeries": [{"name": "Cumulative", "catRange": "$A$2:$A$4", "valRange": "$C$2:$C$4", "color": "E11D48"}],
                     "hideLegend": true},
                    {"sheet": "Cash Flow", "kind": "scatter", "title": "XY", "anchor": {"fromCol": 5, "fromRow": 20, "toCol": 12, "toRow": 36},
                     "series": [{"name": "Pts", "xRange": "$B$2:$B$4", "valRange": "$C$2:$C$4"}]},
                    {"sheet": "Cash Flow", "kind": "col", "title": "Bars", "series": [{"name": "In", "catRange": "$A$2:$A$4", "valRange": "$B$2:$B$4"}]}
                ]
            }),
        )
        .unwrap();
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        assert_eq!(sh.charts.len(), 3);
        let combo = &sh.charts[0];
        assert_eq!(combo.kind, ChartKind::Combo);
        assert_eq!(combo.series.len(), 2);
        assert_eq!(combo.series[0].kind, Some(ChartKind::ColumnClustered));
        assert_eq!(combo.series[1].kind, Some(ChartKind::Line));
        assert!(!combo.series[1].secondary, "the platform's combo shares one value axis");
        assert_eq!(combo.series[0].values, "'Cash Flow'!$B$2:$B$4");
        assert_eq!(combo.series[0].categories.as_deref(), Some("'Cash Flow'!$A$2:$A$4"));
        assert_eq!(combo.series[0].color, gridcraft_model::Color::from_hex("3B82F6"));
        assert_eq!(combo.legend, LegendPos::None);
        assert_eq!(combo.anchor.cell, CellRef::new(1, 5));
        assert!(combo.anchor.width > 300.0, "{:?}", combo.anchor);
        assert_eq!(combo.x_title.as_deref(), Some("Month"));
        assert_eq!(sh.charts[1].kind, ChartKind::Scatter);
        assert_eq!(sh.charts[1].series[0].categories.as_deref(), Some("'Cash Flow'!$B$2:$B$4"), "scatter X values");
        assert_eq!(sh.charts[2].kind, ChartKind::ColumnClustered);
        assert!(s.execute("file.build", json!({"sheets": [{"name": "A"}], "charts": [{"sheet": "B", "kind": "col", "series": []}]})).is_err());
    }

    /// WHIQ: table slicers are written (slicer, cache, frame, name) and read back.
    #[test]
    fn table_slicers_round_trip_through_xlsx() {
        let mut s = s();
        s.new_workbook();
        s.execute("range.setValues", json!({"range": "A1", "values": [["Region", "Units"], ["North", 1], ["South", 2], ["North", 3]]})).unwrap();
        s.execute("insert.table", json!({"range": "A1:B4", "name": "Sales"})).unwrap();
        s.execute("insert.slicer", json!({"table": "Sales", "column": "Region", "at": "E2", "columns": 2})).unwrap();
        s.execute("slicer.toggle", json!({"value": "South"})).unwrap();
        let d = s.doc().unwrap();
        let before = d.wb.sheet(0).unwrap().slicers.clone();
        assert_eq!(before.len(), 1);
        let bytes = crate::io::save_bytes(&d.wb, "s.xlsx").unwrap();
        let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let names: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
        assert!(
            names.contains(&"xl/slicers/slicer1.xml".to_string()) && names.contains(&"xl/slicerCaches/slicerCache1.xml".to_string()),
            "{names:?}"
        );
        let mut wbx = String::new();
        std::io::Read::read_to_string(&mut z.by_name("xl/workbook.xml").unwrap(), &mut wbx).unwrap();
        assert!(wbx.contains("<definedName name=\"Slicer_Region\">#N/A</definedName>") && wbx.contains("x15:slicerCaches"), "{wbx}");
        let (back, report) = crate::io::open_bytes("s.xlsx", &bytes).unwrap();
        assert!(report.iter().all(|w| !w.contains("slicer")), "{report:?}");
        let after = &back.sheet(0).unwrap().slicers;
        assert_eq!(after.len(), 1, "{after:?}");
        assert_eq!((after[0].table.as_str(), after[0].column.as_str(), after[0].columns), ("Sales", "Region", 2));
        assert_eq!(after[0].anchor.cell, before[0].anchor.cell);
        assert!(back.names.iter().all(|n| !n.name.starts_with("Slicer_")), "the cache name is the writer's");
        // Saved again: still one slicer, one name.
        let again = crate::io::save_bytes(&back, "s.xlsx").unwrap();
        let (back2, _) = crate::io::open_bytes("s.xlsx", &again).unwrap();
        assert_eq!(back2.sheet(0).unwrap().slicers.len(), 1);
    }

    #[test]
    fn recalculate_lists_errors() {
        let mut s = s();
        s.new_workbook();
        s.execute("cell.set", json!({"cell": "A1", "input": "=1/0"})).unwrap();
        s.execute("cell.set", json!({"cell": "A2", "input": "=2+2"})).unwrap();
        let r = s.execute("formulas.recalculate", json!({})).unwrap();
        assert_eq!(r["formulas"], 2);
        s.doc_mut().unwrap().saved = s.doc().unwrap().wb.clone();
        s.execute("formulas.recalculate", json!({})).unwrap();
        assert!(!s.doc().unwrap().is_dirty(), "recalculating a saved workbook leaves it saved");
        assert_eq!(r["errorCount"], 1);
        assert_eq!(r["errors"][0]["error"], "#DIV/0!");
    }
}
