//! CSV import and export (RFC 4180 with the usual real-world leniency).

use std::sync::Arc;

use gridcraft_core::parse::parse_input;
use gridcraft_core::{CellRef, MAX_COLS, MAX_ROWS, RangeRef, Value};
use gridcraft_model::{Cell, NumFmt, Sheet, StyleId, Workbook};

use crate::{CsvOptions, IoError};

/// Windows-1252 code points for bytes 0x80–0x9F (others map to Latin-1).
const CP1252: [u32; 32] = [
    0x20AC, 0x81, 0x201A, 0x0192, 0x201E, 0x2026, 0x2020, 0x2021, 0x02C6, 0x2030, 0x0160, 0x2039, 0x0152, 0x8D, 0x017D, 0x8F, 0x90, 0x2018, 0x2019,
    0x201C, 0x201D, 0x2022, 0x2013, 0x2014, 0x02DC, 0x2122, 0x0161, 0x203A, 0x0153, 0x9D, 0x017E, 0x0178,
];

/// Decodes CSV bytes: UTF-8/UTF-16 BOMs, else UTF-8, else Windows-1252.
pub fn decode_text(bytes: &[u8]) -> String {
    if let Some(rest) = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]) {
        return String::from_utf8_lossy(rest).into_owned();
    }
    let utf16 = |b: &[u8], le: bool| -> String {
        let units = b.as_chunks::<2>().0.iter().map(|c| if le { u16::from_le_bytes(*c) } else { u16::from_be_bytes(*c) });
        char::decode_utf16(units).map(|r| r.unwrap_or('\u{FFFD}')).collect()
    };
    if let Some(rest) = bytes.strip_prefix(&[0xFF, 0xFE]) {
        return utf16(rest, true);
    }
    if let Some(rest) = bytes.strip_prefix(&[0xFE, 0xFF]) {
        return utf16(rest, false);
    }
    match std::str::from_utf8(bytes) {
        Ok(s) => s.to_string(),
        Err(_) => bytes
            .iter()
            .map(|&b| match b {
                0x80..=0x9F => char::from_u32(CP1252.get((b - 0x80) as usize).copied().unwrap_or(0xFFFD)).unwrap_or('\u{FFFD}'),
                _ => b as char,
            })
            .collect(),
    }
}

/// Picks comma, semicolon or tab: the candidate with the most consistent count per line
/// (outside quotes) over the first non-blank lines. A candidate must appear on every such line, otherwise comma wins.
pub fn sniff_delimiter(text: &str, quote: char) -> u8 {
    let mut best = (b',', 0usize, 0usize);
    for d in [',', ';', '\t'] {
        let mut counts = vec![];
        let mut n = 0usize;
        let mut in_q = false;
        let mut blank = true;
        for ch in text.chars().take(64 * 1024) {
            if !in_q && ch == '\n' {
                // Blank lines carry no delimiter evidence.
                if !blank {
                    counts.push(n);
                }
                n = 0;
                blank = true;
                if counts.len() >= 20 {
                    break;
                }
                continue;
            }
            if ch != '\r' {
                blank = false;
            }
            if ch == quote {
                in_q = !in_q;
            } else if !in_q && ch == d {
                n += 1;
            }
        }
        if !blank {
            counts.push(n);
        }
        let min = counts.iter().copied().min().unwrap_or(0);
        let total: usize = counts.iter().sum();
        if min > 0 && (min, total) > (best.1, best.2) {
            best = (d as u8, min, total);
        }
    }
    best.0
}

