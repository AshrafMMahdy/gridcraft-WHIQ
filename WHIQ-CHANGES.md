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
  `too_large`, `locked` (the file is open in another program; `retry_after_ms`), `disk_full`,
  `password_required`, `wrong_password`, `no_document`, `not_available`, `no_ui`,
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

- **`crates/text` (`gridcraft-text`, layer 0, new).** Fonts come from craft-fonts (see "Fonts"
  below). Lines are ordered with the Unicode Bidirectional Algorithm (`unicode-bidi`); the
  paragraph direction is the one given, else the first strong character's (an Arabic cell is
  RTL). Runs are shaped with HarfRust (Arabic joining, ligatures, marks, mirrored brackets);
  characters pick a face by script, neutrals stay with their run, a character the face lacks goes
  to the first face that has it. Glyph outlines come from skrifa.
- **PDF (`crates/pdf`).** New `TextMode`: `Standard` (upstream), `Auto` (standard fonts for
  plain left-to-right WinAnsi text, embedded shaped text for everything else) and `Embedded`
  (every string; the default since round 2, see "PDF fonts by default"). Embedded text is a Type0 /
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
  not flipped; charts, pictures and shapes are anchored from the right. Rotated text is placed
  exactly (round 2): a run from origin O at angle a, ending at E, is drawn from the reflected E at
  angle −a, so it covers the reflected baseline with its ascent on the reflected side (test
  `rotated_text_on_right_to_left_sheets_is_the_exact_mirror`: at ±30°, 45°, ±90° and 15° the RTL
  ink box equals the reflected LTR ink box within one pixel).
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
  `gridlines`, `headings`, `header`, `footer`, `fonts`, `pages: "2-5"`), which applies the page
  setup to the sheet(s) and exports with embedded subset fonts. `file.exportPdf` takes `fonts` and
  `pages` (a part of a long export, numbered as in the whole; an out-of-range part says how many
  pages there are).
- Tests: engine `png_renders_refuse_hostile_sizes`, raster unit tests, contract
  `render::arabic_png_render` (Arabic right-aligned in A1, numbers right, inline image block,
  fitted sheet, RTL sheet with A on the right) and `render::pdf_page_setup` (A4 landscape,
  fit to width over several pages, header row repeated on every page, footer page numbers).

## 5. Workbook authoring: `build_workbook`, native charts, recalculated formulas

- **`file.build`** (engine command, `crates/engine/src/cmd/build.rs`; MCP `build_workbook`; CLI
  `gridcraft-cli build spec.json --out book.xlsx`): one JSON spec makes a whole workbook — sheets
  with a header row and rows (strings starting with `=` are formulas, other strings parsed as typed
  input), `start`, `columns: [{header, width, format}]`, `number_formats` by column or range,
  `column_widths` (map, list or `"auto"`), `header_style`, `freeze_header`, `autofilter`, `table`
  (style / name), `merges`, `conditional_formats`, `validations`, `rtl`, document properties, and
  native **charts**: `bar` (horizontal), `column`, `line`, `pie`, `doughnut`, `area`, `scatter`,
  `combo` (first series columns, the rest lines; `secondary: true` puts a series on a secondary
  axis), with `title`, `x_title` / `y_title`, `legend`, `data_labels`, `stacked`, `colors`, `at`,
  `width`, `height`, and `series: [{name, values, categories, type, secondary, color, smooth}]` by
  range — or `data: "A1:C9"` like Insert › Chart. Everything is applied with the engine's own
  commands, the workbook is recalculated before it is returned or saved (so every formula's cached
  value is right when Excel opens it), and its undo history starts clean. A bad spec fails naming
  the sheet and step ("sheet Sales, chart 2: …") and leaves no half-built workbook open. Over 5
  million cells is refused with a next step.
- **`formulas.recalculate`** (MCP `recalculate`; CLI `gridcraft-cli recalculate book.xlsx [--out]
  [--json]`): a full recalculation, with the count of formulas and the cells that evaluate to errors.
  `gridcraft-cli check` stays as the oracle (cached values vs recalculation, round-trip fidelity).
- **The platform's chart specs** (`shared/exports/chartXml.ts`: `kind: col|line|scatter|combo`,
  `series[].valRange / catRange / xRange / color`, combo `lineSeries` on the same axes,
  `hideLegend`, `xTitle` / `yTitle`, a cell-box `anchor {fromCol, fromRow, toCol, toRow}`, top-level
  `charts` naming their `sheet`) are accepted as they are, so moving the platform to
  `build_workbook` loses nothing its injector made; pie, doughnut, area, bar and a secondary axis
  come on top. Columns given no width are auto-fitted (`autofit: false` turns it off), so numbers
  never show as `####`.
- CLI `gridcraft-cli render book.xlsx out.png [--range] [--sheet] [--scale]`.
- Tests: engine `build::tests::*` (sheets, formats, table, five chart kinds incl. combo with a
  secondary axis, cached values after save, clean failure), contract
  `authoring::build_workbook_with_native_charts` (chart parts and kinds, `barDir`, two value axes,
  frozen header, `<f>…</f><v>…</v>`, table, autoFilter, number formats, author, `check` passes),
  `authoring::recalculate_reports_formulas_and_errors`.

## 6. In-place edits that keep charts, PivotTables, slicers and unmodelled parts

