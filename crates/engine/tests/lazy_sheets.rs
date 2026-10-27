//! WHIQ: a workbook several times the memory budget opens without reading its plain sheets, a
//! sheet is read the first time it is looked at, sheets not in use leave memory between commands
//! (edits and all), a save writes every sheet, and only a sheet that cannot fit by itself is
//! refused. The test sets the process's memory budget, so it has the binary to itself.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use std::sync::Arc;

use gridcraft_core::{CellRef, Value};
use gridcraft_engine::Session;
use gridcraft_model::{Cell, Formula, Sheet, Workbook};
use serde_json::json;

/// The tests set the process-wide memory budget: one at a time.
static BUDGET: std::sync::Mutex<()> = std::sync::Mutex::new(());

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
    let mut calc = Sheet::new("Totals");
    for r in 0..ROWS {
        calc.cells.set(CellRef::new(r, 0), Cell::formula(Formula::new(&format!("=Data0!B{}*2", r + 1))));
    }
    wb.sheets.push(Arc::new(calc));
    wb
}

fn sheet_bytes() -> u64 {
    u64::from(ROWS) * 3 * gridcraft_model::CELL_BYTES
}

fn peak_rss_mb() -> u64 {
    std::fs::read_to_string("/proc/self/status")
        .ok()
        .and_then(|t| t.lines().find(|l| l.starts_with("VmHWM:")).and_then(|l| l.split_whitespace().nth(1)?.parse::<u64>().ok()))
        .unwrap_or(0)
        >> 10
}

