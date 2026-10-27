//! WHIQ: a delimited export has no size cap and writes its rows ragged (the empty cells at the end
//! of a row are dropped, a blank row is an empty line), so two cells far apart in a sheet make a
//! small file, not the whole rectangle between them (17 GB of commas), and it reads back to the
//! same cells.

use gridcraft_engine::Session;
use serde_json::json;

fn session_with(cells: &[(&str, &str)]) -> Session {
    let mut s = Session::new();
    s.new_workbook();
    for (cell, input) in cells {
        s.execute("cell.set", json!({"cell": cell, "input": input})).unwrap();
    }
    s
}

#[test]
fn a_sparse_sheet_spanning_the_grid_exports_small_and_reads_back_to_the_same_cells() {
    let mut s = session_with(&[("A1", "a"), ("XFD1048576", "b")]);
    // 1,048,574 blank lines (2 bytes each), 16,383 commas and the two fields.
    let expected = 3 + 1_048_574 * 2 + 16_383 + 1 + 2;
    for format in ["csv", "tsv"] {
        assert_eq!(s.execute("file.saveBytes", json!({"format": format})).unwrap()["bytes"], expected, "{format}");
    }
    let dir = std::env::temp_dir().join(format!("gridcraft-csv-budget-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let path = dir.join("corners.csv");
    let out = s.execute("file.exportCsv", json!({"path": path.to_str().unwrap()})).unwrap();
    assert_eq!(out["bytes"], expected);
    assert_eq!(std::fs::metadata(&path).unwrap().len(), expected);
    // Open it again: the same two cells, nothing else.
    s.execute("file.open", json!({"path": path.to_str().unwrap()})).unwrap();
    assert_eq!(s.execute("cell.get", json!({"cell": "A1"})).unwrap()["value"], "a");
    assert_eq!(s.execute("cell.get", json!({"cell": "XFD1048576"})).unwrap()["value"], "b");
    assert_eq!(s.execute("cell.get", json!({"cell": "B2"})).unwrap()["value"], json!(null));
    std::fs::remove_dir_all(&dir).unwrap();
}

#[test]
fn exports_keep_their_positions_and_drop_only_trailing_empties() {
    for (corner, bytes) in [("C3", 3 + 2 + 5), ("Z10", 3 + 8 * 2 + 25 + 1 + 2), ("XFD8", 3 + 6 * 2 + 16_383 + 1 + 2)] {
        let mut s = session_with(&[("A1", "a"), (corner, "b")]);
        assert_eq!(s.execute("file.saveBytes", json!({"format": "csv"})).unwrap()["bytes"], bytes, "{corner}");
    }
}
