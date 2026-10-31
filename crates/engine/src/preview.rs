//! WHIQ: the workbook as self-contained HTML for a preview pane, drawn from GridCraft's own model
//! (`file.previewHtml`), and a sheet as Markdown tables for an agent to read (`sheet.markdown`).
//!
//! The page has the structure the platform's preview pane already shows (a file name line
//! `.head`, sheet tabs `.bar` / `.tab` / `.tab.on`, one `.sheet` per tab with `id="s<i>"`, charts
//! in `.chart`, notes in `.cap`), with the cells drawn as the sheet shows them: values with their
//! number formats, fills, fonts, borders, alignment, merged cells, column widths, row heights,
//! frozen panes (sticky), conditional formats (fills, colour scales, data bars, icons), table
//! styles, sparklines, and charts and pictures where they sit on the sheet (charts as inline SVG
//! from the same primitives the PNG and PDF painters draw). A formula is on its cell
//! (`data-f`). Nothing is fetched: no fonts, scripts or images from outside.
//!
//! There is no row or column cap on the workbook: a call draws a **window** of one sheet (`sheet`,
//! `row_start`, `row_count`, `col_start`, `col_count`, 1-based) and answers with the window
//! drawn and every sheet's used rows and columns, so a pane pages as the user scrolls. A window
//! larger than `max_cells` comes back in part, with the next window to ask for.

use std::fmt::Write as _;

use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef, Value, col_to_letters};
use gridcraft_model::{BorderLine, BorderStyle, HAlign, Sheet, Underline, VAlign, VertAlign, Visibility, Workbook};
use serde_json::{Value as Json, json};

use crate::cmd::print::{Look, anchor_rect, cell_look, numfmt_rgb, rgb_of};

/// Rows of the first screenful (the window when the call names none).
pub const DEFAULT_ROWS: u32 = 100;
/// Columns of the first screenful.
pub const DEFAULT_COLS: u32 = 30;
/// Cells one window draws at most unless the call asks for more (`max_cells`).
pub const DEFAULT_MAX_CELLS: u64 = 100_000;
/// The most `max_cells` may be.
pub const MAX_WINDOW_CELLS: u64 = 2_000_000;
/// Cells drawn for the first screenful of the other sheets of a page, all together.
const OTHER_SHEETS_CELLS: u64 = 60_000;
/// A sheet whose cells are on disk and would take more memory than this is not read for the
/// first screenful of a page that is about another sheet (its tab asks for it).
const COLD_SHEET_BYTES: u64 = 16 << 20;
/// Bytes of pictures one call embeds.
const MAX_IMAGE_BYTES: usize = 8 << 20;
/// Width of the row-number column and height of the column-letter row (pixels).
const HEAD_W: f32 = 40.0;
const HEAD_H: f32 = 20.0;

/// What the preview returns.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    /// A whole self-contained page: tabs for every visible sheet, the window's sheet shown.
    Page,
    /// The window's `<div class="sheet on">` alone, to swap into a page.
    Sheet,
    /// The window's body rows (`<tr>`…) alone, to append under a table drawn earlier with the
    /// same columns (no headings, frozen panes, charts or pictures).
    Rows,
}

impl Format {
    pub fn parse(s: &str) -> Option<Format> {
        match s.trim().to_ascii_lowercase().as_str() {
            "page" | "html" | "document" | "" => Some(Format::Page),
            "sheet" | "fragment" | "div" => Some(Format::Sheet),
            "rows" | "tr" | "body" => Some(Format::Rows),
            _ => None,
        }
    }
    fn name(self) -> &'static str {
        match self {
            Format::Page => "page",
            Format::Sheet => "sheet",
            Format::Rows => "rows",
        }
    }
}

/// The window and options of one preview call. Rows and columns are 0-based here.
#[derive(Clone, Debug)]
pub struct Options {
    pub sheet: Option<usize>,
    pub row_start: Option<u32>,
    pub row_count: Option<u32>,
    pub col_start: Option<u32>,
    pub col_count: Option<u32>,
    pub format: Format,
    /// Row numbers and column letters (default true).
    pub headings: bool,
    /// The other sheets' first screenful in a page (default true); off, their tabs are empty
    /// until asked for.
    pub other_sheets: bool,
    pub max_cells: u64,
    /// Name shown on the page (the file name).
    pub title: String,
}

impl Default for Options {
    fn default() -> Self {
        Options {
            sheet: None,
            row_start: None,
            row_count: None,
            col_start: None,
            col_count: None,
            format: Format::Page,
            headings: true,
            other_sheets: true,
            max_cells: DEFAULT_MAX_CELLS,
            title: String::new(),
        }
    }
}