/// Splits CSV text into records of fields.
fn parse_records(text: &str, delim: char, quote: char, mut on_record: impl FnMut(Vec<String>) -> bool) {
    let mut field = String::new();
    let mut record: Vec<String> = vec![];
    let mut in_q = false;
    let mut chars = text.chars().peekable();
    let mut field_started_quoted = false;
    while let Some(c) = chars.next() {
        if in_q {
            if c == quote {
                if chars.peek() == Some(&quote) {
                    field.push(quote);
                    chars.next();
                } else {
                    in_q = false;
                }
            } else {
                field.push(c);
            }
            continue;
        }
        if c == quote && field.is_empty() && !field_started_quoted {
            in_q = true;
            field_started_quoted = true;
        } else if c == delim {
            record.push(std::mem::take(&mut field));
            field_started_quoted = false;
        } else if c == '\r' || c == '\n' {
            if c == '\r' && chars.peek() == Some(&'\n') {
                chars.next();
            }
            record.push(std::mem::take(&mut field));
            field_started_quoted = false;
            if !on_record(std::mem::take(&mut record)) {
                return;
            }
        } else {
            field.push(c);
        }
    }
    if !field.is_empty() || !record.is_empty() || field_started_quoted {
        record.push(field);
        on_record(record);
    }
}

