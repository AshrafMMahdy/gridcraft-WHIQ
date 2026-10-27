use gridcraft_core::{CellRef, DateSystem, Value};
use gridcraft_model::{Sheet, Workbook};

use crate::csv::{decode_text, sniff_delimiter};
use crate::{CsvOptions, Format, read_csv, sniff, write_csv, write_csv_range_into};

fn v(wb: &Workbook, r: u32, c: u32) -> Value {
    wb.sheet(0).unwrap().value(CellRef::new(r, c))
}

#[test]
fn quoting_and_newlines() {
    let src = "a,\"b,c\",\"say \"\"hi\"\"\"\r\n\"multi\nline\",,3\n\nlast";
    let wb = read_csv(src.as_bytes(), &CsvOptions::default()).unwrap();
    assert_eq!(wb.sheets[0].name, "Sheet1");
    assert_eq!(v(&wb, 0, 0), Value::text("a"));
    assert_eq!(v(&wb, 0, 1), Value::text("b,c"));
    assert_eq!(v(&wb, 0, 2), Value::text("say \"hi\""));
    assert_eq!(v(&wb, 1, 0), Value::text("multi\nline"));
    assert_eq!(v(&wb, 1, 1), Value::Empty);
    assert_eq!(v(&wb, 1, 2), Value::Number(3.0));
    assert_eq!(v(&wb, 2, 0), Value::Empty);
    assert_eq!(v(&wb, 3, 0), Value::text("last"));
}

#[test]
fn values_and_formats() {
    let src = "1,2.5,\"1,234\",50%,TRUE,#N/A,2026-03-04,$5.00,'007,text";
    let wb = read_csv(src.as_bytes(), &CsvOptions::default()).unwrap();
    assert_eq!(v(&wb, 0, 0), Value::Number(1.0));
    assert_eq!(v(&wb, 0, 1), Value::Number(2.5));
    assert_eq!(v(&wb, 0, 2), Value::Number(1234.0));
    assert_eq!(v(&wb, 0, 3), Value::Number(0.5));
    assert_eq!(v(&wb, 0, 4), Value::Bool(true));
    assert!(v(&wb, 0, 5).is_error());
    assert!(v(&wb, 0, 6).is_number());
    let s = wb.sheet(0).unwrap();
    let st = wb.styles.get(s.cell(CellRef::new(0, 6)).unwrap().style);
    assert_ne!(st.num_fmt.as_str(), "General");
    assert_eq!(v(&wb, 0, 8), Value::text("007"));
    // 1904 dates differ from 1900 dates.
    let wb2 = read_csv(b"2026-03-04", &CsvOptions { date_system: DateSystem::D1904, ..Default::default() }).unwrap();
    assert_eq!(wb2.date_system, DateSystem::D1904);
    assert_eq!(v(&wb, 0, 6).as_f64().unwrap() - v(&wb2, 0, 0).as_f64().unwrap(), 1462.0);
    // parse_values off keeps text.
    let wb3 = read_csv(b"1,TRUE", &CsvOptions { parse_values: false, ..Default::default() }).unwrap();
    assert_eq!(v(&wb3, 0, 0), Value::text("1"));
}

#[test]
fn encodings() {
    let mut b = vec![0xEF, 0xBB, 0xBF];
    b.extend_from_slice("é,x".as_bytes());
    assert_eq!(decode_text(&b), "é,x");
    let mut le = vec![0xFF, 0xFE];
    for u in "ü;1".encode_utf16() {
        le.extend_from_slice(&u.to_le_bytes());
    }
    assert_eq!(decode_text(&le), "ü;1");
    let mut be = vec![0xFE, 0xFF];
    for u in "ß".encode_utf16() {
        be.extend_from_slice(&u.to_be_bytes());
    }
    assert_eq!(decode_text(&be), "ß");
    assert_eq!(decode_text(&[0x80, b'a', 0xE9]), "€aé");
    let wb = read_csv(&le, &CsvOptions { delimiter: 0, ..Default::default() }).unwrap();
    assert_eq!(v(&wb, 0, 0), Value::text("ü"));
    assert_eq!(v(&wb, 0, 1), Value::Number(1.0));
}

