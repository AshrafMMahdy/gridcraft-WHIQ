//! GridCraft file formats: XLSX (transitional and strict) and CSV import/export, plus XLSB and ODS data import
//! and legacy Excel 97-2003 `.xls` (BIFF8) import, and password-encrypted workbooks (Agile and Standard encryption).
//!
//! Clean-room implementation from ECMA-376 / ISO/IEC 29500 and observed behaviour. All entry
//! points work on byte slices (no file system access), so the crate builds for WebAssembly.
//!
//! Reading is lenient: unsupported or broken optional parts are skipped and reported in
//! [`ReadReport::warnings`]; only a missing/invalid package or workbook part is an error. Hostile
//! input is bounded: decompressed parts are capped at 512 MB (1 GB per package), cells outside
//! Excel's grid are dropped and XML nesting is limited.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

mod array_formula;
mod cfb;
mod chart;
mod chartex;
mod crypto;
mod csv;
mod drawing;
mod feature_bags;
mod fmla;
// WHIQ: what an edit did not touch is written back as the file had it.
mod kept;
mod lazy;
mod ods;
mod package;
mod passthrough;
mod pivot;
mod read;
mod richdata;
mod sheet_read;
mod sheet_write;
mod slicers;
mod styles;
mod tables;
#[cfg(any(test, feature = "test-support"))]
pub mod testing;
mod theme;
mod write;
mod xls;
mod xlsb;
mod xml;

#[cfg(test)]
mod tests;

use gridcraft_core::DateSystem;
use gridcraft_model::Workbook;

pub use crypto::{DecryptError, check_encryption, decrypt};
pub use csv::{read_csv, write_csv, write_csv_range_into};
pub use ods::read_ods;
pub use passthrough::rebase_links;
pub use xls::read_xls;
pub use xlsb::read_xlsb;

/// Errors from reading or writing files.
#[derive(Debug, thiserror::Error)]
pub enum IoError {
    #[error("the file is not a valid zip package: {0}")]
    Zip(String),
    #[error("the file contains invalid XML: {0}")]
    Xml(String),
    #[error("the file is not a valid spreadsheet: {0}")]
    Format(String),
    #[error("the file is too large: {0}")]
    TooLarge(String),
}

/// What a lenient read skipped or repaired.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct ReadReport {
    pub warnings: Vec<String>,
}

impl ReadReport {
    pub(crate) fn warn(&mut self, msg: impl Into<String>) {
        let m = msg.into();
        if self.warnings.len() < 1000 && !self.warnings.contains(&m) {
            self.warnings.push(m);
        }
    }
}

/// Reads an `.xlsx` / `.xlsm` / `.xltx` / `.xltm` package (the workbook part is found through the
/// package relationships, so every main-part content type is accepted).
pub fn read_xlsx(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    read::read_xlsx(bytes)
}

/// WHIQ: [`read_xlsx`] from an open file (never read whole into memory; sheets stream).
pub fn read_xlsx_file(file: std::fs::File) -> Result<(Workbook, ReadReport), IoError> {
    read::read_xlsx_file(file)
}

/// WHIQ: [`sniff`] on an open file: its first bytes, and for a zip its directory only.
pub fn sniff_file(file: &std::fs::File) -> Format {
    use std::io::{Read, Seek};
    let Ok(mut f) = file.try_clone() else { return Format::Unknown };
    let mut head = Vec::with_capacity(8);
    if f.seek(std::io::SeekFrom::Start(0)).is_err() || (&mut f).take(8).read_to_end(&mut head).is_err() {
        return Format::Unknown;
    }
    let _ = f.seek(std::io::SeekFrom::Start(0));
    if !(head.starts_with(b"PK\x03\x04") || head.starts_with(b"PK\x05\x06") || head.starts_with(b"PK\x07\x08")) {
        // Not a zip: the caller reads the bytes (CSV, legacy binary…).
        return Format::Unknown;
    }
    let Ok(mut pkg) = package::Package::open_file(f) else { return Format::Unknown };
    if pkg.has("mimetype") {
        // OpenDocument (or something else carrying a mimetype member): the byte sniff decides.
        return Format::Unknown;
    }
    if pkg.has("xl/workbook.bin") {
        return Format::Xlsb;
    }
    let _ = pkg.rels("");
    Format::Xlsx
}

