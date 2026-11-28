//! WHIQ: the workbook as preview HTML (`file.previewHtml`) and a sheet as Markdown tables
//! (`sheet.markdown`), both by windows of any size. The drawing is in [`crate::preview`].

use serde_json::{Value as Json, json};

use super::*;
use crate::preview::{Format, MarkdownOptions, Options};

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(
            query "file.previewHtml",
            "Preview as Web Page",
            [],
            None,
            "{sheet?: name|index (default: the first visible sheet), row_start?: 1-based, row_count?, col_start?: 1-based or letters, col_count?, format?: page|sheet|rows (page), headings?: bool (true), other_sheets?: bool (true), max_cells?: n (100000), title?, path? (write the HTML into this file instead)} → {html (or path), format, sheets: [{index, name, visibility, used_range, rows, columns, cells, frozen, charts, images}], window: {sheet, row_start, row_count, col_start, col_count, range, rows, columns, more_rows, more_columns, next_rows?, next_columns?, truncated}, cells, bytes} — self-contained HTML of a window of the sheet (no row or column limit on the workbook)",
            has_doc,
            preview_html
        ),
        cmd!(
            query "sheet.markdown",
            "Read as Markdown",
            [],
            None,
            "{sheet?: name|index, continue_sheets?: bool (default true without sheet), row_start?: 1-based, row_count? (page size), col_start?, col_count?, formulas?: bool, keep_empty_rows?: bool, max_bytes?: n (262144)} → {markdown, sheets, read, rows_returned, truncated, next?: {sheet, row_start}}",
            has_doc,
            sheet_markdown
        ),
        cmd!(
            query "review.listComments",
            "List Comments",
            [],
            None,
            "{sheet?: name|index (default: every sheet)} → {count, comments: [{sheet, cell, kind: comment|note, author, text, replies: [{author, text}], resolved}]}",
            has_doc,
            list_comments
        ),
    ]
}

/// WHIQ: every comment and note, for an agent reading the workbook.
fn list_comments(s: &mut Session, p: &Json) -> Result<Json> {
    let only = sheet_param(s, p, "review.listComments")?;
    let d = s.doc()?;
    let mut list = vec![];
    for (si, sh) in d.wb.sheets.iter().enumerate() {
        if only.is_some_and(|o| o != si) {
            continue;
        }
        for (c, cm) in &sh.comments {
            list.push(json!({
                "sheet": sh.name,
                "cell": c.a1(),
                "kind": if cm.threaded { "comment" } else { "note" },
                "author": cm.author,
                "text": cm.text,
                "replies": cm.replies.iter().map(|(a, t)| json!({"author": a, "text": t})).collect::<Vec<_>>(),
                "resolved": cm.resolved,
            }));
        }
    }
    Ok(json!({"count": list.len(), "comments": list}))
}

// WHIQ (item 26, B43): rows and columns are 1-based and within the sheet, counts 1 or more; anything else is refused in words (0 and
// 1 were both the first row, a negative number was read as absent, past the grid it was clamped).
fn whole(cmd: &str, key: &str, v: &Json, least: u64, most: u64, what: &str) -> Result<u32> {
    let n = match v {
        Json::Number(n) => n.as_u64(),
        Json::String(s) => s.trim().parse::<u64>().ok(),
        _ => None,
    };
    match n.filter(|n| (least..=most).contains(n)) {
        Some(n) => Ok(n as u32),
        None => Err(bad(cmd, format!("`{key}` is {what} from {least} to {most} (got {v})"))),
    }
}

/// A 1-based row number.
fn row_param(cmd: &str, p: &Json, key: &str) -> Result<Option<u32>> {
    match any_param(p, key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => whole(cmd, key, v, 1, u64::from(gridcraft_core::MAX_ROWS), "a row number").map(|r| Some(r - 1)),
    }
}

/// A 1-based column number or letters (`"C"` = 3).
fn col_param(cmd: &str, p: &Json, key: &str) -> Result<Option<u32>> {
    match any_param(p, key) {
        None | Some(Json::Null) => Ok(None),
        Some(Json::String(s)) if s.trim().parse::<u64>().is_err() => gridcraft_core::letters_to_col(&s.trim().to_ascii_uppercase())
            .map(Some)
            .ok_or_else(|| bad(cmd, format!("`{key}` is a column number or letters, e.g. 3 or \"C\" (got \"{s}\")"))),
        Some(v) => whole(cmd, key, v, 1, u64::from(gridcraft_core::MAX_COLS), "a column number").map(|c| Some(c - 1)),
    }
}

fn count_param(cmd: &str, p: &Json, key: &str) -> Result<Option<u32>> {
    match any_param(p, key) {
        None | Some(Json::Null) => Ok(None),
        Some(v) => whole(cmd, key, v, 1, u64::from(u32::MAX), "a count").map(Some),
    }
}