pub(crate) fn esc(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

fn hex(c: [u8; 3]) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

fn px(v: f64) -> String {
    let r = (v * 10.0).round() / 10.0;
    if r == r.trunc() { format!("{}", r as i64) } else { format!("{r}") }
}

const CSS: &str = "
html,body{height:100%}
body{margin:0;font-family:'Segoe UI',Arial,sans-serif;background:#f4f3f0;color:#1c1c1c;display:flex;flex-direction:column}
.head{font:12px Consolas,monospace;color:#666;padding:8px 12px 0}
.bar{display:flex;gap:4px;padding:8px 10px 0;flex-wrap:wrap;background:#e8e6e1;border-bottom:1px solid #ccc;position:sticky;top:0;z-index:5}
.tab{padding:6px 14px;font-size:13px;border:1px solid #ccc;border-bottom:none;border-radius:6px 6px 0 0;background:#dcdad5;cursor:pointer}
.tab.on{background:#fff;font-weight:600}
.sheet{display:none;padding:12px;overflow:auto;flex:1;min-height:0}
.sheet.on{display:block}
.grid{position:relative;display:inline-block}
table.gc{border-collapse:collapse;table-layout:fixed;background:#fff}
table.gc td{border:1px solid #e1dfda;padding:0 3px;white-space:nowrap;overflow:hidden;vertical-align:bottom;line-height:1.15;box-sizing:border-box}
table.gc.nogrid td{border-color:transparent}
table.gc td.ov{overflow:visible}
table.gc td.wrap{white-space:pre-wrap;overflow-wrap:anywhere}
table.gc td.note{background-image:linear-gradient(225deg,#c00 4px,transparent 4px)}
table.gc th{background:#efede8;font:600 11px 'Segoe UI',Arial,sans-serif;color:#555;border:1px solid #d5d2cb;padding:0 2px;text-align:center;position:sticky;top:0;z-index:3;overflow:hidden;white-space:nowrap}
table.gc th.rh{position:sticky;left:0;z-index:2}
table.gc th.corner{left:0;z-index:4}
table.gc tbody th{top:auto}
table.gc svg.spark{display:block}
.chart{margin:10px 0 16px;padding:8px;background:#fff;border:1px solid #d5d2cb;border-radius:6px}
.obj{position:absolute;margin:0;padding:0;border:none;border-radius:0;background:none;overflow:hidden;z-index:1}
.obj svg{display:block}
.obj img{display:block;width:100%;height:100%}
.chart-note{font:11px Consolas,monospace;color:#a66f00;padding:4px 2px 0}
.chart-missing{margin:10px 0;padding:8px 10px;background:#fff8e6;border:1px solid #e8d9a8;border-radius:6px;font:12px Consolas,monospace;color:#7a5c00}
.cap{margin:0 0 8px;font:12px Consolas,monospace;color:#a66f00}
.more{margin:8px 0;font:12px Consolas,monospace;color:#666}
";

/// Tab switching, and the hooks a pane can listen to: `{source: \"gridcraft-preview\", type:
/// \"sheet\"}` when a tab is shown, `type: \"more\"` when the shown sheet is scrolled near the end
/// of its window (with the next window to ask for).
const SCRIPT: &str = "
function show(i){document.querySelectorAll('.sheet').forEach(function(s,j){s.classList.toggle('on', j===i);});document.querySelectorAll('.tab').forEach(function(t,j){t.classList.toggle('on', j===i);});var s=document.getElementById('s'+i);post('sheet',s);}
function post(type,s){try{if(s&&window.parent&&window.parent!==window){window.parent.postMessage({source:'gridcraft-preview',type:type,sheet:s.dataset.sheet,index:+s.dataset.index,drawn:s.dataset.drawn==='1',row_start:+s.dataset.rowStart,row_end:+s.dataset.rowEnd,rows:+s.dataset.rows,col_start:+s.dataset.colStart,col_end:+s.dataset.colEnd,columns:+s.dataset.cols},'*');}}catch(e){}}
document.querySelectorAll('.sheet').forEach(function(s){var asked=0;s.addEventListener('scroll',function(){if(s.scrollTop+s.clientHeight>=s.scrollHeight-200&&+s.dataset.rowEnd<+s.dataset.rows&&asked!==+s.dataset.rowEnd){asked=+s.dataset.rowEnd;post('more',s);}});});
";

/// One sheet's numbers for the answer.
fn sheet_meta(wb: &Workbook, i: usize, sh: &Sheet, exact: bool) -> Json {
    let used = if exact || sh.cells.is_resident() { sh.used_range() } else { sh.used_range_hint() };
    let visible = match sh.visibility {
        Visibility::Visible => "visible",
        Visibility::Hidden => "hidden",
        Visibility::VeryHidden => "veryHidden",
    };
    json!({
        "index": i,
        "name": sh.name,
        "visibility": visible,
        "used_range": used.map(|r| r.a1()),
        "rows": used.map_or(0, |r| r.end.row + 1),
        "columns": used.map_or(0, |r| r.end.col + 1),
        "first_row": used.map(|r| r.start.row + 1),
        "first_column": used.map(|r| r.start.col + 1),
        "cells": sh.cells.len(),
        "frozen": sh.freeze.filter(|f| f.0 > 0 || f.1 > 0).map(|f| json!({"rows": f.0, "columns": f.1})),
        "charts": sh.charts.len(),
        "images": sh.images.len(),
        "right_to_left": sh.right_to_left,
        "in_memory": sh.cells.is_resident(),
        "protected": sh.protection.is_some(),
        "protection": crate::cmd::inspect::protection_summary(wb, sh),
    })
}

/// The last row and column (exclusive) that hold data or a drawing object.
fn extent(sh: &Sheet, exact: bool) -> (u32, u32) {
    let used = if exact { sh.used_range() } else { sh.used_range_hint() };
    let (mut rows, mut cols) = used.map_or((0, 0), |r| (r.end.row + 1, r.end.col + 1));
    let anchors = sh.charts.iter().map(|c| &c.anchor).chain(sh.images.iter().map(|i| &i.anchor)).chain(sh.shapes.iter().map(|s| &s.anchor));
    for a in anchors {
        let (x, y, w, h) = anchor_rect(sh, a);
        rows = rows.max(sh.row_at(y + h).saturating_add(1).min(MAX_ROWS));
        cols = cols.max(sh.col_at(x + w).saturating_add(1).min(MAX_COLS));
    }
    for sp in &sh.sparklines {
        rows = rows.max(sp.cell.row + 1);
        cols = cols.max(sp.cell.col + 1);
    }
    // A merge reaching past the last value is drawn whole.
    for m in &sh.merges {
        rows = rows.max(m.end.row.saturating_add(1).min(MAX_ROWS));
        cols = cols.max(m.end.col.saturating_add(1).min(MAX_COLS));
    }
    (rows, cols)
}

/// The window drawn, in sheet coordinates (0-based, inclusive ends).
#[derive(Clone, Copy, Debug)]
struct Window {
    r0: u32,
    r1: u32,
    c0: u32,
    c1: u32,
    /// The window asked for was cut to `max_cells`.
    cut: bool,
    /// The sheet holds nothing in the window asked for.
    empty: bool,
}

fn window(sh: &Sheet, o: &Options, max_cells: u64, defaults: (u32, u32)) -> Window {
    let (rows, cols) = extent(sh, true);
    let r0 = o.row_start.unwrap_or(0).min(MAX_ROWS - 1);
    let c0 = o.col_start.unwrap_or(0).min(MAX_COLS - 1);
    let empty_sheet = rows == 0 && cols == 0;
    // Asked counts are taken as they are, but never past the data (a window over nothing is
    // one empty row); defaults are a screenful of what is there.
    let want_rows = match o.row_count {
        Some(n) => n.max(1).min(rows.saturating_sub(r0).max(1)),
        None if empty_sheet => 20,
        None => rows.saturating_sub(r0).clamp(1, defaults.0),
    };
    let want_cols = match o.col_count {
        Some(n) => n.max(1).min(cols.saturating_sub(c0).max(1)),
        None if empty_sheet => 10,
        None => cols.saturating_sub(c0).clamp(1, defaults.1),
    };
    let want_rows = want_rows.min(MAX_ROWS - r0);
    let want_cols = want_cols.min(MAX_COLS - c0);
    let max_cells = max_cells.max(1);
    let mut cut = false;
    let cols_n = u64::from(want_cols).min(max_cells) as u32;
    if cols_n < want_cols {
        cut = true;
    }
    let rows_n = (max_cells / u64::from(cols_n.max(1))).clamp(1, u64::from(want_rows)) as u32;
    if rows_n < want_rows {
        cut = true;
    }
    Window { r0, r1: r0 + rows_n - 1, c0, c1: c0 + cols_n - 1, cut, empty: !empty_sheet && (r0 >= rows || c0 >= cols) }
}

fn border_css(b: &BorderLine, wb: &Workbook) -> Option<String> {
    if b.is_none() {
        return None;
    }
    let (w, style) = match b.style {
        BorderStyle::None => return None,
        BorderStyle::Thin => (1, "solid"),
        BorderStyle::Medium => (2, "solid"),
        BorderStyle::Thick => (3, "solid"),
        BorderStyle::Dashed | BorderStyle::DashDot => (1, "dashed"),
        BorderStyle::Dotted | BorderStyle::Hair | BorderStyle::DashDotDot => (1, "dotted"),
        BorderStyle::Double => (3, "double"),
        BorderStyle::MediumDashed | BorderStyle::MediumDashDot | BorderStyle::SlantDashDot => (2, "dashed"),
        BorderStyle::MediumDashDotDot => (2, "dotted"),
    };
    let color = rgb_of(b.color, wb).unwrap_or([0, 0, 0]);
    Some(format!("{w}px {style} {}", hex(color)))
}

/// A cell's own border on one side, else its neighbour's on the shared edge, so both cells of an
/// edge carry the same line and the collapsed border is the one the sheet draws (a gridline
/// never wins over a drawn border).
#[derive(Clone, Copy)]
enum Side {
    Top,
    Bottom,
    Left,
    Right,
}

fn edge(wb: &Workbook, sh: &Sheet, c: CellRef, side: Side) -> BorderLine {
    let own = wb.styles.get(sh.style_id(c)).borders;
    let mine = match side {
        Side::Top => own.top,
        Side::Bottom => own.bottom,
        Side::Left => own.left,
        Side::Right => own.right,
    };
    if !mine.is_none() {
        return mine;
    }
    let other = match side {
        Side::Top => c.row.checked_sub(1).map(|r| CellRef::new(r, c.col)),
        Side::Bottom => (c.row + 1 < MAX_ROWS).then(|| CellRef::new(c.row + 1, c.col)),
        Side::Left => c.col.checked_sub(1).map(|k| CellRef::new(c.row, k)),
        Side::Right => (c.col + 1 < MAX_COLS).then(|| CellRef::new(c.row, c.col + 1)),
    };
    let Some(o) = other else { return mine };
    let ob = wb.styles.get(sh.style_id(o)).borders;
    match side {
        Side::Top => ob.bottom,
        Side::Bottom => ob.top,
        Side::Left => ob.right,
        Side::Right => ob.left,
    }
}

/// The icon a conditional format's icon set shows, as a coloured character.
fn icon_html(set: &str, idx: usize) -> String {
    let s = set.to_ascii_lowercase();
    let k = gridcraft_model::icon_count(set).max(1);
    let level = if k > 1 { idx as f32 / (k - 1) as f32 } else { 1.0 };
    let color = if level >= 0.66 {
        "#2e8b3d"
    } else if level >= 0.33 {
        "#d4a017"
    } else {
        "#c0392b"
    };
    let ch = if s.contains("arrow") {
        if level >= 0.66 {
            "\u{2191}"
        } else if level >= 0.33 {
            "\u{2192}"
        } else {
            "\u{2193}"
        }
    } else if s.contains("flag") {
        "\u{2691}"
    } else if s.contains("star") {
        "\u{2605}"
    } else if s.contains("triangle") {
        "\u{25B2}"
    } else {
        "\u{25CF}"
    };
    format!("<span style=\"color:{color};float:left;padding-right:3px\">{ch}</span>")
}

/// Renders one sheet window: the `<div class="sheet">` (or only its rows).
struct SheetDraw<'a> {
    wb: &'a Workbook,
    si: usize,
    sh: &'a Sheet,
    rows: Vec<u32>,
    cols: Vec<u32>,
    /// Frozen rows / columns drawn sticky (the first `frozen_rows` of `rows`).
    frozen_rows: usize,
    frozen_cols: usize,
    headings: bool,
    default_font: (String, f32),
}

impl SheetDraw<'_> {
    /// Visible rows and columns of the window, the frozen ones first (also when the window is
    /// scrolled past them, as the sheet keeps them in view).
    fn new<'a>(wb: &'a Workbook, si: usize, sh: &'a Sheet, w: &Window, headings: bool) -> SheetDraw<'a> {
        let (fr, fc) = sh.freeze.unwrap_or((0, 0));
        let mut rows: Vec<u32> = (0..fr.min(w.r0)).filter(|r| !sh.is_row_hidden(*r)).collect();
        let mut cols: Vec<u32> = (0..fc.min(w.c0)).filter(|c| !sh.is_col_hidden(*c)).collect();
        let pre_rows = rows.len();
        let pre_cols = cols.len();
        rows.extend((w.r0..=w.r1).filter(|r| !sh.is_row_hidden(*r)));
        cols.extend((w.c0..=w.c1).filter(|c| !sh.is_col_hidden(*c)));
        let frozen_rows = pre_rows + (w.r0..fr.min(w.r1 + 1)).filter(|r| !sh.is_row_hidden(*r)).count();
        let frozen_cols = pre_cols + (w.c0..fc.min(w.c1 + 1)).filter(|c| !sh.is_col_hidden(*c)).count();
        let base = wb.styles.get(gridcraft_model::StyleId::DEFAULT);
        SheetDraw { wb, si, sh, rows, cols, frozen_rows, frozen_cols, headings, default_font: (base.font.name.clone(), base.font.size) }
    }

    fn head_w(&self) -> f32 {
        if self.headings { HEAD_W } else { 0.0 }
    }
    fn head_h(&self) -> f32 {
        if self.headings { HEAD_H } else { 0.0 }
    }

    /// Table x of the left edge of each drawn column, plus the right edge of the last.
    fn xs(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.cols.len() + 1);
        let mut x = self.head_w();
        v.push(x);
        for c in &self.cols {
            x += self.sh.col_width(*c);
            v.push(x);
        }
        v
    }
    fn ys(&self) -> Vec<f32> {
        let mut v = Vec::with_capacity(self.rows.len() + 1);
        let mut y = self.head_h();
        v.push(y);
        for r in &self.rows {
            y += self.sh.row_height(*r);
            v.push(y);
        }
        v
    }

    /// The merges that touch the drawn cells, cut to them.
    fn merges(&self) -> Vec<RangeRef> {
        let (Some(r0), Some(r1), Some(c0), Some(c1)) = (self.rows.first(), self.rows.last(), self.cols.first(), self.cols.last()) else {
            return vec![];
        };
        let all = RangeRef::new(CellRef::new(*r0, *c0), CellRef::new(*r1, *c1));
        self.sh.merges.iter().filter(|m| m.intersects(&all) && m.count() > 1).copied().collect()
    }

    fn table_open(&self, out: &mut String) {
        let xs = self.xs();
        let width = xs.last().copied().unwrap_or(0.0);
        let (font, size) = &self.default_font;
        let grid = if self.sh.show_gridlines { "" } else { " nogrid" };
        let dir = if self.sh.right_to_left { " dir=\"rtl\"" } else { "" };
        let _ = write!(
            out,
            "<table class=\"gc{grid}\"{dir} style=\"width:{}px;font-family:'{}',Carlito,Calibri,Arial,sans-serif;font-size:{}pt\"><colgroup>",
            px(f64::from(width)),
            esc(font),
            px(f64::from(*size))
        );
        if self.headings {
            let _ = write!(out, "<col style=\"width:{}px\">", px(f64::from(HEAD_W)));
        }
        for c in &self.cols {
            let _ = write!(out, "<col style=\"width:{}px\">", px(f64::from(self.sh.col_width(*c))));
        }
        out.push_str("</colgroup>");
        if self.headings {
            let _ = write!(out, "<thead><tr style=\"height:{}px\"><th class=\"corner\"></th>", px(f64::from(HEAD_H)));
            let xs = self.xs();
            for (i, c) in self.cols.iter().enumerate() {
                let sticky = if i < self.frozen_cols {
                    format!(" style=\"left:{}px;z-index:4\"", px(f64::from(xs.get(i).copied().unwrap_or(0.0))))
                } else {
                    String::new()
                };
                let _ = write!(out, "<th{sticky}>{}</th>", col_to_letters(*c));
            }
            out.push_str("</tr></thead>");
        }
        out.push_str("<tbody>");
    }

    /// The `<tr>` rows. Returns the cells drawn.
    fn body(&self, out: &mut String, merges: &[RangeRef]) -> u64 {
        let wb = self.wb;
        let sh = self.sh;
        let xs = self.xs();
        let ys = self.ys();
        let mut cf = if sh.cond_formats.is_empty() { None } else { Some(crate::cf::CfCache::new()) };
        let drawn_rows: std::collections::HashSet<u32> = self.rows.iter().copied().collect();
        let drawn_cols: std::collections::HashSet<u32> = self.cols.iter().copied().collect();
        let mut cells = 0u64;
        let (font, size) = &self.default_font;
        for (ri, &row) in self.rows.iter().enumerate() {
            gridcraft_core::cancel::checkpoint();
            gridcraft_core::progress::step_in("drawing the preview", ri as u64, self.rows.len() as u64);
            let frozen_row = ri < self.frozen_rows;
            let _ = write!(out, "<tr data-r=\"{}\" style=\"height:{}px\">", row + 1, px(f64::from(sh.row_height(row))));
            if self.headings {
                let top = if frozen_row { format!(";top:{}px;z-index:4", px(f64::from(ys.get(ri).copied().unwrap_or(0.0)))) } else { String::new() };
                let _ = write!(out, "<th class=\"rh\" style=\"left:0{top}\">{}</th>", row + 1);
            }
            for (ci, &col) in self.cols.iter().enumerate() {
                let c = CellRef::new(row, col);
                let mut span = String::new();
                let mut rect_cell = c;
                if let Some(m) = merges.iter().find(|m| m.contains(c)) {
                    // A merge is drawn once, from its first drawn cell, across the drawn rows and
                    // columns it covers.
                    let first_row = self.rows.iter().find(|r| **r >= m.start.row && **r <= m.end.row).copied();
                    let first_col = self.cols.iter().find(|k| **k >= m.start.col && **k <= m.end.col).copied();
                    if first_row != Some(row) || first_col != Some(col) {
                        continue;
                    }
                    let rs = (m.start.row..=m.end.row).filter(|r| drawn_rows.contains(r)).count();
                    let cs = (m.start.col..=m.end.col).filter(|k| drawn_cols.contains(k)).count();
                    if rs > 1 {
                        let _ = write!(span, " rowspan=\"{rs}\"");
                    }
                    if cs > 1 {
                        let _ = write!(span, " colspan=\"{cs}\"");
                    }
                    rect_cell = m.start;
                }
                cells += 1;
                let merge = merges.iter().find(|m| m.contains(c)).copied();
                let look = cell_look(wb, self.si, sh, rect_cell, &mut cf);
                let mut class: Vec<&str> = vec![];
                let mut st = String::new();
                self.style_css(&look, rect_cell, merge, &mut st, font, *size);
                let frozen_col = ci < self.frozen_cols;
                if frozen_row || frozen_col {
                    st.push_str("position:sticky;");
                    if frozen_row {
                        let _ = write!(st, "top:{}px;", px(f64::from(ys.get(ri).copied().unwrap_or(0.0))));
                    }
                    if frozen_col {
                        let _ = write!(st, "left:{}px;", px(f64::from(xs.get(ci).copied().unwrap_or(0.0))));
                    }
                    let _ = write!(st, "z-index:{};", if frozen_row && frozen_col { 3 } else { 2 });
                    if look.fill.is_none() {
                        st.push_str("background-color:#fff;");
                    }
                }
                let v = sh.value(rect_cell);
                let hide = look.cf.as_ref().is_some_and(|x| x.hide_value) || (!sh.show_zeros && v.as_f64() == Some(0.0) && !sh.show_formulas);
                let text = if hide { String::new() } else { crate::display::cell_text(wb, sh, rect_cell) };
                let style = &look.style;
                if style.align.wrap {
                    class.push("wrap");
                }
                // Text spills into an empty neighbour, as on the sheet.
                if !style.align.wrap && merge.is_none() && !text.is_empty() && !matches!(v, Value::Number(_)) {
                    let next = if sh.right_to_left { col.checked_sub(1) } else { Some(col + 1) };
                    if next.is_some_and(|n| n < MAX_COLS && sh.value_ref(CellRef::new(row, n)).is_none_or(Value::is_empty)) {
                        class.push("ov");
                    }
                }
                if sh.comments.contains_key(&rect_cell) {
                    class.push("note");
                }
                out.push_str("<td");
                if !class.is_empty() {
                    let _ = write!(out, " class=\"{}\"", class.join(" "));
                }
                if !span.is_empty() {
                    out.push_str(&span);
                }
                if !st.is_empty() {
                    let _ = write!(out, " style=\"{}\"", esc(&st));
                }
                if !text.is_empty() || sh.cell(rect_cell).is_some_and(|x| x.formula.is_some()) {
                    let _ = write!(out, " data-c=\"{}\"", rect_cell.a1());
                }
                if let Some(f) = sh.cell(rect_cell).and_then(|x| x.formula.as_ref())
                    && !crate::display::formula_hidden(wb, sh, rect_cell)
                {
                    let _ = write!(out, " data-f=\"={}\"", esc(&f.text));
                }
                if let Some(note) = sh.comments.get(&rect_cell) {
                    let _ = write!(out, " title=\"{}\"", esc(&note.text));
                }
                out.push('>');
                if let Some((set, idx)) = look.cf.as_ref().and_then(|x| x.icon.as_ref()) {
                    out.push_str(&icon_html(set, *idx));
                }
                if let Some(sp) = sh.sparklines.iter().find(|s| s.cell == rect_cell) {
                    let vals: Vec<Option<f64>> = match gridcraft_calc::evaluate(wb, self.si, sp.cell, &sp.source) {
                        Value::Array(a) => a.data.iter().take(10_000).map(Value::as_f64).collect(),
                        v => vec![v.as_f64()],
                    };
                    let color = rgb_of(sp.color, wb).unwrap_or([0x15, 0x60, 0x82]);
                    let (w, h) = (sh.col_width(col).max(1.0), sh.row_height(row).max(1.0));
                    let prims = gridcraft_chart::render_sparkline(sp.kind, &vals, [color[0], color[1], color[2], 255], sp.markers, w, h);
                    out.push_str(&gridcraft_chart::to_svg(&prims, w, h, "spark"));
                } else if !text.is_empty() {
                    let shown = if style.align.wrap { text } else { text.replace(['\r', '\n'], " ") };
                    let shown = esc(&shown);
                    match sh.hyperlinks.get(&rect_cell).map(|l| l.target.as_str()).filter(|t| safe_link(t)) {
                        Some(t) => {
                            let _ = write!(out, "<a href=\"{}\" target=\"_blank\" rel=\"noopener\">{shown}</a>", esc(t));
                        }
                        None => out.push_str(&shown),
                    }
                }
                out.push_str("</td>");
            }
            out.push_str("</tr>");
        }
        cells
    }

    fn style_css(&self, look: &Look, c: CellRef, merge: Option<RangeRef>, st: &mut String, font: &str, size: f32) {
        let wb = self.wb;
        let sh = self.sh;
        let style = &look.style;
        let v = sh.value(c);
        if let Some(f) = look.fill {
            let _ = write!(st, "background-color:{};", hex(f));
        }
        if let Some((frac, rgb, _)) = look.cf.as_ref().and_then(|x| x.bar) {
            let pct = (frac.clamp(0.0, 1.0) * 100.0).round();
            let _ = write!(
                st,
                "background-image:linear-gradient(to right,{c} {pct}%,transparent {pct}%);background-size:100% 70%;background-position:center;background-repeat:no-repeat;",
                c = hex(rgb)
            );
        }
        let fmt = crate::display::format(&v, style.num_fmt.as_str(), wb);
        let color = fmt.color.map(numfmt_rgb).or_else(|| rgb_of(style.font.color, wb));
        if let Some(c) = color {
            let _ = write!(st, "color:{};", hex(c));
        }
        if !style.font.name.is_empty() && !style.font.name.eq_ignore_ascii_case(font) {
            let _ = write!(st, "font-family:'{}',Carlito,Calibri,Arial,sans-serif;", style.font.name.replace(['\'', '"', ';', '<', '>'], ""));
        }
        if (style.font.size - size).abs() > 0.01 && style.font.size.is_finite() {
            let _ = write!(st, "font-size:{}pt;", px(f64::from(style.font.size.clamp(1.0, 409.0))));
        }
        if style.font.bold {
            st.push_str("font-weight:700;");
        }
        if style.font.italic {
            st.push_str("font-style:italic;");
        }
        let mut deco = vec![];
        match style.font.underline {
            Underline::None => {}
            Underline::Double | Underline::DoubleAccounting => deco.push("underline double"),
            _ => deco.push("underline"),
        }
        if style.font.strike {
            deco.push("line-through");
        }
        if !deco.is_empty() {
            let _ = write!(st, "text-decoration:{};", deco.join(" "));
        }
        match style.font.vert {
            VertAlign::Superscript => st.push_str("vertical-align:super;font-size:smaller;"),
            VertAlign::Subscript => st.push_str("vertical-align:sub;font-size:smaller;"),
            VertAlign::Baseline => {}
        }
        // Horizontal alignment as the painter decides it (General: numbers right, booleans and
        // errors centred, text from its reading start).
        let is_num = matches!(v, Value::Number(_));
        let text_rtl = match style.align.reading_order {
            1 => false,
            2 => true,
            _ => match &v {
                Value::Text(t) => gridcraft_text::base_direction(t).is_some_and(|d| d == gridcraft_text::Dir::Rtl),
                _ => false,
            },
        };
        let align = match style.align.h {
            HAlign::General if is_num => Some("right"),
            HAlign::General if matches!(v, Value::Bool(_) | Value::Error(_)) => Some("center"),
            HAlign::General if text_rtl != sh.right_to_left => Some(if sh.right_to_left { "left" } else { "right" }),
            HAlign::General => None,
            HAlign::Left => Some("left"),
            HAlign::Right => Some("right"),
            HAlign::Center | HAlign::CenterAcross | HAlign::Distributed => Some("center"),
            HAlign::Justify => Some("justify"),
            HAlign::Fill => Some("left"),
        };
        if let Some(a) = align {
            let _ = write!(st, "text-align:{a};");
        }
        if text_rtl && !sh.right_to_left {
            st.push_str("direction:rtl;");
        }
        match style.align.v {
            VAlign::Bottom => {}
            VAlign::Top => st.push_str("vertical-align:top;"),
            VAlign::Center | VAlign::Justify | VAlign::Distributed => st.push_str("vertical-align:middle;"),
        }
        if style.align.indent > 0 {
            let side = if style.align.h == HAlign::Right { "right" } else { "left" };
            let _ = write!(st, "padding-{side}:{}px;", 3 + u32::from(style.align.indent) * 9);
        }
        match style.align.rotation {
            90 => st.push_str("writing-mode:vertical-rl;transform:rotate(180deg);"),
            -90 => st.push_str("writing-mode:vertical-rl;"),
            255 => st.push_str("writing-mode:vertical-rl;text-orientation:upright;"),
            _ => {}
        }
        // Borders: a merge takes its outer edges from the cells on them.
        let (top_c, bottom_c, left_c, right_c) = match merge {
            Some(m) => (m.start, CellRef::new(m.end.row, m.start.col), m.start, CellRef::new(m.start.row, m.end.col)),
            None => (c, c, c, c),
        };
        let edges = [
            ("top", edge(wb, sh, top_c, Side::Top)),
            ("bottom", edge(wb, sh, bottom_c, Side::Bottom)),
            ("left", edge(wb, sh, left_c, Side::Left)),
            ("right", edge(wb, sh, right_c, Side::Right)),
        ];
        let cf_borders = look.style.borders;
        for (name, mut b) in edges {
            // A conditional format's border on the cell wins.
            let own = match name {
                "top" => cf_borders.top,
                "bottom" => cf_borders.bottom,
                "left" => cf_borders.left,
                _ => cf_borders.right,
            };
            if !own.is_none() && merge.is_none() {
                b = own;
            }
            if let Some(css) = border_css(&b, wb) {
                let _ = write!(st, "border-{name}:{css};");
            }
        }
    }

    /// Charts, pictures and shapes that overlap the drawn cells, placed where they sit.
    fn objects(&self, out: &mut String, image_budget: &mut usize, notes: &mut Vec<String>) {
        let sh = self.sh;
        let wb = self.wb;
        let (Some(&r_first), Some(&c_first)) =
            (self.rows.get(self.frozen_rows).or(self.rows.first()), self.cols.get(self.frozen_cols).or(self.cols.first()))
        else {
            return;
        };
        let (Some(&r_last), Some(&c_last)) = (self.rows.last(), self.cols.last()) else { return };
        let xs = self.xs();
        let ys = self.ys();
        // The scrolling part of the window, in sheet pixels, and where it starts on the table.
        let (sx0, sy0) = (sh.col_left(c_first), sh.row_top(r_first));
        let (sx1, sy1) = (sh.col_left(c_last + 1), sh.row_top(r_last + 1));
        let tx0 = f64::from(xs.get(self.frozen_cols).copied().unwrap_or(self.head_w()));
        let ty0 = f64::from(ys.get(self.frozen_rows).copied().unwrap_or(self.head_h()));
        let table_w = f64::from(xs.last().copied().unwrap_or(0.0));
        let visible = |r: (f64, f64, f64, f64)| r.2 > 0.0 && r.3 > 0.0 && r.0 < sx1 && r.0 + r.2 > sx0 && r.1 < sy1 && r.1 + r.3 > sy0;
        let place = |r: (f64, f64, f64, f64)| -> String {
            let x = tx0 + (r.0 - sx0);
            let y = ty0 + (r.1 - sy0);
            let side = if sh.right_to_left { format!("right:{}px", px(table_w - x - r.2)) } else { format!("left:{}px", px(x)) };
            format!("{side};top:{}px;width:{}px;height:{}px", px(y), px(r.2), px(r.3))
        };
        for ch in &sh.charts {
            gridcraft_core::cancel::checkpoint();
            let r = anchor_rect(sh, &ch.anchor);
            if !visible(r) {
                continue;
            }
            let data = gridcraft_chart::resolve(wb, self.si, ch);
            let (w, h) = (r.2 as f32, r.3 as f32);
            let prims = gridcraft_chart::render(ch, &data, w, h, &gridcraft_chart::ApproxMeasure);
            let title = ch.title.clone().unwrap_or_default();
            let _ = write!(
                out,
                "<div class=\"chart obj\" data-chart=\"{}\" title=\"{}\" style=\"{}\">{}</div>",
                ch.id,
                esc(&title),
                place(r),
                gridcraft_chart::to_svg(&prims, w, h, "chart-svg")
            );
        }
        for im in &sh.images {
            let r = anchor_rect(sh, &im.anchor);
            if !visible(r) {
                continue;
            }
            let mime = if im.mime.starts_with("image/") { im.mime.as_str() } else { "image/png" };
            if im.data.len() > *image_budget {
                notes.push(format!("a picture of {} KB was left out (pictures past {} MB in one call)", im.data.len() >> 10, MAX_IMAGE_BYTES >> 20));
                continue;
            }
            *image_budget -= im.data.len();
            let _ = write!(
                out,
                "<div class=\"obj pic\" style=\"{}\"><img alt=\"{}\" src=\"data:{};base64,{}\"></div>",
                place(r),
                esc(&im.alt),
                esc(mime),
                crate::io::base64_encode(&im.data)
            );
        }
        for shp in &sh.shapes {
            let r = anchor_rect(sh, &shp.anchor);
            if !visible(r) {
                continue;
            }
            let fill = rgb_of(shp.fill, wb).map_or("none".to_string(), hex);
            let line = rgb_of(shp.line, wb).filter(|_| !shp.format.no_line).map_or("none".to_string(), hex);
            let (w, h) = (r.2, r.3);
            let body = match shp.kind {
                gridcraft_model::ShapeKind::Ellipse => {
                    format!(
                        "<ellipse cx=\"{}\" cy=\"{}\" rx=\"{}\" ry=\"{}\" fill=\"{fill}\" stroke=\"{line}\"/>",
                        px(w / 2.0),
                        px(h / 2.0),
                        px(w / 2.0 - 0.5),
                        px(h / 2.0 - 0.5)
                    )
                }
                gridcraft_model::ShapeKind::Line | gridcraft_model::ShapeKind::Arrow => {
                    let c = if line != "none" { line.clone() } else { fill.clone() };
                    format!("<line x1=\"0\" y1=\"0\" x2=\"{}\" y2=\"{}\" stroke=\"{c}\" stroke-width=\"1.3\"/>", px(w), px(h))
                }
                gridcraft_model::ShapeKind::Triangle => {
                    format!("<polygon points=\"{},0 {},{} 0,{}\" fill=\"{fill}\" stroke=\"{line}\"/>", px(w / 2.0), px(w), px(h), px(h))
                }
                _ => format!(
                    "<rect x=\"0.5\" y=\"0.5\" width=\"{}\" height=\"{}\" fill=\"{fill}\" stroke=\"{line}\"/>",
                    px((w - 1.0).max(0.0)),
                    px((h - 1.0).max(0.0))
                ),
            };
            let text = if shp.text.is_empty() {
                String::new()
            } else {
                let color = shp.format.text_color.and_then(|c| rgb_of(c, wb)).map_or("#000000".into(), hex);
                let size = shp.format.font_size.unwrap_or(11.0).clamp(1.0, 400.0);
                format!(
                    "<div style=\"position:absolute;inset:0;padding:3px;font-size:{}pt;color:{color};{}white-space:pre-wrap;overflow:hidden;display:flex;align-items:center;justify-content:center;text-align:center\">{}</div>",
                    px(f64::from(size)),
                    if shp.format.bold { "font-weight:700;" } else { "" },
                    esc(&shp.text)
                )
            };
            let _ = write!(
                out,
                "<div class=\"obj shape\" style=\"{}\"><svg xmlns=\"http://www.w3.org/2000/svg\" width=\"{}\" height=\"{}\" viewBox=\"0 0 {} {}\">{body}</svg>{text}</div>",
                place(r),
                px(w),
                px(h),
                px(w),
                px(h)
            );
        }
    }
}

fn safe_link(t: &str) -> bool {
    let l = t.trim().to_ascii_lowercase();
    l.starts_with("http://") || l.starts_with("https://") || l.starts_with("mailto:")
}

/// One sheet's div: the window drawn, or (`drawn: false`) a tab waiting to be asked for.
fn sheet_div(
    out: &mut String,
    wb: &Workbook,
    si: usize,
    on: bool,
    w: Option<&Window>,
    headings: bool,
    image_budget: &mut usize,
    notes: &mut Vec<String>,
) -> u64 {
    let Some(sh) = wb.sheet(si) else { return 0 };
    let (rows, cols) = match w {
        Some(_) => extent(sh, true),
        None => extent(sh, false),
    };
    let _ = write!(
        out,
        "<div class=\"sheet{}\" id=\"s{si}\" data-sheet=\"{}\" data-index=\"{si}\" data-drawn=\"{}\" data-rows=\"{rows}\" data-cols=\"{cols}\"",
        if on { " on" } else { "" },
        esc(&sh.name),
        if w.is_some() { 1 } else { 0 }
    );
    let Some(w) = w else {
        let _ = write!(
            out,
            " data-row-start=\"0\" data-row-end=\"0\" data-col-start=\"0\" data-col-end=\"0\"><div class=\"cap\">{} holds {} rows × {} columns: it is drawn when its tab is asked for.</div></div>",
            esc(&sh.name),
            rows,
            cols
        );
        return 0;
    };
    let _ =
        write!(out, " data-row-start=\"{}\" data-row-end=\"{}\" data-col-start=\"{}\" data-col-end=\"{}\">", w.r0 + 1, w.r1 + 1, w.c0 + 1, w.c1 + 1);
    if w.empty {
        let _ = write!(
            out,
            "<div class=\"cap\">Nothing at rows {}–{}, columns {}–{}: the sheet ends at row {rows}, column {}.</div>",
            w.r0 + 1,
            w.r1 + 1,
            col_to_letters(w.c0),
            col_to_letters(w.c1),
            col_to_letters(cols.saturating_sub(1))
        );
    } else if w.r0 > 0 || w.c0 > 0 || w.r1 + 1 < rows || w.c1 + 1 < cols {
        let _ = write!(
            out,
            "<div class=\"cap\">Rows {}–{} of {rows} · columns {}–{} of {} (the rest is drawn as the sheet is scrolled).</div>",
            w.r0 + 1,
            w.r1 + 1,
            col_to_letters(w.c0),
            col_to_letters(w.c1),
            col_to_letters(cols.saturating_sub(1))
        );
    }
    // WHIQ: a protected sheet says so, and what can still be edited.
    if let Some(note) = crate::cmd::inspect::protection_summary(wb, sh) {
        let _ = write!(out, "<div class=\"cap protected\">This is a protected sheet: {}.</div>", esc(&note));
    }
    let d = SheetDraw::new(wb, si, sh, w, headings);
    let merges = d.merges();
    out.push_str("<div class=\"grid\">");
    d.table_open(out);
    let n = d.body(out, &merges);
    out.push_str("</tbody></table>");
    d.objects(out, image_budget, notes);
    out.push_str("</div></div>");
    n
}

/// `file.previewHtml`: the HTML and the numbers the pane pages with.
pub fn render(wb: &Workbook, o: &Options) -> Result<Json, String> {
    let visible: Vec<usize> = wb.sheets.iter().enumerate().filter(|(_, s)| s.visibility == Visibility::Visible).map(|(i, _)| i).collect();
    let si = match o.sheet {
        Some(i) if i < wb.sheets.len() => i,
        Some(i) => return Err(format!("there is no sheet {i} (the workbook has {})", wb.sheets.len())),
        None => visible.first().copied().unwrap_or(0),
    };
    let sh = wb.sheet(si).ok_or("the workbook has no sheets")?;
    let max_cells = o.max_cells.clamp(1, MAX_WINDOW_CELLS);
    let w = window(sh, o, max_cells, (DEFAULT_ROWS, DEFAULT_COLS));
    let mut out = String::new();
    let mut image_budget = MAX_IMAGE_BYTES;
    let mut notes = vec![];
    let mut cells = 0u64;
    let mut drawn_sheets = vec![si];
    match o.format {
        Format::Rows => {
            let d = SheetDraw::new(wb, si, sh, &w, o.headings);
            let d = SheetDraw { frozen_rows: 0, frozen_cols: 0, ..d };
            // Rows alone: the window's own rows, without the frozen ones put first.
            let d = SheetDraw {
                rows: (w.r0..=w.r1).filter(|r| !sh.is_row_hidden(*r)).collect(),
                cols: (w.c0..=w.c1).filter(|c| !sh.is_col_hidden(*c)).collect(),
                ..d
            };
            let merges = d.merges();
            cells += d.body(&mut out, &merges);
        }
        Format::Sheet => {
            cells += sheet_div(&mut out, wb, si, true, Some(&w), o.headings, &mut image_budget, &mut notes);
        }
        Format::Page => {
            let _ = write!(
                out,
                "<!DOCTYPE html><html><head><meta charset=\"utf-8\"><meta name=\"viewport\" content=\"width=device-width, initial-scale=1\">\n<title>{}</title><style>{CSS}</style></head><body>\n<div class=\"head\">{}</div>\n<div class=\"bar\">",
                esc(&o.title),
                esc(&o.title)
            );
            if wb.protected_structure {
                // The head line of the page: the workbook's sheets can't be changed.
                if let Some(i) = out.rfind("</div>\n<div class=\"bar\">") {
                    out.insert_str(i, " · the workbook's structure is protected");
                }
            }
            let tabs: Vec<usize> = if visible.contains(&si) { visible.clone() } else { std::iter::once(si).chain(visible.iter().copied()).collect() };
            for (k, i) in tabs.iter().enumerate() {
                let name = wb.sheet(*i).map(|s| s.name.as_str()).unwrap_or_default();
                let tint = wb
                    .sheet(*i)
                    .and_then(|s| s.tab_color)
                    .and_then(|c| rgb_of(c, wb))
                    .map(|c| format!(" style=\"border-bottom:3px solid {}\"", hex(c)))
                    .unwrap_or_default();
                let _ = write!(
                    out,
                    "<div class=\"tab{}\" data-sheet=\"{}\"{tint} onclick=\"show({k})\">{}</div>",
                    if *i == si { " on" } else { "" },
                    esc(name),
                    esc(name)
                );
            }
            out.push_str("</div>\n");
            let mut others = if o.other_sheets { OTHER_SHEETS_CELLS } else { 0 };
            for (k, i) in tabs.iter().enumerate() {
                // The ids follow the tab order (`show(k)` shows `s<k>`).
                let mut div = String::new();
                if *i == si {
                    cells += sheet_div(&mut div, wb, *i, true, Some(&w), o.headings, &mut image_budget, &mut notes);
                } else {
                    let Some(other) = wb.sheet(*i) else { continue };
                    // A sheet still on disk is read for its first screenful only when small.
                    let small = other.cells.is_resident() || other.cells.loaded_bytes() <= COLD_SHEET_BYTES;
                    let first = Options { sheet: Some(*i), ..Options::default() };
                    let ow = if small && others > 0 { Some(window(other, &first, others, (50, 26))) } else { None };
                    let n = ow.map_or(u64::MAX, |ow| u64::from(ow.r1 - ow.r0 + 1) * u64::from(ow.c1 - ow.c0 + 1));
                    if let Some(ow) = ow.filter(|_| others >= n) {
                        others -= n;
                        cells += sheet_div(&mut div, wb, *i, false, Some(&ow), o.headings, &mut image_budget, &mut notes);
                        drawn_sheets.push(*i);
                    } else {
                        sheet_div(&mut div, wb, *i, false, None, o.headings, &mut image_budget, &mut notes);
                    }
                }
                out.push_str(&div.replacen(&format!("id=\"s{i}\""), &format!("id=\"s{k}\""), 1));
                out.push('\n');
            }
            let _ = write!(out, "<script>{SCRIPT}</script>\n</body></html>");
        }
    }
    let (rows, cols) = extent(sh, true);
    let sheets: Vec<Json> = wb.sheets.iter().enumerate().map(|(i, s)| sheet_meta(wb, i, s, i == si || drawn_sheets.contains(&i))).collect();
    let range = RangeRef::new(CellRef::new(w.r0, w.c0), CellRef::new(w.r1, w.c1));
    let more_rows = w.r1 + 1 < rows;
    let more_cols = w.c1 + 1 < cols;
    let mut window = json!({
        "sheet": sh.name,
        "sheet_index": si,
        "row_start": w.r0 + 1,
        "row_count": w.r1 - w.r0 + 1,
        "col_start": w.c0 + 1,
        "col_count": w.c1 - w.c0 + 1,
        "range": range.a1(),
        "rows": rows,
        "columns": cols,
        "more_rows": more_rows,
        "more_columns": more_cols,
        "truncated": w.cut,
    });
    if more_rows {
        window["next_rows"] =
            json!({"sheet": sh.name, "row_start": w.r1 + 2, "row_count": w.r1 - w.r0 + 1, "col_start": w.c0 + 1, "col_count": w.c1 - w.c0 + 1});
    }
    if more_cols {
        window["next_columns"] =
            json!({"sheet": sh.name, "row_start": w.r0 + 1, "row_count": w.r1 - w.r0 + 1, "col_start": w.c1 + 2, "col_count": w.c1 - w.c0 + 1});
    }
    if w.empty {
        window["empty"] = json!(true);
    }
    if let Some(f) = sh.freeze.filter(|f| f.0 > 0 || f.1 > 0) {
        window["frozen"] = json!({"rows": f.0, "columns": f.1});
    }
    let mut v = json!({
        "html": out,
        "format": o.format.name(),
        "title": o.title,
        "sheets": sheets,
        "window": window,
        "cells": cells,
        "structure_protected": wb.protected_structure,
    });
    v["bytes"] = json!(v["html"].as_str().map_or(0, str::len));
    if !notes.is_empty() {
        v["notes"] = json!(notes);
    }
    Ok(v)
}

// ---------------------------------------------------------------- Markdown

/// What `sheet.markdown` reads.
#[derive(Clone, Debug, Default)]
pub struct MarkdownOptions {
    /// The sheet to read (default: the first visible one).
    pub sheet: Option<usize>,
    /// Go on into the sheets after it (default: true when no sheet is named).
    pub continue_sheets: bool,
    pub row_start: Option<u32>,
    /// The page size: most rows in this answer (all sheets together). `None`: as many as fit.
    pub row_count: Option<u32>,
    pub col_start: Option<u32>,
    pub col_count: Option<u32>,
    /// Stop once the text is this long (the answer says where to go on).
    pub max_bytes: usize,
    /// Formulas as `=…` text instead of their values.
    pub formulas: bool,
    /// Keep rows with nothing in them (default: left out; every row is numbered).
    pub keep_empty_rows: bool,
}

fn md_cell(s: &str) -> String {
    s.replace('\\', "\\\\").replace('|', "\\|").replace("\r\n", "<br>").replace(['\n', '\r'], "<br>")
}

/// WHIQ: the sheet's charts as a Markdown list: title, type, anchor, axis titles, and each series
/// with its kind, references and axis.
fn charts_md(sh: &Sheet) -> String {
    if sh.charts.is_empty() {
        return String::new();
    }
    let mut t = format!("Charts on {}:\n\n", md_cell(&sh.name));
    for c in &sh.charts {
        let (kind, sub) = crate::cmd::inspect::chart_type_name(c.kind);
        let title = c.title.as_deref().map(|x| format!(" \"{}\"", md_cell(x))).unwrap_or_default();
        let _ = write!(
            t,
            "- Chart {}{title} ({kind}{}) at {}, {:.0} × {:.0} pt",
            c.id,
            sub.map(|s| format!(", {s}")).unwrap_or_default(),
            c.anchor.cell.a1(),
            c.anchor.width,
            c.anchor.height
        );
        for (label, v) in [("x axis", &c.x_title), ("y axis", &c.y_title), ("secondary axis", &c.y2_title)] {
            if let Some(v) = v {
                let _ = write!(t, "; {label} \"{}\"", md_cell(v));
            }
        }
        t.push('\n');
        for (i, se) in c.series.iter().enumerate() {
            let (k, _) = crate::cmd::inspect::chart_type_name(crate::cmd::inspect::series_kind(c, i));
            let name = se.name.as_deref().map(|n| format!(" {}", md_cell(n))).unwrap_or_default();
            let _ = write!(
                t,
                "  - series {}{name} ({k}, {} axis): values {}",
                i + 1,
                if se.secondary { "secondary" } else { "primary" },
                md_cell(&se.values)
            );
            if let Some(cat) = &se.categories {
                let _ = write!(t, ", categories {}", md_cell(cat));
            }
            t.push('\n');
        }
    }
    t.push('\n');
    t
}

/// `sheet.markdown`: sheets as Markdown tables (the row number first, column letters as the
/// header), paged by rows (`row_start`, `row_count`) and by bytes; `next` says where to go on.
pub fn markdown(wb: &Workbook, o: &MarkdownOptions) -> Result<Json, String> {
    let visible: Vec<usize> = wb.sheets.iter().enumerate().filter(|(_, s)| s.visibility == Visibility::Visible).map(|(i, _)| i).collect();
    let first = match o.sheet {
        Some(i) if i < wb.sheets.len() => i,
        Some(i) => return Err(format!("there is no sheet {i} (the workbook has {})", wb.sheets.len())),
        None => visible.first().copied().unwrap_or(0),
    };
    let mut list = vec![first];
    if o.continue_sheets {
        list.extend(visible.iter().copied().filter(|i| *i > first));
    }
    let max = o.max_bytes.max(1024);
    let budget_rows = o.row_count.map_or(u64::MAX, |n| u64::from(n.max(1)));
    let mut text = String::new();
    let mut parts = vec![];
    let mut next: Option<Json> = None;
    let mut rows_out = 0u64;
    let headings = o.continue_sheets || wb.sheets.len() > 1;
    // WHIQ: what protection allows, said where an agent reads it.
    if wb.protected_structure {
        text.push_str("> The workbook's structure is protected: sheets can't be inserted, deleted, renamed, moved, copied, hidden, unhidden or recoloured (ask the owner to unprotect the workbook).\n\n");
    }
    'sheets: for (k, &si) in list.iter().enumerate() {
        let Some(sh) = wb.sheet(si) else { continue };
        let used = sh.used_range();
        let meta = |from: Option<u32>, to: Option<u32>| {
            json!({"sheet": sh.name, "index": si, "used_range": used.map(|u| u.a1()), "rows": used.map_or(0, |u| u.end.row + 1),
                   "columns": used.map_or(0, |u| u.end.col + 1), "row_start": from, "row_end": to})
        };
        let go_on = |row: u32| {
            let mut n = json!({"sheet": sh.name, "row_start": row + 1});
            if o.continue_sheets {
                n["continue_sheets"] = json!(true);
            }
            n
        };
        if rows_out >= budget_rows || (!text.is_empty() && text.len() + 64 > max) {
            next = Some(go_on(if k == 0 { o.row_start.unwrap_or(0) } else { 0 }));
            break;
        }
        if headings {
            let _ = write!(text, "## {}{}\n\n", md_cell(&sh.name), used.map(|u| format!(" ({})", u.a1())).unwrap_or_default());
        }
        if let Some(note) = crate::cmd::inspect::protection_summary(wb, sh) {
            let _ = write!(text, "> This is a protected sheet: {}; ask the owner to unprotect it for anything else.\n\n", md_cell(&note));
        }
        let Some(u) = used else {
            text.push_str("(empty sheet)\n\n");
            text.push_str(&charts_md(sh));
            parts.push(meta(None, None));
            continue;
        };
        let r_start = if k == 0 { o.row_start.unwrap_or(u.start.row) } else { u.start.row };
        let c0 = o.col_start.unwrap_or(u.start.col).min(MAX_COLS - 1);
        let c1 = match o.col_count {
            Some(n) => c0.saturating_add(n.max(1) - 1).min(MAX_COLS - 1),
            None => u.end.col.max(c0),
        };
        if r_start > u.end.row {
            text.push_str("(nothing at or after the row asked for)\n\n");
            parts.push(meta(None, None));
            continue;
        }
        let mut header = String::from("| Row |");
        let mut sep = String::from("|---:|");
        for c in c0..=c1 {
            let _ = write!(header, " {} |", col_to_letters(c));
            sep.push_str("---|");
        }
        let _ = writeln!(text, "{header}\n{sep}");
        let mut last = None;
        for row in r_start..=u.end.row {
            gridcraft_core::cancel::checkpoint();
            if rows_out >= budget_rows {
                next = Some(go_on(row));
                parts.push(meta(Some(r_start + 1), last.map(|r: u32| r + 1)));
                break 'sheets;
            }
            let mut line = format!("| {} |", row + 1);
            let mut any = false;
            for col in c0..=c1 {
                let c = CellRef::new(row, col);
                let t = if o.formulas
                    && let Some(f) = sh.cell(c).and_then(|x| x.formula.as_ref())
                    && !crate::display::formula_hidden(wb, sh, c)
                {
                    format!("={}", f.text)
                } else {
                    crate::display::value_text(wb, sh, c)
                };
                if !t.is_empty() {
                    any = true;
                }
                let _ = write!(line, " {} |", md_cell(&t));
            }
            if !any && !o.keep_empty_rows {
                continue;
            }
            line.push('\n');
            if text.len() + line.len() > max && rows_out > 0 {
                next = Some(go_on(row));
                parts.push(meta(Some(r_start + 1), last.map(|r: u32| r + 1)));
                break 'sheets;
            }
            text.push_str(&line);
            rows_out += 1;
            last = Some(row);
        }
        text.push('\n');
        text.push_str(&charts_md(sh));
        parts.push(meta(Some(r_start + 1), Some(u.end.row + 1)));
    }
    let sheets: Vec<Json> = wb.sheets.iter().enumerate().map(|(i, s)| sheet_meta(wb, i, s, list.contains(&i))).collect();
    let mut v = json!({
        "structure_protected": wb.protected_structure,
        "markdown": text,
        "sheets": sheets,
        "read": parts,
        "rows_returned": rows_out,
        "truncated": next.is_some(),
    });
    v["bytes"] = json!(v["markdown"].as_str().map_or(0, str::len));
    if let Some(n) = next {
        v["next"] = n;
    }
    Ok(v)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;

    use gridcraft_core::Value;
    use serde_json::json;

    use crate::Session;

    fn session() -> Session {
        let mut s = Session::new();
        s.new_workbook();
        s
    }

    fn preview(s: &mut Session, p: serde_json::Value) -> serde_json::Value {
        s.execute("file.previewHtml", p).unwrap()
    }

    #[test]
    fn a_large_sheet_has_no_row_cap_and_pages_by_window() {
        let mut s = session();
        // 300,000 rows × 2 columns, put straight into the model (the commands would take long in
        // a debug build).
        {
            let d = s.doc_mut().unwrap();
            let wb = Arc::make_mut(&mut d.wb);
            let sh = Arc::make_mut(&mut wb.sheets[0]);
            for r in 0..300_000u32 {
                sh.set_value(gridcraft_core::CellRef::new(r, 0), Value::number(f64::from(r + 1)));
                sh.set_value(gridcraft_core::CellRef::new(r, 1), Value::Text(format!("row {}", r + 1).into()));
            }
        }
        // The first screenful, and the numbers to page with.
        let v = preview(&mut s, json!({}));
        let w = &v["window"];
        assert_eq!(w["row_start"], 1);
        assert_eq!(w["row_count"], super::DEFAULT_ROWS);
        assert_eq!(w["rows"], 300_000);
        assert_eq!(w["columns"], 2);
        assert_eq!(w["more_rows"], true);
        assert_eq!(w["next_rows"]["row_start"], super::DEFAULT_ROWS + 1);
        assert_eq!(v["sheets"][0]["rows"], 300_000);
        assert_eq!(v["sheets"][0]["used_range"], "A1:B300000");
        let html = v["html"].as_str().unwrap();
        assert!(html.starts_with("<!DOCTYPE html>"));
        for part in ["class=\"head\"", "class=\"bar\"", "class=\"tab on\"", "class=\"sheet on\" id=\"s0\"", "data-r=\"100\"", "row 100<"] {
            assert!(html.contains(part), "{part}");
        }
        assert!(!html.contains("data-r=\"101\""));
        // Far down the sheet (well past the old 2,000-row cap).
        let v = preview(&mut s, json!({"row_start": 250_001, "row_count": 50, "format": "sheet"}));
        let html = v["html"].as_str().unwrap();
        assert!(html.starts_with("<div class=\"sheet on\""), "{}", &html[..80]);
        assert!(html.contains("data-r=\"250001\"") && html.contains("data-r=\"250050\"") && html.contains("row 250050<"));
        assert!(!html.contains("data-r=\"250051\""));
        assert_eq!(v["window"]["range"], "A250001:B250050");
        // The last rows: no more after them.
        let v = preview(&mut s, json!({"row_start": 299_990, "row_count": 1000, "format": "rows"}));
        assert_eq!(v["window"]["row_count"], 11);
        assert_eq!(v["window"]["more_rows"], false);
        assert!(v["html"].as_str().unwrap().starts_with("<tr data-r=\"299990\""));
        // A window larger than max_cells comes back in part, with where to go on.
        let v = preview(&mut s, json!({"row_start": 1, "row_count": 200_000, "max_cells": 10_000, "format": "rows"}));
        assert_eq!(v["window"]["truncated"], true);
        assert_eq!(v["window"]["row_count"], 5_000);
        assert_eq!(v["window"]["next_rows"]["row_start"], 5_001);
        // And a whole 200,000-row window in one call when asked for.
        let v = preview(&mut s, json!({"row_count": 200_000, "max_cells": 400_000, "format": "rows"}));
        assert_eq!(v["window"]["row_count"], 200_000);
        assert_eq!(v["cells"], 400_000);
    }

    #[test]
    fn merged_cells_number_formats_formulas_borders_and_frozen_panes() {
        let mut s = session();
        s.execute("range.setValues", json!({"range": "A1", "values": [["Title", null, null], [1234.5, 0.25, "=A2*2"], [3, 4, "=SUM(A2:A3)"]]}))
            .unwrap();
        s.execute("home.mergeCenter", json!({"range": "A1:C1"})).unwrap();
        s.execute("home.numberFormat", json!({"range": "A2", "format": "#,##0.00"})).unwrap();
        s.execute("home.numberFormat", json!({"range": "B2", "format": "0%"})).unwrap();
        s.execute("home.numberFormat", json!({"range": "A3", "format": "0;[Red]-0"})).unwrap();
        s.execute("cell.set", json!({"cell": "A3", "input": "-3"})).unwrap();
        s.execute("home.bold", json!({"range": "A1", "on": true})).unwrap();
        s.execute("home.fillColor", json!({"range": "A1", "color": "#3B82F6"})).unwrap();
        s.execute("home.borders", json!({"range": "B3", "preset": "top"})).unwrap();
        s.execute("view.freezePanes", json!({"cell": "A2"})).unwrap();
        let v = preview(&mut s, json!({"format": "sheet"}));
        let html = v["html"].as_str().unwrap();
        // The merge is one cell spanning three columns, its covered cells not drawn.
        assert!(html.contains("colspan=\"3\""), "{html}");
        // A merge past the last value is drawn whole (the window reaches it).
        s.execute("home.mergeCells", json!({"range": "A5:F5"})).unwrap();
        assert_eq!(preview(&mut s, json!({"format": "sheet"}))["window"]["columns"], 6);
        assert!(!html.contains("data-c=\"B1\""));
        assert!(html.contains("background-color:#3b82f6") && html.contains("font-weight:700") && html.contains("text-align:center"));
        // Values as displayed.
        assert!(html.contains(">1,234.50<") && html.contains(">25%<") && html.contains(">2469<"), "{html}");
        assert!(html.contains("color:#ff0000") && html.contains(">-3<"), "the red negative format: {html}");
        // Formulas on their cells.
        assert!(html.contains("data-c=\"C2\" data-f=\"=A2*2\"") && html.contains("data-f=\"=SUM(A2:A3)\""), "{html}");
        // B3's top border is drawn on B2's bottom too (the same line on both cells).
        let b2 = html.find("data-c=\"B2\"").unwrap();
        let b2_td = &html[html[..b2].rfind("<td").unwrap()..b2];
        assert!(b2_td.contains("border-bottom:1px solid #000000"), "{b2_td}");
        // Frozen row 1 stays on top: sticky, and drawn first when the window starts lower.
        assert!(html.contains("position:sticky;top:20px"));
        assert_eq!(v["window"]["frozen"]["rows"], 1);
        s.execute("range.setValues", json!({"range": "A500", "values": [["far"]]})).unwrap();
        let v = preview(&mut s, json!({"format": "sheet", "row_start": 400, "row_count": 101}));
        let html = v["html"].as_str().unwrap();
        let first = html.find("data-r=\"1\"").unwrap();
        let later = html.find("data-r=\"400\"").unwrap();
        assert!(first < later, "the frozen row comes first");
        assert!(html.contains(">far<"));
    }

    #[test]
    fn charts_are_inline_svg_where_they_sit() {
        let mut s = session();
        s.execute("range.setValues", json!({"range": "A1", "values": [["Q", "Units"], ["Q1", 10], ["Q2", 30], ["Q3", 20]]})).unwrap();
        s.execute("insert.chart", json!({"range": "A1:B4", "type": "column", "title": "Units by quarter", "at": "D2"})).unwrap();
        let v = preview(&mut s, json!({}));
        let html = v["html"].as_str().unwrap();
        let at = html.find("class=\"chart obj\"").expect("the chart is drawn");
        let div = &html[at..];
        assert!(div.contains("<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"chart-svg\""));
        assert!(div.contains("Units by quarter"));
        assert!(div.contains("<rect "), "column bars");
        // The window reaches the chart (to its right of the data) and its position is the sheet's.
        assert!(v["window"]["columns"].as_u64().unwrap() > 4, "{}", v["window"]);
        assert!(div.contains("left:") && div.contains("top:"));
        assert_eq!(v["sheets"][0]["charts"], 1);
        // Nothing outside: no scripts or images from elsewhere.
        assert!(!html.contains("src=\"http") && !html.contains("href=\"http"));
    }

    #[test]
    fn every_sheet_has_its_tab_and_the_window_names_its_sheet() {
        let mut s = session();
        s.execute("range.setValues", json!({"range": "A1", "values": [["one"]]})).unwrap();
        s.execute("home.insertSheet", json!({"name": "Second"})).unwrap();
        s.execute("range.setValues", json!({"range": "A1", "values": [["two"]]})).unwrap();
        let v = preview(&mut s, json!({}));
        let html = v["html"].as_str().unwrap();
        assert!(html.contains("onclick=\"show(0)\">Sheet1</div>") && html.contains("onclick=\"show(1)\">Second</div>"));
        assert!(html.contains(">one<") && html.contains(">two<"), "the other tab shows at once");
        assert_eq!(v["window"]["sheet"], "Sheet1");
        let v = preview(&mut s, json!({"sheet": "second"}));
        assert_eq!(v["window"]["sheet"], "Second");
        assert!(v["html"].as_str().unwrap().contains("class=\"tab on\" data-sheet=\"Second\""));
        assert!(s.execute("file.previewHtml", json!({"sheet": "Nope"})).is_err());
    }

    #[test]
    fn markdown_pages_by_rows_and_bytes() {
        let mut s = session();
        let rows: Vec<serde_json::Value> = (1..=500).map(|i| json!([format!("item | {i}"), i * 10])).collect();
        s.execute("range.setValues", json!({"range": "A1", "values": rows})).unwrap();
        s.execute("home.insertSheet", json!({"name": "Notes"})).unwrap();
        s.execute("range.setValues", json!({"range": "B2", "values": [["hello"]]})).unwrap();
        // The whole workbook in one call (small enough), each sheet under its heading.
        let v = s.execute("sheet.markdown", json!({})).unwrap();
        let md = v["markdown"].as_str().unwrap();
        assert!(md.starts_with("## Sheet1 (A1:B500)\n\n| Row | A | B |\n|---:|---|---|\n| 1 | item \\| 1 | 10 |"), "{}", &md[..120]);
        assert!(md.contains("## Notes (B2)") && md.contains("| 2 | hello |"));
        assert_eq!(v["truncated"], false);
        assert_eq!(v["rows_returned"], 501);
        // A page of 100 rows from row 201 of one sheet.
        let v = s.execute("sheet.markdown", json!({"sheet": "Sheet1", "row_start": 201, "row_count": 100})).unwrap();
        let md = v["markdown"].as_str().unwrap();
        assert!(md.contains("| 201 | item \\| 201 | 2010 |") && md.contains("| 300 |") && !md.contains("| 301 |"));
        assert_eq!(v["next"]["row_start"], 301);
        // A byte budget pages too.
        let v = s.execute("sheet.markdown", json!({"sheet": 0, "max_bytes": 2048})).unwrap();
        assert!(v["bytes"].as_u64().unwrap() <= 2048);
        assert_eq!(v["truncated"], true);
        let n = v["next"]["row_start"].as_u64().unwrap();
        assert!(n > 10 && n < 500);
        assert_eq!(v["sheets"][1]["used_range"], "B2");
    }
}
