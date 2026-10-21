//! WHIQ: the tools the platform needs on top of upstream's, and the contract pipeline every
//! `tools/call` goes through ([`run_tool`]): forgiving names and arguments, `caller` identity,
//! `overwrite`, eviction, errors with a code / reason / next step, trimmed results and
//! `structuredContent`.

use std::path::{Path, PathBuf};

use gridcraft_engine::core::{CellRef, RangeRef};
use serde_json::{Map, Value, json};

use crate::backend::{Backend, DocSnapshot};
use crate::contract::{self, ToolError};
use crate::tools::{Args, Hints, ToolResult, check_arguments, exec, pick};

// ---------------------------------------------------------------- settings and state

/// Limits and identity for one server.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `--author NAME`: who the work is done for, unless a call names its own `caller`.
    pub author: Option<String>,
    /// Results above this many bytes of JSON are trimmed with a continue marker.
    pub max_result_bytes: usize,
    /// read_range returns at most this many cells per call (the rest via `continue_from`).
    pub max_read_cells: u64,
    /// `GRIDCRAFT_MCP_TEST_HOOKS=1`: the `debug_sleep` / `debug_panic` tools exist (contract tests).
    pub test_hooks: bool,
    /// A save over an existing file keeps it as `OLD_<name>` unless the call passes `overwrite:
    /// true`. Off (`--automation-no-old-copies`): saves replace files, a supervisor keeps backups.
    pub old_copies: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self {
            author: None,
            max_result_bytes: contract::MAX_RESULT_BYTES,
            max_read_cells: 10_000,
            test_hooks: test_hooks_from_env(),
            old_copies: true,
        }
    }
}

/// WHIQ: tools that open or start a document (it becomes the active one; the result names its
/// `document` handle).
pub(crate) const OPENS_DOCUMENT: &[&str] = &["open_workbook", "new_workbook", "build_workbook"];

/// WHIQ: the files a tool reads, for a supervisor that stages them (`_meta["whiq/files"]`, the
/// same field WordCraft and DeckCraft carry): `read` lists the arguments naming existing files
/// the call reads; `opens` says the call opens or starts the document the server works on (the
/// file of the first `read` argument when one is given). Tools that read no file have no entry.
pub(crate) fn files_meta(name: &str) -> Option<Value> {
    let (read, opens): (&[&str], bool) = match name {
        "open_workbook" => (&["path"], true),
        "new_workbook" | "build_workbook" => (&[], true),
        "roundtrip_check" => (&["path"], false),
        _ => return None,
    };
    Some(json!({"read": read, "opens": opens}))
}

/// `GRIDCRAFT_MCP_TEST_HOOKS=1` turns on the tools the contract tests use to provoke a slow call
/// or a panic.
pub fn test_hooks_from_env() -> bool {
    std::env::var("GRIDCRAFT_MCP_TEST_HOOKS").is_ok_and(|v| v == "1")
}

/// Why the open document was closed, and where its work went.
#[derive(Clone, Debug, PartialEq)]
pub struct Eviction {
    pub reason: String,
    /// The auto-saved copy (`<name>.recovered-<stamp>.xlsx`), when there was unsaved work.
    pub recovered: Option<PathBuf>,
    /// WHIQ: the recovered files of the other open workbooks that had unsaved work.
    pub others: Vec<PathBuf>,
    /// The file the document came from, if any.
    pub original: Option<String>,
}

impl Eviction {
    fn error(&self) -> ToolError {
        let (reason, next) = match (&self.recovered, &self.original) {
            (Some(r), _) => (
                format!("{}; your work is auto-saved at {}", self.reason, r.display()),
                format!("call open_workbook with path \"{}\" and carry on", r.display()),
            ),
            (None, Some(o)) => (format!("{}; it had no unsaved changes", self.reason), format!("call open_workbook with path \"{o}\" and carry on")),
            (None, None) => (format!("{}; it had no unsaved changes", self.reason), "call new_workbook or open_workbook to carry on".to_string()),
        };
        let reason = if self.others.is_empty() {
            reason
        } else {
            format!(
                "{reason}; the other open workbooks' unsaved work is auto-saved at {}",
                self.others.iter().map(|p| p.display().to_string()).collect::<Vec<_>>().join(", ")
            )
        };
        let mut e = ToolError::new("evicted", reason, next);
        if let Some(r) = &self.recovered {
            e = e.with("recovered_path", json!(r.display().to_string()));
        }
        if !self.others.is_empty() {
            let all: Vec<String> = self.recovered.iter().chain(self.others.iter()).map(|p| p.display().to_string()).collect();
            e = e.with("recovered_paths", json!(all));
        }
        if let Some(o) = &self.original {
            e = e.with("original_path", json!(o));
        }
        e
    }
}

/// WHIQ: `save_workbook` (or `execute_command` `file.save` / `file.saveAs`) of a workbook the
/// server closed: its auto-saved copy (or its unchanged original) is copied to the path asked,
/// which is the save. `None` when the call is no such save, or when the format asked is not the
/// copy's (that needs the workbook open again; the `evicted` answer says so).
fn save_evicted(name: &str, a: &Value, ev: &Eviction, overwrite: bool) -> Option<Result<Value, String>> {
    let params = match name {
        "save_workbook" => a.clone(),
        "execute_command" if matches!(a.get("command").and_then(Value::as_str), Some("file.save" | "file.saveAs")) => {
            a.get("params").cloned().unwrap_or(Value::Null)
        }
        _ => return None,
    };
    let source = ev.recovered.as_ref().map(|p| p.display().to_string()).or_else(|| ev.original.clone())?;
    let target = params.get("path").and_then(Value::as_str).map(String::from).or_else(|| ev.original.clone())?;
    let kind = |p: &str| gridcraft_engine::io::FileKind::from_path(p);
    let xlsx_family = |p: &str| kind(p) == Some(gridcraft_engine::io::FileKind::Xlsx);
    if !(xlsx_family(&source) && xlsx_family(&target))
        || gridcraft_engine::xlsx::XlsxKind::from_path(&source) != gridcraft_engine::xlsx::XlsxKind::from_path(&target)
    {
        return None;
    }
    let same = std::fs::canonicalize(&source).ok().zip(std::fs::canonicalize(&target).ok()).is_some_and(|(x, y)| x == y);
    let copy = || -> Result<u64, String> {
        if same {
            return std::fs::metadata(&source).map(|m| m.len()).map_err(|e| format!("{source}: {e}"));
        }
        let mut src = std::fs::File::open(&source).map_err(|e| format!("{source}: {e}"))?;
        let mut n = 0;
        gridcraft_engine::io::write_file_with(&target, &mut |f| {
            n = std::io::copy(&mut src, f).map_err(|e| gridcraft_engine::EngineError::Other(format!("{source}: {e}")))?;
            Ok(())
        })
        .map_err(|e| e.to_string())?;
        Ok(n)
    };
    let _ = gridcraft_engine::io::save_policy::take_reports();
    let r = gridcraft_engine::io::save_policy::allow_overwrite(overwrite, copy);
    let reports = gridcraft_engine::io::save_policy::take_reports();
    Some(r.map(|bytes| {
        let abs = |p: &str| std::path::absolute(p).map(|x| x.display().to_string()).unwrap_or_else(|_| p.to_string());
        let mut v = json!({
            "path": abs(&target),
            "bytes": bytes,
            "saved_from": abs(&source),
            "evicted": {"reason": ev.reason, "recovered_path": ev.recovered.as_ref().map(|p| p.display().to_string()), "original_path": ev.original},
            "next_step": format!(
                "saved. The workbook had been closed by the server ({}); the save wrote its work from {source}. Call open_workbook {{path: \"{}\"}} to edit it further",
                ev.reason,
                abs(&target)
            ),
        });
        if let Some(old) = reports.iter().find_map(|r| r.previous_renamed_to.clone()) {
            v["previous_file_renamed_to"] = json!(old.display().to_string());
        }
        v
    }))
}

/// What a server keeps between calls.
#[derive(Debug, Default)]
pub struct CallState {
    pub settings: Settings,
    /// Set when the document was evicted; cleared by open_workbook / new_workbook / build_workbook.
    pub evicted: Option<Eviction>,
    /// Said once, on the next successful call (e.g. recovered files found at start).
    pub notices: Vec<String>,
    /// Warnings about this call found before it ran (a tool name read in another spelling).
    pub call_warnings: Vec<String>,
}

// ---------------------------------------------------------------- definitions

