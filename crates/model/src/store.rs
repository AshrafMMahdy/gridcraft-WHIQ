//! Copy-on-write sparse cell storage.
//!
//! Cells live in bands of 64 rows, each band behind an `Arc`. Cloning a store (an undo snapshot)
//! copies only the band pointers; editing a cell copies just its band.
//!
//! WHIQ: a row keeps its cells in one vector sorted by column (48 bytes a cell) instead of a
//! B-tree per row (about twice that with the node overhead), so a workbook of millions of cells
//! takes half the memory; rows are short, and a column is found by binary search.

use std::collections::BTreeMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use gridcraft_core::{CellRef, RangeRef};
use serde::{Deserialize, Deserializer, Serialize, Serializer};

use crate::cell::Cell;

const BAND: u32 = 64;

/// One row's cells, sorted by column. Serialized as `{col: cell}`, as the B-tree it replaced.
#[derive(Clone, Debug, Default, PartialEq)]
struct Row(Vec<(u32, Cell)>);

impl Row {
    fn find(&self, col: u32) -> Result<usize, usize> {
        self.0.binary_search_by_key(&col, |e| e.0)
    }
    fn get(&self, col: u32) -> Option<&Cell> {
        self.find(col).ok().and_then(|i| self.0.get(i)).map(|e| &e.1)
    }
    fn get_mut(&mut self, col: u32) -> Option<&mut Cell> {
        let i = self.find(col).ok()?;
        self.0.get_mut(i).map(|e| &mut e.1)
    }
    fn contains_key(&self, col: &u32) -> bool {
        self.find(*col).is_ok()
    }
    fn insert(&mut self, col: u32, cell: Cell) -> Option<Cell> {
        match self.find(col) {
            Ok(i) => self.0.get_mut(i).map(|e| std::mem::replace(&mut e.1, cell)),
            Err(i) => {
                self.0.insert(i, (col, cell));
                None
            }
        }
    }
    fn remove(&mut self, col: &u32) -> Option<Cell> {
        let i = self.find(*col).ok()?;
        Some(self.0.remove(i).1)
    }
    fn is_empty(&self) -> bool {
        self.0.is_empty()
    }
    /// Cells in columns `c0..=c1`, in column order.
    fn range(&self, cols: std::ops::RangeInclusive<u32>) -> impl Iterator<Item = (&u32, &Cell)> + '_ {
        let (c0, c1) = (*cols.start(), *cols.end());
        let a = self.0.partition_point(|e| e.0 < c0);
        let b = self.0.partition_point(|e| e.0 <= c1).max(a);
        self.0.get(a..b).unwrap_or_default().iter().map(|(c, cell)| (c, cell))
    }
    fn iter(&self) -> impl Iterator<Item = (&u32, &Cell)> + '_ {
        self.0.iter().map(|(c, cell)| (c, cell))
    }
    fn keys(&self) -> impl DoubleEndedIterator<Item = &u32> + '_ {
        self.0.iter().map(|(c, _)| c)
    }
}

impl Serialize for Row {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        s.collect_map(self.0.iter().map(|(c, cell)| (c, cell)))
    }
}

impl<'de> Deserialize<'de> for Row {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        let m = BTreeMap::<u32, Cell>::deserialize(d)?;
        Ok(Row(m.into_iter().collect()))
    }
}

#[derive(Clone, Debug, Default, PartialEq, Serialize, Deserialize)]
pub struct Band {
    /// row → its cells by column
    rows: BTreeMap<u32, Row>,
}

type Bands = BTreeMap<u32, Arc<Band>>;

impl Band {
    /// Every cell of the band with its position's row and column.
    pub fn cells(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.rows.iter().flat_map(|(row, cols)| cols.iter().map(move |(col, cell)| (CellRef::new(*row, *col), cell)))
    }
}

/// WHIQ: where a store's cells can be read again after they were dropped from memory (the
/// worksheet part of the package they were read from, or a file they were written to).
pub trait ColdCells: Send + Sync + std::fmt::Debug {
    /// Reads the cells afresh.
    fn load(&self) -> Result<CellStore, String>;
    /// How many cells there are (exact for a spill file, the count of `<c>` elements for a part).
    fn cells(&self) -> usize;
    /// Whether any of them may hold a formula.
    fn formulas(&self) -> bool;
    /// Where the cells are, if the source knows without reading them (`Some(None)`: nowhere, it
    /// is empty). The answer for a part is the cells it counts, so a cell with an empty `<v>` can
    /// make it larger than the cells really are; reading the cells gives the exact range.
    fn used_range_hint(&self) -> Option<Option<RangeRef>> {
        None
    }
    /// A short description for messages ("sheet3.xml of book.xlsx", "spill file …").
    fn describe(&self) -> String;
}

