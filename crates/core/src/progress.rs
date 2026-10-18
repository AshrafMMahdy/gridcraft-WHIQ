//! WHIQ parity pass: progress of the call in progress, for `job_status` and `server_status`
//! (the shape WordCraft and DeckCraft report too: progress 0..1, stage, steps, stages passed).
//!
//! The MCP worker thread turns reporting on for itself ([`watch_this_thread`]); long-running
//! code says where it is with [`step_in`] (a stage name and `done` of `total` steps). On any
//! other thread — the desktop app, the tests — every call is a no-op that costs one
//! thread-local read. The fraction never goes back, and only [`finish`] makes it 1.

use std::cell::Cell;
use std::sync::Mutex;

/// How far the call is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Progress {
    pub stage: String,
    pub done: u64,
    pub total: u64,
    /// Stages passed so far, in order, the current one last.
    pub stages: Vec<String>,
    /// 0..1, never going back.
    pub overall: f64,
}

static STATE: Mutex<Option<Progress>> = Mutex::new(None);

thread_local! {
    static WATCHED: Cell<bool> = const { Cell::new(false) };
}

fn state() -> std::sync::MutexGuard<'static, Option<Progress>> {
    STATE.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// The calling thread runs the calls whose progress is reported.
pub fn watch_this_thread() {
    WATCHED.with(|w| w.set(true));
}

fn watched() -> bool {
    WATCHED.with(Cell::get)
}

/// A call starts: nothing done yet, in `stage`.
pub fn reset(stage: &str) {
    *state() = Some(Progress { stage: stage.to_string(), stages: vec![stage.to_string()], ..Progress::default() });
}

/// The call is at step `done` of `total` (0: not counted) of `stage`.
pub fn step_in(stage: &str, done: u64, total: u64) {
    if !watched() {
        return;
    }
    let mut g = state();
    let p = g.get_or_insert_with(Progress::default);
    if p.stage != stage {
        p.stage = stage.to_string();
        if p.stages.last().map(String::as_str) != Some(stage) && p.stages.len() < 64 {
            p.stages.push(stage.to_string());
        }
    }
    p.done = done;
    p.total = total.max(done);
    if p.total > 0 {
        p.overall = p.overall.max((done as f64 / p.total as f64).min(0.99));
    }
}

/// The call ended: 1.
pub fn finish() {
    if let Some(p) = state().as_mut() {
        p.overall = 1.0;
        if p.total > 0 {
            p.done = p.total;
        }
    }
}

/// How far the call in progress is.
pub fn current() -> Option<Progress> {
    state().clone()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stages_and_steps_never_go_back() {
        watch_this_thread();
        reset("write_range");
        step_in("writing cells", 5, 10);
        step_in("recalculating formulas", 1, 10);
        let p = current().unwrap_or_default();
        assert_eq!(p.stages, ["write_range", "writing cells", "recalculating formulas"]);
        assert_eq!((p.done, p.total, p.overall), (1, 10, 0.5));
        finish();
        assert_eq!(current().unwrap_or_default().overall, 1.0);
        // Another thread reports nothing.
        std::thread::spawn(|| step_in("elsewhere", 1, 1)).join().ok();
        assert_eq!(current().unwrap_or_default().stage, "recalculating formulas");
    }
}