fn string(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn boolean(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}
#[allow(dead_code)]
fn number(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}
fn int(desc: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": desc})
}
fn obj(props: Value, required: &[&str]) -> Value {
    let mut o = json!({"type": "object", "properties": props, "additionalProperties": false});
    if !required.is_empty() {
        o["required"] = json!(required);
    }
    o
}
fn tool(name: &str, title: &str, desc: &str, schema: Value, hints: Hints) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": desc,
        "inputSchema": schema,
        "annotations": {
            "title": title,
            "readOnlyHint": hints.read_only,
            "destructiveHint": hints.destructive,
            "idempotentHint": hints.idempotent,
            "openWorldHint": false,
        },
    })
}

/// Tools that write files: they take `overwrite`.
pub(crate) const WRITES_FILES: &[&str] = &["save_workbook", "export_pdf", "render_range", "render_sheet", "build_workbook", "screenshot", "export_range"];

/// Tools that work without an open document (they stay available after an eviction).
const NEEDS_NO_DOCUMENT: &[&str] = &[
    "open_workbook",
    "new_workbook",
    "build_workbook",
    "server_status",
    "job_status",
    "list_jobs",
    "list_commands",
    "list_functions",
    "debug_sleep",
    "cancel",
    "evict",
    "roundtrip_check",
];

/// Tools that run on the dispatcher thread, never queued behind document work.
pub const CONTROL_TOOLS: &[&str] = &["server_status", "job_status", "list_jobs", "cancel"];

/// The WHIQ tools (definitions only; the global arguments are added by [`with_globals`]).
pub fn definitions() -> Vec<Value> {
    let mut v = vec![
        tool(
            "server_status",
            "Server status",
            "Health of this GridCraft server: version, memory in use, the open document (title, path, unsaved changes, sheets, \
             cells), jobs running or finished, whether a call is in progress, the read/write roots, the author, and any \
             recovered (auto-saved) files waiting to be reopened. Always answers at once, even while a long call runs.",
            obj(json!({}), &[]),
            crate::tools::READ,
        ),
        tool(
            "cancel",
            "Cancel",
            "Stop the call in progress (no job_id) or a running job (job_id). It stops at the next safe point — between \
             formulas of a recalculation, rows being read, written or drawn, steps of a build — and the workbook is left exactly \
             as it was before that call. Answers at once ({cancelled, status}); the stopped call or job then ends with code \
             `cancelled` (job status `cancelled`). queued: true also drops calls waiting behind it.",
            obj(
                json!({
                    "job_id": string("The job to stop (from a call's answer or server_status); default: the call in progress"),
                    "queued": boolean("Also drop the calls waiting behind the running one (each answers `cancelled`)"),
                }),
                &[],
            ),
            crate::tools::SET,
        ),
        tool(
            "job_status",
            "Job status",
            "Progress and result of a job: status (running, done, failed, cancelled), progress 0..1, stage, steps and the stages \
             passed, then its result (or error). Long work (opening a very large file, a full recalculation, exporting or \
             rendering many pages, or any call that runs past --job-after-ms) answers with a job_id at once; poll this tool \
             with it. The result survives client timeouts. Without job_id: every job this server knows, like list_jobs.",
            obj(json!({"job_id": string("The job id a call returned, e.g. \"job-3\"")}), &[]),
            crate::tools::READ,
        ),
        tool(
            "list_jobs",
            "List jobs",
            "Every job this server knows (running and finished, newest 200): job_id, tool, status, progress, stage, elapsed time.",
            obj(json!({}), &[]),
            crate::tools::READ,
        ),
        tool(
            "evict",
            "Evict",
            "For a supervisor: auto-save the open workbook's unsaved work to a recovered file inside the write root and close it. \
             The next call that needs it answers `evicted` with the file to reopen.",
            obj(json!({"reason": string("Why (shown to the agent)")}), &[]),
            crate::tools::SET,
        ),
        tool(
            "export_range",
            "Export range",
            "Write a range (default: the sheet's used range) as a CSV file and answer with its path, row and column counts: the \
             way to read a large range in one call (read_range returns a window per call). Values are written as Save As CSV writes \
             them. Streamed a row at a time, so any size works.",
            obj(
                json!({
                    "path": string("The .csv file to write (inside the write root)"),
                    "range": string("Range, e.g. \"A1:L500000\" or \"Sheet2!A:F\" (default: the used range)"),
                    "sheet": string("Sheet name (default: the active sheet)"),
                }),
                &["path"],
            ),
            crate::tools::SET,
        ),
        tool(
            "roundtrip_check",
            "Round-trip check",
            "Open and save an .xlsx/.xlsm in memory (without touching the open workbook or the file) and report whether anything \
             would be dropped: parts, relationships, binary parts, and the elements of every XML part. Use it on a customer's file \
             before editing it.",
            obj(json!({"path": string("The .xlsx or .xlsm file")}), &["path"]),
            crate::tools::READ,
        ),
    ];
    v.extend(more_definitions());
    v.extend(editing_definitions());
    v
}

const PAPERS: &[&str] = &["Letter", "Legal", "A4", "A3", "A5", "Tabloid", "Executive", "Ledger", "B5"];

/// The authoring, rendering and editing tools.
fn more_definitions() -> Vec<Value> {
    let png_common = || {
        json!({
            "sheet": string("Sheet name (default: the active sheet)"),
            "scale": number("Pixels per sheet pixel, 0.1–8 (default 2; 1 = 96 dpi)"),
            "gridlines": boolean("Draw gridlines (default true)"),
            "headings": boolean("Draw row numbers and column letters (default false)"),
            "path": string("Write the PNG here (inside the write root). Without it the picture comes back inline (up to 2 MB)"),
        })
    };
    let mut range_props = png_common();
    range_props["range"] = string("Range to draw, e.g. \"A1:F20\" or \"Sheet2!B2:H30\"");
    let mut sheet_props = png_common();
    sheet_props["max_pixels"] = int("Longest side in pixels (default 4000): the scale shrinks to fit");
    vec![
        tool(
            "render_range",
            "Render range",
            "A PNG picture of a range exactly as it prints: fills, borders, number formats, fonts, merged cells, conditional \
             formats, charts, pictures and shapes. Arabic and other complex scripts are shaped and right-to-left; right-to-left \
             sheets are mirrored. Give `path` to save the file, or get the picture inline.",
            obj(range_props, &["range"]),
            crate::tools::OVERWRITE,
        ),
        tool(
            "render_sheet",
            "Render sheet",
            "A PNG picture of a whole sheet (its print area, else everything used, charts included), scaled to fit `max_pixels`.",
            obj(sheet_props, &[]),
            crate::tools::OVERWRITE,
        ),
        tool(
            "export_pdf",
            "Export PDF",
            "Export the active sheet (or every sheet) to PDF with page setup: paper, orientation, fit to width, repeat header \
             rows, margins, gridlines, headings, header/footer. Every font is embedded as a subset (Latin in the cell's family \
             or its open substitute, Arabic shaped), so the PDF looks the same everywhere and its text can be selected; \
             `standard_fonts: true` gives the smallest file. The page \
             setup given is kept on the sheet (it prints the same way next time).",
            obj(
                json!({
                    "path": string("Destination .pdf (inside the write root). Without it the PDF comes back as base64"),
                    "sheets": enumeration("Which sheets (default: active)", &["active", "all"]),
                    "range": string("Print only this range of the active sheet, e.g. \"A1:H60\""),
                    "paper": enumeration("Paper size", PAPERS),
                    "orientation": enumeration("Page orientation", &["portrait", "landscape"]),
                    "fit_width": boolean("Fit all columns on one page width (pages run down as needed)"),
                    "fit_to_page": boolean("Fit the whole print area on one page"),
                    "repeat_header_rows": string("Rows repeated at the top of every page, e.g. \"1:1\" or \"1:2\""),
                    "margins": {"type": "object", "description": "Margins in inches: {left, right, top, bottom, header, footer}"},
                    "gridlines": boolean("Print gridlines"),
                    "headings": boolean("Print row and column headings"),
                    "header": string("Page header, e.g. \"&CQuarterly report\""),
                    "footer": string("Page footer, e.g. \"&CPage &P of &N\""),
                    "standard_fonts": boolean("Smallest file: PDF base fonts for plain Latin text (Arabic and other scripts stay embedded). Default false: every font embedded as a subset, so the PDF looks the same everywhere"),
                    "fonts": enumeration("Finer choice: embedded (default), auto (= standard_fonts), standard (base fonts only; non-Latin text is lost)", &["embedded", "auto", "standard"]),
                    "pages": string("Only these pages of the export, e.g. \"3\" or \"2-5\" (numbered as in the whole export)"),
                }),
                &[],
            ),
            crate::tools::OVERWRITE,
        ),
    ]
}

