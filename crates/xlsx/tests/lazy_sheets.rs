//! WHIQ: a workbook several times the memory budget opens without reading its plain worksheets,
//! reads one when it is first looked at, saves sheet by sheet (the unread sheets from the file)
//! and every sheet comes out intact. This test sets the process's memory budget, so it has the
//! binary to itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use gridcraft_core::{CellRef, Value};
use gridcraft_model::{Cell, Formula, Sheet, Workbook};
use gridcraft_xlsx::{XlsxKind, read_xlsx_file, write_xlsx_into};

const SHEETS: usize = 8;
const ROWS: u32 = 30_000;

fn book() -> Workbook {
    let mut wb = Workbook::new();
    wb.sheets.clear();
    for s in 0..SHEETS {
        let mut sh = Sheet::new(format!("Data{s}"));
        for r in 0..ROWS {
            sh.cells.set(CellRef::new(r, 0), Cell::value(Value::from(format!("sheet {s} item {r} of a long bill of quantities").as_str())));
            sh.cells.set(CellRef::new(r, 1), Cell::value(Value::Number(f64::from(r) * 1.5 + s as f64)));
            sh.cells.set(CellRef::new(r, 4), Cell::value(Value::Bool(r % 2 == 0)));
        }
        wb.sheets.push(Arc::new(sh));
    }
    // A sheet with formulas is read whole (a calculation needs them), but still leaves memory.
    let mut calc = Sheet::new("Totals");
    for r in 0..ROWS {
        calc.cells.set(CellRef::new(r, 0), Cell::formula(Formula::new(&format!("=Data0!B{}*2", r + 1))));
    }
    wb.sheets.push(Arc::new(calc));
    wb
}

fn peak_rss_kb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse().ok()))
        .unwrap_or(0)
}

#[test]
fn a_workbook_several_times_the_budget_opens_edits_one_sheet_and_saves_intact() {
    let dir = std::env::temp_dir().join(format!("gc-lazy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (src, out) = (dir.join("big.xlsx"), dir.join("saved.xlsx"));
    {
        let wb = book();
        let mut f = std::io::BufWriter::new(std::fs::File::create(&src).unwrap());
        write_xlsx_into(&wb, XlsxKind::Workbook, &mut f).unwrap();
    }
    let file_len = std::fs::metadata(&src).unwrap().len();

    // The cells of the workbook in memory would take about this much; the budget is a fraction.
    let in_memory = (u64::from(ROWS) * 3 * SHEETS as u64 + u64::from(ROWS)) * gridcraft_model::CELL_BYTES;
    let budget = in_memory / 6;
    gridcraft_core::memory::set_budget(budget);

    let (mut wb, report) = read_xlsx_file(std::fs::File::open(&src).unwrap()).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(wb.sheets.len(), SHEETS + 1);
    // Nothing of the plain sheets was read; the formula sheet was written to disk as it was.
    let resident: u64 = wb.sheets.iter().map(|s| s.cells.resident_bytes()).sum();
    assert_eq!(resident, wb.sheet(SHEETS).unwrap().cells.resident_bytes(), "only the formula sheet is in memory after the open");
    assert!(resident <= budget / 2, "{resident} is within half the budget");
    for s in 0..SHEETS {
        let sh = wb.sheet(s).unwrap();
        assert!(sh.cells.is_cold_without_formulas() && sh.cells.len() == 3 * ROWS as usize, "{}: {}", sh.name, sh.cells.len());
    }
    assert!(!wb.sheet(SHEETS).unwrap().cells.is_cold_without_formulas());
    let after_open = peak_rss_kb();

    // Looking at one cell reads that sheet and no other.
    let v = wb.sheet(3).unwrap().value(CellRef::new(10, 0));
    assert_eq!(v, Value::from("sheet 3 item 10 of a long bill of quantities"));
    assert!(wb.sheet(3).unwrap().cells.is_resident());
    assert_eq!(wb.sheets.iter().filter(|s| s.cells.is_resident()).count(), 2, "the formula sheet and this one");

    // Edit it; the others stay in the file.
    Arc::make_mut(&mut wb.sheets[3]).cells.set(CellRef::new(5, 2), Cell::value(Value::from("edited")));
    {
        let mut f = std::io::BufWriter::new(std::fs::File::create(&out).unwrap());
        write_xlsx_into(&wb, XlsxKind::Workbook, &mut f).unwrap();
    }
    let resident_after_save = wb.sheets.iter().filter(|s| s.cells.is_resident()).count();
    assert_eq!(resident_after_save, 2, "a save reads the other sheets one at a time and keeps none");
    let peak = peak_rss_kb();
    eprintln!(
        "file {} MB; cells in memory would be ~{} MB; budget {} MB; peak RSS after open {} MB, after save {} MB",
        file_len >> 20,
        in_memory >> 20,
        budget >> 20,
        after_open >> 10,
        peak >> 10
    );

    // Read it back whole and compare every sheet with what was written.
    gridcraft_core::memory::set_budget(0);
    let original = book();
    let (back, report) = read_xlsx_file(std::fs::File::open(&out).unwrap()).unwrap();
    assert!(report.warnings.is_empty(), "{:?}", report.warnings);
    assert_eq!(back.sheets.len(), SHEETS + 1);
    for s in 0..=SHEETS {
        let (a, b) = (back.sheet(s).unwrap(), original.sheet(s).unwrap());
        assert_eq!(a.name, b.name);
        if s == 3 {
            assert_eq!(a.cells.len(), b.cells.len() + 1);
            assert_eq!(a.value(CellRef::new(5, 2)), Value::from("edited"));
            for r in (0..ROWS).step_by(997) {
                assert_eq!(a.value(CellRef::new(r, 0)), b.value(CellRef::new(r, 0)));
            }
        } else {
            assert_eq!(a.cells, b.cells, "sheet {} is intact", a.name);
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