/// The sheet a call names (by name in any case, or index); `None` when it names none.
fn sheet_param(s: &Session, p: &Json, cmd: &str) -> Result<Option<usize>> {
    let d = s.doc()?;
    match any_param(p, "sheet") {
        None | Some(Json::Null) => Ok(None),
        Some(Json::Number(n)) => {
            let i = n.as_u64().unwrap_or(u64::MAX) as usize;
            if i < d.wb.sheets.len() { Ok(Some(i)) } else { Err(bad(cmd, format!("there is no sheet {i} (the workbook has {})", d.wb.sheets.len()))) }
        }
        Some(Json::String(name)) if name.trim().is_empty() => Ok(None),
        // (item 26, B43: a name is a name — "2" was the third sheet unless a sheet had that name; an index is a number)
        Some(Json::String(name)) => {
            d.wb.sheet_index(name.trim()).map(Some).ok_or_else(
                || {
                    let names: Vec<&str> = d.wb.sheets.iter().map(|s| s.name.as_str()).collect();
                    bad(cmd, format!("no sheet named `{name}` (sheets: {})", names.join(", ")))
                },
            )
        }
        Some(_) => Err(bad(cmd, "`sheet` is a sheet name or index")),
    }
}

fn preview_html(s: &mut Session, p: &Json) -> Result<Json> {
    const CMD: &str = "file.previewHtml";
    let sheet = sheet_param(s, p, CMD)?;
    let d = s.doc()?;
    let format = match str_param(p, "format") {
        Some(f) => Format::parse(f).ok_or_else(|| bad(CMD, format!("`format` is page, sheet or rows, not `{f}`")))?,
        None => Format::Page,
    };
    let o = Options {
        sheet,
        row_start: row_param(CMD, p, "row_start")?.or(row_param(CMD, p, "rowStart")?),
        row_count: count_param(CMD, p, "row_count")?.or(count_param(CMD, p, "rowCount")?),
        col_start: col_param(CMD, p, "col_start")?.or(col_param(CMD, p, "colStart")?),
        col_count: count_param(CMD, p, "col_count")?.or(count_param(CMD, p, "colCount")?),
        format,
        headings: bool_param(p, "headings").unwrap_or(true),
        other_sheets: bool_param(p, "other_sheets").unwrap_or(true),
        // (item 26, B39: 1 to 2,000,000, refused outside them; above it was lowered without a word)
        max_cells: match any_param(p, "max_cells") {
            None | Some(Json::Null) => crate::preview::DEFAULT_MAX_CELLS,
            Some(v) => u64::from(whole(CMD, "max_cells", v, 1, crate::preview::MAX_WINDOW_CELLS, "a number of cells")?),
        },
        title: str_param(p, "title").map(str::to_string).unwrap_or_else(|| d.display_title()),
    };
    let mut v = crate::preview::render(&d.wb, &o).map_err(|e| bad(CMD, e))?;
    // `path`: the HTML goes into that file (confined like every write) instead of the answer.
    if let Some(path) = str_param(p, "path").filter(|x| !x.trim().is_empty())
        && let Some(o) = v.as_object_mut()
        && let Some(Json::String(html)) = o.remove("html")
    {
        crate::io::write_file(path, html.as_bytes())?;
        o.insert("path".into(), json!(path));
    }
    Ok(v)
}

fn sheet_markdown(s: &mut Session, p: &Json) -> Result<Json> {
    const CMD: &str = "sheet.markdown";
    let sheet = sheet_param(s, p, CMD)?;
    let d = s.doc()?;
    let o = MarkdownOptions {
        sheet,
        continue_sheets: bool_param(p, "continue_sheets").unwrap_or(sheet.is_none()),
        row_start: row_param(CMD, p, "row_start")?,
        row_count: count_param(CMD, p, "row_count")?,
        col_start: col_param(CMD, p, "col_start")?,
        col_count: count_param(CMD, p, "col_count")?,
        // (item 26, A2: 1 KiB to 64 MiB, refused outside them; they were clamped)
        // AGREED LIMIT (owner, 2026-11-28): 262,144 bytes — a Markdown part when `max_bytes` is not given, the rest reached with `next`
        // — why: item 26 rule 4, "Defaults - agreed" — arbitrary by agreement.
        max_bytes: match any_param(p, "max_bytes") {
            None | Some(Json::Null) => 262_144,
            Some(v) => whole(CMD, "max_bytes", v, 1024, 64 << 20, "a number of bytes")? as usize,
        },
        formulas: bool_param(p, "formulas").unwrap_or(false),
        keep_empty_rows: bool_param(p, "keep_empty_rows").unwrap_or(false),
    };
    let mut v = crate::preview::markdown(&d.wb, &o).map_err(|e| bad(CMD, e))?;
    v["title"] = json!(d.display_title());
    Ok(v)
}