#[test]
fn delimiter_sniffing() {
    assert_eq!(sniff_delimiter("a;b;c\n1;2;3\n", '"'), b';');
    assert_eq!(sniff_delimiter("a\tb\n1\t2\n", '"'), b'\t');
    assert_eq!(sniff_delimiter("a,b\n\"x;y;z\",2\n", '"'), b',');
    assert_eq!(sniff_delimiter("single", '"'), b',');
    // A delimiter missing from some lines is text, not a separator (single-column export round trip).
    assert_eq!(sniff_delimiter("alpha;beta\r\ngamma\r\n", '"'), b',');
    assert_eq!(sniff_delimiter("alpha\tbeta\r\ngamma\r\n", '"'), b',');
    assert_eq!(sniff_delimiter("a;b\r\n\r\nc;d\r\n", '"'), b';');
    let wb = read_csv(b"x;y\n1,5;2", &CsvOptions { delimiter: 0, ..Default::default() }).unwrap();
    assert_eq!(v(&wb, 1, 1), Value::Number(2.0));
    // The workbook remembers the separator it was read with, sniffed or given.
    assert_eq!(wb.csv_delimiter, Some(b';'));
    assert_eq!(read_csv(b"a\tb", &CsvOptions { delimiter: 0, ..Default::default() }).unwrap().csv_delimiter, Some(b'\t'));
    assert_eq!(read_csv(b"a;b", &CsvOptions::default()).unwrap().csv_delimiter, Some(b','));
    assert_eq!(Workbook::new().csv_delimiter, None);
    assert!(read_csv(b"a", &CsvOptions { delimiter: b'"', ..Default::default() }).is_err());
}

#[test]
fn writing() {
    let mut wb = Workbook::new();
    let mut s = Sheet::new("S");
    s.set_value(CellRef::new(1, 1), Value::text("a,b"));
    s.set_value(CellRef::new(1, 2), Value::text("q\"t"));
    s.set_value(CellRef::new(2, 1), Value::Number(0.1));
    s.set_value(CellRef::new(2, 2), Value::Bool(false));
    s.set_value(CellRef::new(3, 3), Value::text("x\ny"));
    s.set_value(CellRef::new(3, 1), Value::Error(gridcraft_core::CellError::Div0));
    wb.sheets = vec![std::sync::Arc::new(s)];
    // The caller supplies each field's text; quoting and line ends are the writer's.
    let sh = wb.sheet(0).unwrap();
    let raw = |c: CellRef| match sh.value(c) {
        Value::Empty => String::new(),
        Value::Number(n) => gridcraft_core::number_to_text(n),
        Value::Text(t) => t.to_string(),
        Value::Bool(b) => if b { "TRUE" } else { "FALSE" }.into(),
        Value::Error(e) => e.as_str().into(),
        Value::Array(_) => String::new(),
    };
    let out = String::from_utf8(write_csv(sh, b',', raw).unwrap()).unwrap();
    assert_eq!(out, "\"a,b\",\"q\"\"t\",\r\n0.1,FALSE\r\n#DIV/0!,,\"x\ny\"\r\n");
    let semi = String::from_utf8(write_csv(sh, b';', raw).unwrap()).unwrap();
    assert!(semi.starts_with("a,b;"));
    assert!(write_csv(&Sheet::new("E"), b',', raw).unwrap().is_empty());
    // Round trip.
    let back = read_csv(out.as_bytes(), &CsvOptions::default()).unwrap();
    assert_eq!(v(&back, 0, 0), Value::text("a,b"));
    assert_eq!(v(&back, 2, 2), Value::text("x\ny"));
}

#[test]
fn sniffing_formats() {
    assert_eq!(sniff(b"PK\x03\x04rest"), Format::Xlsx);
    assert_eq!(sniff(b"a,b\n1,2"), Format::Csv);
    // An Excel password-protected file is a CFB/OLE2 compound file with an EncryptedPackage
    // stream; any other compound file is a legacy binary (.xls), not an encrypted workbook.
    assert_eq!(sniff(&crate::cfb::tests::build(&["EncryptionInfo", "EncryptedPackage"])), Format::Encrypted);
    assert_eq!(sniff(&crate::cfb::tests::build(&["Workbook"])), Format::LegacyBinary);
    assert_eq!(sniff(&[0xD0, 0xCF, 0x11, 0xE0, 0xA1, 0xB1, 0x1A, 0xE1, 0, 0]), Format::LegacyBinary);
    assert_eq!(sniff(&[0xD0, 0xCF, 0x11, 0xE0, 0, 0, 1, 2]), Format::Unknown);
    assert_eq!(sniff(b""), Format::Unknown);
    assert_eq!(sniff(&[0xFF, 0xFE, b'a', 0]), Format::Csv);
}