pub fn read_csv(bytes: &[u8], opts: &CsvOptions) -> Result<Workbook, IoError> {
    let text = decode_text(bytes);
    let quote = if opts.quote == 0 { '"' } else { opts.quote as char };
    let delim = if opts.delimiter == 0 { sniff_delimiter(&text, quote) } else { opts.delimiter } as char;
    if delim == quote {
        return Err(IoError::Format("the delimiter and quote character must differ".into()));
    }
    let mut wb = Workbook::new();
    wb.date_system = opts.date_system;
    wb.csv_delimiter = u8::try_from(delim).ok();
    let mut sheet = Sheet::new("Sheet1");
    let mut styles = std::mem::take(&mut wb.styles);
    let mut fmt_cache: Vec<(&'static str, StyleId)> = vec![];
    let mut row: u32 = 0;
    parse_records(&text, delim, quote, |rec| {
        if row >= MAX_ROWS {
            return false;
        }
        // A blank line is an empty row.
        for (col, f) in rec.into_iter().enumerate() {
            if col as u32 >= MAX_COLS {
                break;
            }
            if f.is_empty() {
                continue;
            }
            let pos = CellRef::new(row, col as u32);
            let cell = if opts.parse_values {
                let p = parse_input(&f, opts.date_system);
                let style = match p.format {
                    Some(code) => match fmt_cache.iter().find(|(c, _)| *c == code) {
                        Some((_, id)) => *id,
                        None => {
                            let id = styles.derive(StyleId::DEFAULT, |s| s.num_fmt = NumFmt::new(code));
                            fmt_cache.push((code, id));
                            id
                        }
                    },
                    None => StyleId::DEFAULT,
                };
                Cell { value: p.value, formula: None, style }
            } else {
                Cell::value(Value::Text(Arc::from(f)))
            };
            sheet.cells.set(pos, cell);
        }
        row += 1;
        true
    });
    wb.styles = styles;
    wb.sheets = vec![Arc::new(sheet)];
    Ok(wb)
}

/// Writes the sheet's used range as CSV (CRLF line ends, UTF-8 without BOM). `cell_text` gives
/// each cell's field text: the caller decides how values look (Save As CSV writes them as
/// displayed, number formats applied); quoting is done here.
///
/// WHIQ: rows are written ragged (see [`write_csv_range_into`]), so two cells far apart make a
/// file of a few bytes, not the whole rectangle between them (`A1` and `XFD1048576` were 16 GiB of
/// commas), and there is no size cap: the file is as big as the data.
pub fn write_csv(sheet: &Sheet, delimiter: u8, cell_text: impl FnMut(CellRef) -> String) -> Result<Vec<u8>, IoError> {
    let mut out = Vec::new();
    if let Some(r) = sheet.used_range() {
        write_csv_range_into(sheet, r, delimiter, &mut out, cell_text)?;
    }
    Ok(out)
}

/// The spilled arrays crossing each row of a range, for rows asked in increasing order.
struct RowSpills {
    /// Spill ranges that reach the range, by first row.
    all: Vec<RangeRef>,
    next: usize,
    active: Vec<RangeRef>,
}

impl RowSpills {
    fn new(sheet: &Sheet, range: RangeRef) -> Self {
        let mut all: Vec<RangeRef> = sheet
            .spill_ranges
            .values()
            .filter(|s| s.end.row >= range.start.row && s.start.row <= range.end.row && s.end.col >= range.start.col && s.start.col <= range.end.col)
            .copied()
            .collect();
        all.sort_by_key(|s| s.start.row);
        RowSpills { all, next: 0, active: Vec::new() }
    }

    fn at(&mut self, row: u32) -> &[RangeRef] {
        while let Some(s) = self.all.get(self.next).filter(|s| s.start.row <= row) {
            self.active.push(*s);
            self.next += 1;
        }
        self.active.retain(|s| s.end.row >= row);
        &self.active
    }
}

/// WHIQ: the cells of `range` as CSV streamed into `w` a row at a time: no size cap (the disk is
/// the bound) and nothing held but one row. Returns the rows written.
///
/// Rows are RAGGED: the empty cells at the end of a row are dropped (the row ends at its last
/// non-empty cell), so a sparse sheet costs what its data costs, not the area of its used range.
/// Two things keep it readable by tools that take the width from the first row and skip blank
/// lines (pandas, Excel): the FIRST row is padded to the width of the range, and a row with
/// nothing in it is one empty quoted field (`""`), not an empty line. Every line starts at the
/// range's first column, and a reader puts each field back in its cell (a row with fewer fields
/// leaves the rest empty). Only the cells that exist (stored, or spilled into) are asked for text.
pub fn write_csv_range_into(
    sheet: &Sheet,
    range: RangeRef,
    delimiter: u8,
    w: &mut dyn std::io::Write,
    mut cell_text: impl FnMut(CellRef) -> String,
) -> Result<u64, IoError> {
    let delim = if delimiter == 0 { ',' } else { delimiter as char };
    let (c0, c1) = (range.start.col, range.end.col);
    let mut spills = RowSpills::new(sheet, range);
    let mut line = String::new();
    let mut rows = 0u64;
    for row in range.start.row..=range.end.row {
        // WHIQ: a safe point for `cancel` in a long export.
        gridcraft_core::cancel::checkpoint();
        line.clear();
        // The column of the last field written (the line starts "before" the first column).
        let mut prev: Option<u32> = None;
        let mut field = |col: u32, t: String, line: &mut String| {
            if t.is_empty() {
                return;
            }
            let gap = col - prev.unwrap_or(c0);
            line.extend(std::iter::repeat_n(delim, gap as usize));
            prev = Some(col);
            if t.contains(delim) || t.contains('"') || t.contains('\n') || t.contains('\r') {
                line.push('"');
                line.push_str(&t.replace('"', "\"\""));
                line.push('"');
            } else {
                line.push_str(&t);
            }
        };
        let crossing = spills.at(row);
        if crossing.is_empty() {
            for (col, _) in sheet.cells.row(row, c0, c1) {
                field(col, cell_text(CellRef::new(row, col)), &mut line);
            }
        } else {
            // Spilled values are not stored cells: walk the columns up to the last one that can
            // hold anything.
            let spill_end = crossing.iter().map(|s| s.end.col.min(c1)).max().unwrap_or(c0);
            let stored_end = sheet.cells.row(row, c0, c1).map(|(c, _)| c).last().unwrap_or(c0);
            for col in c0..=spill_end.max(stored_end) {
                field(col, cell_text(CellRef::new(row, col)), &mut line);
            }
        }
        // The first row is as wide as the range: readers that take their width from it (pandas) see
        // every later row fit. A row with nothing in it is one empty quoted field, not an empty
        // line, which readers skip.
        if row == range.start.row {
            match prev {
                Some(p) => line.extend(std::iter::repeat_n(delim, (c1 - p) as usize)),
                None if c1 > c0 => line.extend(std::iter::repeat_n(delim, (c1 - c0) as usize)),
                None => line.push_str("\"\""),
            }
        } else if prev.is_none() {
            line.push_str("\"\"");
        }
        line.push_str("\r\n");
        w.write_all(line.as_bytes()).map_err(|e| IoError::Zip(format!("writing the CSV: {e}")))?;
        rows += 1;
    }
    Ok(rows)
}
