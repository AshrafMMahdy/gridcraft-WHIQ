//! OPC package access: reading zip parts with decompression limits, relationships and part-name
//! resolution; writing a zip package deterministically.
//!
//! WHIQ: a package is read from bytes in memory or straight from the file (never read whole):
//! parts are inflated on request, and a worksheet streams from the zip into the cell store
//! ([`Package::part_reader`]) without its XML ever being held whole. The in-memory limits are
//! shares of the process's memory budget ([`gridcraft_core::memory`]).

use std::collections::HashMap;
use std::io::{Cursor, Read, Seek, SeekFrom, Write};
use std::sync::Arc;

use crate::IoError;
use crate::xml::{self, El};

/// Largest decompressed part read into memory at the smallest budget (half the memory budget
/// when that is more). Worksheets are streamed, not read into memory.
pub const MAX_PART: u64 = 512 * 1024 * 1024;
/// Largest total decompressed size read into memory from one package at the smallest budget
/// (twice the memory budget when that is more).
pub const MAX_TOTAL: u64 = 1024 * 1024 * 1024;
/// Most entries accepted in the zip directory at the smallest budget (one per kilobyte of the
/// budget when that is more).
pub const MAX_ENTRIES: usize = 200_000;

fn max_part() -> u64 {
    MAX_PART.max(gridcraft_core::memory::budget() / 2)
}
fn max_total() -> u64 {
    MAX_TOTAL.max(gridcraft_core::memory::budget().saturating_mul(2))
}
fn max_entries() -> usize {
    MAX_ENTRIES.max(usize::try_from(gridcraft_core::memory::budget() / 1024).unwrap_or(usize::MAX))
}

/// A file read by position, so clones of a package's zip each keep their own place.
#[derive(Clone, Debug)]
pub struct PosFile {
    file: Arc<std::fs::File>,
    pos: u64,
    len: u64,
}

impl PosFile {
    pub fn new(file: std::fs::File) -> std::io::Result<PosFile> {
        let len = file.metadata()?.len();
        Ok(PosFile { file: Arc::new(file), pos: 0, len })
    }
}

impl Read for PosFile {
    #[cfg_attr(not(any(unix, windows)), allow(unused_variables, unreachable_code))]
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        #[cfg(unix)]
        let n = std::os::unix::fs::FileExt::read_at(&*self.file, buf, self.pos)?;
        #[cfg(windows)]
        let n = std::os::windows::fs::FileExt::seek_read(&*self.file, buf, self.pos)?;
        #[cfg(not(any(unix, windows)))]
        let n = {
            let _ = buf;
            return Err(std::io::Error::new(std::io::ErrorKind::Unsupported, "positional reads"));
        };
        self.pos = self.pos.saturating_add(n as u64);
        Ok(n)
    }
}

impl Seek for PosFile {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        let next = match to {
            SeekFrom::Start(n) => Some(n),
            SeekFrom::End(d) => self.len.checked_add_signed(d),
            SeekFrom::Current(d) => self.pos.checked_add_signed(d),
        };
        self.pos = next.ok_or_else(|| std::io::Error::new(std::io::ErrorKind::InvalidInput, "seek before the start"))?;
        Ok(self.pos)
    }
}

/// Where a package's bytes are: in memory, or in a file read by position.
#[derive(Clone, Debug)]
pub enum Source<'a> {
    Mem(Cursor<&'a [u8]>),
    File(PosFile),
}

impl Read for Source<'_> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        match self {
            Source::Mem(c) => c.read(buf),
            Source::File(f) => f.read(buf),
        }
    }
}

impl Seek for Source<'_> {
    fn seek(&mut self, to: SeekFrom) -> std::io::Result<u64> {
        match self {
            Source::Mem(c) => c.seek(to),
            Source::File(f) => f.seek(to),
        }
    }
}

pub struct Package<'a> {
    zip: zip::ZipArchive<Source<'a>>,
    /// Lower-case normalized part name → zip index.
    names: HashMap<String, usize>,
    total: u64,
}

/// One part opened for streaming, independent of the package it came from (its own clone of the
/// zip), so the reader can run while the package is used for other parts.
pub struct PartReader<'a> {
    zip: zip::ZipArchive<Source<'a>>,
    index: usize,
    name: String,
}

impl PartReader<'_> {
    /// Runs `f` on the part's inflated bytes as a stream. The part may not inflate past the size
    /// its zip entry declares (a bomb is an error, not a flood).
    pub fn read<T>(&mut self, f: impl FnOnce(&mut dyn Read) -> Result<T, IoError>) -> Result<T, IoError> {
        let mut file = self.zip.by_index(self.index).map_err(|e| IoError::Zip(format!("{}: {e}", self.name)))?;
        let declared = file.size();
        let mut limited = (&mut file).take(declared.saturating_add(1));
        f(&mut limited)
    }
}

/// A relationship.
#[derive(Clone, Debug)]
pub struct Rel {
    pub id: String,
    /// Last path segment of the relationship type (`worksheet`, `styles`, …): the same for the
    /// transitional and strict namespaces.
    pub kind: String,
    /// Resolved part name (no leading `/`), or the raw target for external relationships.
    pub target: String,
    pub external: bool,
}

