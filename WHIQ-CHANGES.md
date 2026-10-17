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

## 2. The "no dead stops" contract on the MCP server

Code: `crates/mcp/src/contract.rs` (error shape, forgiving inputs, trimming), `whiq.rs` (the
pipeline every `tools/call` goes through, `server_status`, `job_status`, error classification),
`runtime.rs` (the concurrent server `gridcraft-cli mcp` runs). Tests: `tests/mcp-contract/`
(`cargo test -p gridcraft-cli --test mcp_contract`, see section "Tests").

- **Error shape.** Every failed call is an `isError` result whose `structuredContent` is
  `{"error": {"code", "reason", "next_step", …}}` and whose text says the same. Codes:
  `invalid_argument` (with an example of good arguments), `not_found` (with `closest`: files next
  to a missing path, command ids, sheet names, tool names), `outside_root` (with the roots and the
  path to use instead), `busy` (`retry_after_ms`, `job_id`), `evicted` (`recovered_path`),
  `too_large`, `password_required`, `wrong_password`, `no_document`, `not_available`, `no_ui`,
  `internal_error`, `failed`. Argument problems are tool results the model sees, not JSON-RPC
  errors; protocol errors (bad JSON, unknown tool) carry the same object as `error.data`.
- **Nothing hangs.** One worker thread owns the document; the stdio loop reads requests on its own
  thread and never waits on it. A call arriving while another runs waits at most 300 ms, then
  answers `busy` with `retry_after_ms` and the running job's id. Every document call is a job: one
  still running after `--call-timeout-ms` (default 20 s) answers `{job_id, status: "running"}` and
  carries on; `job_status(job_id)` reports progress and keeps the result (it survives client
  timeouts). Long work starts as a job at once: `async: true` on any tool, opening a file over
  `--job-open-mb` (20), recalculating / exporting / rendering a workbook over `--job-cells`
  (250 000) cells. `server_status` and `job_status` are answered even mid-call.
- **Work is never lost.** After every call the worker publishes a snapshot of the document
  (`Arc<Workbook>`, so it costs nothing). It is written to
  `<name>.recovered-<YYYYMMDD-HHMMSS-mmm>.xlsx` inside the write root when the document is
  evicted (`--evict-idle-secs N` without calls, or the process over `--max-memory-mb`), when a
  call panics (the call answers `internal_error` with `recovered_path`; the server carries on),
  on SIGTERM / SIGINT / SIGHUP (Ctrl-C, Ctrl-Break, console close on Windows; then exit 143), and
  when the client hangs up with unsaved changes. After an eviction every document call answers
  `evicted` naming the file to reopen; a new process finds recovered files in the write root and
  names them on its first call and in `server_status`.
- **Oversized results are trimmed.** `read_range` returns at most `--max-read-cells` (10 000) cells
  (`max_cells` per call) with `truncated: true`, `continue_from`, `next_range` and a `next_step`;
  any result over `--max-result-kb` (256) is cut at its largest list with `truncated`,
  `continue_from` (an `offset`); `list_commands` / `list_functions` take `offset` / `limit`.
- **server_status**: version, memory (RSS on Linux, an estimate elsewhere), open document (title,
  path, unsaved changes, sheets, cells), busy call, jobs, roots, author, limits, eviction, recovered
  files.
- **Forgiving inputs.** Tool names in any spelling (`readRange`, `read-range`) plus common aliases
  (`save`, `status`, `pdf`); argument keys case- and spelling-insensitive plus synonyms (`file` →
  `path`, `address` → `cell`/`range`, `data` → `values`); scalars coerced (`"true"`, `"12"`, a
  flat list as one row); enum values in any case; command ids in `execute_command` in any case;
  sheet names case-insensitive. What was reinterpreted or ignored comes back in `warnings`.
- **Caller identity.** `--author NAME` and a `caller` argument on every tool: notes/comments carry
  it, and saves write it as `cp:lastModifiedBy` (and `dc:creator` when the workbook has none).
  New `DocProps::last_modified_by` (model, xlsx read/write, `file.properties lastModifiedBy`).
- `structuredContent` on every result; `caller` / `async` on every tool, `overwrite` on the tools
  that write files.
- Contract-test hooks `debug_sleep` / `debug_panic` exist only with `GRIDCRAFT_MCP_TEST_HOOKS=1`.
- Upstream unit tests in `crates/mcp/src/tests.rs` that expected JSON-RPC `-32602` for bad
  arguments now expect an `invalid_argument` tool result (the contract's choice).
