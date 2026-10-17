# WHIQ changes to GridCraft

This is the White Hat IQ fork of GridCraft (branch `whiq`). The platform runs `gridcraft-cli mcp`
headless, one process per caller and document, behind a supervisor, with the read and write roots
set to the chat's folder and the author set to the caller. Its agents are LLMs in long unattended
runs, so the rule for every tool is **no dead stops**: every limit is a recoverable answer, never a
failure. Each entry says what changed, why, and how it is tested.

Upstream code is changed in place only where the fork has to hook in; nothing is reformatted, so
upstream merges stay possible.

## 1. Paths, saves and annotations

- **Path confinement** — `crates/engine/src/file_access.rs`, ported from WordCraft's
  `file_access`. `gridcraft-cli mcp --automation-read-root DIR --automation-write-root DIR`
  confines every file the headless server's commands read or write: paths are made absolute,
  links followed (a symlink out of the root is refused, a dangling link too), compared by whole
  components; read and write access are separate. Every engine file access goes through
  `io::read_file` / `io::write_file`, which check the roots (the system spelling word list is read
  `unconfined`, it is the app's own file). Relative paths resolve inside the root of their kind,
  not the working directory. With `--connect` (a running app) the roots are refused, like in
  WordCraft.
  Tests: `file_access::tests::*` (inside/outside, prefix siblings, `..`, links, scopes, relative
  paths), `file_access::tests::commands_do_not_touch_the_file_system_themselves` (no `std::fs`
  outside `io.rs`), `gridcraft-mcp` `automation_roots_confine_the_headless_server`, and the stdio
  contract suite (`tests/mcp-contract`).
- **Atomic saves, never a silent overwrite** — `io::write_file` writes a temporary file, `fsync`s
  it, renames it into place and flushes the folder. With the keep-old policy on
  (`io::save_policy`, always on in `gridcraft-cli mcp`) an existing file is renamed
  `OLD_<name>` (then `OLD_<stem>-2.<ext>`…, an `OLD_` file is never replaced) before the new one
  takes its place, unless the call passes `overwrite: true` or the file is one this process saved
  itself earlier in the session (re-saving your own output replaces it; the customer's original is
  kept once as `OLD_`). Each rename is reported back to the caller. Upstream callers (desktop app,
  `gridcraft-cli run/convert`) keep upstream behaviour.
  Tests: `io::tests::keep_old_policy_renames_the_previous_file`,
  `io::tests::failed_save_leaves_no_temporary_file`, contract suite `old_rename`.
- **MCP annotations** — every tool now states `readOnlyHint` (only reads), `destructiveHint`
  (tools that overwrite or delete: `save_workbook`, `write_range`, `set_cell`, `execute_command`,
  `screenshot`…), `idempotentHint` and `openWorldHint`. Upstream said `destructiveHint: false`
  everywhere, `save_workbook` included. Test: `annotations_say_what_each_tool_does`.
- **`--author NAME`** on `gridcraft-cli mcp` sets the session's user name (comments, notes).