/// The authoring and in-place editing tools.
fn editing_definitions() -> Vec<Value> {
    let rows = || string("Rows, e.g. \"5\" or \"5:7\"");
    let cols = || string("Columns, e.g. \"C\" or \"C:E\"");
    let sheet = || string("Sheet name (default: the active sheet)");
    vec![
        tool(
            "build_workbook",
            "Build workbook",
            "Create a complete workbook from JSON in one call and (with `path`) save it: sheets with a header row and rows of \
             values (strings starting with = are formulas), per-column number formats and widths, a frozen header, an \
             AutoFilter or a formatted table, merges, conditional formats, validation, right-to-left sheets, and native charts \
             (bar, column, line, pie, doughnut, area, scatter, combo with a secondary axis; title, axis titles, series by \
             range). Scatter series are solid straight lines with markers by default (`line: false` = markers only, \
             `markers: false` = the line alone, `line_width` in points, `color`; on the chart or per series), e.g. one \
             production line per trade. Formatting in the same call: `styles` [{range, bold, italic, font_size, font_color, \
             fill, borders, align, v_align, wrap, number_format}] per sheet, a cell object with its own look ({\"value\": \"\", \
             \"fill\": \"#3B82F6\"} paints a Gantt bar), `row_heights` in points, `column_widths`, `freeze` (\"C2\"). \
             `literal_strings: true` stores every string as the text it is (\"2.10\", \"1,234\", \"=x\", \"+5\"; no leading \
             apostrophe needed) and ISO dates (\"2026-04-03\", \"2026-04-03T08:30\") as dates shown yyyy-mm-dd; a single cell \
             can say {\"text\": \"2.10\"}, {\"date\": \"2026-04-03\"}, {\"formula\": \"=SUM(B2:B9)\"}. The result's \
             `document` handle names this workbook in later calls. Formulas are recalculated so the file opens with correct values. Example: {\"path\": \"report.xlsx\", \
             \"sheets\": [{\"name\": \"Sales\", \"header\": [\"Region\", \"Units\"], \"rows\": [[\"North\", 10], [\"Total\", \"=SUM(B2:B2)\"]], \
             \"columns\": [{\"width\": 20}, {\"format\": \"#,##0\"}], \"freeze_header\": true, \"table\": \"TableStyleMedium2\", \
             \"charts\": [{\"type\": \"column\", \"title\": \"Units\", \"series\": [{\"name\": \"Units\", \"values\": \"B2:B2\", \"categories\": \"A2:A2\"}]}]}]}",
            obj(
                json!({
                    "sheets": {"type": "array", "description": "Sheets: {name, header?, rows (cells: a value, or {text | formula | date | number | value + type?, plus look keys or style: {…}}), start?, columns?: [{header?, width?, format?}], number_formats?, column_widths?, freeze_header?, freeze?: \"C2\" | {rows, columns}, autofilter?, table?, header_style?, merges?, rtl?, charts?, conditional_formats?, validations?, styles?: [{range, bold?, italic?, underline?, font?, font_size?, font_color?, fill?, align?, v_align?, wrap?, indent?, rotation?, number_format?, borders?: \"all\" | \"outside\" | {top?, bottom?, left?, right?: \"thin\" | {style, color}}}], row_heights?: [{row, height (points)}] | {\"3\": 20}, literal_strings?, iso_dates?}", "items": {"type": "object"}},
                    "charts": {"type": "array", "description": "Charts naming their sheet: {sheet, type|kind: bar|column|col|line|pie|doughnut|area|scatter|combo, title?, x_title|xTitle?, y_title|yTitle?, series: [{name, values|valRange, categories|catRange|xRange, type?, secondary?, color?, line?, markers?, line_width?}], lineSeries? (combo lines), data?: \"A1:C9\", scatter: line? (default true: solid straight lines), markers? (default true), line_width? (points, default 2.25), color? (\"#RRGGBB\", every series), at?: \"H2\" | anchor?: {fromCol, fromRow, toCol, toRow}, legend?, hideLegend?, data_labels?, stacked?, width?, height?} (also accepted per sheet)", "items": {"type": "object"}},
                    "path": string("Save the workbook here (inside the write root); without it the workbook stays open unsaved"),
                    "literal_strings": boolean("Every sheet: strings are stored as the text they are, never read as numbers, formulas or dates (ISO dates excepted, see iso_dates). Default false: strings are typed input, as in Excel"),
                    "iso_dates": boolean("With literal_strings: ISO dates and date-times are dates, shown yyyy-mm-dd (default true)"),
                    "title": string("Document title"),
                    "author": string("Document author (default: the caller)"),
                    "company": string("Company"),
                    "subject": string("Subject"),
                }),
                &["sheets"],
            ),
            crate::tools::OVERWRITE,
        ),
        tool(
            "close_workbook",
            "Close workbook",
            "Close an open workbook (by its `document` handle, default the active one) to free its memory. A workbook with \
             unsaved changes stays open unless `discard: true`. The other open workbooks are untouched.",
            obj(json!({"discard": boolean("Close even with unsaved changes (they are lost). Default false")}), &[]),
            crate::tools::OVERWRITE,
        ),
        tool(
            "recalculate",
            "Recalculate",
            "Recalculate every formula in the workbook now (the cached values a saved file carries) and list the cells that \
             evaluate to errors (#DIV/0!, #REF!, #NAME?…).",
            obj(json!({}), &[]),
            crate::tools::SET,
        ),
        tool(
            "insert_rows",
            "Insert rows",
            "Insert empty rows above `rows` (formulas, charts, tables and names adjust).",
            obj(json!({"rows": rows(), "sheet": sheet()}), &["rows"]),
            crate::tools::ADD,
        ),
        tool(
            "insert_columns",
            "Insert columns",
            "Insert empty columns left of `cols` (formulas, charts, tables and names adjust).",
            obj(json!({"cols": cols(), "sheet": sheet()}), &["cols"]),
            crate::tools::ADD,
        ),
        tool(
            "delete_rows",
            "Delete rows",
            "Delete whole rows (cells below move up; references to them become #REF!).",
            obj(json!({"rows": rows(), "sheet": sheet()}), &["rows"]),
            crate::tools::ANY,
        ),
        tool(
            "delete_columns",
            "Delete columns",
            "Delete whole columns (cells to the right move left; references to them become #REF!).",
            obj(json!({"cols": cols(), "sheet": sheet()}), &["cols"]),
            crate::tools::ANY,
        ),
        tool(
            "rename_sheet",
            "Rename sheet",
            "Rename a sheet; formulas, charts and names that refer to it follow.",
            obj(json!({"sheet": string("The sheet to rename (default: the active sheet)"), "name": string("New name")}), &["name"]),
            crate::tools::SET,
        ),
        tool(
            "add_sheet",
            "Add sheet",
            "Add a worksheet (and make it active).",
            obj(json!({"name": string("Name (default Sheet2, Sheet3…)"), "before": int("Position (0-based); default: after the last")}), &[]),
            crate::tools::ADD,
        ),
        tool(
            "conditional_format",
            "Conditional format",
            "Add a conditional format to a range: {range, rule: {type: cellIs|expression|containsText|duplicate|top10|aboveAverage|colorScale|dataBar|iconSet…, operator?, value?, formula?, preset?: lightRedFill|greenFill…}}.",
            obj(
                json!({"range": string("Range, e.g. \"B2:B50\""), "rule": {"type": "object", "description": "The rule (see home.conditionalFormat in list_commands)"}}),
                &["range", "rule"],
            ),
            crate::tools::ADD,
        ),
        tool(
            "data_validation",
            "Data validation",
            "Set data validation on a range: {range, type: list|whole|decimal|date|time|textLength|custom, formula1 (e.g. \"Yes,No\" or \"=$H$2:$H$9\"), operator?, formula2?, inputMessage?, errorMessage?} or clear: true.",
            obj(
                json!({
                    "range": string("Range, e.g. \"C2:C100\""),
                    "type": enumeration("Validation type", &["any", "whole", "decimal", "list", "date", "time", "textLength", "custom"]),
                    "operator": string("between|notBetween|equal|notEqual|greater|less|greaterOrEqual|lessOrEqual"),
                    "formula1": string("First value or formula; for a list, the items separated by commas"),
                    "formula2": string("Second value (between / notBetween)"),
                    "inputTitle": string("Input message title"),
                    "inputMessage": string("Input message"),
                    "errorTitle": string("Error alert title"),
                    "errorMessage": string("Error alert message"),
                    "errorStyle": enumeration("Error alert style", &["stop", "warning", "information"]),
                    "clear": boolean("Remove validation from the range"),
                }),
                &["range"],
            ),
            crate::tools::SET,
        ),
    ]
}

