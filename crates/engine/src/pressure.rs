//! WHIQ: memory pressure. A workbook's cells are the only thing that grows with its size; a sheet
//! that is not being used is written to disk (or, when the file it came from can give it back,
//! simply dropped) and read again the first time a cell of it is looked at. This runs between
//! commands, when nothing borrows the workbook, and replaces the sheet in every snapshot that
//! shares it (the saved copy, the undo and redo steps, the clipboard), so the memory really goes.
//! A refusal is stated, and only for a sheet that cannot fit in the budget by itself.

#[cfg(not(target_arch = "wasm32"))]
use std::collections::HashMap;
#[cfg(not(target_arch = "wasm32"))]
use std::sync::Arc;

#[cfg(not(target_arch = "wasm32"))]
use gridcraft_model::{Sheet, Workbook};
use serde_json::Value as Json;

use crate::{DocState, EngineError, Session};

/// What the cells of one open workbook may take: half the process's memory budget (the rest is
/// for the calculation graph, the undo steps and the commands themselves).
pub fn share() -> u64 {
    gridcraft_core::memory::budget() / 2
}

/// Commands that never read a sheet's cells, so a sheet too large to read is no reason to refuse
/// them (saving it, closing it, moving to another sheet).
fn needs_no_cells(id: &str) -> bool {
    id.starts_with("file.") || id.starts_with("window.") || id.starts_with("sheet.") || id.starts_with("view.")
}

#[cfg(not(target_arch = "wasm32"))]
type Swaps = Vec<(Arc<Sheet>, Arc<Sheet>)>;

/// Puts the new sheets of `swaps` in the workbook `h` (a copy of it, so the same old workbook
/// is the same new one wherever it is held).
#[cfg(not(target_arch = "wasm32"))]
fn swap_in(h: &mut Arc<Workbook>, swaps: &Swaps, done: &mut HashMap<*const Workbook, Arc<Workbook>>, keep: &mut Vec<Arc<Workbook>>) {
    let key = Arc::as_ptr(h);
    if let Some(n) = done.get(&key) {
        *h = n.clone();
        return;
    }
    if !h.sheets.iter().any(|s| swaps.iter().any(|(old, _)| Arc::ptr_eq(s, old))) {
        return;
    }
    let mut w = (**h).clone();
    for s in &mut w.sheets {
        if let Some((_, new)) = swaps.iter().find(|(old, _)| Arc::ptr_eq(s, old)) {
            *s = new.clone();
        }
    }
    let new = Arc::new(w);
    // The old workbook stays alive until every holder is done, so its address is not reused.
    keep.push(h.clone());
    done.insert(key, new.clone());
    *h = new;
}

impl DocState {
    /// About how much memory the cells of the open workbook take now.
    pub fn resident_bytes(&self) -> u64 {
        self.wb.sheets.iter().map(|s| s.cells.resident_bytes()).sum()
    }

    /// Takes the cells of sheets other than the active one out of memory, largest first, until the
    /// workbook is within `share`. Returns the sheets that left memory and what could not be done
    /// (a disk that refused a write: the sheet stays where it is, nothing is lost).
    #[cfg(not(target_arch = "wasm32"))]
    fn relieve(&mut self, share: u64) -> (Swaps, Vec<String>) {
        let mut notes = Vec::new();
        let mut swaps: Swaps = Vec::new();
        let total = self.resident_bytes();
        if total <= share {
            return (swaps, notes);
        }
        let active = self.wb.active_sheet;
        let mut candidates: Vec<(usize, u64)> = self
            .wb
            .sheets
            .iter()
            .enumerate()
            .filter(|(i, s)| *i != active && s.cells.is_resident())
            .map(|(i, s)| (i, s.cells.resident_bytes()))
            .filter(|(_, b)| *b > 0)
            .collect();
        candidates.sort_by_key(|c| std::cmp::Reverse(c.1));
        let mut freed = 0u64;
        for (i, bytes) in candidates {
            if total.saturating_sub(freed) <= share {
                break;
            }
            let Some(old) = self.wb.sheets.get(i).cloned() else { continue };
            let mut new = (*old).clone();
            match new.cells.spill() {
                Ok(true) => {
                    freed += bytes;
                    swaps.push((old, Arc::new(new)));
                }
                Ok(false) => {}
                Err(e) => notes.push(format!("sheet \"{}\" stays in memory, it could not be written to disk: {e}", old.name)),
            }
        }
        (swaps, notes)
    }
}

impl Session {
    /// Between commands: when the open workbook's cells are past their share of the memory
    /// budget, move the sheets in use least out of memory. Failures are said ([`Session::memory_notes`]).
    #[cfg(not(target_arch = "wasm32"))]
    pub(crate) fn relieve_memory(&mut self) {
        let Some(d) = self.docs.get_mut(self.active) else { return };
        let (swaps, notes) = d.relieve(share());
        let mut keep = Vec::new();
        if !swaps.is_empty() {
            let mut done = HashMap::new();
            swap_in(&mut d.wb, &swaps, &mut done, &mut keep);
            swap_in(&mut d.saved, &swaps, &mut done, &mut keep);
            for e in d.undo.iter_mut().chain(d.redo.iter_mut()) {
                swap_in(&mut e.wb, &swaps, &mut done, &mut keep);
            }
            if let Some(c) = &mut self.clipboard {
                swap_in(&mut c.wb, &swaps, &mut done, &mut keep);
            }
            if let Some((wb, ..)) = &mut self.format_painter {
                swap_in(wb, &swaps, &mut done, &mut keep);
            }
        }
        for n in notes {
            log::warn!("{n}");
            if !self.memory_notes.contains(&n) {
                self.memory_notes.push(n);
            }
        }
    }

    /// The reason a command cannot run: it would read a sheet that alone needs more memory than
    /// this process may use. `None` for everything else, including a sheet too large that is only
    /// saved or left.
    pub(crate) fn sheet_too_large_for(&self, id: &str, params: &Json) -> Option<EngineError> {
        if needs_no_cells(id) {
            return None;
        }
        let d = self.active()?;
        let si = crate::cmd::target_sheet(self, params).ok()?;
        let sh = d.wb.sheet(si)?;
        let need = sh.cells.loaded_bytes();
        let budget = gridcraft_core::memory::budget();
        if sh.cells.is_resident() || need <= budget {
            return None;
        }
        Some(EngineError::Other(format!(
            "sheet \"{}\" is too large to read here: its {} cells need about {} MB in memory and this engine process may use {} MB \
(--max-memory-mb, set by the platform from the machine's memory; WHIQ_ENGINE_PROC_MB). Nothing was changed. Save the workbook (a save streams it sheet by sheet), \
or split this sheet into several",
            sh.name,
            sh.cells.len(),
            need >> 20,
            budget >> 20
        )))
    }

    /// What the memory pressure pass could not do, since the last time this was taken.
    pub fn take_memory_notes(&mut self) -> Vec<String> {
        std::mem::take(&mut self.memory_notes)
    }
}
