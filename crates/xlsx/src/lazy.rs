//! WHIQ: worksheets whose cells stay in the file. A workbook larger than the memory it may use
//! cannot hold every sheet's cells, and most of its sheets are never looked at in a session, so a
//! plain worksheet (values only: no formula, no cell picture) is read for everything but its
//! cells, and its cells are parsed from the package the first time one is asked for
//! (`gridcraft_model::CellStore::from_cold`). The parse is the one a whole read does for a
//! plain cell: same values, same styles (the style table is complete before any sheet is read).

use std::sync::Arc;

use gridcraft_core::{CellRef, DateSystem, MAX_COLS, MAX_ROWS, RangeRef, Value};
use gridcraft_model::{Cell, CellStore, ColdCells, StyleId};

use crate::package::{Package, PosFile, Source};
use crate::read::Ctx;
use crate::xml::{self, El};

/// What reading a worksheet's cells later needs, shared by all the sheets of a package.
pub struct Shared {
    file: PosFile,
    stamp: (u64, Option<std::time::SystemTime>),
    /// The shared strings part: a sheet takes the strings it uses from it when it is read.
    sst_part: Option<String>,
    xf_map: Arc<Vec<StyleId>>,
    date_system: DateSystem,
}

impl std::fmt::Debug for Shared {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Shared({:?}, {} formats)", self.sst_part, self.xf_map.len())
    }
}

#[derive(Clone, Debug)]
pub struct LazyPlan {
    shared: Arc<Shared>,
}

impl LazyPlan {
    /// The plan for the package being read, `None` when it is not read from a file.
    pub fn new(cx: &Ctx<'_>, sst_part: Option<String>) -> Option<LazyPlan> {
        let file = cx.pkg.file()?;
        let stamp = file.stamp();
        Some(LazyPlan { shared: Arc::new(Shared { file, stamp, sst_part, xf_map: Arc::new(cx.xf_map.clone()), date_system: cx.date_system }) })
    }

    pub(crate) fn cells_of(&self, part: &str, sheet: &str, cells: u64, used: Option<RangeRef>) -> XlsxCells {
        XlsxCells {
            shared: self.shared.clone(),
            part: part.to_string(),
            sheet: sheet.to_string(),
            cells: usize::try_from(cells).unwrap_or(usize::MAX),
            used,
        }
    }
}

/// The cells of one worksheet part.
#[derive(Debug)]
pub struct XlsxCells {
    shared: Arc<Shared>,
    part: String,
    sheet: String,
    cells: usize,
    used: Option<RangeRef>,
}

impl ColdCells for XlsxCells {
    fn load(&self) -> Result<CellStore, String> {
        let sh = &self.shared;
        if sh.file.stamp() != sh.stamp {
            return Err("the workbook file changed on disk after it was opened; open it again".into());
        }
        let pkg = Package::open_source(Source::File(sh.file.clone())).map_err(|e| e.to_string())?;
        let mut reader = pkg.part_reader(&self.part).ok_or_else(|| format!("missing part {}", self.part))?;
        let mut store = CellStore::new();
        let mut next_row = 0u32;
        let mut dropped = 0u64;
        let mut warnings: Vec<&'static str> = Vec::new();
        // Cells of shared strings, by the string's index; the strings are read from their part
        // afterwards, only the ones this sheet uses.
        let mut pending: Vec<(CellRef, u32, StyleId)> = Vec::new();
        reader
            .read(|r| {
                use std::io::{BufRead, Read};
                let mut buf = std::io::BufReader::with_capacity(1 << 16, r);
                let head = buf.fill_buf().map_err(|e| crate::IoError::Zip(e.to_string()))?.to_vec();
                let mut on_row = |row: El| {
                    read_plain_row(sh, &mut store, &mut next_row, &mut dropped, &mut warnings, &mut pending, &row);
                    Ok(())
                };
                if xml::is_utf16(&head) {
                    let mut all = Vec::new();
                    buf.read_to_end(&mut all).map_err(|e| crate::IoError::Zip(e.to_string()))?;
                    xml::parse_streaming(&all, &["row"], &mut on_row)
                } else {
                    if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
                        buf.consume(3);
                    }
                    xml::parse_streaming_reader(buf, &["row"], &mut on_row)
                }
            })
            .map_err(|e| format!("sheet \"{}\" is damaged: {e}", self.sheet))?;
        if !pending.is_empty() {
            let strings = shared_strings_used(sh, &pending)?;
            for (pos, idx, style) in pending {
                let value = match strings.get(&idx) {
                    Some(t) => Value::Text(t.clone()),
                    None => {
                        warnings.push("some cells refer to missing shared strings");
                        Value::Empty
                    }
                };
                store.set(pos, Cell { value, formula: None, style });
            }
        }
        store.shrink_to_fit();
        if dropped > 0 {
            log::warn!("sheet \"{}\": {dropped} cells outside the 1,048,576 × 16,384 grid were dropped", self.sheet);
        }
        warnings.dedup();
        for w in warnings {
            log::warn!("sheet \"{}\": {w}", self.sheet);
        }
        Ok(store)
    }

    fn cells(&self) -> usize {
        self.cells
    }

    fn formulas(&self) -> bool {
        false
    }

    fn used_range_hint(&self) -> Option<Option<RangeRef>> {
        Some(self.used)
    }

    fn describe(&self) -> String {
        format!("worksheet {} of the workbook file", self.part)
    }
}