fn enumeration(desc: &str, values: &[&str]) -> Value {
    json!({"type": "string", "enum": values, "description": desc})
}

/// The WHIQ tools that upstream's dispatcher doesn't know.
pub(crate) fn dispatch_extra(b: &mut dyn Backend, name: &str, a: &Args) -> Result<Value, String> {
    match name {
        "render_range" | "render_sheet" => render(b, name, a),
        "export_pdf" => export_pdf(b, a),
        "build_workbook" => {
            let mut p = Value::Object(a.clone());
            // The caller (or --author) is the new workbook's author and last modifier.
            let user = b.set_user("GridCraft User").inspect(|u| {
                b.set_user(u);
            });
            if let Some(u) = user.filter(|u| !u.is_empty() && u != "GridCraft User") {
                if p.get("author").is_none() {
                    p["author"] = json!(u);
                }
                p["last_modified_by"] = json!(u);
            }
            let mut v = exec(b, "file.build", p)?;
            v["next_step"] = json!(if a.get("path").is_some() {
                "the workbook is saved and open: edit it further, or render_sheet to look at it"
            } else {
                "the workbook is open: save_workbook with a path to keep it"
            });
            Ok(v)
        }
        "recalculate" => exec(b, "formulas.recalculate", json!({})),
        "close_workbook" => {
            let handle = b.active_document();
            let discard = a.get("discard").and_then(Value::as_bool).unwrap_or(false);
            let r = exec(b, "file.close", json!({"force": discard}))?;
            if r["closed"] != true {
                return Err("the workbook has unsaved changes: save_workbook it first, or pass discard: true to close it anyway".into());
            }
            Ok(json!({"closed": handle, "active_document": b.active_document()}))
        }
        "roundtrip_check" => {
            let path = a.get("path").and_then(Value::as_str).unwrap_or("");
            gridcraft_engine::core::progress::step_in("round trip: reading", 0, 2);
            // WHIQ: neither package is held in memory: the workbook is read from its file, saved
            // into a scratch file, and binary parts compare by their zip CRC.
            let (wb, _) = match gridcraft_engine::io::open_xlsx_file(path).map_err(|e| format!("{path}: {e}"))? {
                Some(x) => x,
                None => {
                    let bytes = gridcraft_engine::io::read_file(path).map_err(|e| e.to_string())?;
                    gridcraft_engine::io::open_bytes(path, &bytes).map_err(|e| format!("{path}: {e}"))?
                }
            };
            gridcraft_engine::core::progress::step_in("round trip: saving and comparing", 1, 2);
            let kind = gridcraft_engine::xlsx::XlsxKind::from_path(path);
            let scratch = std::env::temp_dir().join(format!(
                "gridcraft-roundtrip-{}-{}.xlsx",
                std::process::id(),
                std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
            ));
            let r = (|| -> Result<Value, String> {
                {
                    let f = std::fs::File::create(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
                    let mut w = std::io::BufWriter::with_capacity(1 << 20, f);
                    gridcraft_engine::xlsx::write_xlsx_into(&wb, kind, &mut w).map_err(|e| format!("{path}: {e}"))?;
                    std::io::Write::flush(&mut w).map_err(|e| e.to_string())?;
                }
                let before = gridcraft_engine::io::open_for_read(path).map_err(|e| e.to_string())?;
                let after = std::fs::File::open(&scratch).map_err(|e| format!("{}: {e}", scratch.display()))?;
                crate::roundtrip::compare_readers(std::io::BufReader::new(before), std::io::BufReader::new(after)).map_err(|e| format!("{path}: {e}"))
            })();
            let _ = std::fs::remove_file(&scratch);
            let mut v = r?;
            v["path"] = json!(path);
            if v["lossless"] != true {
                v["next_step"] =
                    json!("the parts listed under problems would change on save: tell the owner before editing, or keep the original file");
            }
            Ok(v)
        }
        "insert_rows" => with_sheet(b, a, |b| exec(b, "home.insertRows", json!({"rows": span(a, "rows")}))),
        "insert_columns" => with_sheet(b, a, |b| exec(b, "home.insertColumns", json!({"cols": span(a, "cols")}))),
        "delete_rows" => with_sheet(b, a, |b| exec(b, "home.deleteRows", json!({"rows": span(a, "rows")}))),
        "delete_columns" => with_sheet(b, a, |b| exec(b, "home.deleteColumns", json!({"cols": span(a, "cols")}))),
        "rename_sheet" => exec(b, "sheet.rename", pick(a, &["sheet", "name"])),
        "add_sheet" => exec(b, "home.insertSheet", pick(a, &["name", "before"])),
        "conditional_format" => exec(b, "home.conditionalFormat", pick(a, &["range", "rule"])),
        "data_validation" => exec(
            b,
            "data.validation",
            pick(
                a,
                &[
                    "range",
                    "type",
                    "operator",
                    "formula1",
                    "formula2",
                    "inputTitle",
                    "inputMessage",
                    "errorTitle",
                    "errorMessage",
                    "errorStyle",
                    "clear",
                ],
            ),
        ),
        other => Err(format!("unknown tool `{other}`")),
    }
}

/// `"7"` → `"7:7"`, `"C"` → `"C:C"`, `"5-7"` → `"5:7"`; a sheet prefix is dropped (the
/// `sheet` argument says which sheet).
fn span(a: &Args, key: &str) -> String {
    let raw = match a.get(key) {
        Some(Value::String(t)) => t.clone(),
        Some(other) => other.to_string(),
        None => String::new(),
    };
    let t = split_sheet(raw.trim()).1.trim().replace('-', ":").replace(' ', "");
    if t.contains(':') { t } else { format!("{t}:{t}") }
}

/// Runs `f` with `sheet` (when given) active, then puts the previous active sheet back.
fn with_sheet(b: &mut dyn Backend, a: &Args, f: impl FnOnce(&mut dyn Backend) -> Result<Value, String>) -> Result<Value, String> {
    let Some(sheet) = a.get("sheet").and_then(Value::as_str) else { return f(b) };
    let before = b.call("document.inspect", json!({}))?["activeSheet"].as_u64();
    exec(b, "sheet.activate", json!({"sheet": sheet}))?;
    let r = f(b);
    if let Some(i) = before {
        let _ = exec(b, "sheet.activate", json!({"sheet": i}));
    }
    r
}

/// Largest inline picture (bytes of PNG).
const MAX_INLINE_PNG: usize = 2 * 1024 * 1024;

fn render(b: &mut dyn Backend, name: &str, a: &Args) -> Result<Value, String> {
    let mut p = pick(a, &["range", "sheet", "scale", "gridlines", "headings", "path"]);
    if name == "render_sheet" {
        p["maxPixels"] = json!(a.get("max_pixels").and_then(Value::as_f64).unwrap_or(4000.0));
        if let Some(o) = p.as_object_mut() {
            o.remove("range");
        }
    }
    if let Some(r) = a.get("range").and_then(Value::as_str)
        && let (Some(sheet), _) = split_sheet(r)
    {
        p["sheet"] = json!(sheet);
    }
    let mut v = exec(b, "file.renderPng", p)?;
    if let Some(data) = v.as_object_mut().and_then(|o| o.remove("base64")) {
        let size = v["bytes"].as_u64().unwrap_or(0) as usize;
        if size > MAX_INLINE_PNG {
            return Err(format!(
                "the picture is too large to return inline ({size} bytes): pass `path` to save it, or a lower `scale` (render it in parts too large)"
            ));
        }
        v["__image"] = json!({"data": data, "mimeType": "image/png"});
    }
    Ok(v)
}

fn export_pdf(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    let mut setup = Map::new();
    for (from, to) in [
        ("paper", "paper"),
        ("orientation", "orientation"),
        ("margins", "margins"),
        ("gridlines", "gridlines"),
        ("headings", "headings"),
        ("header", "header"),
        ("footer", "footer"),
    ] {
        if let Some(v) = a.get(from).filter(|v| !v.is_null()) {
            setup.insert(to.into(), v.clone());
        }
    }
    if a.get("fit_width").and_then(Value::as_bool) == Some(true) {
        setup.insert("fitWidth".into(), json!(1));
        setup.insert("fitHeight".into(), json!(0));
    }
    if let Some(rows) = a.get("repeat_header_rows").and_then(Value::as_str) {
        let rows = rows.trim();
        let rows = match rows.parse::<u32>() {
            Ok(n) if n >= 1 => format!("1:{n}"),
            _ => rows.trim_start_matches('$').to_string(),
        };
        setup.insert("titleRows".into(), json!(rows));
    }
    let all = a.get("sheets").and_then(Value::as_str) == Some("all");
    if !setup.is_empty() {
        let sheets: Vec<String> = if all {
            b.call("document.inspect", json!({}))?["sheets"]
                .as_array()
                .cloned()
                .unwrap_or_default()
                .iter()
                .filter_map(|s| s["name"].as_str().map(str::to_string))
                .collect()
        } else {
            vec![]
        };
        if sheets.is_empty() {
            exec(b, "pageLayout.pageSetup", Value::Object(setup.clone()))?;
        }
        for sh in sheets {
            let mut p = setup.clone();
            p.insert("sheet".into(), json!(sh));
            exec(b, "pageLayout.pageSetup", Value::Object(p))?;
        }
    }
    let mut p = pick(a, &["path", "sheets", "range", "fit_to_page", "pages"]);
    if let Some(o) = p.as_object_mut()
        && let Some(f) = o.remove("fit_to_page")
    {
        o.insert("fitToPage".into(), f);
    }
    if a.get("standard_fonts").and_then(Value::as_bool) == Some(true) {
        p["standardFonts"] = json!(true);
    } else {
        p["fonts"] = json!(a.get("fonts").and_then(Value::as_str).unwrap_or("embedded"));
    }
    exec(b, "file.exportPdf", p)
}

/// Upstream's and the WHIQ tools with the test hooks when enabled.
pub fn all_definitions() -> Vec<Value> {
    let mut defs = crate::tools::tool_definitions();
    if test_hooks_from_env() {
        defs.extend(with_globals(vec![
            tool(
                "debug_sleep",
                "Debug: sleep",
                "Contract-test hook: hold the document for `ms` milliseconds.",
                obj(json!({"ms": int("Milliseconds")}), &[]),
                crate::tools::READ,
            ),
            tool("debug_panic", "Debug: panic", "Contract-test hook: panic inside a call.", obj(json!({}), &[]), crate::tools::READ),
        ]));
    }
    defs
}

/// Adds `caller` and `async` to every tool, `overwrite` to the ones that write files, and
/// `offset`/`limit` to the list tools.
pub fn with_globals(mut tools: Vec<Value>) -> Vec<Value> {
    for t in &mut tools {
        let name = t["name"].as_str().unwrap_or("").to_string();
        if let Some(files) = files_meta(&name) {
            t["_meta"] = json!({"whiq/files": files});
        }
        let Some(props) = t["inputSchema"]["properties"].as_object_mut() else { continue };
        props.insert(
            "caller".into(),
            string("Who this call is for (a person or agent name); tracked changes, comments and the file's last-modified-by carry it. Default: --author"),
        );
        if !CONTROL_TOOLS.contains(&name.as_str()) {
            props
                .insert("async".into(), boolean("Run as a job: answer at once with a job_id and poll job_status (default: automatic for long work)"));
            if name != "read_range" {
                props.entry("continue_from").or_insert(int("Where to resume a trimmed result (its continue_from)"));
            }
            props.entry("max_bytes").or_insert(int("Largest result to return before trimming (default --max-result-bytes)"));
        }
        if !CONTROL_TOOLS.contains(&name.as_str()) && !OPENS_DOCUMENT.contains(&name.as_str()) && !NEEDS_NO_DOCUMENT.contains(&name.as_str()) {
            props.insert(
                "document".into(),
                string(
                    "The workbook to act on: the `document` handle open_workbook / new_workbook / build_workbook returned (default: the active one)",
                ),
            );
        }
        if WRITES_FILES.contains(&name.as_str()) {
            props.insert(
                "overwrite".into(),
                boolean("Replace an existing file at the destination. Default false: the existing file is renamed OLD_<name> first and the result says so"),
            );
        }
        if matches!(name.as_str(), "list_commands" | "list_functions") {
            props.insert("offset".into(), int("Skip this many items (continue_from of a trimmed result)"));
            props.insert("limit".into(), int("Return at most this many items"));
        }
        if name == "read_range" {
            props.insert("max_cells".into(), int("Most cells to return in one call (default 10000); the rest is reached with continue_from"));
        }
    }
    tools
}

// ---------------------------------------------------------------- the pipeline

/// Runs one tool through the whole contract. `name` must be a known tool (resolve it with
/// [`contract::normalize_tool_name`] first).
pub fn run_tool(b: &mut dyn Backend, st: &mut CallState, name: &str, args: &Value) -> ToolResult {
    let mut warnings = std::mem::take(&mut st.call_warnings);
    let defs = all_definitions();
    let Some(def) = defs.iter().find(|d| d["name"] == name) else {
        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        let near = contract::closest(name, names.iter().copied(), 3);
        return ToolResult::failure(
            &ToolError::new(
                "not_found",
                format!("there is no tool `{name}`"),
                "call tools/list to see the tools; the closest are listed in `closest`",
            )
            .with("closest", json!(near)),
        );
    };
    let empty = Map::new();
    let raw = match args {
        Value::Object(o) => o,
        Value::Null => &empty,
        _ => {
            return ToolResult::failure(&ToolError::new(
                "invalid_argument",
                "tool arguments must be a JSON object",
                format!("pass the arguments as an object, e.g. {}", example_args(def)),
            ));
        }
    };
    let (mut a, mut fixes) = contract::normalize_args(&def["inputSchema"], raw);
    warnings.append(&mut fixes);
    // `continue_from` (the shared continuation) on a tool that pages with `offset`.
    if def["inputSchema"]["properties"]["offset"].is_object()
        && !a.contains_key("offset")
        && let Some(n) = raw.get("continue_from").and_then(Value::as_u64)
    {
        a.insert("offset".into(), json!(n));
    }
    if let Err(e) = check_arguments(name, &Value::Object(a.clone())) {
        return ToolResult::failure(&invalid_argument(def, &e));
    }
    if let Some(ev) = &st.evicted
        && !NEEDS_NO_DOCUMENT.contains(&name)
    {
        // WHIQ: a save is never lost to an eviction: the workbook's auto-saved copy (or its
        // unchanged original) is written where the save asks, byte for byte, without opening it.
        let overwrite = !st.settings.old_copies || raw.get("overwrite").is_some_and(|v| v.as_bool() == Some(true));
        if let Some(r) = save_evicted(name, &Value::Object(a.clone()), ev, overwrite) {
            return match r {
                Ok(v) => ToolResult::json(&v),
                Err(e) => ToolResult::failure(&ToolError::new("failed", e, "check the path and the write root, or open the workbook again and save it")),
            };
        }
        return ToolResult::failure(&ev.error());
    }
    // WHIQ: `document` names the workbook a call acts on; the active one is restored after.
    let target = a.get("document").and_then(|v| v.as_str().map(str::to_string).or_else(|| v.as_u64().map(crate::backend::handle_of)));
    let restore = match &target {
        None => None,
        Some(h) => {
            let before = b.active_document();
            match b.select_document(h) {
                Ok(()) => before,
                Err(_) if !b.has_handles() => {
                    return ToolResult::failure(&ToolError::new(
                        "not_available",
                        "document handles exist in the headless server only",
                        "leave out `document`: the call acts on the app's active workbook",
                    ));
                }
                Err(open) => {
                    let near = contract::closest(h, open.iter().map(String::as_str), 3);
                    return ToolResult::failure(
                        &ToolError::new(
                            "not_found",
                            format!("no open workbook has the handle `{h}` (closed, evicted or never opened)"),
                            "use a handle from server_status `documents`, or open the file again with open_workbook",
                        )
                        .with("closest", json!(near))
                        .with("open_documents", json!(open)),
                    );
                }
            }
        }
    };
    let caller = raw.get("caller").and_then(Value::as_str).map(str::to_string).or_else(|| st.settings.author.clone());
    let overwrite = !st.settings.old_copies
        || raw.get("overwrite").is_some_and(|v| v.as_bool() == Some(true) || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")));
    let previous_user = caller.as_deref().and_then(|c| b.set_user(c));
    // Saves carry who saved; exports and renders change nothing in the workbook.
    if let Some(c) = &caller
        && (name == "save_workbook"
            || (name == "execute_command" && matches!(a.get("command").and_then(Value::as_str), Some("file.save" | "file.saveAs"))))
    {
        b.stamp_author(c);
    }
    let _ = gridcraft_engine::io::save_policy::take_reports();
    let r = gridcraft_engine::io::save_policy::allow_overwrite(overwrite, || dispatch(b, st, name, &a, &mut warnings));
    if let Some(p) = previous_user {
        b.set_user(&p);
    }
    // The document a call opened stays active; otherwise the one active before comes back (when
    // it is still open), so calls without a handle keep acting where they did.
    let handle = OPENS_DOCUMENT.contains(&name).then(|| b.active_document()).flatten();
    if let Some(prev) = restore
        && !OPENS_DOCUMENT.contains(&name)
    {
        let _ = b.select_document(&prev);
    }
    let reports = gridcraft_engine::io::save_policy::take_reports();
    match r {
        Ok(v) => {
            if matches!(name, "open_workbook" | "new_workbook" | "build_workbook") {
                st.evicted = None;
            }
            let mut v = v;
            if let (Some(h), Some(o)) = (&handle, v.as_object_mut()) {
                o.insert("document".into(), json!(h));
            }
            let max = raw.get("max_bytes").and_then(Value::as_u64).map_or(st.settings.max_result_bytes, |m| (m as usize).clamp(1024, 64 << 20));
            let from =
                raw.get("continue_from").and_then(Value::as_u64).filter(|_| !def["inputSchema"]["properties"]["offset"].is_object()).unwrap_or(0);
            let mut v = contract::trim_from(v, from as usize, max);
            warnings.append(&mut st.notices);
            if !v.is_object() && (reports.iter().any(|r| r.previous_renamed_to.is_some()) || !warnings.is_empty()) {
                v = json!({"result": v});
            }
            let renamed: Vec<Value> = reports
                .iter()
                .filter_map(|r| {
                    r.previous_renamed_to.as_ref().map(|old| json!({"path": r.path.display().to_string(), "previous": old.display().to_string()}))
                })
                .collect();
            if !renamed.is_empty() {
                for r in &reports {
                    if let Some(old) = &r.previous_renamed_to {
                        warnings.push(format!(
                            "{} already existed: it was renamed to {} (pass overwrite: true to replace a file instead)",
                            r.path.display(),
                            old.display()
                        ));
                    }
                }
                v["notice"] = json!(format!(
                    "an existing file was kept: {} (pass overwrite: true to replace instead)",
                    renamed.iter().filter_map(|r| r["previous"].as_str()).collect::<Vec<_>>().join(", ")
                ));
                v["previous_files_renamed"] = Value::Array(renamed);
            }
            if !warnings.is_empty() {
                match v.get_mut("warnings").and_then(Value::as_array_mut) {
                    Some(list) => list.extend(warnings.into_iter().map(Value::String)),
                    None => v["warnings"] = json!(warnings),
                }
            }
            let image = v.as_object_mut().and_then(|o| o.remove("__image"));
            let mut r = ToolResult::json(&v);
            if let Some(img) = image {
                r.content.push(json!({"type": "image", "data": img["data"], "mimeType": img["mimeType"]}));
            }
            r
        }
        Err(e) => ToolResult::failure(&e),
    }
}

/// An example of a tool's arguments from its schema: the required ones with placeholders.
fn example_args(def: &Value) -> String {
    let schema = &def["inputSchema"];
    let mut o = Map::new();
    for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let p = &schema["properties"][r];
        let ex = match p["type"].as_str() {
            Some("array") => json!([]),
            Some("boolean") => json!(true),
            Some("number" | "integer") => json!(1),
            Some("object") => json!({}),
            _ => match p["enum"].as_array().and_then(|e| e.first()) {
                Some(first) => first.clone(),
                None => json!(format!("<{r}>")),
            },
        };
        o.insert(r.to_string(), ex);
    }
    Value::Object(o).to_string()
}

fn invalid_argument(def: &Value, msg: &str) -> ToolError {
    let props: Vec<&str> = def["inputSchema"]["properties"].as_object().map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
    ToolError::new(
        "invalid_argument",
        msg,
        format!("call {} again with arguments like {}", def["name"].as_str().unwrap_or("the tool"), example_args(def)),
    )
    .with("accepted_arguments", json!(props))
}

fn dispatch(b: &mut dyn Backend, st: &mut CallState, name: &str, a: &Args, warnings: &mut Vec<String>) -> Result<Value, ToolError> {
    match name {
        "server_status" => Ok(status_value(b.snapshot().as_ref(), b.roots(), &st.settings, json!({"jobs": []}))),
        "list_jobs" => Ok(json!({"jobs": []})),
        "evict" => Ok(json!({"evicted": [], "note": "eviction exists under `gridcraft-cli mcp`"})),
        "job_status" if a.get("job_id").is_none() => Ok(json!({"jobs": []})),
        "job_status" => Err(ToolError::new(
            "not_found",
            "this server runs calls synchronously and has no jobs",
            "the result of every call is in its own answer; jobs exist under `gridcraft-cli mcp`",
        )),
        "debug_sleep" => {
            let ms = a.get("ms").and_then(Value::as_u64).unwrap_or(1000).min(600_000);
            // Sleeps in 5 ms steps, each a safe point for `cancel` (no clock: wasm-safe).
            let steps = ms.div_ceil(5);
            for k in 0..steps {
                gridcraft_engine::core::cancel::checkpoint();
                gridcraft_engine::core::progress::step_in("sleeping", k, steps);
                std::thread::sleep(std::time::Duration::from_millis(5));
            }
            Ok(json!({"slept_ms": ms}))
        }
        "cancel" => Ok(json!({"cancelled": false, "status": "not_running", "next_step": "no call is running: nothing to stop"})),
        "debug_panic" => provoked_panic(),
        "read_range" => read_range(b, st, a),
        "export_range" => export_range(b, a),
        "list_commands" => {
            let v = b.call("engine.commands", pick(a, &["search"])).map_err(|e| classify(b, name, a, &e))?;
            Ok(paged(v, a))
        }
        "list_functions" => {
            let v = exec(b, "formulas.functions", pick(a, &["search", "category"])).map_err(|e| classify(b, name, a, &e))?;
            Ok(paged(v, a))
        }
        "execute_command" => execute_command(b, a, warnings),
        _ => crate::tools::dispatch(b, name, a).map_err(|e| classify(b, name, a, &e)),
    }
}

/// The contract tests' way to prove a panic inside a call loses no work (test hooks only).
#[allow(clippy::panic)]
fn provoked_panic() -> Result<Value, ToolError> {
    panic!("debug_panic: provoked by the contract tests")
}

fn paged(v: Value, a: &Args) -> Value {
    let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = a.get("limit").and_then(Value::as_u64).map(|l| l as usize);
    contract::page(v, offset, limit)
}

/// execute_command with the command id matched in any spelling.
fn execute_command(b: &mut dyn Backend, a: &Args, warnings: &mut Vec<String>) -> Result<Value, ToolError> {
    let cmd = a.get("command").and_then(Value::as_str).unwrap_or("").to_string();
    let p = match a.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(ToolError::new("invalid_argument", "`params` must be an object", "pass params as {\"key\": value}")),
    };
    match exec(b, &cmd, p.clone()) {
        Ok(v) => Ok(v),
        Err(e) if e.contains("unknown command") => {
            let ids = command_ids(b);
            let sq = contract::squash(&cmd);
            if let Some(id) = ids.iter().find(|i| contract::squash(i) == sq) {
                warnings.push(format!("command `{cmd}` was read as `{id}`"));
                let id = id.clone();
                return exec(b, &id, p).map_err(|e| classify(b, "execute_command", a, &e));
            }
            Err(classify(b, "execute_command", a, &e))
        }
        Err(e) => Err(classify(b, "execute_command", a, &e)),
    }
}

