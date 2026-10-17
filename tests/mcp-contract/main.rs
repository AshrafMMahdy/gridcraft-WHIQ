//! WHIQ MCP contract suite: drives the built `gridcraft-cli mcp` over stdio and proves the "no
//! dead stops" contract (WHIQ-CHANGES.md): error shape, busy, jobs, oversized results,
//! eviction and recovery, SIGTERM and panic auto-save, OLD_ renames, path confinement,
//! annotations, forgiving inputs, caller identity — and the gridcraft features (Arabic
//! rendering, PNG/PDF export, build_workbook, recalculation, in-place edits).
//!
//! Run: `cargo test -p gridcraft-cli --test mcp_contract` (debug binary), or against a release
//! build: `GRIDCRAFT_CLI=target/release/gridcraft-cli cargo test -p gridcraft-cli --test mcp_contract`.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::disallowed_methods, dead_code)]

mod harness;

mod contract;
