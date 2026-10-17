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

## 3. Arabic and complex text: shaping, bidi, embedded subset fonts, RTL sheets

- **`crates/text` (`gridcraft-text`, layer 0, new).** Noto Sans and Noto Sans Arabic (regular,
  bold; SIL OFL 1.1, `assets/fonts/whiq/`, ATTRIBUTION rows) are compiled into native builds, so
  an Arabic fallback face is always in the binary (web builds carry none and keep the standard
  PDF fonts). Lines are ordered with the Unicode Bidirectional Algorithm (`unicode-bidi`); the
  paragraph direction is the one given, else the first strong character's (an Arabic cell is
  RTL). Runs are shaped with HarfRust (Arabic joining, ligatures, marks, mirrored brackets);
  characters pick a face by script, neutrals stay with their run, a face lacking a character
  hands it to the other. Glyph outlines come from skrifa. *Deviation from upstream policy:*
  upstream keeps fonts out of the repository (craft-fonts); the WHIQ contract requires an Arabic
  fallback always present, so these four OFL faces are vendored, documented in ATTRIBUTION.md.
- **PDF (`crates/pdf`).** New `TextMode`: `Standard` (upstream), `Auto` (standard fonts for
  plain left-to-right WinAnsi text, embedded shaped text for everything else — the engine's new
  default) and `Embedded` (every string; the MCP `export_pdf` default). Embedded text is a Type0 /
  CIDFontType2 font per face with a **subset** FontFile2 (`subsetter`, Flate-compressed),
  `CIDToGIDMap`, `/W` widths and a `ToUnicode` map. Every (glyph, text) pair has its own CID, because
  shaped Arabic draws different letters with one glyph plus dots; marks follow their base in the
  content stream; glyphs are placed with one `Tm` per line and `TJ` adjustments, so text
  extraction (pdftotext) reads `البند`, `الإجمالي` in order and copy/paste works.
- **Cells.** `Alignment::reading_order` (model, xlsx `readingOrder` read/write, new command
  `home.textDirection {range, direction: context|ltr|rtl}`, MCP `format_range direction`).
  General-aligned text starts at its reading start (Arabic right-aligned), numbers stay right.
- **RTL sheets** (`view.rightToLeft`, `sheet.right_to_left`): PDF and PNG draw the sheet mirrored
  (column A on the right, row headings on the right, overflow to the left), text and pictures
  not flipped; charts, pictures and shapes are anchored from the right.
- The sheet painter (`crates/engine/src/cmd/print.rs`) now draws on a `Canvas` trait (PDF page or
  raster canvas) and measures text with the surface's own metrics (shaped widths for wrapping,
  alignment, shrink-to-fit, `####`, chart labels).
- Tests: `gridcraft-text` (bundled faces, joining, visual order, mixed lines, outlines),
  `gridcraft-pdf` (`embedded_fonts_carry_arabic_as_subset_type0_fonts`,
  `embedded_text_is_extractable` with pdftotext), engine `print::tests::arabic_cells_are_right_aligned_…`,
  `right_to_left_sheets_put_column_a_on_the_right`, `pdf_embeds_subset_fonts_for_arabic_only_where_needed`,
  xlsx round trip of `readingOrder`, contract `render::arabic_pdf_export_embeds_subset_fonts`.

## 4. Headless renders and PDF export with page setup

- **`crates/raster` (`gridcraft-raster`, layer 0, new):** a tiny-skia canvas with the PDF page's
  drawing calls (fills, strokes, dashes, polygons, circles, clips, images) and shaped text filled
  from glyph outlines; PNG output. Sizes are capped (16 384 px a side, 100 Mpx).
- **`file.renderPng`** (engine command): `{range?, sheet?, scale?: 0.1–8, gridlines?, headings?,
  maxPixels?, path?}` draws a range (default the print range) with the PDF painter — fills,
  borders, number formats, fonts, merged cells, conditional formats, charts, pictures, shapes,
  sparklines — to PNG (file or base64). Whole columns/rows mean their used part.
- **MCP tools:** `render_range` (a range; inline MCP `image` content when no `path`, ≤ 2 MB),
  `render_sheet` (the whole sheet scaled to `max_pixels`), `export_pdf` (`path`, `sheets`, `range`,
  `paper`, `orientation`, `fit_width`, `fit_to_page`, `repeat_header_rows`, `margins`,
  `gridlines`, `headings`, `header`, `footer`, `fonts`), which applies the page setup to the
  sheet(s) and exports with embedded subset fonts. `file.exportPdf` takes `fonts`.
- Tests: engine `png_renders_refuse_hostile_sizes`, raster unit tests, contract
  `render::arabic_png_render` (Arabic right-aligned in A1, numbers right, inline image block,
  fitted sheet, RTL sheet with A on the right) and `render::pdf_page_setup` (A4 landscape,
  fit to width over several pages, header row repeated on every page, footer page numbers).