/// Which member of the SpreadsheetML family a package is saved as. Only the content type of the
/// workbook part differs (ECMA-376 Part 1 Workbook part; MS-OI29500 adds the macro-enabled types).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub enum XlsxKind {
    /// `.xlsx`
    #[default]
    Workbook,
    /// `.xlsm`
    MacroWorkbook,
    /// `.xltx`
    Template,
    /// `.xltm`
    MacroTemplate,
}

impl XlsxKind {
    /// The kind a file name's extension asks for; anything else is a plain workbook.
    pub fn from_path(path: &str) -> XlsxKind {
        let ext = std::path::Path::new(path).extension().and_then(|e| e.to_str()).unwrap_or_default().to_ascii_lowercase();
        match ext.as_str() {
            "xlsm" => XlsxKind::MacroWorkbook,
            "xltx" => XlsxKind::Template,
            "xltm" => XlsxKind::MacroTemplate,
            _ => XlsxKind::Workbook,
        }
    }

    /// Content type of `/xl/workbook.xml`.
    pub fn main_content_type(self) -> &'static str {
        match self {
            XlsxKind::Workbook => "application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml",
            XlsxKind::MacroWorkbook => "application/vnd.ms-excel.sheet.macroEnabled.main+xml",
            XlsxKind::Template => "application/vnd.openxmlformats-officedocument.spreadsheetml.template.main+xml",
            XlsxKind::MacroTemplate => "application/vnd.ms-excel.template.macroEnabled.main+xml",
        }
    }

    /// Whether this kind may carry a VBA project (`.xlsm`, `.xltm`).
    pub fn is_macro_enabled(self) -> bool {
        matches!(self, XlsxKind::MacroWorkbook | XlsxKind::MacroTemplate)
    }
}

/// Writes a workbook as an `.xlsx` package, or as an `.xlsm` one when it holds a VBA project.
pub fn write_xlsx(wb: &Workbook) -> Result<Vec<u8>, IoError> {
    write::write_xlsx(wb, if wb.vba_project.is_some() { XlsxKind::MacroWorkbook } else { XlsxKind::Workbook })
}

/// Writes a workbook as the given member of the `.xlsx` family.
pub fn write_xlsx_as(wb: &Workbook, kind: XlsxKind) -> Result<Vec<u8>, IoError> {
    write::write_xlsx(wb, kind)
}

/// WHIQ: [`write_xlsx_as`] straight into `w` (a file): sheets are compressed row by row, and
/// neither their XML nor the package is held whole in memory.
pub fn write_xlsx_into<W: std::io::Write + std::io::Seek>(wb: &Workbook, kind: XlsxKind, w: &mut W) -> Result<(), IoError> {
    write::write_xlsx_into(wb, kind, w)
}

/// CSV import options.
#[derive(Clone, Debug, PartialEq)]
pub struct CsvOptions {
    /// Field separator; `0` auto-detects comma, semicolon or tab.
    pub delimiter: u8,
    pub quote: u8,
    pub date_system: DateSystem,
    /// Interpret fields like typed input (numbers, dates, booleans…); otherwise keep text.
    pub parse_values: bool,
}

impl Default for CsvOptions {
    fn default() -> Self {
        CsvOptions { delimiter: b',', quote: b'"', date_system: DateSystem::D1900, parse_values: true }
    }
}

/// A file format guessed from content.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Format {
    Xlsx,
    Xlsb,
    Ods,
    Csv,
    /// A password-protected (encrypted) workbook: a compound file with an `EncryptedPackage`.
    Encrypted,
    /// An Excel 97-2003 `.xls` workbook: a compound file with a `Workbook` (or BIFF5 `Book`) stream.
    Xls,
    /// Another compound (CFB/OLE2) file: a legacy Office binary that isn't an Excel workbook.
    LegacyBinary,
    Unknown,
}

