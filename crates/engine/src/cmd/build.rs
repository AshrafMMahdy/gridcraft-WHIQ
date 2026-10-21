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
            "{sheets: [{name, header?: [..], rows: [[..]], start?: \"A1\", columns?: [{header?, width?, format?}], number_formats?: {\"B\": \"#,##0\", \"C2:D9\": \"0%\"}, column_widths?: {\"A\": 30} | [30, 12] | \"auto\", autofit?: bool (default true: columns without a width fit their contents), freeze_header?: bool, autofilter?: bool, table?: bool | \"TableStyleMedium2\" | {name?, style?}, header_style?: {bold?, fill?, font_color?}, merges?: [\"A1:D1\"], rtl?: bool, charts?: [{type: bar|column|line|pie|doughnut|area|scatter|combo, title?, data?: \"A1:C9\", series?: [{name?, values, categories?, type?, secondary?, color?}], x_title?, y_title?, legend?, data_labels?, stacked?, at?, width?, height?}], conditional_formats?: [{range, rule}], validations?: [{range, type, …}], styles?: [{range: \"A1:D1\" | \"A1:D1,F3\", bold?, italic?, underline?, strike?, font?, font_size?, font_color?, fill?: \"#RRGGBB\" | \"none\" | {color, pattern?}, align?: left|center|right|justify, v_align?: top|middle|bottom, wrap?, indent?, rotation?, number_format?, borders?: preset | {preset, style?, color?} | {top?, bottom?, left?, right?: \"thin\" | {style, color}}}], row_heights?: [{row: 3 | \"3:5\", height: 20 (points)}] | {\"3\": 20}, freeze?: \"C2\" | {rows, columns}, literal_strings?: bool (strings stay text; ISO dates become dates), iso_dates?: bool (default true)}], literal_strings?, iso_dates? (defaults for every sheet), title?, author?, company?, path?} — a cell is a value or an object {text | formula | date | number | bool | value (+ type: text|number|date|formula|boolean|auto), style?: {…look}, or the look keys beside the value} → {sheets, charts, path?}",
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
/// Largest workbook a spec may describe (cells written) at the smallest memory budget. WHIQ: the
/// working limit is one cell per 64 bytes of the memory budget when that is more (a spec arrives
/// whole in one call; larger workbooks are built in parts with write_range).
const MAX_CELLS_FLOOR: u64 = 5_000_000;