fn command_ids(b: &mut dyn Backend) -> Vec<String> {
    b.call("engine.commands", json!({}))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect()
}

fn sheet_names(b: &mut dyn Backend) -> Vec<String> {
    b.call("document.inspect", json!({}))
        .ok()
        .and_then(|v| v["sheets"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect()
}

/// `Sheet2!A1:B3` → (`Some("Sheet2")`, `"A1:B3"`); quotes are removed from the sheet name.
pub(crate) fn split_sheet(r: &str) -> (Option<String>, &str) {
    match r.rsplit_once('!') {
        Some((s, rest)) => {
            let s = s.trim();
            let s = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).map(|x| x.replace("''", "'")).unwrap_or_else(|| s.to_string());
            (Some(s), rest)
        }
        None => (None, r),
    }
}

/// `Sheet 2` → `'Sheet 2'`; plain names as they are.
pub(crate) fn quote_sheet(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') && !s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// WHIQ: a range written as CSV (the bulk read): streamed by the engine, any size.
fn export_range(b: &mut dyn Backend, a: &Args) -> Result<Value, ToolError> {
    let path = a.get("path").and_then(Value::as_str).unwrap_or("").to_string();
    let given = a.get("range").and_then(Value::as_str).map(str::to_string);
    let (prefix, rest) = match &given {
        Some(g) => {
            let (s, r) = split_sheet(g);
            (s, Some(r.to_string()))
        }
        None => (None, None),
    };
    let sheet = prefix.or_else(|| a.get("sheet").and_then(Value::as_str).map(str::to_string));
    let info = b.call("document.inspect", json!({})).map_err(|e| classify(b, "export_range", a, &e))?;
    let sheets = info["sheets"].as_array().cloned().unwrap_or_default();
    let found = match &sheet {
        Some(w) => sheets.iter().find(|s| s["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(w))),
        None => info["activeSheet"].as_u64().and_then(|i| sheets.get(i as usize)),
    };
    let Some(found) = found else {
        let names: Vec<&str> = sheets.iter().filter_map(|s| s["name"].as_str()).collect();
        return Err(ToolError::new("not_found", format!("there is no sheet `{}`", sheet.unwrap_or_default()), "inspect_workbook lists the sheets")
            .with("sheets", json!(names)));
    };
    let used = found["usedRange"].as_str().map(str::to_string);
    let range = match rest.or(used) {
        Some(r) => r,
        None => return Ok(json!({"path": path, "rows": 0, "note": "the sheet holds no data"})),
    };
    let mut p = json!({"path": path, "range": range});
    if let Some(n) = found["name"].as_str() {
        p["sheet"] = json!(n);
    }
    let mut v = exec(b, "file.exportCsv", p).map_err(|e| classify(b, "export_range", a, &e))?;
    v["next_step"] = json!("read the CSV file (it holds the whole range)");
    Ok(v)
}

/// read_range that never floods the caller: at most `max_cells` cells per call; the answer says
/// `truncated`, `continue_from` (the first cell not returned) and `next_range` to ask for next.
fn read_range(b: &mut dyn Backend, st: &CallState, a: &Args) -> Result<Value, ToolError> {
    let cap = a.get("max_cells").and_then(Value::as_u64).map(|n| n.clamp(1, 1_000_000)).unwrap_or(st.settings.max_read_cells).max(1);
    let mut p = pick(a, &["range", "sheet", "formulas", "formatted"]);
    let given = a.get("range").and_then(Value::as_str).map(str::to_string);
    let (prefix, rest) = match &given {
        Some(g) => {
            let (s, r) = split_sheet(g);
            (s, r.to_string())
        }
        None => (None, String::new()),
    };
    let info = b.call("document.inspect", json!({})).map_err(|e| classify(b, "read_range", a, &e))?;
    let sheets = info["sheets"].as_array().cloned().unwrap_or_default();
    let wanted = prefix.clone().or_else(|| a.get("sheet").and_then(Value::as_str).map(str::to_string));
    let sheet = match &wanted {
        Some(w) => sheets.iter().find(|s| s["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(w))),
        None => info["activeSheet"].as_u64().and_then(|i| sheets.get(i as usize)),
    };
    let Some(sheet) = sheet else {
        let names: Vec<&str> = sheets.iter().filter_map(|s| s["name"].as_str()).collect();
        return Err(ToolError::new(
            "not_found",
            format!("there is no sheet `{}`", wanted.unwrap_or_default()),
            "use one of the sheets in `closest` (inspect_workbook lists them all)",
        )
        .with("closest", json!(contract::closest(&prefix.unwrap_or_default(), names.iter().copied(), 5)))
        .with("sheets", json!(names)));
    };
    let sheet_name = sheet["name"].as_str().unwrap_or("").to_string();
    let used = sheet["usedRange"].as_str().and_then(RangeRef::parse);
    let requested = if given.is_some() { RangeRef::parse(&rest) } else { used };
    let Some(mut r) = requested else {
        // Unparseable (the engine words the error) or an empty sheet.
        return b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e));
    };
    if r.count() > cap
        && let Some(u) = used
    {
        r = match r.intersection(&u) {
            Some(x) => x,
            None => return Ok(json!({"range": r.a1(), "sheet": sheet_name, "values": [], "note": "the range holds no data"})),
        };
    }
    if r.count() <= cap {
        return b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e));
    }
    let width = u64::from(r.width());
    let part = if width > cap {
        RangeRef::new(r.start, CellRef::new(r.start.row, r.start.col.saturating_add(cap as u32 - 1)))
    } else {
        let rows = (cap / width).max(1) as u32;
        RangeRef::new(r.start, CellRef::new(r.start.row.saturating_add(rows - 1).min(r.end.row), r.end.col))
    };
    let (next_start, next_range) = if width > cap {
        let c = CellRef::new(part.start.row, part.end.col + 1);
        (c, RangeRef::new(c, CellRef::new(part.start.row, r.end.col)))
    } else {
        let c = CellRef::new(part.end.row + 1, r.start.col);
        (c, RangeRef::new(c, r.end))
    };
    let q = quote_sheet(&sheet_name);
    p["range"] = json!(format!("{q}!{}", part.a1()));
    if let Some(o) = p.as_object_mut() {
        o.remove("sheet");
    }
    let mut v = b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e))?;
    let next = format!("{q}!{}", next_range.a1());
    v["truncated"] = json!(true);
    v["total_cells"] = json!(r.count());
    v["returned_cells"] = json!(part.count());
    v["continue_from"] = json!(format!("{q}!{}", next_start.a1()));
    v["next_range"] = json!(next);
    v["next_step"] = json!(format!("call read_range with range \"{next}\" for the next part"));
    Ok(v)
}