/// A [`ColdCells`] and what went wrong reading it, if anything.
#[derive(Debug)]
struct Cold {
    src: Box<dyn ColdCells>,
    failure: Mutex<Option<String>>,
}

/// Memory a loaded cell takes, for the estimates that decide what to evict and what to refuse.
pub const CELL_BYTES: u64 = 105;

/// Copy-on-write sparse cells, which may be COLD: not in memory, read from where they were
/// (`ColdCells`) the first time a cell is looked at, and dropped again ([`CellStore::evict`])
/// when the process needs the room. A cell store that never had a source is always resident.
pub struct CellStore {
    bands: OnceLock<Bands>,
    /// Never read: what `&mut` access falls back on if the cells are somehow not there.
    spare: Bands,
    /// Cells held (exact when resident; a count from the source while cold).
    len: AtomicUsize,
    cold: Option<Arc<Cold>>,
    /// The cells in memory are what `cold` holds (nothing was changed since they were read).
    pristine: bool,
}

impl Default for CellStore {
    fn default() -> Self {
        CellStore { bands: OnceLock::from(Bands::new()), spare: Bands::new(), len: AtomicUsize::new(0), cold: None, pristine: false }
    }
}

impl Clone for CellStore {
    fn clone(&self) -> Self {
        CellStore {
            bands: self.bands.clone(),
            spare: Bands::new(),
            len: AtomicUsize::new(self.len.load(Ordering::Relaxed)),
            cold: self.cold.clone(),
            pristine: self.pristine,
        }
    }
}

impl std::fmt::Debug for CellStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self.bands.get() {
            Some(b) => f.debug_struct("CellStore").field("bands", b).field("len", &self.len.load(Ordering::Relaxed)).finish(),
            None => write!(
                f,
                "CellStore(cold, {} cells: {})",
                self.len.load(Ordering::Relaxed),
                self.cold.as_ref().map(|c| c.src.describe()).unwrap_or_default()
            ),
        }
    }
}

impl PartialEq for CellStore {
    fn eq(&self, other: &Self) -> bool {
        // The same cold cells, neither read or changed: equal without reading them.
        if self.bands.get().is_none()
            && other.bands.get().is_none()
            && let (Some(a), Some(b)) = (&self.cold, &other.cold)
            && Arc::ptr_eq(a, b)
        {
            return true;
        }
        self.bands() == other.bands()
    }
}

impl Serialize for CellStore {
    fn serialize<S: Serializer>(&self, s: S) -> Result<S::Ok, S::Error> {
        use serde::ser::SerializeStruct;
        let mut st = s.serialize_struct("CellStore", 2)?;
        st.serialize_field("bands", self.bands())?;
        st.serialize_field("len", &self.len())?;
        st.end()
    }
}

impl<'de> Deserialize<'de> for CellStore {
    fn deserialize<D: Deserializer<'de>>(d: D) -> Result<Self, D::Error> {
        #[derive(Deserialize)]
        struct Plain {
            bands: Bands,
            len: usize,
        }
        let p = Plain::deserialize(d)?;
        Ok(CellStore { bands: OnceLock::from(p.bands), spare: Bands::new(), len: AtomicUsize::new(p.len), cold: None, pristine: false })
    }
}

