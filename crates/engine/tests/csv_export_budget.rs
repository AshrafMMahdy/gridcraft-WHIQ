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
    // The first row padded to the width (16,383 commas), 1,048,574 blank rows of `""` (4 bytes
    // each), and the last row's 16,383 commas and field.
    let expected = (1 + 16_383 + 2) + 1_048_574 * 4 + 16_383 + 1 + 2;
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
    for (corner, bytes) in [("C3", 5 + 4 + 5), ("Z10", 28 + 8 * 4 + 28), ("XFD8", 16_386 + 6 * 4 + 16_386)] {
        let mut s = session_with(&[("A1", "a"), (corner, "b")]);
        assert_eq!(s.execute("file.saveBytes", json!({"format": "csv"})).unwrap()["bytes"], bytes, "{corner}");
    }
}

/// A pandas that is installed reads the export of a sparse sheet at the shape of its used range:
/// the first row is as wide as the range and a blank row is not skipped.
#[test]
fn pandas_reads_a_sparse_export_at_the_shape_of_the_sheet() {
    let python = ["python3.13", "python3", "python"]
        .into_iter()
        .find(|p| std::process::Command::new(p).args(["-c", "import pandas"]).output().is_ok_and(|o| o.status.success()));
    let Some(python) = python else {
        eprintln!("pandas is not installed: skipped");
        return;
    };
    let dir = std::env::temp_dir().join(format!("gridcraft-csv-pandas-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    for (cells, shape) in [(vec![("A1", "h1"), ("B1", "h2"), ("E5", "note")], (5, 5)), (vec![("B2", "x"), ("Z50", "y")], (49, 25))] {
        let mut s = session_with(&cells);
        let path = dir.join("sparse.csv");
        s.execute("file.exportCsv", json!({"path": path.to_str().unwrap()})).unwrap();
        let code = format!("import pandas as pd; d = pd.read_csv(r'{}', header=None); print(d.shape[0], d.shape[1])", path.display());
        let out = std::process::Command::new(python).args(["-c", &code]).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        assert_eq!(String::from_utf8_lossy(&out.stdout).trim(), format!("{} {}", shape.0, shape.1), "{cells:?}");
    }
    std::fs::remove_dir_all(&dir).unwrap();
}
