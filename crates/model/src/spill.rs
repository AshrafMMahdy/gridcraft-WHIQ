//! WHIQ: cells written to disk. A sheet that is not being used is serialised (one JSON line a
//! band of 64 rows) into a spill file and dropped from memory; the first time a cell is looked at
//! again the file is read back. The file goes away with the last store that points at it.

use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::PathBuf;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use gridcraft_core::{RangeRef, Value};

use crate::store::{Band, CellStore, ColdCells};

static NEXT: AtomicU64 = AtomicU64::new(1);

/// Where spill files go: `GRIDCRAFT_SPILL_DIR`, else a folder of the program's own data (not the
/// system's temporary folder, which cleaners empty), one per process.
pub fn spill_dir() -> PathBuf {
    let own = format!("pid-{}", std::process::id());
    match std::env::var_os("GRIDCRAFT_SPILL_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d).join(own),
        _ => data_dir().unwrap_or_else(std::env::temp_dir).join("gridcraft").join("spill").join(own),
    }
}

/// The per-user data folder: `%LOCALAPPDATA%`, `$XDG_DATA_HOME`, or `~/.local/share`.
fn data_dir() -> Option<PathBuf> {
    let var = |k: &str| std::env::var_os(k).filter(|v| !v.is_empty()).map(PathBuf::from);
    var("LOCALAPPDATA").or_else(|| var("XDG_DATA_HOME")).or_else(|| var("HOME").map(|h| h.join(".local").join("share")))
}

/// Cells in a file, which stays open while the cells are cold: a cleaner that deletes the file
/// does not take them (reads go through the handle), and a file that is damaged or short is an
/// error, never a sheet read as empty.
#[derive(Debug)]
pub struct SpillFile {
    path: PathBuf,
    file: Mutex<std::fs::File>,
    cells: usize,
    formulas: bool,
    used: Option<RangeRef>,
}

impl Drop for SpillFile {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

impl ColdCells for SpillFile {
    fn load(&self) -> Result<CellStore, String> {
        let mut guard = self.file.lock().map_err(|_| format!("{}: lock poisoned", self.path.display()))?;
        guard.seek(SeekFrom::Start(0)).map_err(|e| format!("{}: {e}", self.path.display()))?;
        let mut bands = Vec::new();
        for line in std::io::BufReader::with_capacity(1 << 20, &mut *guard).lines() {
            let line = line.map_err(|e| format!("{}: {e}", self.path.display()))?;
            if line.is_empty() {
                continue;
            }
            let (key, band): (u32, Band) = serde_json::from_str(&line).map_err(|e| format!("{}: {e}", self.path.display()))?;
            bands.push((key, band));
        }
        let store = CellStore::from_bands(bands);
        if store.len() != self.cells {
            return Err(format!("{}: holds {} cells, wrote {}", self.path.display(), store.len(), self.cells));
        }
        Ok(store)
    }
    fn cells(&self) -> usize {
        self.cells
    }
    fn formulas(&self) -> bool {
        self.formulas
    }
    fn used_range_hint(&self) -> Option<Option<RangeRef>> {
        Some(self.used)
    }
    fn describe(&self) -> String {
        format!("spill file {}", self.path.display())
    }
}

impl CellStore {
    /// Frees the memory the cells take, writing them to a spill file first when no source holds
    /// them as they are now. `Ok(true)` when the cells left memory; `Ok(false)` when they were not
    /// in memory or cannot be spilled (a number JSON cannot hold); `Err` when the disk refused
    /// them, in which case the cells are still in memory and nothing is lost.
    pub fn spill(&mut self) -> Result<bool, String> {
        if !self.is_resident() {
            return Ok(false);
        }
        if self.is_pristine() {
            return Ok(self.evict());
        }
        let dir = spill_dir();
        std::fs::create_dir_all(&dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        let path = dir.join(format!("sheet-{}.jsonl", NEXT.fetch_add(1, Ordering::Relaxed)));
        let f = std::fs::OpenOptions::new().read(true).write(true).create_new(true).open(&path).map_err(|e| format!("{}: {e}", path.display()))?;
        // Removed again on any failure below (the drop).
        let mut file = SpillFile { path: path.clone(), file: Mutex::new(f), cells: 0, formulas: false, used: None };
        let (mut cells, mut formulas, mut finite) = (0usize, false, true);
        {
            let handle = file.file.get_mut().map_err(|_| "lock poisoned".to_string())?;
            let mut w = std::io::BufWriter::with_capacity(1 << 20, handle);
            let res = self.for_each_band(&mut |key, band| {
                for (_, cell) in band.cells() {
                    cells += 1;
                    formulas |= cell.formula.is_some();
                    if matches!(cell.value, Value::Number(n) if !n.is_finite()) {
                        finite = false;
                    }
                }
                if !finite {
                    return Ok(());
                }
                serde_json::to_writer(&mut w, &(key, band)).map_err(|e| format!("{}: {e}", path.display()))?;
                w.write_all(b"\n").map_err(|e| format!("{}: {e}", path.display()))
            });
            res.and_then(|()| w.flush().map_err(|e| format!("{}: {e}", path.display())))?;
        }
        if !finite {
            return Ok(false);
        }
        file.used = self.used_range();
        file.cells = cells;
        file.formulas = formulas;
        self.replace_source(Box::new(file));
        Ok(true)
    }
}