fn max_cells() -> u64 {
    MAX_CELLS_FLOOR.max(gridcraft_core::memory::budget() / 64)
}

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
    if total > max_cells() {
        return Err(bad(
            CMD,
            format!(
                "the spec holds {total} cells, more than one call builds within this process's memory budget ({}): build it in parts (write_range more rows after)",
                max_cells()
            ),
        ));
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
        out_sheets.push(fill_sheet(s, spec, p, name, &mut charts)?);
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

fn fill_sheet(s: &mut Session, spec: &Json, top: &Json, name: &str, charts: &mut usize) -> Result<Json> {
    let step = |what: &str| format!("sheet {name}, {what}");
    // WHIQ: how string cells are read (typed input, or literal text with ISO dates as dates).
    let flag = |k: &str| spec.get(k).or_else(|| top.get(k)).and_then(Json::as_bool);
    let mode = CellMode { literal: flag("literal_strings").unwrap_or(false), iso_dates: flag("iso_dates").unwrap_or(true) };
    let sys = s.doc()?.wb.date_system;
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
    // WHIQ: cell objects ({text}, {formula}, {date}, {value, type}, with a look of their own) and
    // literal strings become the input setValues takes; their looks are applied after.
    let mut date_looks: Vec<(Vec<RangeRef>, looks::Look)> = Vec::new();
    let mut cell_looks: Vec<(Vec<RangeRef>, looks::Look)> = Vec::new();
    for (ri, row) in values.iter_mut().enumerate() {
        let Some(cells) = row.as_array_mut() else { continue };
        for (ci, v) in cells.iter_mut().enumerate() {
            if !matches!(v, Json::String(_) | Json::Object(_)) {
                continue;
            }
            let at = CellRef::new(start.row + ri as u32, start.col + ci as u32);
            let (input, date_fmt, look) = cell_input(v, &mode, sys).map_err(|e| bad(CMD, step(&format!("cell {}: {e}", at.a1()))))?;
            *v = input;
            if let Some(f) = date_fmt {
                date_looks.push((vec![RangeRef::new(at, at)], looks::Look::number_format(f)));
            }
            if let Some(l) = look {
                cell_looks.push((vec![RangeRef::new(at, at)], l));
            }
        }
    }
    let q = q(name);
    let first_data = start.row + u32::from(header.is_some());
    let last_row = start.row + (values.len() as u32).saturating_sub(1);
    let block = (width > 0 && !values.is_empty()).then(|| RangeRef::new(start, CellRef::new(last_row, start.col + width.saturating_sub(1))));
    if !values.is_empty() {
        run(s, &step("values"), "range.setValues", json!({"range": format!("{q}{}", start.a1()), "values": values}))?;
    }
    // Dates written as dates show as dates (before the column formats, which win).
    looks::apply(s, &date_looks, max_cells(), &step)?;
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
    // WHIQ: cell looks by range (`styles`), then each cell object's own (a cell's wins).
    let mut range_looks: Vec<(Vec<RangeRef>, looks::Look)> = Vec::new();
    for (k, st) in spec.get("styles").and_then(Json::as_array).into_iter().flatten().enumerate() {
        let o = st
            .as_object()
            .ok_or_else(|| bad(CMD, step(&format!("style {} is not an object like {{\"range\": \"A1:D1\", \"bold\": true}}", k + 1))))?;
        let range = o.get("range").or(o.get("ranges")).and_then(Json::as_str).unwrap_or("");
        let rs = looks::ranges(range).map_err(|e| bad(CMD, step(&format!("style {}: {e}", k + 1))))?;
        if let Some(l) = looks::Look::parse(o, &["range", "ranges"]).map_err(|e| bad(CMD, step(&format!("style {}: {e}", k + 1))))? {
            range_looks.push((rs, l));
        }
    }
    range_looks.append(&mut cell_looks);
    looks::apply(s, &range_looks, max_cells(), &step)?;
    row_heights(s, spec, &step)?;
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
    if let Some(f) = spec.get("freeze").filter(|f| !f.is_null()) {
        // WHIQ: freeze the rows above and the columns left of a cell ("C2"), or {rows, columns}.
        let at = match f {
            Json::String(c) => CellRef::parse(c.trim()).ok_or_else(|| bad(CMD, step(&format!("`freeze` is a cell like \"C2\" (got `{c}`)"))))?,
            Json::Object(o) => {
                let n = |k: &str| o.get(k).and_then(Json::as_u64).unwrap_or(0).min(u64::from(gridcraft_core::MAX_ROWS - 1)) as u32;
                CellRef::new(n("rows"), n("columns").max(n("cols")).min(gridcraft_core::MAX_COLS - 1))
            }
            _ => return Err(bad(CMD, step("`freeze` is a cell like \"C2\" or {rows, columns}"))),
        };
        run(s, &step("freeze panes"), "view.freezePanes", json!({"cell": at.a1()}))?;
    } else if spec.get("freeze_header").and_then(Json::as_bool) == Some(true) {
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

/// WHIQ: how a sheet's string cells are read.
struct CellMode {
    /// `literal_strings: true`: a string is stored as the text it is ("2.10", "1,234", "=x", "+5"),
    /// never parsed as a number, formula or date…
    literal: bool,
    /// … except an ISO date or date-time, which is a date unless `iso_dates: false`.
    iso_dates: bool,
}

/// The keys a cell object holds for its value (the others are its look).
const VALUE_KEYS: &[&str] = &["text", "formula", "date", "number", "value", "type", "bool", "boolean", "style"];

/// An ISO date (`2026-04-03`) or date-time (`2026-04-03T08:30`, `…T08:30:15`, a space for the
/// `T`, an optional `.sss` / `Z`) as a serial number and the format it shows with.
fn iso_date(t: &str, sys: gridcraft_core::DateSystem) -> Option<(f64, &'static str)> {
    let t = t.trim();
    let b = t.as_bytes();
    let digits = |r: std::ops::Range<usize>| -> Option<i64> {
        let part = t.get(r)?;
        (!part.is_empty() && part.bytes().all(|c| c.is_ascii_digit())).then(|| part.parse().ok()).flatten()
    };
    if b.len() < 10 || b.get(4) != Some(&b'-') || b.get(7) != Some(&b'-') {
        return None;
    }
    let (y, m, d) = (digits(0..4)?, digits(5..7)?, digits(8..10)?);
    let leap = (y % 4 == 0 && y % 100 != 0) || y % 400 == 0;
    let days = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    if !(1..=days).contains(&d) {
        return None;
    }
    let day = gridcraft_core::date::serial_from_ymd(sys, y, m, d)?;
    if b.len() == 10 {
        return Some((day, "yyyy-mm-dd"));
    }
    if !matches!(b.get(10), Some(b'T' | b't' | b' ')) || b.len() < 16 || b.get(13) != Some(&b':') {
        return None;
    }
    let (h, mi) = (digits(11..13)?, digits(14..16)?);
    let mut rest = t.get(16..)?;
    let mut sec = 0.0;
    let mut with_secs = false;
    if let Some(r) = rest.strip_prefix(':') {
        let end = r.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(r.len());
        sec = r.get(..end)?.parse::<f64>().ok().filter(|s| (0.0..60.0).contains(s))?;
        with_secs = true;
        rest = r.get(end..)?;
    }
    if !(rest.is_empty() || rest.eq_ignore_ascii_case("z")) || h > 23 || mi > 59 {
        return None;
    }
    let f = gridcraft_core::date::time_fraction(h as f64, mi as f64, sec);
    Some((day + f, if with_secs { "yyyy-mm-dd hh:mm:ss" } else { "yyyy-mm-dd hh:mm" }))
}

/// A literal text cell as typed input: the leading apostrophe keeps it text (it is not stored).
fn text_input(t: &str) -> Json {
    if t.is_empty() { Json::Null } else { json!(format!("'{t}")) }
}

/// One spec cell as setValues input, the date format it needs and its own look.
fn cell_input(
    v: &Json,
    mode: &CellMode,
    sys: gridcraft_core::DateSystem,
) -> std::result::Result<(Json, Option<&'static str>, Option<looks::Look>), String> {
    let date = |t: &str| -> Option<(Json, Option<&'static str>, Option<looks::Look>)> { iso_date(t, sys).map(|(n, f)| (json!(n), Some(f), None)) };
    match v {
        Json::String(t) if mode.literal => Ok(match date(t).filter(|_| mode.iso_dates) {
            Some(d) => d,
            None => (text_input(t), None, None),
        }),
        Json::Object(o) => {
            let scalar = |x: &Json| match x {
                Json::String(t) => t.clone(),
                Json::Null => String::new(),
                other => other.to_string(),
            };
            let kind = o.get("type").and_then(Json::as_str).map(str::to_ascii_lowercase);
            let (input, fmt) = if let Some(t) = o.get("text") {
                (text_input(&scalar(t)), None)
            } else if let Some(f) = o.get("formula") {
                let f = scalar(f);
                let f = f.trim();
                (
                    if f.is_empty() {
                        Json::Null
                    } else if f.starts_with('=') {
                        json!(f)
                    } else {
                        json!(format!("={f}"))
                    },
                    None,
                )
            } else if let Some(d) = o.get("date") {
                match d {
                    Json::Number(_) => (d.clone(), Some("yyyy-mm-dd")),
                    _ => {
                        let t = scalar(d);
                        let (n, f) = iso_date(&t, sys)
                            .ok_or_else(|| format!("`date` is an ISO date like \"2026-04-03\" or \"2026-04-03T08:30\" (got `{t}`)"))?;
                        (json!(n), Some(f))
                    }
                }
            } else if let Some(n) = o.get("number") {
                let x = n.as_f64().or_else(|| n.as_str().and_then(|t| t.trim().parse().ok())).filter(|x: &f64| x.is_finite());
                (json!(x.ok_or_else(|| format!("`number` is a number (got {n})"))?), None)
            } else if let Some(b) = o.get("bool").or(o.get("boolean")) {
                (json!(b.as_bool().ok_or_else(|| format!("`bool` is true or false (got {b})"))?), None)
            } else {
                let value = o.get("value").cloned().unwrap_or(Json::Null);
                match kind.as_deref() {
                    None | Some("auto") => match &value {
                        Json::String(_) | Json::Object(_) => {
                            let (i, f, _) = cell_input(&value, mode, sys)?;
                            (i, f)
                        }
                        other => (other.clone(), None),
                    },
                    Some("text" | "string") => (text_input(&scalar(&value)), None),
                    Some("formula") => {
                        let f = scalar(&value);
                        (if f.starts_with('=') { json!(f) } else { json!(format!("={f}")) }, None)
                    }
                    Some("number") => {
                        let x = value.as_f64().or_else(|| value.as_str().and_then(|t| t.trim().parse().ok())).filter(|x: &f64| x.is_finite());
                        (json!(x.ok_or_else(|| format!("a number cell needs a number (got {value})"))?), None)
                    }
                    Some("date") => {
                        let t = scalar(&value);
                        match value.as_f64() {
                            Some(n) => (json!(n), Some("yyyy-mm-dd")),
                            None => {
                                let (n, f) =
                                    iso_date(&t, sys).ok_or_else(|| format!("a date cell needs an ISO date like \"2026-04-03\" (got `{t}`)"))?;
                                (json!(n), Some(f))
                            }
                        }
                    }
                    Some("bool" | "boolean") => (json!(as_bool_cell(&value)?), None),
                    Some(other) => return Err(format!("cell `type` `{other}` is text, number, date, formula, boolean or auto")),
                }
            };
            // The look: the keys beside the value, and those of `style`.
            let mut keys: Map<String, Json> =
                o.iter().filter(|(k, _)| !VALUE_KEYS.iter().any(|v| v.eq_ignore_ascii_case(k))).map(|(k, v)| (k.clone(), v.clone())).collect();
            match o.get("style") {
                Some(Json::Object(st)) => keys.extend(st.iter().map(|(k, v)| (k.clone(), v.clone()))),
                None | Some(Json::Null) => {}
                Some(other) => return Err(format!("a cell's `style` is an object like {{\"bold\": true, \"fill\": \"#3B82F6\"}} (got {other})")),
            }
            let look = looks::Look::parse(&keys, &[])?;
            Ok((input, fmt, look))
        }
        other => Ok((other.clone(), None, None)),
    }
}

fn as_bool_cell(v: &Json) -> std::result::Result<bool, String> {
    match v {
        Json::Bool(b) => Ok(*b),
        Json::String(t) if t.eq_ignore_ascii_case("true") => Ok(true),
        Json::String(t) if t.eq_ignore_ascii_case("false") => Ok(false),
        _ => Err(format!("a boolean cell needs true or false (got {v})")),
    }
}

/// WHIQ: `row_heights` in points: `[{row: 3, height: 20}]` (`row` may be "3:5"), or
/// `{"3": 20, "5:7": 14}`.
fn row_heights(s: &mut Session, spec: &Json, step: &dyn Fn(&str) -> String) -> Result<()> {
    let mut items: Vec<(String, f64)> = Vec::new();
    let as_rows = |r: &Json| -> Option<String> {
        match r {
            Json::Number(n) => n.as_u64().filter(|n| (1..=u64::from(gridcraft_core::MAX_ROWS)).contains(n)).map(|n| format!("{n}:{n}")),
            Json::String(t) => {
                let t = t.trim();
                let (a, b) = t.split_once(':').unwrap_or((t, t));
                let (a, b): (u32, u32) = (a.trim().parse().ok()?, b.trim().parse().ok()?);
                (a >= 1 && b >= a && b <= gridcraft_core::MAX_ROWS).then(|| format!("{a}:{b}"))
            }
            _ => None,
        }
    };
    match spec.get("row_heights") {
        None | Some(Json::Null) => return Ok(()),
        Some(Json::Array(list)) => {
            for (k, it) in list.iter().enumerate() {
                let r = it.get("row").or(it.get("rows")).and_then(as_rows);
                let h = it.get("height").and_then(Json::as_f64);
                match (r, h) {
                    (Some(r), Some(h)) => items.push((r, h)),
                    _ => {
                        return Err(bad(
                            CMD,
                            step(&format!("row height {}: give {{\"row\": 3, \"height\": 20}} (points; `row` a number or \"3:5\")", k + 1)),
                        ));
                    }
                }
            }
        }
        Some(Json::Object(m)) => {
            for (k, h) in m {
                let r = as_rows(&json!(k)).ok_or_else(|| bad(CMD, step(&format!("row heights: `{k}` is not a row number or \"3:5\""))))?;
                let h = h.as_f64().ok_or_else(|| bad(CMD, step(&format!("row heights: the height of `{k}` is a number of points"))))?;
                items.push((r, h));
            }
        }
        Some(_) => return Err(bad(CMD, step("`row_heights` is [{row, height}] or {\"3\": 20} (points)"))),
    }
    // One Row Height per distinct height (the command takes pixels on screen: points ÷ 0.75).
    let mut by: Vec<(f64, Vec<String>)> = Vec::new();
    for (r, h) in items {
        if !(0.0..=409.0).contains(&h) || !h.is_finite() {
            return Err(bad(CMD, step(&format!("row height {h} for rows {r}: heights are 0 to 409 points"))));
        }
        match by.iter_mut().find(|(x, _)| *x == h) {
            Some((_, rows)) => rows.push(r),
            None => by.push((h, vec![r])),
        }
    }
    for (h, rows) in by {
        for chunk in rows.chunks(500) {
            run(s, &step("row heights"), "home.rowHeight", json!({"rows": chunk.join(","), "height": h / 0.75}))?;
        }
    }
    Ok(())
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
                    line: None,
                    markers: None,
                    line_width: None,
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
    // WHIQ: scatter series are drawn with straight lines and markers unless the spec says
    // otherwise (`line`, `markers`, `line_width`, `color` on the chart or a series).
    let line_default = matches!(t.to_ascii_lowercase().as_str(), "scatter" | "xy").then_some(true);
    let theme = s.doc()?.wb.theme.clone();
    super::insert::apply_scatter_lines(&mut chart, c, explicit.map_or(&[][..], Vec::as_slice), line_default, &theme)?;
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
        // WHIQ: a scatter is drawn with straight lines (and markers) unless the spec says `line: false`.
        assert_eq!(sh.charts[1].kind, ChartKind::ScatterLines);
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

    /// WHIQ: scatter series are solid straight lines with markers by default (the platform's
    /// Line-of-Balance chart: one production line per trade); `line`, `markers`, `line_width` and
    /// `color` on the chart or on a series say otherwise, and the saved file carries them.
    #[test]
    fn scatter_charts_draw_lines_unless_told_otherwise() {
        let mut s = s();
        s.execute(
            "file.build",
            json!({
                "sheets": [{
                    "name": "LOB",
                    "header": ["Day", "Excavation", "Day2", "Concrete"],
                    "rows": [[0, 1, 5, 1], [10, 20, 15, 20]],
                    "charts": [
                        {"type": "scatter", "title": "LOB", "series": [
                            {"name": "Excavation", "categories": "A2:A3", "values": "B2:B3"},
                            {"name": "Concrete", "categories": "C2:C3", "values": "D2:D3", "color": "#E11D48", "markers": false, "line_width": 3}
                        ]},
                        {"type": "scatter", "line": false, "series": [{"categories": "A2:A3", "values": "B2:B3"}]},
                        {"type": "scatter", "markers": false, "color": "#123456", "lineWidth": 1.5, "series": [{"categories": "A2:A3", "values": "B2:B3"}]},
                        {"type": "line", "line_width": 3, "series": [{"categories": "A2:A3", "values": "B2:B3"}]}
                    ]
                }]
            }),
        )
        .unwrap();
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        let lob = &sh.charts[0];
        assert_eq!(lob.kind, ChartKind::ScatterLines);
        let (a, b) = (&lob.series[0], &lob.series[1]);
        assert_eq!((a.line, a.markers, a.line_width), (Some(true), Some(true), None));
        assert!(a.color.is_some(), "a palette colour, so the file's line and the render agree");
        assert_eq!((b.line, b.markers, b.line_width), (Some(true), Some(false), Some(3.0)));
        assert_eq!(b.color, gridcraft_model::Color::from_hex("E11D48"));
        assert_ne!(a.color, b.color);
        let pts = &sh.charts[1];
        assert_eq!(pts.kind, ChartKind::Scatter, "line: false = markers only");
        assert_eq!((pts.series[0].line, pts.series[0].markers), (Some(false), Some(true)));
        let plain = &sh.charts[2];
        assert_eq!((plain.series[0].line, plain.series[0].markers, plain.series[0].line_width), (Some(true), Some(false), Some(1.5)));
        assert_eq!(plain.series[0].color, gridcraft_model::Color::from_hex("123456"));
        let line = &sh.charts[3];
        assert_eq!(line.kind, ChartKind::Line, "other chart types are left alone");
        assert_eq!((line.series[0].line, line.series[0].markers, line.series[0].line_width, line.series[0].color), (None, None, None, None));
        // The file: one solid, round-capped line per series, markers off where asked.
        let bytes = crate::io::save_bytes(&d.wb, "lob.xlsx").unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let mut xml = String::new();
        std::io::Read::read_to_string(&mut zip.by_name("xl/charts/chart1.xml").unwrap(), &mut xml).unwrap();
        assert!(xml.contains("<c:scatterStyle val=\"lineMarker\"/>"), "{xml}");
        assert_eq!(xml.matches("<a:ln w=\"28575\" cap=\"rnd\"><a:solidFill>").count(), 1, "{xml}");
        assert!(xml.contains("<a:ln w=\"38100\" cap=\"rnd\"><a:solidFill><a:srgbClr val=\"E11D48\"/></a:solidFill>"), "{xml}");
        assert!(!xml.contains("<a:noFill/>"), "{xml}");
        assert_eq!(xml.matches("<c:symbol val=\"none\"/>").count(), 1, "{xml}");
        assert_eq!(xml.matches("<c:symbol val=\"circle\"/>").count(), 1, "{xml}");
        // Read back: the same chart.
        let (back, _) = gridcraft_xlsx::read_xlsx(&bytes).unwrap();
        let bl = &back.sheet(0).unwrap().charts[0];
        assert_eq!(bl.kind, ChartKind::ScatterLines);
        assert_eq!(bl.series[0].markers, Some(true));
        assert_eq!(bl.series[1].markers, Some(false), "a line without markers stays without markers");
        assert_eq!(bl.series[1].line_width, Some(3.0));
        assert_eq!(bl.series[1].color, gridcraft_model::Color::from_hex("E11D48"));
        assert_eq!(back.sheet(0).unwrap().charts[1].kind, ChartKind::Scatter);
        // A bad width says what to give.
        let e = s
            .execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1, 2]], "charts": [{"type": "scatter", "line_width": -1, "series": [{"values": "B1:B1", "categories": "A1:A1"}]}]}]}))
            .unwrap_err();
        assert!(e.to_string().contains("line_width"), "{e}");
    }

    fn style_at(s: &Session, sheet: usize, a1: &str) -> gridcraft_model::Style {
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(sheet).unwrap();
        d.wb.styles.get(sh.style_id(CellRef::parse(a1).unwrap())).clone()
    }

    /// WHIQ: one call builds a formatted workbook — cell looks by range and per cell (a cell's
    /// own fill paints a Gantt bar), borders, row heights in points, frozen columns — and the
    /// saved file carries them.
    #[test]
    fn styles_row_heights_and_freeze_in_one_call() {
        use gridcraft_model::{BorderStyle, Color, HAlign, PatternType, VAlign};
        let mut s = s();
        s.execute(
            "file.build",
            json!({"sheets": [{
                "name": "Gantt",
                "rows": [
                    ["High-Level Timeline"],
                    ["ID", "Activity", "W1", "W2", "W3"],
                    [1, "Excavation", {"value": null, "fill": "#3B82F6"}, {"value": "", "style": {"fill": "#3B82F6"}}, null],
                    [2, "Concrete", null, {"value": null, "fill": "#E11D48", "borders": {"left": "thin", "right": {"style": "medium", "color": "#111827"}}}, null],
                    ["Notes", "A long paragraph that wraps", null, null, null]
                ],
                "merges": ["A1:E1"],
                "styles": [
                    {"range": "A1:E1", "bold": true, "font_size": 12, "fill": "#EAF1FB", "font_color": "#1D4ED8"},
                    {"range": "A2:E2", "bold": true, "fill": "#1D4ED8", "fontColor": "#FFFFFF", "wrap": true, "align": "center", "v_align": "middle", "borders": "all"},
                    {"range": "B5", "italic": true, "wrap": true, "number_format": "@"},
                    {"range": "A3:A4,A5", "align": "right"}
                ],
                "row_heights": [{"row": 1, "height": 20}, {"row": 2, "height": 22}, {"row": "5:5", "height": 60}],
                "column_widths": [6, 38, 3.2, 3.2, 3.2],
                "freeze": "C3"
            }]}),
        )
        .unwrap();
        let title = style_at(&s, 0, "A1");
        assert!(title.font.bold && title.font.size == 12.0 && title.font.color == Color::Rgb(0x1D4ED8));
        assert_eq!(title.fill.fg, Color::Rgb(0xEAF1FB));
        let head = style_at(&s, 0, "C2");
        assert!(head.font.bold && head.align.wrap && head.align.h == HAlign::Center && head.align.v == VAlign::Center);
        assert_eq!((head.fill.pattern, head.fill.fg, head.font.color), (PatternType::Solid, Color::Rgb(0x1D4ED8), Color::Rgb(0xFFFFFF)));
        assert_eq!(head.borders.top.style, BorderStyle::Thin, "the `all` preset");
        for bar in ["C3", "D3"] {
            assert_eq!(style_at(&s, 0, bar).fill.fg, Color::Rgb(0x3B82F6), "{bar}: a real fill, no code to paint");
        }
        let red = style_at(&s, 0, "D4");
        assert_eq!(red.fill.fg, Color::Rgb(0xE11D48));
        assert_eq!(
            (red.borders.left.style, red.borders.right.style, red.borders.right.color),
            (BorderStyle::Thin, BorderStyle::Medium, Color::Rgb(0x111827))
        );
        assert_eq!(style_at(&s, 0, "E3").fill.pattern, PatternType::None, "only the bar cells");
        let note = style_at(&s, 0, "B5");
        assert!(note.font.italic && note.align.wrap && note.num_fmt.as_str() == "@");
        assert_eq!(style_at(&s, 0, "A5").align.h, HAlign::Right);
        let d = s.doc().unwrap();
        let sh = d.wb.sheet(0).unwrap();
        assert_eq!(sh.freeze, Some((2, 2)), "rows above and columns left of C3");
        assert!(sh.value(CellRef::parse("C3").unwrap()).is_empty(), "a bar cell holds no code");
        // The file: fills, fonts, borders, heights in points, the frozen pane.
        let bytes = crate::io::save_bytes(&d.wb, "g.xlsx").unwrap();
        let mut zip = zip::ZipArchive::new(std::io::Cursor::new(bytes.clone())).unwrap();
        let read = |zip: &mut zip::ZipArchive<std::io::Cursor<Vec<u8>>>, n: &str| {
            let mut x = String::new();
            std::io::Read::read_to_string(&mut zip.by_name(n).unwrap(), &mut x).unwrap();
            x
        };
        let sheet = read(&mut zip, "xl/worksheets/sheet1.xml");
        assert!(sheet.contains("ht=\"20\"") && sheet.contains("ht=\"22\"") && sheet.contains("ht=\"60\""), "{sheet}");
        assert!(sheet.contains("xSplit=\"2\"") && sheet.contains("ySplit=\"2\""), "{sheet}");
        let styles = read(&mut zip, "xl/styles.xml");
        for rgb in ["3B82F6", "E11D48", "1D4ED8", "EAF1FB"] {
            assert!(styles.contains(rgb), "{rgb}: {styles}");
        }
        let (back, _) = gridcraft_xlsx::read_xlsx(&bytes).unwrap();
        let b = back.sheet(0).unwrap();
        assert_eq!(back.styles.get(b.style_id(CellRef::parse("C3").unwrap())).fill.fg, Color::Rgb(0x3B82F6));
        assert_eq!(b.freeze, Some((2, 2)));
    }

    /// WHIQ: `literal_strings` keeps the platform's text as text (no leading apostrophe), ISO
    /// dates become dates the engine formats itself; cell objects say a cell's type; without the
    /// flag strings are typed input as before.
    #[test]
    fn literal_strings_and_iso_dates() {
        let mut s = s();
        s.execute(
            "file.build",
            json!({"literal_strings": true, "sheets": [
                {"name": "Lit", "header": ["WBS", "Qty", "Note", "Sign", "Start", "At", "Pct"],
                 "rows": [
                    ["2.10", "1,234", "=x", "+5", "2026-04-03", "2026-04-03T08:30", "50%"],
                    [{"formula": "=1+1"}, {"number": "7"}, {"text": 12}, {"value": "2026-02-30", "type": "text"}, {"date": "2024-02-29"}, {"value": "3", "type": "number"}, "2026-13-01"]
                 ]},
                {"name": "NoDates", "literal_strings": true, "iso_dates": false, "rows": [["2026-04-03"]]},
                {"name": "Typed", "literal_strings": false, "rows": [["2.10", "1,234", "2026-04-03"]]}
            ]}),
        )
        .unwrap();
        let d = s.doc().unwrap();
        let v = |si: usize, a1: &str| d.wb.sheet(si).unwrap().value(CellRef::parse(a1).unwrap());
        let fmt = |si: usize, a1: &str| d.wb.styles.get(d.wb.sheet(si).unwrap().style_id(CellRef::parse(a1).unwrap())).num_fmt.as_str().to_string();
        for (a1, t) in
            [("A2", "2.10"), ("B2", "1,234"), ("C2", "=x"), ("D2", "+5"), ("G2", "50%"), ("C3", "12"), ("D3", "2026-02-30"), ("G3", "2026-13-01")]
        {
            assert_eq!(v(0, a1), gridcraft_core::Value::text(t), "{a1} stays the text it was");
        }
        assert_eq!(v(0, "A1"), gridcraft_core::Value::text("WBS"));
        assert_eq!(v(0, "E2").as_f64(), Some(46115.0), "2026-04-03 is a date");
        assert_eq!(fmt(0, "E2"), "yyyy-mm-dd");
        assert!((v(0, "F2").as_f64().unwrap() - (46115.0 + 8.5 / 24.0)).abs() < 1e-9, "a date-time");
        assert_eq!(fmt(0, "F2"), "yyyy-mm-dd hh:mm");
        assert_eq!(v(0, "A3").as_f64(), Some(2.0), "a formula cell");
        assert_eq!(v(0, "B3").as_f64(), Some(7.0));
        assert_eq!(v(0, "E3").as_f64(), Some(45351.0), "a leap day");
        assert_eq!(v(0, "F3").as_f64(), Some(3.0));
        assert_eq!(v(1, "A1"), gridcraft_core::Value::text("2026-04-03"), "iso_dates: false keeps the text");
        assert_eq!(v(2, "A1").as_f64(), Some(2.1), "typed input as before");
        assert_eq!(v(2, "B1").as_f64(), Some(1234.0));
        assert_eq!(v(2, "C1").as_f64(), Some(46115.0));
        // A bad cell or style says what to give.
        let e = s.execute("file.build", json!({"sheets": [{"name": "A", "rows": [[{"date": "next week"}]]}]})).unwrap_err().to_string();
        assert!(e.contains("cell A1") && e.contains("ISO date"), "{e}");
        let e = s
            .execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1]], "styles": [{"range": "A1", "boldness": true}]}]}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("boldness") && e.contains("font_color"), "{e}");
        let e = s
            .execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1]], "styles": [{"range": "A:A", "bold": true}]}]}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("whole row or column"), "{e}");
        let e = s
            .execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1]], "row_heights": [{"row": 1, "height": 500}]}]}))
            .unwrap_err()
            .to_string();
        assert!(e.contains("0 to 409 points"), "{e}");
        let e = s.execute("file.build", json!({"sheets": [{"name": "A", "rows": [[1]], "freeze": "nope"}]})).unwrap_err().to_string();
        assert!(e.contains("`freeze`"), "{e}");
    }
}