/// The shared strings with these indices, read from their part in one pass (which stops at the
/// largest index); the part is never held, nor the strings no cell of the sheet uses.
fn shared_strings_used(sh: &Shared, pending: &[(CellRef, u32, StyleId)]) -> Result<std::collections::HashMap<u32, Arc<str>>, String> {
    let mut wanted: Vec<u32> = pending.iter().map(|p| p.1).collect();
    wanted.sort_unstable();
    wanted.dedup();
    let mut found = std::collections::HashMap::with_capacity(wanted.len());
    let Some(part) = &sh.sst_part else { return Ok(found) };
    let pkg = Package::open_source(Source::File(sh.file.clone())).map_err(|e| e.to_string())?;
    let Some(mut reader) = pkg.part_reader(part) else { return Ok(found) };
    let last = wanted.last().copied().unwrap_or(0);
    let mut next = 0usize;
    let mut index = 0u32;
    let res = reader.read(|r| {
        use std::io::BufRead;
        let mut buf = std::io::BufReader::with_capacity(1 << 16, r);
        let head = buf.fill_buf().map_err(|e| crate::IoError::Zip(e.to_string()))?.to_vec();
        if head.starts_with(&[0xEF, 0xBB, 0xBF]) {
            buf.consume(3);
        }
        xml::parse_streaming_reader(buf, &["si"], &mut |si| {
            if wanted.get(next) == Some(&index) {
                found.insert(index, Arc::<str>::from(xml::rich_text(&si)));
                next += 1;
                while wanted.get(next) == Some(&index) {
                    next += 1;
                }
            }
            if index >= last {
                return Err(crate::IoError::Xml("done".into()));
            }
            index += 1;
            Ok(())
        })
    });
    match res {
        Ok(_) => Ok(found),
        Err(crate::IoError::Xml(m)) if m == "done" => Ok(found),
        Err(e) => Err(format!("the shared strings could not be read: {e}")),
    }
}

fn read_plain_row(
    sh: &Shared,
    store: &mut CellStore,
    next_row: &mut u32,
    dropped: &mut u64,
    warnings: &mut Vec<&'static str>,
    pending: &mut Vec<(CellRef, u32, StyleId)>,
    row: &El,
) {
    let r = match row.attr_u32("r") {
        Some(r) if r >= 1 => r - 1,
        Some(_) => return,
        None => *next_row,
    };
    if r >= MAX_ROWS {
        return;
    }
    *next_row = r + 1;
    let mut next_col = 0u32;
    for c in row.kids("c") {
        let pos = match c.attr("r") {
            Some(a) => match CellRef::parse(a.trim()) {
                Some(p) => p,
                None => {
                    *dropped += 1;
                    continue;
                }
            },
            None => CellRef::new(r, next_col),
        };
        if pos.col >= MAX_COLS || pos.row >= MAX_ROWS {
            *dropped += 1;
            continue;
        }
        next_col = pos.col + 1;
        let style = c.attr_u32("s").and_then(|i| sh.xf_map.get(i as usize).copied()).unwrap_or_default();
        if c.attr("t") == Some("s") {
            match c.child("v").and_then(|v| v.text.trim().parse::<u32>().ok()) {
                Some(i) => pending.push((pos, i, style)),
                None => store.set(pos, Cell { value: Value::Empty, formula: None, style }),
            }
            continue;
        }
        let value = crate::sheet_read::cell_value(c, &[], sh.date_system, &mut |m| warnings.push(m));
        store.set(pos, Cell { value, formula: None, style });
    }
}