- **Table slicers in XLSX** (`crates/xlsx/src/slicers.rs`, new): upstream modelled slicers but
  neither wrote nor read them (a save dropped them). Now the slicer part, the slicer cache
  (`x15:tableSlicerCache tableId column`), the workbook and sheet extensions, the `Slicer_<column>`
  defined name and the drawing frame (`mc:AlternateContent` `sle:slicer`) are written, and read
  back from Excel files; the selection is the table's filter.
- **PivotTable slicers** (round 2). Model: `Slicer::pivots` lists the connected PivotTables
  (`Sheet2!PivotTable1`, Excel's Report Connections); `column` is the source field. Engine: the
  slicer's selection filters every connected PivotTable when it is computed (`pivot::compute`),
  `insert.slicer {pivot, column}`, `slicer.connections {pivots}`, `slicer.list`, and
  `slicer.toggle` / `slicer.selectAll` accept a slicer by caption; renaming the PivotTable's sheet
  rewrites the links, deleting the PivotTable removes its slicers. XLSX: written as Excel 2010
  does — the `x14` slicer cache naming its PivotTables by `tabId` (the sheet's `sheetId`) and name,
  `data/tabular pivotCacheId` matching the cache's new `x14:pivotCacheDefinition pivotCacheId`,
  the items with their `s` selection flags, the `x14` sheet `slicerList`, the `a14` drawing frame,
  the `Slicer_<field>` name, and the selection as hidden items of that field in every connected
  PivotTable (on an axis or not). Every id is regenerated on each save; a slicer whose
  PivotTables or field are gone is left out, so nothing dangles. PivotTables one slicer connects
  now share one PivotCache (Excel's rule; `pivot.rs` builds a cache over several PivotTables, with
  the union of their axis fields, groupings, calculated fields and the slicer fields). Read back
  from Excel files: the selection comes from the cache items and the PivotCache's shared items;
  the hidden-items filter Excel also wrote for that field is folded into the slicer (it owns it).
  `refreshOnLoad="1"` stays on every cache, so Excel recomputes the report on open; PivotTable
  definitions, caches, records and styles (`pivotTableStyleInfo`) are rewritten from the model.
- Test fixture `tests/mcp-contract/fixtures/pivot-slicer.xlsx` (made by `make_pivot_slicer.py` in
  the same folder, laid out as Excel 2016 saves it: cacheId 5 vs pivotCacheId 2, a slicer on a
  field on no axis with one item selected, a slicer on a row field, custom properties). Tests:
  `gridcraft-xlsx tests::pivot_slicer::*` (read, write with a renamed and moved sheet, every
  reference consistent, nothing dangling when the PivotTable is gone), engine
  `cmd::slicer::tests::*` (the report follows the slicers, sheet rename, save/reopen, delete,
  `insert.slicer {pivot}`), contract `authoring::pivot_and_slicers_survive_open_edit_save_reopen`
  (open → edit source → refresh → rename sheet → insert rows → save → reopen in a new process →
  clear a slicer → save; `tabId`s, `refreshOnLoad`, style and custom properties checked; `check`
  passes).
- Fixed along the way (an upstream calc bug the command fuzzer hit once the command list grew): a
  dynamic array that reads its own old spill range now gets the same values from an incremental
  recalculation as from a full one — stale spilled values of a formula being recalculated are
  never read back (`calc/src/recalc.rs`; test `self_referencing_spill_agrees_with_full_recalc`).
- **MCP editing tools**: `insert_rows`, `insert_columns`, `delete_rows`, `delete_columns`
  (`"7"`, `"5:7"`, `"C"`, on any `sheet`), `rename_sheet`, `add_sheet`, `conditional_format`,
  `data_validation`, plus the existing `write_range`, `set_cell`, `format_range`.
- Charts, PivotTables (caches and tables) and the pass-through parts (custom document properties,
  custom XML, external links, controls, OLE objects…) were already kept by upstream's model and
  `passthrough.rs`; the contract test proves it end to end over MCP.
- Test: contract `authoring::in_place_edits_keep_charts_pivots_slicers_and_unmodelled_parts` — a
  customer workbook with a table, chart, slicer, PivotTable, custom document properties and a
  custom XML part is opened, edited (cells, rows, columns, conditional format, validation, a new
  sheet with an Arabic name, a formula across sheets) and saved in place: the original becomes
  `OLD_customer.xlsx`, every part survives (custom parts byte for byte), the edits are there,
  `lastModifiedBy` is the caller and `gridcraft-cli check` passes. Engine
  `build::tests::table_slicers_round_trip_through_xlsx`.

## Fonts (round 2): craft-fonts, as in WordCraft and DeckCraft

- The four Noto files vendored in round 1 are gone (`assets/fonts/whiq/` and their ATTRIBUTION
  rows): fonts live in [storytold/craft-fonts](https://github.com/storytold/craft-fonts), as the
  upstream rule says. `crates/text/build.rs` follows craft-fonts' `docs/integration.md` recipe and
  DeckCraft's: with `CRAFT_FONTS_DIR=<checkout>` it embeds every font of `fonts/latin-manifest.txt`
  and `fonts/manifest.txt`; without it exports use the PDF standard fonts (the app builds, tests and
  runs; font-dependent tests skip, see below). `.cargo/config.toml` points `CRAFT_FONTS_DIR` at a
  checkout next to this repository (`../craft-fonts`), like DeckCraft's.
- **Pinned craft-fonts commit: `43913056ce82a3fdad0ef69f6b88d1dc922a8104`** (branch
  `pr/latin-presentation-fonts`, the commit DeckCraft's CI and releases pin): Carlito (Calibri's
  metric-compatible substitute), Caladea (Cambria), Liberation Sans / Serif / Mono (Arial, Times New
  Roman, Courier New), each with **Regular, Bold, Italic and Bold Italic**, Inter, Lato, Source
  Sans 3 / Serif 4, Merriweather, Poppins, Montserrat, Open Sans, Roboto, Nunito Sans, Playfair
  Display; Noto Sans Arabic (variable: instanced at weight 400 and 700, so bold Arabic is the real
  bold design); BIZ UDPGothic, BIZ UDMincho, Shippori Mincho (Japanese) and Noto Sans CJK SC
  (Chinese, a CFF font, embedded as CIDFontType0 / `FontFile3 /OpenType`).
- A cell's font name picks the family: the family itself when embedded (Lato, Roboto…), else its
  open substitute (Calibri, Aptos, Segoe UI → Carlito; Cambria → Caladea; Arial, Helvetica,
  Tahoma, Verdana → Liberation Sans; Times New Roman, Georgia → Liberation Serif; Courier New,
  Consolas → Liberation Mono), else Carlito. **Italic and bold italic use the family's italic
  faces** (PDF `ItalicAngle` and the Italic flag set); only a face with no italic design is slanted.
- **`CRAFT_FONTS_REQUIRED=1`** (releases): the build fails when the checkout is missing, a manifest
  is malformed, there is no Arabic font, or no Latin family has all four styles (an un-pinned
  checkout without the Latin manifest fails here). Tests that need fonts fail instead of skipping
  when it is set at test time.
- `server_status` reports `fonts: {faces, families, default}`.
- Binary size: the release `gridcraft-cli` grows from ~20 MB to ~80 MB (about 60 MB of fonts, CJK
  included).

**Release recipe:**

```sh
git clone https://github.com/storytold/craft-fonts ../craft-fonts
git -C ../craft-fonts fetch origin pr/latin-presentation-fonts
git -C ../craft-fonts checkout --detach 43913056ce82a3fdad0ef69f6b88d1dc922a8104
CRAFT_FONTS_DIR=$PWD/../craft-fonts CRAFT_FONTS_REQUIRED=1 cargo build --release -p gridcraft-cli
CRAFT_FONTS_DIR=$PWD/../craft-fonts CRAFT_FONTS_REQUIRED=1 cargo test
tests/mcp-contract/smoke.sh target/release/gridcraft-cli
```

(Windows: the same with `set CRAFT_FONTS_DIR=%CD%\..\craft-fonts` and `set CRAFT_FONTS_REQUIRED=1`;
the path must be absolute.)

## Cancel (round 2)

- **`cancel` tool** (answered by the stdio loop, like `server_status`, so it works while a call
  runs): `{job_id?}` stops the call in progress, or the running job named; it answers at once with
  `status: "cancelling"`, and the call or job then ends with error code **`cancelled`** (job state
  `cancelled` in `job_status` and `server_status`). Asking for an ended job answers `not_running`
  with its status; for an unknown one `not_found`. The client's own `notifications/cancelled`
  (MCP) stops the request it names when that is the call in progress.
- **Safe points** (`gridcraft_core::cancel::checkpoint`): every formula a recalculation evaluates,
  every row `range.setValues` writes, every row read from or written to an XLSX file, every 1 024
  PivotTable source records, every cell an export or picture draws, and the debug sleep. A
  checkpoint that sees the request unwinds (`resume_unwind`, no panic message) to the command
  runner, which **puts the workbook back exactly as it was before the command** (the same snapshot
  the panic guard uses) and rebuilds the calculation state; a command that failed while a stop was
  pending (a nested command was stopped) is rolled back and reported cancelled as a whole. Files
  are written only after the workbook is serialized, so a stopped save or export writes nothing.
  The request targets the worker thread only (`request(thread)`), so nothing else is affected.
- Tests: contract `cancel_stops_calls_and_jobs_and_keeps_the_document` (a 20 s call stopped in
  well under 10 s; a 150 000-row write with formulas run as a job, cancelled part-way, leaves the
  workbook exactly as before and still working; `notifications/cancelled`; `not_running`,
  `not_found`), engine `cancelled_commands_roll_back`.

## Checkpoints and memory (round 2)

- **Periodic checkpoint auto-save** (`gridcraft-cli mcp --checkpoint-edits N` (default 20)
  `--checkpoint-secs S` (default 60), 0 turns either off): a background thread writes the open
  document's unsaved work to its recovered file (`<name>.recovered-<stamp>.xlsx` in the write
  root) after N calls that changed the workbook, or once unsaved changes are S seconds old. One
  file per document, rewritten in place (atomically) each time; eviction, signal and hang-up
  auto-saves update the same file instead of adding one; saving the document deletes it (the
  saved file is newer). So a hard kill (`kill -9`, Task Manager's End task, a power cut) loses at
  most N calls or S seconds of work, and the next server announces the file (`recovered_files`).
  Writing never blocks calls (the workbook snapshot is immutable). `server_status` reports
  `checkpoint: {path, pending_edits, writes, every_edits, every_secs}`.
- **Memory** is measured on Linux, Windows (working set) and macOS (resident size) with the
  `memory-stats` crate (`/proc/self/statm` remains the Linux fallback), so `server_status`
  `memory.rss_bytes` and `--max-memory-mb` eviction work on all three (`memory-stats` checked for
  `x86_64-pc-windows-gnu` and `x86_64-apple-darwin`).
- Test: contract `checkpoints_survive_a_hard_kill` (no file after 2 changes, one after 3, the same
  file rewritten after 3 more, SIGKILL, the file holds all six edits and the next server lists it;
  a time-based checkpoint; saving removes it; real `rss_bytes`).

## PDF fonts by default (round 2)

- Every PDF the engine writes (`file.exportPdf`, `file.print`, Save As PDF, MCP `export_pdf`)
  now embeds **every** string — Latin included — in subset fonts by default (`TextMode::Embedded`:
  Carlito for Calibri, Liberation for Arial / Times / Courier… see "Fonts"), so the PDF looks the
  same in every reader and on every machine and no base font is left for the reader to supply.
  `standardFonts: true` (MCP `standard_fonts: true`) keeps the smallest file: PDF base fonts for
  plain Latin text, embedded subsets only for Arabic and other scripts; `fonts: "standard"` is
  upstream's base-fonts-only output. Without craft-fonts the base fonts are used.
- Tests: engine `pdf_embeds_every_string_by_default_and_standard_fonts_on_request` (no
  `/BaseFont /Helvetica` by default, `(hello) Tj` only with `standardFonts`, which is smaller);
  upstream's tests that read drawn text as `(text) Tj` ask for `standardFonts`.

## Parity pass (2026-10-18): one contract surface for the three craft forks

WordCraft, GridCraft and DeckCraft were built independently against the same contract; this pass
read the three MCP servers side by side and gave them one shared surface. **`CONTRACT-PARITY.md`**
(the same file in the three repositories) is the table: every contract item, what each app did
before, and the one shape all three answer with now. Where an earlier section of this file says
otherwise (an error code, a field name, a flag, a default), the parity pass and
`CONTRACT-PARITY.md` are what holds. Domain tools keep their names.

| Change | Ported from | Where | Tested by (`tests/mcp-contract/` unless noted) |
|---|---|---|---|
| Error codes from the shared set of 18 (`contract::code::ALL`): `internal_error` → `internal`, `no_ui` → `unsupported`, `unavailable` → `not_available`, `parse_error` → `invalid_request`, `out_of_range` classified; cancel's `not_running` error became a success answer; an unknown tool is a tool result (`not_found` + `closest`), no longer JSON-RPC `-32602`; the error text block is `[code] reason` / `next_step: …` | WC, DC | `crates/mcp/src/contract.rs`, `whiq.rs`, `runtime.rs`, `server.rs` | every call: `harness::Mcp::call` checks the code is in the shared list; `contract::errors_carry_code_reason_and_next_step`, `crates/mcp/src/tests.rs` |
| Progress on every call and job — `progress` 0..1, `stage`, `steps`, `stages` — from a new `gridcraft_core::progress` (the MCP worker turns it on for itself; elsewhere a no-op): formulas recalculated, rows of cells written, sheets read and written, PDF pages drawn; `job_status` and `server_status.current_call` report it | DC, WC | `crates/core/src/progress.rs` (new), `crates/calc/src/recalc.rs`, `crates/engine/src/cmd/edit.rs`, `crates/engine/src/cmd/print.rs`, `crates/xlsx/src/read.rs`, `crates/xlsx/src/write.rs`, `runtime.rs` | `contract::busy_and_jobs`, `contract::large_open_is_a_job`, `progress::tests` |
| `job_status` in the shared shape (`status`, `result` or `error`, a finished job's pictures as content; `is_error` gone); `list_jobs`; `job_status {}` lists | DC, WC | `runtime.rs` (`job_summary`, `job_status`, `list_jobs`) | `contract::busy_and_jobs` |
| `notifications/cancelled` (clients send it on their own timeouts) no longer stops the call: it carries on as a job, its answer kept for `job_status`; `cancel` answers `{cancelled, status, job_id, tool, next_step}` and takes `queued` | WC | `runtime.rs` (`cancel`, `Job::client_cancelled`) | `contract::cancel_stops_calls_and_jobs_and_keeps_the_document` |
| Progress notifications: a request with `_meta.progressToken` gets `notifications/progress` while it runs | WC | `runtime.rs` (`progress_notification`, `gridcraft-progress` thread) | `contract::a_call_with_a_progress_token_gets_progress_notifications` |
| Flags: `--job-after-ms` (15000; was `--call-timeout-ms` 20000), `--busy-after-ms` (300, now settable), `--open-job-mb` (was `--job-open-mb`), `--idle-evict-secs` (was `--evict-idle-secs`), `--max-result-bytes` (was `--max-result-kb`); the old names are still read | WC, DC | `apps/gridcraft-cli/src/main.rs` | `contract::*` use the new names |
| `evict` tool (for a supervisor); `server_status.evicted` is a list `[{name, reason, recovered_path, original_path}]` | DC | `runtime.rs` (`worker_loop`, `evict`), `whiq.rs` | `contract::evicted_work_is_recovered`, `smoke.sh` |
| Recovered files: `<name>.recovered-<YYYYMMDD-HHMMSS>.xlsx` (no milliseconds; `-2`… if taken); reported as `recovered_files: [{path, reason, original, at}]` on the next answer (once) and in `server_status` (was a warning line and a list of paths); a live checkpoint shows under `server_status.checkpoints.files` (`checkpoint` → `checkpoints {every_edits, every_secs, pending_edits, writes, files}`); the start-up scan skips a crash's `.sctmp` | WC, DC | `runtime.rs` (`autosave`, `announce`, `find_recovered`, `stamp`) | `contract::sigterm_autosaves_and_the_next_start_names_it`, `contract::checkpoints_survive_a_hard_kill` |
| `server_status` in the shared shape: `documents [{name, title, path, dirty, active, sheets, cells}]` (was `document` with `unsaved_changes`), `current_call` (was `busy`), `memory {rss_bytes, committed_bytes, limit_bytes, measured, source}`, `fonts {source, faces, families, default, arabic}`, `protocol`, `pid`, `backend`, `idle_ms`, `calls`, `busy_replies`, `limits` | WC, DC | `whiq.rs` (`status_value`), `runtime.rs` (`status`) | `contract::oversized_results_are_trimmed` |
| OLD_: `OLD_<name>`, then `OLD_2_<name>` (was `OLD_<stem>-2.<ext>`), and the server's own earlier save is kept too (it was replaced silently: the contract says only `overwrite: true` replaces); reported as `previous_files_renamed: [{path, previous}]` + `notice` + a warning (was `saved[].previous_renamed_to`); recovered files are rewritten in place (they are the server's own) | WC, DC | `crates/engine/src/io.rs` (`save_policy`), `whiq.rs` (`run_tool`), `runtime.rs` | `contract::old_rename`, `authoring::in_place_edits_keep_charts_pivots_slicers_and_unmodelled_parts`, engine `io::tests::keep_old_policy_renames_the_previous_file` |
| Trimmed results continue with `continue_from` on any tool (`contract::trim_from`; on `list_commands` / `list_functions` it is read as `offset`); `max_bytes` per call | WC | `contract.rs`, `whiq.rs` | `contract::byte_budget_trims_any_result`, `contract::tests` |
| `roundtrip_check`: open + save a workbook in memory and report what a save would drop — parts, relationships, content types, binary parts, each XML part's element inventory — with WordCraft's report keys | WC | `crates/mcp/src/roundtrip.rs` (new), `whiq.rs` | `contract::roundtrip_check_reports_what_a_save_would_keep`, `roundtrip::tests`, `smoke.sh` |
| Annotations: `cancel` and `evict` not destructive, idempotent (`cancel` was destructive); `list_jobs`, `roundtrip_check` read-only | — | `whiq.rs` | `contract::annotations` |
| `whiq-release.yml` and `engines.lock.example` (below) | — | `.github/workflows/whiq-release.yml`, `engines.lock.example` | the workflow runs the contract suite and `smoke.sh` on the built binary |

### Fonts: one craft-fonts pin for the three forks

The WHIQ release, the recipe above ("Fonts (round 2)") and `whiq-release.yml` pin craft-fonts
**`43913056ce82a3fdad0ef69f6b88d1dc922a8104`**, the same commit WordCraft and DeckCraft pin now.
The upstream `release.yml` (which says abb8331, for upstream's own signed releases) is not
modified.

### Release workflow

`.github/workflows/whiq-release.yml` (new; the upstream `release.yml` is untouched): on a tag
`whiq-v*` (or Run workflow, with an optional tag to publish) it builds only `gridcraft-cli`, for
`x86_64-unknown-linux-musl` (static: the host's glibc does not matter) on ubuntu-22.04 and
`x86_64-pc-windows-msvc` (static C runtime) on windows-2022, with craft-fonts checked out at the
pin and `CRAFT_FONTS_REQUIRED=1`; runs the contract suite against the built binary on both (`GRIDCRAFT_CLI`), and
`smoke.sh` plus a "statically linked" check on Linux; and publishes a GitHub release with
`gridcraft-cli-<tag>-linux-x86_64.tar.gz`, `gridcraft-cli-<tag>-windows-x86_64.zip` (binary, licences,
the embedded fonts' licences, `CONTRACT-PARITY.md`, `WHIQ-CHANGES.md`, `BUILD.txt`), `SHA256SUMS`
and `engines.lock`. Actions are pinned by commit (checkout, rust-toolchain, rust-cache, as in
upstream CI) or by major (upload/download-artifact); no signing; no secrets (the job's own
`GITHUB_TOKEN` publishes). `engines.lock.example` at the root shows the JSON the platform's build
script reads: a list of `{app, tag, file, sha256}`, one per archive; the release's `engines.lock`
is that file with the real hashes.

Hardened 2026-10-20 (as the P6 clone's workflow, p6-clone de992a2):
- **A published asset is never replaced** (the platform pins each by sha256): `--clobber` is
  gone; a release that already has files makes the publish job fail with a clear error (publish a
  new tag); a release made empty on the Releases page still gets its files.
- **What is built**: a `plan` job resolves the ref. A dispatch naming a tag that already exists
  builds that tag; a new tag builds the dispatched commit, and the release creates the tag there.
  Every checkout uses the resolved ref; the release `--target` and `BUILD.txt` name the checked-out
  commit (`git rev-parse HEAD`), never the dispatched branch head.
- **Reproducible archives**: the Linux `.tar.gz` is `tar --sort=name --mtime=@<commit time>
  --owner=0 --group=0 --numeric-owner | gzip -n`; the Windows `.zip` is written by an inline
  Python `zipfile` script (entries sorted, every date the commit's time in UTC, fixed modes and
  host, deflate level 9) in place of `Compress-Archive`. Checked locally: the same files packed
  twice, with different file times in between, give byte-identical `.zip` and `.tar.gz`.
- **Publish onto a draft** (as WordCraft 228f5e4, after its whiq-v0.7.1 43 MB zip upload reached
  GitHub but its answer did not and gh's retry got 422): the release is created as an empty
  draft, each file is uploaded on its own, after a failed upload the file on the release is
  checked by size and sha256 and kept when it is this file; on a draft only an incomplete file is
  removed and uploaded again (a published release with files still fails the run); the draft is
  published once every file is there, and a draft left by an earlier run is resumed. Checked
  locally against a stub `gh`: a partial file left on a draft is replaced, an upload that
  "failed" after landing is kept, the draft is published; a published release with files fails.


### Test status after the parity pass (Linux x86_64, 2026-10-18, craft-fonts 43913056, `CRAFT_FONTS_REQUIRED=1`)

- `cargo build --release -p gridcraft-cli` succeeds; the contract suite against the release binary
  (`GRIDCRAFT_CLI=target/release/gridcraft-cli cargo test --release -p gridcraft-cli`) passes:
  `mcp_contract` 26/26 (24 before, plus `roundtrip_check` and progress notifications) and the
  CLI's other tests; `smoke.sh` passes on it.
- The crates the pass touched (`gridcraft-core`, `-calc`, `-xlsx`, `-engine`, `-mcp`, `-text`,
  `-pdf`): 720 tests pass, 0 fail, 1 ignored (upstream's); `cargo clippy --no-deps -D warnings`
  and `cargo fmt --check` on `gridcraft-mcp` / `gridcraft-cli` pass.
- musl: `cargo build --release --locked -p gridcraft-cli --target x86_64-unknown-linux-musl` gives a
  `static-pie linked` binary; the contract suite (26/26) and `smoke.sh` pass on it.

## Scatter charts with solid lines (2026-10-20)

The platform's Line-of-Balance chart needs one solid production line per trade. Until now a
`build_workbook` / `insert_chart` scatter wrote every series as markers only (`a:ln` with
`a:noFill`), and a `scatter_lines` series without a colour wrote no `a:ln` at all.

- **Options**, on the chart or on one series (a series' own wins): `line: true|false` (default
  **true** for `scatter` / `xy`: Excel's "Scatter with Straight Lines and Markers"; `false` is the
  old markers-only scatter), `markers: true|false` (default true), `line_width` (points, also
  `lineWidth`; default 2.25, 0.25–20) and `color` (`"#RRGGBB"`; per series as before, on the chart
  it colours every series that has none). `insert_chart` takes the same four (chart-level); the
  MCP tool defaults `line` to true for `type: scatter` (subtype `lines` stays lines without
  markers). The engine's `insert.chart` without these params is unchanged (the desktop app's
  Insert › Scatter is still markers only). Documented in the `build_workbook` and `insert_chart`
  tool schemas and `insert.chart`'s params.
- **Model**: `Series::{line, markers, line_width}` (`Option`, serde default; `None` = what the
  chart kind draws, so every existing chart and file is written exactly as before).
  `engine::cmd::insert::apply_scatter_lines` resolves the options; a styled series without a
  colour gets the renderer's palette colour of its index, so the file and the rendered PNG agree.
- **XLSX** (`crates/xlsx/src/chart.rs`): a styled scatter series writes `<c:scatterStyle
  val="lineMarker"/>`, `<c:spPr><a:ln w=… cap="rnd"><a:solidFill>…</a:solidFill><a:round/></a:ln></c:spPr>`
  (or `a:noFill` when `line: false`), and `<c:marker><c:symbol val="none"/></c:marker>` (markers
  off) or a circle marker in the series colour. Reading keeps width, markers and line where they
  differ from the group's kind. Excel and LibreOffice both draw the line from `a:ln`. Other chart
  types are untouched.
- **Render** (`crates/chart`): the scatter renderer and legend swatches follow a series' `line`,
  `markers` and `line_width`.
- Tests: xlsx `scatter_lines::*` (solid `a:ln` per series, widths, `symbol none`, palette
  fallback, read-back; unstyled and markers-only scatters keep their XML), engine
  `build::tests::scatter_charts_draw_lines_unless_told_otherwise` (defaults, chart- and
  series-level options, `line: false`, other kinds untouched, saved XML, read-back, a bad
  `line_width` names the argument), contract `authoring::scatter_charts_are_solid_lines_per_series`
  (build_workbook and insert_chart over MCP stdio), and the chart crate's fuzz test with random
  `line` / `markers` / `line_width`. `platform_chart_specs_are_understood` now expects the
  platform's `kind: scatter` to come out as `ScatterLines`. Reading a `lineMarker` series with a
  solid `a:ln` and `c:symbol none` keeps `markers: false`, so GridCraft's own render of a saved
  file draws no markers either.
- Also: `tests/mcp-contract/harness.rs` `plain_path` collapsed to a let-chain (clippy
  `collapsible_if` failed `-D warnings` on the previous commit) and `tests/mcp-contract/render.rs`
  `cargo fmt`-ed.
- Status (Linux x86_64, 2026-10-20, craft-fonts 43913056, `CRAFT_FONTS_REQUIRED=1`): every default
  member's tests pass, 1 691 passed, 0 failed, 1 ignored (run one package / test target at a time
  on a small disk), `cargo clippy --workspace --all-targets -D warnings`, `cargo fmt --check`,
  `cargo xtask layers / assets / wasm`, and `tests/mcp-contract/smoke.sh` on the release binary
  (35/35). Proof: `build_workbook` over MCP stdio with a two-series scatter writes one solid
  round-capped `a:ln` per series (`w="28575"` and `w="38100"`, `symbol none` on the second);
  LibreOffice 24 (PDF export) and `gridcraft-cli render` both draw the two lines.

## Formatted workbooks in one call, literal text, document handles (2026-10-20)

What the platform worked around (`shared/chat-engine/server/engines/officeRender.ts`:
`postFormat`, the `'` prefix, `dateFormats`, `serial()` per chat; `scheduleWorkbook.ts`: Gantt and
Timeline bars as codes painted by conditional formats in ink the colour of the fill), now in the
engine. Without the new arguments everything behaves as before.

- **Cell looks in `build_workbook`** (`crates/engine/src/cmd/looks.rs`, `build.rs`): per sheet
  `styles: [{range, …look}]` (`range` a cell, a range or a comma list; whole rows / columns are
  refused) and per cell (a cell object). Look keys, any spelling (`font_color`, `fontColor`):
  `bold`, `italic`, `underline`, `strike`, `font`, `font_size`, `font_color`, `fill` (`"#RRGGBB"`,
  `"none"`, `{color, pattern}`), `align` (left / center / right / justify / general / fill /
  center_across / distributed), `v_align` (top / middle / bottom…), `wrap`, `indent`, `rotation`,
  `number_format`, `borders` (a preset `"all"`, `"outside"`, `"bottom"`, `"thickOutside"`…, or
  `{preset, style, color}` run as Home › Borders, or per side `{top, bottom, left, right,
  diagonal_down, diagonal_up: "thin" | {style, color}}`). Applied in one edit per sheet (one
  interned style per distinct look); a later look wins where they overlap, a cell's own over a
  range's; unknown keys are an error naming the accepted ones.
- **`row_heights`** in points (`[{row: 3 | "3:5", height: 20}]` or `{"3": 20}`, 0–409),
  **`freeze`** (`"C2"`: rows above and columns left; or `{rows, columns}`; wins over
  `freeze_header`); column widths were already there (`columns[].width`, `column_widths`).
- **Cell objects** in `rows` / `header`: `{text}`, `{formula}`, `{date}` (ISO, or a serial),
  `{number}`, `{bool}`, or `{value, type: text|number|date|formula|boolean|auto}`, each with
  look keys beside the value or in `style`. `{"value": "", "fill": "#3B82F6"}` is a Gantt bar: a
  real fill, no code in the cell.
- **`literal_strings: true`** (top level for every sheet, or per sheet): a string is stored as
  the text it is (`"2.10"`, `"1,234"`, `"=x"`, `"+5"`, `"50%"`; no apostrophe needed), except an
  ISO date (`2026-04-03`) or date-time (`2026-04-03T08:30[:ss][.sss][Z]`), which is a date shown
  `yyyy-mm-dd` / `yyyy-mm-dd hh:mm[:ss]` (formats given by the spec still win);
  `iso_dates: false` keeps those as text too. Without the flag strings stay typed input.
- **Document handles** (`crates/mcp/src/backend.rs`, `headless.rs`, `whiq.rs`):
  `open_workbook`, `new_workbook` and `build_workbook` answer with `document: "doc-<uid>"`; every
  document tool takes `document` (the handle or its number) and acts on that workbook, after which
  the workbook active before comes back, so two exports in one process never touch each other's
  document. `server_status.documents` lists every open workbook with its `document` handle (the
  active one first, with sheets and cells). New tool **`close_workbook {document?, discard?}`**
  (refuses unsaved changes without `discard: true`). An unknown handle is `not_found` with
  `open_documents` and `closest`; with `--connect` (a running app) a handle is `not_available`.
  **Every open workbook's unsaved work is kept**: the document snapshot published after each call
  holds every open workbook (`DocSnapshot::others`), and eviction, SIGTERM / SIGINT / SIGHUP, the
  client hanging up and a call that panics auto-save each dirty workbook to its own
  `<name>.recovered-<stamp>.xlsx` (the `evicted` error and `evict` answer list them all in
  `recovered_paths`; `recovered_path` stays the active one's). Periodic checkpoints are per
  document too (`server_status.checkpoints.files[]` with `document` and `handle`), and a workbook
  first seen with unsaved work (just built) is checkpointed without waiting for another edit.
  Test: contract `every_dirty_workbook_is_recovered` (two dirty workbooks → two recovered files on
  SIGTERM, on stdin EOF, on `evict`, and after `kill -9` from their checkpoints; a saved one is
  not copied).
- **`--automation-no-old-copies`** (the name WordCraft and DeckCraft take): saves and exports
  replace files in place, no `OLD_` copies (the supervisor keeps backups);
  `server_status.limits.old_copies` says which (true by default).
- **`_meta["whiq/files"]`** on the tools that read a file or open the document, as in the other
  forks: `open_workbook` `{read: ["path"], opens: true}`, `new_workbook` / `build_workbook`
  `{read: [], opens: true}`, `roundtrip_check` `{read: ["path"], opens: false}`.
- Errors: already one shape everywhere (`isError` with `structuredContent.error {code, reason,
  next_step, …}`, `retry_after_ms` on `busy` / `locked`, the same object as JSON-RPC `error.data`);
  now asserted for a `tools/call` without a name and for unknown handles too.
- Tests: engine `build::tests::styles_row_heights_and_freeze_in_one_call` (looks by range and
  per cell, Gantt fills, borders by side and preset, heights in points, freeze, the saved XML and
  read-back), `build::tests::literal_strings_and_iso_dates` (text kept, dates and date-times,
  leap day, invalid dates stay text, `iso_dates: false`, typed input unchanged, errors naming the
  argument); contract `authoring::build_workbook_formats_in_one_call` (one MCP call, styles.xml,
  `ht`, pane, text and date cells, `check`), `contract::document_handles_keep_two_exports_apart`,
  `contract::old_copies_can_be_turned_off_for_a_supervisor`, and `contract::annotations` /
  `errors_carry_code_reason_and_next_step` extended (`_meta`, `document` on document tools,
  protocol errors).

## Tests

- `cargo test` runs everything; the WHIQ-specific suites:
  - `cargo test -p gridcraft-cli --test mcp_contract` — **the MCP contract suite**
    (`tests/mcp-contract/`, registered as a test of `gridcraft-cli` so it drives that binary over
    stdio). `contract.rs` proves sections 1–8 of the shared contract (error shape, busy, jobs,
    large-open job, oversized results and byte budget, idle and memory eviction with recovery,
    SIGTERM auto-save named by the next process, panic, hang-up, OLD_ rename, confinement with
    symlink escapes, annotations, forgiving inputs, caller identity); `render.rs` the Arabic PNG and
    PDF checks and page setup; `authoring.rs` build_workbook, recalculate, in-place edits and the
    PivotTable + slicer fixture (open → edit → save → reopen → `check`); round 2 adds `cancel`
    and checkpoint tests to `contract.rs`. Set
    `GRIDCRAFT_CLI=target/release/gridcraft-cli` to test a release build. The tests that read PDF
    text use poppler's `pdftotext` when it is installed (and are skipped otherwise).
  - unit tests in `gridcraft-text`, `gridcraft-raster`, `gridcraft-pdf`, `gridcraft-mcp`
    (`contract.rs`, `runtime.rs`, `tests.rs`), engine (`file_access`, `io`, `print`, `build`).
- **Smoke test**: `cargo build --release -p gridcraft-cli && tests/mcp-contract/smoke.sh`
  (or `tests/mcp-contract/smoke.sh path/to/gridcraft-cli`): one JSON-RPC request at a time, the
  way the platform's client talks — initialize, tools/list, server_status, build_workbook (Arabic
  RTL sheet, table, chart, formulas), read_range, render_sheet, export_pdf with page setup, a save
  over an existing file (OLD_), a refused path outside the root, forgiving names, recalculate,
  `cancel`, and the PivotTable + slicer fixture opened, listed and saved — and checks the files,
  that craft-fonts are embedded and memory is measured, every PDF font embedded as a subset
  (Carlito and Noto Sans Arabic, `pdffonts`), the extracted Arabic text (`pdftotext`), both slicer
  caches in the saved file and `gridcraft-cli check` on it. It prints the folder with every output
  and exits non-zero on a failure.
- Run the tests with craft-fonts and `CRAFT_FONTS_REQUIRED=1` (see "Fonts"): font-dependent tests
  then fail rather than skip. Every test executable links the ~60 MB of fonts; on a small disk run
  `cargo test -p <crate>` one crate (or `--test` target) at a time.
- Round 2 status (2026-10-18, craft-fonts at the pinned commit, `CRAFT_FONTS_REQUIRED=1`): every
  default member's tests pass — 1 683 passed, 0 failed, 1 ignored (upstream's) — plus `cargo clippy
  --all-targets -D warnings`, `cargo fmt --check`, `cargo xtask layers / assets / wasm`, the
  contract suite against the release binary (24/24) and the smoke script on the release binary.

## Not done / known limits

- Scripts with no craft-fonts face (Hebrew, Devanagari, Korean Hangul…) have no glyphs in
  exports; Arabic, Latin, Greek and Cyrillic (Carlito, Liberation), Japanese and Chinese do.
  Arabic script has no italic design anywhere, so italic Arabic is slanted; CJK has no bold or
  italic face in craft-fonts, so bold CJK is the regular weight and italic CJK is slanted.
  System fonts are not looked up (every export looks the same on every machine).
- Upstream's docs (`docs/*.md`, ROADMAP) are not edited by the fork; this file is the record.