pub fn normalize(name: &str) -> String {
    name.replace('\\', "/").trim_start_matches('/').to_ascii_lowercase()
}

impl<'a> Package<'a> {
    pub fn open(bytes: &'a [u8]) -> Result<Package<'a>, IoError> {
        Self::open_source(Source::Mem(Cursor::new(bytes)))
    }

    /// WHIQ: a package read straight from its file (never read whole into memory).
    pub fn open_file(file: std::fs::File) -> Result<Package<'static>, IoError> {
        let f = PosFile::new(file).map_err(|e| IoError::Zip(e.to_string()))?;
        Package::open_source(Source::File(f))
    }

    pub fn open_source(src: Source<'a>) -> Result<Package<'a>, IoError> {
        let zip = zip::ZipArchive::new(src).map_err(|e| IoError::Zip(e.to_string()))?;
        if zip.len() > max_entries() {
            return Err(IoError::TooLarge(format!("{} zip entries (this process's memory budget indexes up to {})", zip.len(), max_entries())));
        }
        let mut names = HashMap::new();
        for i in 0..zip.len() {
            if let Some(n) = zip.name_for_index(i) {
                names.entry(normalize(n)).or_insert(i);
            }
        }
        Ok(Package { zip, names, total: 0 })
    }

    /// WHIQ: `part` opened for streaming (`None` when it doesn't exist).
    pub fn part_reader(&self, part: &str) -> Option<PartReader<'a>> {
        let &index = self.names.get(&normalize(part))?;
        Some(PartReader { zip: self.zip.clone(), index, name: part.to_string() })
    }

    pub fn has(&self, part: &str) -> bool {
        self.names.contains_key(&normalize(part))
    }

    /// Reads a part's bytes; `Ok(None)` when it doesn't exist.
    pub fn read(&mut self, part: &str) -> Result<Option<Vec<u8>>, IoError> {
        self.read_limited(part, max_part())
    }

    /// Reads a small metadata part without allowing it to expand to the general part limit.
    pub fn read_limited(&mut self, part: &str, maximum: u64) -> Result<Option<Vec<u8>>, IoError> {
        let maximum = maximum.min(max_part());
        let Some(&idx) = self.names.get(&normalize(part)) else {
            return Ok(None);
        };
        let mut f = self.zip.by_index(idx).map_err(|e| IoError::Zip(format!("{part}: {e}")))?;
        if f.size() > maximum {
            return Err(IoError::TooLarge(format!(
                "{part} expands to {} MB, more than one part this process holds in memory ({} MB)",
                f.size() >> 20,
                maximum >> 20
            )));
        }
        let remaining = max_total().saturating_sub(self.total);
        let limit = maximum.min(remaining);
        let mut out = Vec::with_capacity(f.size().min(16 * 1024 * 1024) as usize);
        (&mut f).take(limit + 1).read_to_end(&mut out).map_err(|e| IoError::Zip(format!("{part}: {e}")))?;
        if out.len() as u64 > limit {
            return Err(IoError::TooLarge(if remaining < maximum {
                format!("the package expands to more than this process holds in memory ({} MB)", max_total() >> 20)
            } else {
                format!("{part} expands to more than {maximum} bytes")
            }));
        }
        self.total += out.len() as u64;
        Ok(Some(out))
    }

    pub fn read_xml(&mut self, part: &str) -> Result<Option<El>, IoError> {
        match self.read(part)? {
            Some(b) => xml::parse(&b).map(Some).map_err(|e| prefix(part, e)),
            None => Ok(None),
        }
    }

    /// Relationships of a part (`xl/workbook.xml` → `xl/_rels/workbook.xml.rels`). Missing or
    /// broken rels parts give an empty list.
    pub fn rels(&mut self, part: &str) -> Result<Vec<Rel>, IoError> {
        let (dir, file) = split_dir(part);
        let rels_name = if dir.is_empty() { format!("_rels/{file}.rels") } else { format!("{dir}/_rels/{file}.rels") };
        let root = match self.read(&rels_name)? {
            Some(b) => match xml::parse(&b) {
                Ok(r) => r,
                Err(_) => return Ok(vec![]),
            },
            None => return Ok(vec![]),
        };
        Ok(root
            .kids("Relationship")
            .filter_map(|r| {
                let id = r.attr("Id")?.to_string();
                let ty = r.attr("Type").unwrap_or("");
                let kind = ty.rsplit('/').next().unwrap_or("").to_string();
                let target_raw = r.attr("Target")?;
                let external = r.attr("TargetMode").is_some_and(|m| m.eq_ignore_ascii_case("External"));
                let target = if external { target_raw.to_string() } else { resolve(dir, target_raw) };
                Some(Rel { id, kind, target, external })
            })
            .collect())
    }
}

fn prefix(part: &str, e: IoError) -> IoError {
    match e {
        IoError::Xml(m) => IoError::Xml(format!("{part}: {m}")),
        o => o,
    }
}

