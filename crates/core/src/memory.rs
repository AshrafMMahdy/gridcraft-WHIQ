//! WHIQ: the process's memory budget, which every in-memory limit of the readers and the commands
//! is a share of, and a guard that stops work growing past it with a clear answer.
//!
//! The owner's rule: no tool is limited by file size. Large files stream; what has to sit in
//! memory (the cells of an open workbook) is bounded by the machine's memory, never by a fixed
//! number, and a limit that is reached is always said, never a silent cut.
//!
//! The budget is the `--max-memory-mb` the host gave (the platform sizes it from the machine);
//! without one, half the machine's memory where it can be read, else 4 GiB.

use std::sync::OnceLock;
use std::sync::atomic::{AtomicU64, Ordering};

static BUDGET: AtomicU64 = AtomicU64::new(0);
static PROBE: OnceLock<fn() -> Option<u64>> = OnceLock::new();

/// The memory budget in bytes.
pub fn budget() -> u64 {
    match BUDGET.load(Ordering::Relaxed) {
        0 => machine_memory().map(|m| m / 2).unwrap_or(4 << 30).max(256 << 20),
        b => b,
    }
}

/// Set the memory budget (bytes; 0 goes back to the machine's default).
pub fn set_budget(bytes: u64) {
    BUDGET.store(bytes, Ordering::Relaxed);
}

/// Was a budget given by the host (`--max-memory-mb`)?
pub fn budget_given() -> bool {
    BUDGET.load(Ordering::Relaxed) != 0
}

/// Physical memory of the machine, where the platform tells it without native calls (Linux).
pub fn machine_memory() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/meminfo").ok()?;
    let line = text.lines().find(|l| l.starts_with("MemTotal:"))?;
    let kb: u64 = line.split_whitespace().nth(1)?.parse().ok()?;
    kb.checked_mul(1024)
}

/// Install the resident-memory probe the guard uses (the MCP server's; nothing else measures).
pub fn set_probe(probe: fn() -> Option<u64>) {
    let _ = PROBE.set(probe);
}

/// Resident memory now, when a probe is installed.
pub fn resident() -> Option<u64> {
    PROBE.get().and_then(|p| p())
}

/// `Err` with a sentence for the user when the process is past its given budget while doing
/// `what` (e.g. "opening sheet BOQ, row 812,000"): a read stops there rather than growing until
/// the host closes the document. `Ok` without a host budget or a probe.
pub fn check(what: &str) -> Result<(), String> {
    if !budget_given() {
        return Ok(());
    }
    let (Some(rss), b) = (resident(), budget()) else { return Ok(()) };
    if rss <= b {
        return Ok(());
    }
    Err(format!(
        "{what}: GridCraft used {} MB, over the {} MB this engine process may use (--max-memory-mb, set by the platform from the machine's memory). \
Nothing was lost; the workbook needs a larger memory limit on this machine (WHIQ_ENGINE_PROC_MB), or split it into smaller workbooks",
        rss >> 20,
        b >> 20
    ))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn the_budget_follows_the_host_and_the_guard_names_both_numbers() {
        assert!(check("nothing").is_ok(), "no host budget: no guard");
        set_budget(1 << 20);
        assert_eq!(budget(), 1 << 20);
        set_probe(|| Some(5 << 20));
        let e = check("opening sheet A, row 10").unwrap_err();
        assert!(e.contains("5 MB") && e.contains("1 MB") && e.contains("row 10"), "{e}");
        set_budget(0);
        assert!(check("again").is_ok());
        assert!(budget() >= 256 << 20);
    }
}