#[test]
fn a_workbook_several_times_the_budget_opens_edits_one_sheet_and_saves_intact() {
    let _one = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("gc-engine-lazy-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (src, out) = (dir.join("big.xlsx"), dir.join("saved.xlsx"));
    gridcraft_engine::io::save_to_path(&book(), src.to_str().unwrap(), None).unwrap();

    // The budget is a fifth of what the cells of the workbook take; a data sheet is a share.
    let in_memory = sheet_bytes() * SHEETS as u64 + u64::from(ROWS) * gridcraft_model::CELL_BYTES;
    let budget = in_memory / 5;
    gridcraft_core::memory::set_budget(budget);
    let share = gridcraft_engine::pressure::share();
    assert!(sheet_bytes() <= share + share / 4, "a data sheet is about a share of the budget");

    let mut s = Session::new();
    s.execute("file.open", json!({"path": src.to_str().unwrap()})).unwrap();
    let resident = |s: &Session| s.active().unwrap().resident_bytes();
    // Opened: no data sheet read.
    let d = s.active().unwrap();
    assert_eq!(d.wb.sheets.len(), SHEETS + 1);
    assert!((0..SHEETS).all(|i| !d.wb.sheets[i].cells.is_resident()), "no data sheet is in memory after the open");
    assert!(resident(&s) <= share);
    // A summary of the workbook says where each sheet's cells are without reading any of them.
    let info = s.execute("document.inspect", json!({})).unwrap();
    assert_eq!(info["sheets"][2]["usedRange"], format!("A1:E{ROWS}"));
    assert_eq!(info["sheets"][2]["cells"], 3 * ROWS);
    assert!((0..SHEETS).all(|i| !s.active().unwrap().wb.sheets[i].cells.is_resident()), "the summary read no data sheet");

    // Formulas saved with no value are calculated on open, from sheets that stayed on disk.
    let got = s.execute("cell.get", json!({"cell": "Totals!A5"})).unwrap();
    assert_eq!(got["value"], 12.0, "=Data0!B5*2 was calculated: {got}");

    // Reading a cell reads its sheet (and the pass between commands puts it back).
    let got = s.execute("cell.get", json!({"cell": "Data3!A11"})).unwrap();
    assert_eq!(got["value"], "sheet 3 item 10 of a long bill of quantities");
    assert!(resident(&s) <= share + sheet_bytes(), "never more than a share and the sheet in use: {} MB", resident(&s) >> 20);

    // Edit one sheet; sheets are used in turn and the memory stays within a share and one sheet.
    s.execute("cell.set", json!({"cell": "Data3!C6", "input": "edited"})).unwrap();
    for i in 0..SHEETS {
        let got = s.execute("cell.get", json!({"cell": format!("Data{i}!B3")})).unwrap();
        assert_eq!(got["value"], 3.0 + i as f64);
        assert!(resident(&s) <= share + sheet_bytes(), "after sheet {i}: {} MB", resident(&s) >> 20);
    }
    assert_eq!(s.execute("cell.get", json!({"cell": "Data3!C6"})).unwrap()["value"], "edited", "an edit survives its sheet leaving memory");

    // Save As streams every sheet.
    s.execute("file.saveAs", json!({"path": out.to_str().unwrap()})).unwrap();
    assert!(resident(&s) <= share + sheet_bytes());
    eprintln!("cells in memory would be ~{} MB, budget {} MB, peak RSS {} MB", in_memory >> 20, budget >> 20, peak_rss_mb());

    // The saved workbook, whole, is the original with the one edit.
    gridcraft_core::memory::set_budget(0);
    let original = book();
    let (back, _) = gridcraft_engine::io::open_xlsx_file(out.to_str().unwrap()).unwrap().unwrap();
    assert_eq!(back.sheets.len(), SHEETS + 1);
    for i in 0..=SHEETS {
        let (a, b) = (back.sheet(i).unwrap(), original.sheet(i).unwrap());
        assert_eq!(a.name, b.name);
        if i == 3 {
            assert_eq!(a.cells.len(), b.cells.len() + 1);
            assert_eq!(a.value(CellRef::new(5, 2)), Value::from("edited"));
        } else if i < SHEETS {
            assert_eq!(a.cells, b.cells, "sheet {} is intact", a.name);
        } else {
            assert_eq!(a.cells.len(), b.cells.len());
            assert_eq!(a.value(CellRef::new(4, 0)), Value::Number(12.0), "the saved formula carries its value");
        }
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn only_a_sheet_that_cannot_fit_by_itself_is_refused_and_a_save_still_writes_it() {
    let _one = BUDGET.lock().unwrap_or_else(|e| e.into_inner());
    let dir = std::env::temp_dir().join(format!("gc-engine-lazy-refuse-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let (src, out) = (dir.join("big.xlsx"), dir.join("copy.xlsx"));
    let mut wb = book();
    wb.sheets.truncate(3);
    gridcraft_engine::io::save_to_path(&wb, src.to_str().unwrap(), None).unwrap();
    // A budget below one sheet's cells.
    gridcraft_core::memory::set_budget(sheet_bytes() / 2);
    let mut s = Session::new();
    s.execute("file.open", json!({"path": src.to_str().unwrap()})).unwrap();
    s.execute("sheet.activate", json!({"sheet": "Data1"})).unwrap();
    let err = s.execute("cell.get", json!({"cell": "A1"})).unwrap_err().to_string();
    assert!(err.contains("too large") && err.contains("Data1") && err.contains("MB"), "{err}");
    // Not read, not changed, still saved.
    s.execute("file.saveAs", json!({"path": out.to_str().unwrap()})).unwrap();
    gridcraft_core::memory::set_budget(0);
    let (back, _) = gridcraft_engine::io::open_xlsx_file(out.to_str().unwrap()).unwrap().unwrap();
    for i in 0..3 {
        assert_eq!(back.sheet(i).unwrap().cells, wb.sheet(i).unwrap().cells, "sheet {i}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}

/// Measurement, not a check: `GC_MEASURE=gen:<file>` writes a 2.4 M-cell workbook; `GC_MEASURE=open:<file>:<MB>` opens it
/// with that memory budget (0 = the machine's default, which holds everything), edits a cell on one
/// sheet, saves it and prints the peak resident memory. Run each in a process of its own:
/// `cargo test -p gridcraft-engine --test lazy_sheets measure -- --ignored --nocapture`.
#[test]
#[ignore]
fn measure() {
    let Ok(mode) = std::env::var("GC_MEASURE") else { return };
    let parts: Vec<&str> = mode.split(':').collect();
    match parts.as_slice() {
        ["gen", file] => {
            let mut wb = Workbook::new();
            wb.sheets.clear();
            for s in 0..8 {
                let mut sh = Sheet::new(format!("Data{s}"));
                for r in 0..100_000u32 {
                    sh.cells.set(CellRef::new(r, 0), Cell::value(Value::from(format!("sheet {s} item {r} of a long bill of quantities").as_str())));
                    sh.cells.set(CellRef::new(r, 1), Cell::value(Value::Number(f64::from(r) * 1.5 + s as f64)));
                    sh.cells.set(CellRef::new(r, 4), Cell::value(Value::Bool(r % 2 == 0)));
                }
                wb.sheets.push(Arc::new(sh));
            }
            gridcraft_engine::io::save_to_path(&wb, file, None).unwrap();
        }
        ["open", file, mb] => {
            gridcraft_core::memory::set_budget(mb.parse::<u64>().unwrap() << 20);
            let mut s = Session::new();
            eprintln!("MEASURED start: peak RSS {} MB", peak_rss_mb());
            s.execute("file.open", json!({"path": file})).unwrap();
            eprintln!("MEASURED opened: peak RSS {} MB", peak_rss_mb());
            s.execute("cell.set", json!({"cell": "Data5!C6", "input": "edited"})).unwrap();
            eprintln!("MEASURED edited: peak RSS {} MB", peak_rss_mb());
            let out = format!("{file}.saved.xlsx");
            s.execute("file.saveAs", json!({"path": out})).unwrap();
            let _ = std::fs::remove_file(&out);
            eprintln!("MEASURED budget {mb} MB: peak RSS {} MB", peak_rss_mb());
        }
        _ => {}
    }
}