// ---------------------------------------------------------------- errors

/// Turns an engine or backend message into an error the agent can act on.
pub fn classify(b: &mut dyn Backend, tool: &str, a: &Args, msg: &str) -> ToolError {
    let m = msg.to_ascii_lowercase();
    let msg = format!("{tool}: {msg}");
    let path = a.get("path").and_then(Value::as_str).unwrap_or("");
    let (read_root, write_root) = b.roots();
    if m.contains("automation path rejected") || m.contains("automation filesystem access is not granted") {
        let root = if m.contains("read") { read_root.clone().or(write_root.clone()) } else { write_root.clone().or(read_root.clone()) };
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "book.xlsx".into());
        let next = match &root {
            Some(r) => format!("use a path inside {}: e.g. the relative name \"{name}\" resolves to {}", r.display(), r.join(&name).display()),
            None => "this server has no root for that kind of access; ask the operator".to_string(),
        };
        let mut e = ToolError::new("outside_root", msg, next);
        if let Some(r) = read_root {
            e = e.with("read_root", json!(r.display().to_string()));
        }
        if let Some(w) = write_root {
            e = e.with("write_root", json!(w.display().to_string()));
        }
        return e;
    }
    if m.starts_with("cancelled:") || m.contains(": cancelled:") {
        return ToolError::new("cancelled", msg, "the call was stopped as asked and changed nothing; run it again when you need it");
    }
    if m.contains("no ui in headless mode") {
        return ToolError::new(
            contract::code::UNSUPPORTED,
            msg,
            "use render_range or render_sheet for a picture of the cells, or connect to the desktop app",
        );
    }
    if m.contains(&gridcraft_engine::io::WRONG_PASSWORD.to_ascii_lowercase()) {
        return ToolError::new("wrong_password", msg, "call open_workbook again with the right `password`");
    }
    if m.contains("password-protected") {
        return ToolError::new("password_required", msg, "call open_workbook again with `password`");
    }
    if m.contains("unknown command") {
        let wanted = a.get("command").and_then(Value::as_str).unwrap_or("");
        let ids = command_ids(b);
        let near = contract::closest(wanted, ids.iter().map(String::as_str), 5);
        return ToolError::new("not_found", msg, "use one of the command ids in `closest`, or list_commands with a search word")
            .with("closest", json!(near));
    }
    if m.contains("unknown sheet") || m.contains("no sheet") || (m.contains("sheet") && m.contains("not found")) {
        let names = sheet_names(b);
        let wanted = a
            .get("sheet")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| a.get("range").or(a.get("cell")).and_then(Value::as_str).and_then(|r| split_sheet(r).0))
            .unwrap_or_default();
        let near = contract::closest(&wanted, names.iter().map(String::as_str), 5);
        return ToolError::new("not_found", msg, "use one of the sheet names in `sheets`").with("closest", json!(near)).with("sheets", json!(names));
    }
    if m.contains("no such file") || m.contains("os error 2") || m.contains("cannot find the file") || m.contains("cannot find the path") {
        let near = nearby_files(path);
        return ToolError::new(
            "not_found",
            msg,
            if near.is_empty() {
                "check the path; relative paths resolve inside the read root".to_string()
            } else {
                "did you mean one of the files in `closest`?".to_string()
            },
        )
        .with("closest", json!(near));
    }
    if m.contains("os error 32")
        || m.contains("os error 33")
        || m.contains("being used by another process")
        || m.contains("os error 5)")
        || m.contains("permission denied")
        || m.contains("access is denied")
    {
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "the file".into());
        return ToolError::new(
            "locked",
            msg,
            format!("{name} is open in another program or read-only: close it there and retry, or save under another name"),
        )
        .with("retry_after_ms", json!(5000));
    }
    if m.contains("no space left") || m.contains("os error 28") || m.contains("os error 112") || m.contains("disk full") {
        return ToolError::new(
            "disk_full",
            msg,
            "free space on the disk (or save to another drive inside the write root) and save again; the workbook is still open",
        );
    }
    if m.contains("no workbook open") {
        return ToolError::new("no_document", msg, "call new_workbook, open_workbook or build_workbook first");
    }
    if m.contains("is not available right now") {
        return ToolError::new("not_available", msg, "inspect_workbook shows the current state (selection, sheets); change it and try again");
    }
    // WHIQ: a limit of this process's memory says how to get more memory, not to ask for less.
    if m.contains("--max-memory-mb") {
        return ToolError::new(
            "too_large",
            msg,
            "the workbook needs more memory than this engine process may use: ask the administrator to raise the engine memory limit (WHIQ_ENGINE_PROC_MB), or split the workbook",
        );
    }
    if m.contains("too large") || m.contains("too many") || m.contains("too tall") || m.contains("larger than") || m.contains("read it in parts") {
        return ToolError::new("too_large", msg, "ask for a smaller part: a range like A1:D500, fewer rows, or fewer pages");
    }
    if m.contains("out of range") || m.contains("beyond the last") || m.contains("no page ") {
        return ToolError::new(
            contract::code::OUT_OF_RANGE,
            msg,
            "ask for rows, columns or pages that exist (inspect_workbook shows the used range)",
        );
    }
    if m.contains("internal error") {
        return ToolError::new(
            contract::code::INTERNAL,
            msg,
            "the workbook was kept as it was before this call; try again with simpler input or another way",
        );
    }
    if m.contains("not reachable") || m.contains("control channel closed") {
        return ToolError::new(contract::code::NOT_AVAILABLE, msg, "start the app with `gridcraft --control PORT`, or use the headless server");
    }
    if m.contains("invalid parameters")
        || m.contains("missing")
        || m.contains("must be")
        || m.contains("invalid")
        || m.contains("expected")
        || m.contains("not a valid")
        || m.contains("unknown")
        || m.contains("bad ")
    {
        let next = if tool == "execute_command" {
            let cmd = a.get("command").and_then(Value::as_str).unwrap_or("");
            format!("list_commands with search \"{cmd}\" shows the command's parameters; fix them and call again")
        } else {
            "fix the arguments named in the reason and call again (tools/list has each tool's schema)".to_string()
        };
        return ToolError::new("invalid_argument", msg, next);
    }
    ToolError::new("failed", msg, "check the arguments against the tool's schema; inspect_workbook shows the current state")
}

