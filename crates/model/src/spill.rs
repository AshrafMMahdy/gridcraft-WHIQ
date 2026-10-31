//! WHIQ: cells written to disk. A sheet that is not being used is serialised (one JSON line a
//! band of 64 rows) into a spill file and dropped from memory; the first time a cell is looked at
//! again the file is read back. The file goes away with the last store that points at it.

use std::io::{BufRead, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};

use gridcraft_core::{RangeRef, Value};

use crate::store::{Band, CellStore, ColdCells};

static NEXT: AtomicU64 = AtomicU64::new(1);

static BASE: Mutex<Option<PathBuf>> = Mutex::new(None);

/// Fix the folder all processes' spill folders are in (before the first sheet is spilled).
pub fn set_spill_base(base: Option<PathBuf>) {
    if let Ok(mut b) = BASE.lock() {
        *b = base;
    }
}

/// The folder all processes' spill folders are in: `GRIDCRAFT_SPILL_DIR`, else a folder of the
/// program's own data (not the system's temporary folder, which cleaners empty).
pub fn spill_base() -> PathBuf {
    if let Some(b) = BASE.lock().ok().and_then(|b| b.clone()) {
        return b;
    }
    match std::env::var_os("GRIDCRAFT_SPILL_DIR") {
        Some(d) if !d.is_empty() => PathBuf::from(d),
        _ => data_dir().unwrap_or_else(std::env::temp_dir).join("gridcraft").join("spill"),
    }
}

const LOCK_NAME: &str = "lock";

/// The folder this process claimed and the lock on it, held until it exits.
static CLAIM: std::sync::OnceLock<Option<(PathBuf, std::fs::File)>> = std::sync::OnceLock::new();

/// Waits between tries to take the lock of `pid-<pid>` when someone else holds it for a moment (a
/// sweeper looking at the folder of a dead process that had our PID).
const CLAIM_WAITS_MS: [u64; 7] = [10, 20, 40, 80, 160, 320, 640];

/// Claim a spill folder for process `pid` under `base`: `pid-<pid>` when its lock can be taken
/// (retrying after each of `waits`, the holder being a sweeper that lets go in a moment), else a
/// fresh `pid-<pid>-<random>`. The folder is emptied once locked (what is in it is a crashed
/// process's). A process never runs without the lock of its own folder.
fn claim_folder(base: &Path, pid: u32, waits: &[std::time::Duration]) -> Option<(PathBuf, std::fs::File)> {
    let lock_in = |dir: &Path| -> Option<std::fs::File> {
        std::fs::create_dir_all(dir).ok()?;
        let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(dir.join(LOCK_NAME)).ok()?;
        f.try_lock().ok()?;
        empty_folder(dir);
        Some(f)
    };
    let own = base.join(format!("pid-{pid}"));
    for wait in std::iter::once(&std::time::Duration::ZERO).chain(waits) {
        std::thread::sleep(*wait);
        if let Some(f) = lock_in(&own) {
            return Some((own, f));
        }
    }
    static SALT: AtomicU64 = AtomicU64::new(0);
    for _ in 0..16 {
        // WHIQ: the clock through core (wasm-safe; the std clock is a disallowed method here).
        let nanos = gridcraft_core::date::unix_now_ms().map_or(0, |ms| (ms * 1_000_000.0) as u64);
        let r = nanos ^ SALT.fetch_add(0x9E37_79B9_7F4A_7C15, Ordering::Relaxed).rotate_left(17);
        let dir = base.join(format!("pid-{pid}-{:08x}", r as u32));
        if let Some(f) = lock_in(&dir) {
            return Some((dir, f));
        }
    }
    None
}

/// The spill folder of THIS process: `pid-<pid>` under [`spill_base`], with a lock file held open
/// and locked for as long as the process lives, which is what tells another process the folder is
/// in use. Claiming it empties it: what is in it is the leftover of a crashed process of the same PID.
/// If another process holds that lock (a sweeper, for a moment) the claim waits, then takes
/// `pid-<pid>-<random>`.
pub fn spill_dir() -> PathBuf {
    let base = spill_base();
    let pid = std::process::id();
    match CLAIM.get_or_init(|| {
        let waits: Vec<_> = CLAIM_WAITS_MS.iter().map(|ms| std::time::Duration::from_millis(*ms)).collect();
        claim_folder(&base, pid, &waits)
    }) {
        Some((dir, _)) => dir.clone(),
        None => base.join(format!("pid-{pid}")),
    }
}