#[test]
fn hostile_csv() {
    let _ = read_csv(&[0xFF; 1000], &CsvOptions::default()).unwrap();
    let _ = read_csv(b"\"unterminated,quote\n1,2", &CsvOptions::default()).unwrap();
    let wide = ",".repeat(20_000);
    let wb = read_csv(format!("{wide}x").as_bytes(), &CsvOptions::default()).unwrap();
    assert!(wb.sheet(0).unwrap().cells.is_empty(), "fields past column XFD are dropped");
}

/// A sheet with `a` in A1 and `b` in `corner`.
fn two_cells(corner: (u32, u32)) -> Sheet {
    let mut s = Sheet::new("S");
    s.set_value(CellRef::new(0, 0), Value::text("a"));
    s.set_value(CellRef::new(corner.0, corner.1), Value::text("b"));
    s
}

fn text_of(sh: &Sheet) -> impl FnMut(CellRef) -> String + '_ {
    |c| match sh.value(c) {
        Value::Text(t) => t.to_string(),
        _ => String::new(),
    }
}

#[test]
fn two_far_apart_cells_write_a_small_file_that_reads_back_to_the_same_cells() {
    // A1 and XFD1048576 span the whole grid (16 GiB of commas when every row was padded to the
    // widest): rows are ragged now, the first row alone is as wide as the range, and a blank row
    // is `""`.
    let s = two_cells((gridcraft_core::MAX_ROWS - 1, gridcraft_core::MAX_COLS - 1));
    let out = write_csv(&s, b',', text_of(&s)).unwrap();
    let blank_rows = u64::from(gridcraft_core::MAX_ROWS) - 2;
    let commas = u64::from(gridcraft_core::MAX_COLS) - 1;
    assert_eq!(out.len() as u64, (1 + commas + 2) + blank_rows * 4 + commas + 1 + 2, "first row, blank rows, last row");
    assert!(out.len() < 5 << 20, "{} bytes", out.len());
    let back = read_csv(&out, &CsvOptions { parse_values: false, ..CsvOptions::default() }).unwrap();
    let sh = back.sheet(0).unwrap();
    assert_eq!(sh.cells.len(), 2);
    assert_eq!(sh.value(CellRef::new(0, 0)), Value::text("a"));
    assert_eq!(sh.value(CellRef::new(gridcraft_core::MAX_ROWS - 1, gridcraft_core::MAX_COLS - 1)), Value::text("b"));
}

#[test]
fn rows_are_ragged_the_first_is_full_width_and_blank_rows_are_quoted() {
    // 3 rows x 3 columns: trailing empty cells are dropped (but for the first row, which is as
    // wide as the range), interior ones kept, a blank row is an empty quoted field.
    let s = two_cells((2, 2));
    assert_eq!(String::from_utf8(write_csv(&s, b',', text_of(&s)).unwrap()).unwrap(), "a,,\r\n\"\"\r\n,,b\r\n");
    assert_eq!(write_csv(&s, b'\t', text_of(&s)).unwrap(), b"a\t\t\r\n\"\"\r\n\t\tb\r\n");
    // A stored cell whose text is empty ends no row.
    let mut e = Sheet::new("E");
    e.set_value(CellRef::new(0, 0), Value::text("x"));
    e.set_value(CellRef::new(0, 3), Value::text(""));
    e.set_value(CellRef::new(1, 1), Value::text("y"));
    assert_eq!(String::from_utf8(write_csv(&e, b',', text_of(&e)).unwrap()).unwrap(), "x,,,\r\n,y\r\n");
    // A range starting past column A keeps its own first column.
    let mut out = Vec::new();
    let r = gridcraft_core::RangeRef::parse("B1:D2").unwrap();
    write_csv_range_into(&e, r, b',', &mut out, text_of(&e)).unwrap();
    assert_eq!(String::from_utf8(out).unwrap(), ",,\r\ny\r\n");
}

#[test]
fn a_long_dense_export_is_not_capped() {
    let mut long = Sheet::new("L");
    for r in 0..50u32 {
        long.set_value(CellRef::new(r, 0), Value::text("x".repeat(100)));
    }
    assert_eq!(write_csv(&long, b',', text_of(&long)).unwrap().len(), 50 * 102);
}