/// Guesses the format from content: ODS mimetype, XLSB package content types, other ZIP packages as XLSX, text as CSV.
pub fn sniff(bytes: &[u8]) -> Format {
    if bytes.starts_with(b"PK\x03\x04") || bytes.starts_with(b"PK\x05\x06") || bytes.starts_with(b"PK\x07\x08") {
        return if ods::has_mimetype(bytes) {
            Format::Ods
        } else if xlsb::is_xlsb(bytes) {
            Format::Xlsb
        } else {
            Format::Xlsx
        };
    }
    // CFB/OLE2 compound file: a password-protected workbook when it holds an encryption
    // stream, an Excel 97-2003 workbook when it holds a workbook stream, otherwise another
    // legacy binary (.doc…).
    if bytes.starts_with(&cfb::SIGNATURE) {
        return if cfb::is_encrypted(bytes) {
            Format::Encrypted
        } else if xls::is_xls(bytes) {
            Format::Xls
        } else {
            Format::LegacyBinary
        };
    }
    if bytes.starts_with(&[0xFF, 0xFE]) || bytes.starts_with(&[0xFE, 0xFF]) || bytes.starts_with(&[0xEF, 0xBB, 0xBF]) {
        return Format::Csv;
    }
    if bytes.is_empty() {
        return Format::Unknown;
    }
    let head = bytes.get(..bytes.len().min(4096)).unwrap_or(bytes);
    // Binary files (old .xls, images…) contain NULs and many control characters.
    let control = head.iter().filter(|&&b| b < 0x09 || (0x0E..0x20).contains(&b) || b == 0x7F).count();
    if head.contains(&0) || control * 20 > head.len() {
        return Format::Unknown;
    }
    Format::Csv
}

#[cfg(test)]
mod whiq_stream_tests {
    //! WHIQ: workbooks stream through the reader and the writer: neither the file, a sheet's XML
    //! nor the package is held whole, and the result is what the in-memory path gives.
    #![allow(clippy::unwrap_used, clippy::expect_used)]
    use gridcraft_core::{CellRef, Value};
    use gridcraft_model::{Cell, Workbook};

    fn big_book(rows: u32) -> Workbook {
        let mut wb = Workbook::new();
        let sh = wb.sheet_mut(0).unwrap();
        for r in 0..rows {
            sh.cells.set(CellRef::new(r, 0), Cell::value(Value::from(format!("item {r} of a long bill of quantities").as_str())));
            sh.cells.set(CellRef::new(r, 1), Cell::value(Value::Number(f64::from(r) * 1.5)));
            sh.cells.set(CellRef::new(r, 2), Cell::formula(gridcraft_model::Formula::new(&format!("=B{}*2", r + 1))));
        }
        wb
    }

    #[test]
    fn a_workbook_written_into_a_file_and_read_from_it_is_the_in_memory_one() {
        // Enough rows that the sheet's XML is flushed to the staging zip several times (4 MB each).
        let wb = big_book(60_000);
        let bytes = crate::write_xlsx(&wb).unwrap();
        let dir = std::env::temp_dir().join(format!("gc-stream-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("big.xlsx");
        {
            let mut f = std::io::BufWriter::new(std::fs::File::create(&path).unwrap());
            crate::write_xlsx_into(&wb, crate::XlsxKind::Workbook, &mut f).unwrap();
        }
        assert_eq!(std::fs::read(&path).unwrap(), bytes, "the streamed package is byte for byte the in-memory one");
        let file = std::fs::File::open(&path).unwrap();
        assert_eq!(crate::sniff_file(&file), crate::Format::Xlsx);
        let (back, report) = crate::read_xlsx_file(file).unwrap();
        let (mem, _) = crate::read_xlsx(&bytes).unwrap();
        assert!(report.warnings.is_empty(), "{:?}", report.warnings);
        let (a, b) = (back.sheet(0).unwrap(), mem.sheet(0).unwrap());
        assert_eq!(a.cells.len(), 180_000);
        assert_eq!(a.cells, b.cells, "read from the file = read from bytes");
        assert_eq!(a.cells.get(CellRef::new(59_999, 0)).unwrap().value, Value::from("item 59999 of a long bill of quantities"));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_range_streams_out_as_csv() {
        let wb = big_book(3);
        let sh = wb.sheet(0).unwrap();
        let mut out = Vec::new();
        let r = gridcraft_core::RangeRef::parse("A1:B3").unwrap();
        let rows = crate::write_csv_range_into(sh, r, b',', &mut out, |c| sh.value(c).display()).unwrap();
        assert_eq!(rows, 3);
        let text = String::from_utf8(out).unwrap();
        assert_eq!(text.lines().count(), 3);
        assert!(text.starts_with("item 0 of a long bill of quantities,0\r\n"), "{text}");
    }
}