/// Files next to `path` whose names are close to its name (only inside the read root, if any).
fn nearby_files(path: &str) -> Vec<String> {
    if path.is_empty() {
        return vec![];
    }
    let p = Path::new(path);
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => match gridcraft_engine::file_access::current().and_then(|r| r.read_root().map(Path::to_path_buf)) {
            Some(r) => r,
            None => PathBuf::from("."),
        },
    };
    let dir_text = dir.to_string_lossy().into_owned();
    if gridcraft_engine::file_access::check_read(&dir_text).is_err() {
        return vec![];
    }
    let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { return vec![] };
    let names: Vec<String> = std::fs::read_dir(&dir)
        .map(|it| it.filter_map(|e| e.ok()).take(5000).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    contract::closest(&name, names.iter().map(String::as_str), 5).into_iter().map(|n| dir.join(n).display().to_string()).collect()
}

// ---------------------------------------------------------------- status

/// Resident memory of this process in bytes (Linux), else `None`.
pub fn rss_bytes() -> Option<u64> {
    // Resident set size on Linux, working set on Windows, resident size on macOS.
    if let Some(m) = memory_stats::memory_stats() {
        return Some(m.physical_mem as u64);
    }
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

/// The fonts exports embed (craft-fonts), in the shape every craft app reports: how many faces,
/// the families, the default Latin family and the Arabic fallback.
fn fonts_value() -> Value {
    let faces = gridcraft_engine::text::faces();
    let mut families: Vec<&str> = Vec::new();
    for f in faces {
        if !families.contains(&f.family) {
            families.push(f.family);
        }
    }
    let arabic = families.iter().find(|f| f.to_ascii_lowercase().contains("arabic")).copied();
    json!({"source": "craft-fonts", "faces": faces.len(), "families": families, "default": gridcraft_engine::text::resolve_family("Calibri"), "arabic": arabic})
}

/// The committed (virtual) size of this process as the operating system counts it.
fn committed_bytes() -> Option<u64> {
    memory_stats::memory_stats().map(|m| m.virtual_mem as u64).filter(|b| *b > 0)
}

/// Where the memory figures come from (the same words in every craft app).
fn memory_source() -> &'static str {
    if cfg!(target_os = "windows") {
        "GetProcessMemoryInfo: WorkingSetSize (rss), PagefileUsage (committed)"
    } else if cfg!(target_os = "macos") {
        "task_info(MACH_TASK_BASIC_INFO): resident_size (rss), virtual_size (committed)"
    } else if cfg!(target_os = "linux") {
        "/proc/self: resident set size (rss), virtual size (committed)"
    } else {
        "memory-stats"
    }
}