/// Remove everything in `dir` but the lock file.
fn empty_folder(dir: &std::path::Path) {
    let Ok(rd) = std::fs::read_dir(dir) else { return };
    for e in rd.flatten() {
        if e.file_name() == LOCK_NAME {
            continue;
        }
        let p = e.path();
        let _ = if e.file_type().is_ok_and(|t| t.is_dir()) { std::fs::remove_dir_all(&p) } else { std::fs::remove_file(&p) };
    }
}

/// Remove the spill folders of processes that are gone: a folder whose lock file nobody holds (the
/// system releases the lock when a process dies, so a reused PID is no problem). A folder of a live
/// process (its lock is held) is never touched, whatever its age; one with no lock file is left for
/// a minute (it is being made), then goes. Cells in these files are customer data in plain text, so
/// a crash must not leave them behind. Returns how many files.
pub fn sweep_stale() -> usize {
    let base = spill_base();
    let Ok(rd) = std::fs::read_dir(&base) else { return 0 };
    let mine = spill_dir();
    let mut n = 0;
    for e in rd.flatten() {
        let path = e.path();
        if path == mine || !path.is_dir() {
            continue;
        }
        // `held` keeps the lock while the folder goes; `free` says the folder is nobody's.
        let mut held = None;
        let free = match std::fs::OpenOptions::new().read(true).write(true).open(path.join(LOCK_NAME)) {
            Ok(f) => {
                let got = f.try_lock().is_ok();
                held = got.then_some(f);
                got
            }
            // No lock file (a folder of an older build, or one still being made): free once it is
            // not young. (A directory cannot be opened as a file on Windows, so no handle.)
            Err(_) => !e
                .metadata()
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_none_or(|d| d < std::time::Duration::from_secs(60)),
        };
        if free {
            n += std::fs::read_dir(&path).map(|r| r.flatten().filter(|x| x.file_name() != LOCK_NAME).count()).unwrap_or(0);
            drop(held);
            let _ = std::fs::remove_dir_all(&path);
        }
    }
    n
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

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn a_claim_while_another_handle_holds_the_lock_still_gets_a_locked_folder() {
        let base = std::env::temp_dir().join(format!("gc-claim-race-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&base);
        let pid = 4_000_010;
        let own = base.join(format!("pid-{pid}"));
        std::fs::create_dir_all(&own).unwrap();
        std::fs::write(own.join("junk.part"), b"x").unwrap();
        let hold = || {
            let f = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(own.join(LOCK_NAME)).unwrap();
            f.try_lock().unwrap();
            f
        };
        // A sweeper holds the lock for good: no wait helps, so the claim takes a fresh folder, locked.
        let sweeper = hold();
        let (dir, held) = claim_folder(&base, pid, &[std::time::Duration::from_millis(5)]).unwrap();
        assert_ne!(dir, own);
        assert!(dir.file_name().unwrap().to_string_lossy().starts_with("pid-4000010-"));
        let second = std::fs::OpenOptions::new().read(true).write(true).open(dir.join(LOCK_NAME)).unwrap();
        assert!(matches!(second.try_lock(), Err(std::fs::TryLockError::WouldBlock)), "the fresh folder is locked by the claimer");
        assert!(own.join("junk.part").exists(), "the folder the other handle holds is left alone");
        drop((held, second));
        // The sweeper lets go in a moment: the retry gets pid-<pid> itself, emptied.
        let t = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(100));
            drop(sweeper);
        });
        let waits = [std::time::Duration::from_millis(50); 40];
        let (dir, held) = claim_folder(&base, pid, &waits).unwrap();
        t.join().unwrap();
        assert_eq!(dir, own);
        assert!(!own.join("junk.part").exists());
        drop(held);
        std::fs::remove_dir_all(&base).unwrap();
    }
}