impl CellStore {
    pub fn new() -> Self {
        Self::default()
    }
    /// A store whose cells are not in memory: they are read from `src` the first time one is
    /// looked at.
    pub fn from_cold(src: Box<dyn ColdCells>) -> Self {
        let len = src.cells();
        CellStore {
            bands: OnceLock::new(),
            spare: Bands::new(),
            len: AtomicUsize::new(len),
            cold: Some(Arc::new(Cold { src, failure: Mutex::new(None) })),
            pristine: true,
        }
    }
    /// Number of cells. While the cells are cold this is the source's count (the `<c>` elements
    /// of a worksheet part), exact again once they are read.
    pub fn len(&self) -> usize {
        self.len.load(Ordering::Relaxed)
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
    /// The cells, read from the source the first time they are needed. A source that cannot be
    /// read gives no cells and is remembered ([`CellStore::load_failure`]): a save refuses.
    fn bands(&self) -> &Bands {
        self.bands.get_or_init(|| self.read_bands())
    }
    fn read_bands(&self) -> Bands {
        let Some(cold) = &self.cold else { return Bands::new() };
        match cold.src.load() {
            Ok(store) => {
                self.len.store(store.len(), Ordering::Relaxed);
                store.into_bands()
            }
            Err(e) => {
                if let Ok(mut f) = cold.failure.lock() {
                    f.get_or_insert(format!("{}: {e}", cold.src.describe()));
                }
                Bands::new()
            }
        }
    }
    fn into_bands(mut self) -> Bands {
        std::mem::take(&mut self.bands).into_inner().unwrap_or_default()
    }
    /// Exclusive access to the cells (read first if they are cold). The store is no longer what
    /// its source holds.
    fn bands_mut(&mut self) -> &mut Bands {
        self.pristine = false;
        if self.bands.get().is_none() {
            let b = self.read_bands();
            let _ = self.bands.set(b);
        }
        match self.bands.get_mut() {
            Some(b) => b,
            None => &mut self.spare,
        }
    }
    /// Are the cells in memory?
    pub fn is_resident(&self) -> bool {
        self.bands.get().is_some()
    }
    /// Is this store backed by a source (so that dropping its cells loses nothing)?
    pub fn is_backed(&self) -> bool {
        self.cold.is_some()
    }
    /// Cold and known to hold no formula: a calculation has nothing to read from it.
    pub fn is_cold_without_formulas(&self) -> bool {
        !self.is_resident() && self.cold.as_ref().is_some_and(|c| !c.src.formulas())
    }
    /// About how much memory the cells take now (0 while cold).
    pub fn resident_bytes(&self) -> u64 {
        if self.is_resident() { (self.len() as u64).saturating_mul(CELL_BYTES) } else { 0 }
    }
    /// About how much memory the cells will take once read.
    pub fn loaded_bytes(&self) -> u64 {
        (self.len() as u64).saturating_mul(CELL_BYTES)
    }
    /// Why the cells could not be read, if they could not (the store then looks empty, which is
    /// not the truth: callers that write it out must refuse).
    pub fn load_failure(&self) -> Option<String> {
        self.cold.as_ref().and_then(|c| c.failure.lock().ok().and_then(|f| f.clone()))
    }
    /// The cells as a store of their own, resident, without keeping them in `self`: what a save
    /// reads one sheet at a time, so a workbook larger than memory goes out sheet by sheet.
    pub fn transient(&self) -> CellStore {
        if self.is_resident() {
            return self.clone();
        }
        let copy =
            CellStore { bands: OnceLock::new(), spare: Bands::new(), len: AtomicUsize::new(self.len()), cold: self.cold.clone(), pristine: true };
        let b = copy.read_bands();
        let _ = copy.bands.set(b);
        // The copy answers for itself: a failed read is still said by `self`.
        if let (Some(mine), Some(theirs)) = (&self.cold, &copy.cold)
            && let Ok(f) = theirs.failure.lock()
            && let Some(f) = f.clone()
            && let Ok(mut m) = mine.failure.lock()
        {
            m.get_or_insert(f);
        }
        copy
    }
    /// Drops the cells from memory when a source can read them again unchanged; `false` when
    /// they are not backed, were changed since, or are not in memory anyway.
    pub fn evict(&mut self) -> bool {
        if self.pristine && self.cold.is_some() && self.bands.get().is_some() {
            self.bands = OnceLock::new();
            return true;
        }
        false
    }
    /// Replaces the source (and so the cells in memory, which must be the same cells) with
    /// `src`, dropping the cells from memory.
    pub fn replace_source(&mut self, src: Box<dyn ColdCells>) {
        let len = src.cells();
        self.len.store(len, Ordering::Relaxed);
        self.cold = Some(Arc::new(Cold { src, failure: Mutex::new(None) }));
        self.bands = OnceLock::new();
        self.pristine = true;
    }
    /// What the cells are read from, for messages.
    pub fn describe_source(&self) -> Option<String> {
        self.cold.as_ref().map(|c| c.src.describe())
    }
    /// Whether the cells in memory are what the source holds.
    pub fn is_pristine(&self) -> bool {
        self.pristine && self.cold.is_some()
    }
    /// The bands, for a source that writes them out (one JSON line a band).
    pub fn for_each_band(&self, f: &mut dyn FnMut(u32, &Band) -> Result<(), String>) -> Result<(), String> {
        for (k, b) in self.bands() {
            f(*k, b)?;
        }
        Ok(())
    }
    /// A store from bands read back from a spill.
    pub fn from_bands(bands: Vec<(u32, Band)>) -> CellStore {
        let mut len = 0;
        let mut map = Bands::new();
        for (k, b) in bands {
            len += b.rows.values().map(|r| r.0.len()).sum::<usize>();
            map.insert(k, Arc::new(b));
        }
        CellStore { bands: OnceLock::from(map), spare: Bands::new(), len: AtomicUsize::new(len), cold: None, pristine: false }
    }
    pub fn get(&self, c: CellRef) -> Option<&Cell> {
        self.bands().get(&(c.row / BAND))?.rows.get(&c.row)?.get(c.col)
    }
    /// WHIQ: give back the spare room rows grew into while a file was read (each row's vector is
    /// trimmed to its cells; bands are not copied when shared).
    pub fn shrink_to_fit(&mut self) {
        for band in self.bands_mut().values_mut() {
            if let Some(b) = Arc::get_mut(band) {
                for row in b.rows.values_mut() {
                    row.0.shrink_to_fit();
                }
            }
        }
    }
    /// Mutable access (copies the band if shared).
    pub fn get_mut(&mut self, c: CellRef) -> Option<&mut Cell> {
        let band = self.bands_mut().get_mut(&(c.row / BAND))?;
        if !band.rows.get(&c.row).is_some_and(|r| r.contains_key(&c.col)) {
            return None;
        }
        Arc::make_mut(band).rows.get_mut(&c.row)?.get_mut(c.col)
    }
    /// Stores a cell; a default (empty, unstyled) cell is removed instead.
    pub fn set(&mut self, c: CellRef, cell: Cell) {
        if cell.is_blank() {
            self.remove(c);
            return;
        }
        let band = Arc::make_mut(self.bands_mut().entry(c.row / BAND).or_default());
        if band.rows.entry(c.row).or_default().insert(c.col, cell).is_none() {
            *self.len.get_mut() += 1;
        }
    }
    pub fn remove(&mut self, c: CellRef) -> Option<Cell> {
        let key = c.row / BAND;
        let band = self.bands_mut().get_mut(&key)?;
        if !band.rows.get(&c.row).is_some_and(|r| r.contains_key(&c.col)) {
            return None;
        }
        let b = Arc::make_mut(band);
        let row = b.rows.get_mut(&c.row)?;
        let old = row.remove(&c.col);
        if row.is_empty() {
            b.rows.remove(&c.row);
        }
        if b.rows.is_empty() {
            self.bands_mut().remove(&key);
        }
        if old.is_some() {
            let n = self.len.get_mut();
            *n = n.saturating_sub(1);
        }
        old
    }
    /// Cells in `r`, row-major. Efficient for sparse data: skips empty bands and rows.
    pub fn iter_range(&self, r: RangeRef) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        let (b0, b1) = (r.start.row / BAND, r.end.row / BAND);
        self.bands().range(b0..=b1).flat_map(move |(_, band)| {
            band.rows
                .range(r.start.row..=r.end.row)
                .flat_map(move |(row, cols)| cols.range(r.start.col..=r.end.col).map(move |(col, cell)| (CellRef::new(*row, *col), cell)))
        })
    }
    pub fn iter(&self) -> impl Iterator<Item = (CellRef, &Cell)> + '_ {
        self.bands()
            .values()
            .flat_map(|band| band.rows.iter().flat_map(|(row, cols)| cols.iter().map(move |(col, cell)| (CellRef::new(*row, *col), cell))))
    }
    /// Cells of one row in `c0..=c1`.
    pub fn row(&self, row: u32, c0: u32, c1: u32) -> impl Iterator<Item = (u32, &Cell)> + '_ {
        self.bands()
            .get(&(row / BAND))
            .and_then(|b| b.rows.get(&row))
            .into_iter()
            .flat_map(move |cols| cols.range(c0..=c1).map(|(c, cell)| (*c, cell)))
    }
    /// [`CellStore::used_range`] without reading cells that are on disk when their source can say
    /// (see [`ColdCells::used_range_hint`]): for a summary of every sheet of a large workbook.
    pub fn used_range_hint(&self) -> Option<RangeRef> {
        if !self.is_resident()
            && let Some(c) = &self.cold
            && let Some(hint) = c.src.used_range_hint()
        {
            return hint;
        }
        self.used_range()
    }
    /// Smallest range containing every stored cell, `None` when empty.
    pub fn used_range(&self) -> Option<RangeRef> {
        let first_band = self.bands().values().next()?;
        let last_band = self.bands().values().next_back()?;
        let r0 = *first_band.rows.keys().next()?;
        let r1 = *last_band.rows.keys().next_back()?;
        let mut c0 = u32::MAX;
        let mut c1 = 0;
        for band in self.bands().values() {
            for cols in band.rows.values() {
                if let (Some(a), Some(b)) = (cols.keys().next(), cols.keys().next_back()) {
                    c0 = c0.min(*a);
                    c1 = c1.max(*b);
                }
            }
        }
        if c0 == u32::MAX {
            return None;
        }
        Some(RangeRef::new(CellRef::new(r0, c0), CellRef::new(r1, c1)))
    }
    /// Last stored row in column `col` at or above `below_or_at` (for Ctrl+arrow navigation).
    pub fn has(&self, c: CellRef) -> bool {
        self.get(c).is_some_and(|cell| !cell.value.is_empty() || cell.formula.is_some())
    }
    /// Positions whose cells differ between `self` and `other` (stored on one side only, or not
    /// equal). Bands both stores share are skipped, so comparing an edited copy with the store
    /// it was cloned from only visits the bands the edit touched.
    pub fn diff(&self, other: &CellStore) -> Vec<CellRef> {
        let empty = Band::default();
        let mut keys: Vec<u32> = self.bands().keys().chain(other.bands().keys()).copied().collect();
        keys.sort_unstable();
        keys.dedup();
        let mut out = Vec::new();
        for k in keys {
            let (a, b) = (self.bands().get(&k), other.bands().get(&k));
            if let (Some(a), Some(b)) = (a, b)
                && Arc::ptr_eq(a, b)
            {
                continue;
            }
            let (a, b) = (a.map_or(&empty, |x| x), b.map_or(&empty, |x| x));
            for (row, cols) in &a.rows {
                for (col, cell) in cols.iter() {
                    if b.rows.get(row).and_then(|r| r.get(*col)) != Some(cell) {
                        out.push(CellRef::new(*row, *col));
                    }
                }
            }
            for (row, cols) in &b.rows {
                for col in cols.keys() {
                    if !a.rows.get(row).is_some_and(|r| r.contains_key(col)) {
                        out.push(CellRef::new(*row, *col));
                    }
                }
            }
        }
        out
    }
    /// Removes every cell in `r` and returns them.
    pub fn take_range(&mut self, r: RangeRef) -> Vec<(CellRef, Cell)> {
        let keys: Vec<CellRef> = self.iter_range(r).map(|(c, _)| c).collect();
        keys.into_iter().filter_map(|c| self.remove(c).map(|cell| (c, cell))).collect()
    }
    /// Rows strictly greater than or equal to `from` moved by `delta` (positive = down). Rows that
    /// would land before `from`... callers delete the vacated block first.
    pub fn shift_rows(&mut self, from: u32, delta: i64) {
        let moved: Vec<(CellRef, Cell)> = {
            let keys: Vec<CellRef> = self.iter().filter(|(c, _)| c.row >= from).map(|(c, _)| c).collect();
            keys.into_iter().filter_map(|c| self.remove(c).map(|cell| (c, cell))).collect()
        };
        for (c, cell) in moved {
            let nr = c.row as i64 + delta;
            if (0..gridcraft_core::MAX_ROWS as i64).contains(&nr) {
                self.set(CellRef::new(nr as u32, c.col), cell);
            }
        }
    }
    pub fn shift_cols(&mut self, from: u32, delta: i64) {
        let keys: Vec<CellRef> = self.iter().filter(|(c, _)| c.col >= from).map(|(c, _)| c).collect();
        let moved: Vec<(CellRef, Cell)> = keys.into_iter().filter_map(|c| self.remove(c).map(|cell| (c, cell))).collect();
        for (c, cell) in moved {
            let nc = c.col as i64 + delta;
            if (0..gridcraft_core::MAX_COLS as i64).contains(&nc) {
                self.set(CellRef::new(c.row, nc as u32), cell);
            }
        }
    }
    /// Shifts cells within rows `r0..=r1` and columns `>= from` horizontally (Insert/Delete Cells
    /// with shift right/left).
    pub fn shift_cols_in_rows(&mut self, r0: u32, r1: u32, from: u32, delta: i64) {
        let range = RangeRef::new(CellRef::new(r0, from), CellRef::new(r1, gridcraft_core::MAX_COLS - 1));
        let moved = self.take_range(range);
        for (c, cell) in moved {
            let nc = c.col as i64 + delta;
            if (0..gridcraft_core::MAX_COLS as i64).contains(&nc) {
                self.set(CellRef::new(c.row, nc as u32), cell);
            }
        }
    }
    /// Shifts cells within columns `c0..=c1` and rows `>= from` vertically.
    pub fn shift_rows_in_cols(&mut self, c0: u32, c1: u32, from: u32, delta: i64) {
        let range = RangeRef::new(CellRef::new(from, c0), CellRef::new(gridcraft_core::MAX_ROWS - 1, c1));
        let moved = self.take_range(range);
        for (c, cell) in moved {
            let nr = c.row as i64 + delta;
            if (0..gridcraft_core::MAX_ROWS as i64).contains(&nr) {
                self.set(CellRef::new(nr as u32, c.col), cell);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use gridcraft_core::Value;

    fn c(r: u32, col: u32) -> CellRef {
        CellRef::new(r, col)
    }

    #[test]
    fn set_get_remove() {
        let mut s = CellStore::new();
        s.set(c(0, 0), Cell::value(1.0.into()));
        s.set(c(100, 5), Cell::value("x".into()));
        assert_eq!(s.len(), 2);
        assert_eq!(s.get(c(100, 5)).map(|x| x.value.clone()), Some(Value::from("x")));
        assert_eq!(s.used_range().unwrap().a1(), "A1:F101");
        s.set(c(0, 0), Cell::default());
        assert_eq!(s.len(), 1);
        assert!(s.remove(c(100, 5)).is_some());
        assert!(s.is_empty());
        assert!(s.used_range().is_none());
    }

    #[test]
    fn cow_snapshots() {
        let mut s = CellStore::new();
        for r in 0..1000 {
            s.set(c(r, 0), Cell::value((r as f64).into()));
        }
        let snap = s.clone();
        s.set(c(5, 0), Cell::value(99.0.into()));
        assert_eq!(snap.get(c(5, 0)).unwrap().value, Value::Number(5.0));
        assert_eq!(s.get(c(5, 0)).unwrap().value, Value::Number(99.0));
    }

    #[test]
    fn diff_visits_changed_bands() {
        let mut s = CellStore::new();
        for r in 0..1000 {
            s.set(c(r, 0), Cell::value((r as f64).into()));
        }
        let snap = s.clone();
        assert!(s.diff(&snap).is_empty());
        s.set(c(5, 0), Cell::value(99.0.into()));
        s.set(c(700, 3), Cell::value("new".into()));
        s.remove(c(900, 0));
        assert_eq!(snap.diff(&s), [c(5, 0), c(700, 3), c(900, 0)]);
    }

    #[test]
    fn range_iteration_order() {
        let mut s = CellStore::new();
        s.set(c(70, 2), Cell::value(3.0.into()));
        s.set(c(1, 1), Cell::value(1.0.into()));
        s.set(c(1, 3), Cell::value(2.0.into()));
        s.set(c(500, 0), Cell::value(4.0.into()));
        let v: Vec<f64> = s.iter_range(RangeRef::parse("A1:D100").unwrap()).map(|(_, x)| x.value.as_f64().unwrap()).collect();
        assert_eq!(v, [1.0, 2.0, 3.0]);
    }

    #[test]
    fn rows_keep_their_json_shape_and_order() {
        // WHIQ: a row is a sorted vector now, written as the {col: cell} map it was.
        let mut s = CellStore::new();
        s.set(c(3, 7), Cell::value(1.0.into()));
        s.set(c(3, 2), Cell::value(2.0.into()));
        s.set(c(3, 5), Cell::value(3.0.into()));
        let json = serde_json::to_value(&s).unwrap();
        let row = &json["bands"]["0"]["rows"]["3"];
        assert!(row.is_object(), "{json}");
        assert_eq!(row.as_object().unwrap().keys().cloned().collect::<Vec<_>>(), ["2", "5", "7"]);
        let back: CellStore = serde_json::from_value(json).unwrap();
        assert_eq!(back, s);
        let cols: Vec<u32> = s.row(3, 0, 10).map(|(col, _)| col).collect();
        assert_eq!(cols, [2, 5, 7]);
        assert_eq!(s.row(3, 3, 6).map(|(col, _)| col).collect::<Vec<_>>(), [5]);
        assert_eq!(s.row(3, 8, 9).count(), 0);
        // Replacing and removing keep the order.
        s.set(c(3, 5), Cell::value(9.0.into()));
        assert_eq!(s.get(c(3, 5)).unwrap().value, Value::Number(9.0));
        assert!(s.remove(c(3, 2)).is_some());
        assert_eq!(s.row(3, 0, 10).map(|(col, _)| col).collect::<Vec<_>>(), [5, 7]);
        s.shrink_to_fit();
        assert_eq!(s.len(), 2);
    }

    #[test]
    fn shifting() {
        let mut s = CellStore::new();
        s.set(c(2, 0), Cell::value(1.0.into()));
        s.set(c(5, 0), Cell::value(2.0.into()));
        s.shift_rows(3, 2);
        assert!(s.get(c(7, 0)).is_some());
        assert!(s.get(c(2, 0)).is_some());
        s.shift_cols(0, 1);
        assert!(s.get(c(7, 1)).is_some());
    }

    /// A store whose cells are only in a spill file reads them back, equal, and drops them again.
    #[test]
    fn whiq_spilled_cells_come_back_equal_and_evict_again() {
        let mut s = CellStore::new();
        for r in 0..300u32 {
            s.set(c(r, r % 7), Cell::value((r as f64).into()));
            s.set(c(r, 9), Cell::value(format!("text {r}").as_str().into()));
        }
        s.set(c(5, 5), Cell::formula(crate::Formula::new("SUM(A1:A9)")));
        let reference = s.clone();
        assert!(s.spill().unwrap(), "left memory");
        assert!(!s.is_resident() && s.is_backed());
        assert_eq!(s.len(), reference.len(), "the count is kept while cold");
        assert!(!s.is_cold_without_formulas(), "it holds a formula");
        // Looking at a cell reads them all back.
        assert_eq!(s.get(c(299, 9)).map(|x| x.value.clone()), Some(Value::from("text 299")));
        assert!(s.is_resident() && s == reference);
        // Unchanged since it was read: dropped without writing again.
        assert!(s.is_pristine() && s.spill().unwrap() && !s.is_resident());
        // A change makes it not pristine: written again, the edit kept.
        s.set(c(0, 0), Cell::value(7.5.into()));
        assert!(!s.is_pristine());
        assert!(s.spill().unwrap());
        assert_eq!(s.get(c(0, 0)).map(|x| x.value.clone()), Some(Value::Number(7.5)));
        assert_eq!(s.len(), reference.len());
        // A transient copy reads without keeping the cells in the original.
        assert!(s.spill().unwrap());
        let t = s.transient();
        assert!(t.is_resident() && !s.is_resident() && t.get(c(1, 9)).is_some());
        assert!(s.load_failure().is_none());
    }

    #[test]
    fn whiq_a_spill_file_a_cleaner_deletes_still_reads_and_a_damaged_one_is_said() {
        let mut s = CellStore::new();
        s.set(c(1, 1), Cell::value(1.0.into()));
        assert!(s.spill().unwrap());
        let path = s.describe_source().unwrap().trim_start_matches("spill file ").to_string();
        // Deleted behind our back (a temp cleaner): the handle we hold still reads it.
        #[cfg(unix)]
        {
            std::fs::remove_file(&path).unwrap();
            assert_eq!(s.get(c(1, 1)).map(|x| x.value.clone()), Some(Value::Number(1.0)));
            assert!(s.load_failure().is_none());
        }
        // Damaged in place (truncated): said, never read as an empty sheet.
        let mut t = CellStore::new();
        t.set(c(2, 2), Cell::value(2.0.into()));
        assert!(t.spill().unwrap());
        let path = t.describe_source().unwrap().trim_start_matches("spill file ").to_string();
        std::fs::OpenOptions::new().write(true).open(&path).unwrap().set_len(0).unwrap();
        assert!(t.get(c(2, 2)).is_none());
        assert!(t.load_failure().is_some_and(|m| m.contains("spill file")), "{:?}", t.load_failure());
    }
}