/// What server_status answers. `extra` adds the runtime's part (jobs, busy, limits).
pub fn status_value(snap: Option<&DocSnapshot>, roots: (Option<PathBuf>, Option<PathBuf>), settings: &Settings, extra: Value) -> Value {
    let stem = |t: &str| Path::new(t).file_stem().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| t.to_string());
    let doc: Vec<Value> = match snap {
        None => vec![],
        Some(s) => {
            let active = json!({
                "document": s.handle,
                "name": stem(&s.title),
                "title": s.title,
                "path": s.path,
                "dirty": s.dirty,
                "active": true,
                "sheets": s.sheets,
                "cells": s.cells,
            });
            // WHIQ: every open workbook with its handle, the active one first.
            std::iter::once(active)
                .chain(s.open.iter().filter(|o| !o.active).map(
                    |o| json!({"document": o.handle, "name": stem(&o.title), "title": o.title, "path": o.path, "dirty": o.dirty, "active": false}),
                ))
                .collect()
        }
    };
    let (read, write) = roots;
    let rss = rss_bytes();
    let mut v = json!({
        "server": "gridcraft",
        "version": env!("CARGO_PKG_VERSION"),
        "protocol": crate::server::PROTOCOL_VERSION,
        "pid": std::process::id(),
        "memory": {"rss_bytes": rss, "committed_bytes": committed_bytes(), "limit_bytes": Value::Null, "measured": rss.is_some(), "source": memory_source()},
        "fonts": fonts_value(),
        "documents": doc,
        "roots": {"read": read.map(|p| p.display().to_string()), "write": write.map(|p| p.display().to_string())},
        "author": settings.author,
        "current_call": Value::Null,
        "jobs": [],
        "recovered_files": [],
        "evicted": [],
        "limits": {"max_result_bytes": settings.max_result_bytes, "max_read_cells": settings.max_read_cells, "old_copies": settings.old_copies},
    });
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        for (k, x) in e {
            o.insert(k, x);
        }
    }
    v
}