/// `xl/worksheets/sheet1.xml` → (`xl/worksheets`, `sheet1.xml`).
pub fn split_dir(part: &str) -> (&str, &str) {
    let p = part.trim_start_matches('/');
    match p.rfind('/') {
        Some(i) => (p.get(..i).unwrap_or(""), p.get(i + 1..).unwrap_or("")),
        None => ("", p),
    }
}

/// Resolves a relationship target against the source part's directory.
pub fn resolve(dir: &str, target: &str) -> String {
    let target = target.replace('\\', "/");
    let target = target.split('#').next().unwrap_or("").to_string();
    let mut segs: Vec<&str> = if target.starts_with('/') { vec![] } else { dir.split('/').filter(|s| !s.is_empty()).collect() };
    for s in target.split('/') {
        match s {
            "" | "." => {}
            ".." => {
                segs.pop();
            }
            s => segs.push(s),
        }
    }
    segs.join("/")
}

// ---------------------------------------------------------------- writing

pub struct ZipOut<W: Write + Seek> {
    zip: zip::ZipWriter<W>,
}

/// The options every part is written with (deterministic: fixed date, level 6).
pub fn part_options(large: bool) -> zip::write::SimpleFileOptions {
    zip::write::SimpleFileOptions::default()
        .compression_method(zip::CompressionMethod::Deflated)
        .compression_level(Some(6))
        .last_modified_time(zip::DateTime::default())
        .large_file(large)
}

impl<W: Write + Seek> ZipOut<W> {
    /// WHIQ: a package written straight into `w` (a file).
    pub fn into_writer(w: W) -> ZipOut<W> {
        ZipOut { zip: zip::ZipWriter::new(w) }
    }
    pub fn add(&mut self, name: &str, data: &[u8]) -> Result<(), IoError> {
        self.zip.start_file(name, part_options(data.len() as u64 >= u32::MAX as u64)).map_err(|e| IoError::Zip(e.to_string()))?;
        self.zip.write_all(data).map_err(|e| IoError::Zip(e.to_string()))?;
        Ok(())
    }
    /// WHIQ: copy an entry already compressed (a part streamed into a staging zip) as it is.
    pub fn add_raw<R: Read + Seek>(&mut self, from: &mut zip::ZipArchive<R>, name: &str) -> Result<(), IoError> {
        let f = from.by_name(name).map_err(|e| IoError::Zip(format!("{name}: {e}")))?;
        self.zip.raw_copy_file(f).map_err(|e| IoError::Zip(format!("{name}: {e}")))
    }
    pub fn finish_writer(self) -> Result<W, IoError> {
        self.zip.finish().map_err(|e| IoError::Zip(e.to_string()))
    }
}

/// WHIQ: parts compressed as they are produced (worksheets written row by row), kept until the
/// package is assembled in its conventional order. Only the compressed bytes are held.
pub struct Staging {
    zip: Option<zip::ZipWriter<Cursor<Vec<u8>>>>,
    done: Option<zip::ZipArchive<Cursor<Vec<u8>>>>,
}

impl Default for Staging {
    fn default() -> Self {
        Staging { zip: Some(zip::ZipWriter::new(Cursor::new(Vec::new()))), done: None }
    }
}

impl Staging {
    /// Starts a part; write its bytes with [`Staging::writer`].
    /// `large`: the part may pass 2 GB (its size is not known in advance; zip64 then).
    pub fn start(&mut self, name: &str, large: bool) -> Result<&mut dyn Write, IoError> {
        let z = self.zip.as_mut().ok_or_else(|| IoError::Zip("staging already finished".into()))?;
        z.start_file(name, part_options(large)).map_err(|e| IoError::Zip(e.to_string()))?;
        Ok(z)
    }
    /// The staged parts for reading (raw copies), once every part is written.
    pub fn archive(&mut self) -> Result<&mut zip::ZipArchive<Cursor<Vec<u8>>>, IoError> {
        if let Some(z) = self.zip.take() {
            let mut cur = z.finish().map_err(|e| IoError::Zip(e.to_string()))?;
            cur.set_position(0);
            self.done = Some(zip::ZipArchive::new(cur).map_err(|e| IoError::Zip(e.to_string()))?);
        }
        self.done.as_mut().ok_or_else(|| IoError::Zip("no staged parts".into()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolving() {
        assert_eq!(resolve("xl", "worksheets/sheet1.xml"), "xl/worksheets/sheet1.xml");
        assert_eq!(resolve("xl/worksheets", "../drawings/d1.xml"), "xl/drawings/d1.xml");
        assert_eq!(resolve("xl/worksheets", "/xl/tables/t.xml"), "xl/tables/t.xml");
        assert_eq!(resolve("", "xl/workbook.xml"), "xl/workbook.xml");
        assert_eq!(resolve("a", "../../../x"), "x");
        assert_eq!(split_dir("xl/workbook.xml"), ("xl", "workbook.xml"));
        assert_eq!(split_dir("x.xml"), ("", "x.xml"));
    }
}
