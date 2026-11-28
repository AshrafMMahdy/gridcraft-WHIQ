//! The shared contract (sections 1–8).

use std::time::{Duration, Instant};

use serde_json::{Value, json};

use crate::harness::{Mcp, cells_of, files_in, folder};

fn rooted(dir: &std::path::Path, extra: &[&str]) -> Vec<String> {
    let d = dir.to_str().unwrap().to_string();
    let mut v = vec!["--automation-read-root".to_string(), d.clone(), "--automation-write-root".to_string(), d];
    v.extend(extra.iter().map(|s| s.to_string()));
    v
}

fn start(dir: &std::path::Path, extra: &[&str]) -> Mcp {
    let args = rooted(dir, extra);
    let refs: Vec<&str> = args.iter().map(String::as_str).collect();
    Mcp::start(&refs)
}

// 1. Every error carries a code, a reason and a next step.
#[test]
fn errors_carry_code_reason_and_next_step() {
    let dir = folder("errors");
    std::fs::write(dir.join("budget-2026.xlsx"), b"not really").unwrap();
    let mut m = start(&dir, &[]);
    let e = m.err("open_workbook", json!({"path": "budget-2025.xlsx"}));
    assert_eq!(e["code"], "not_found", "{e}");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c.as_str().unwrap().ends_with("budget-2026.xlsx")), "closest matches: {e}");
    let e = m.err("set_cell", json!({"cell": "A1"}));
    assert_eq!(e["code"], "invalid_argument", "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("input"), "{e}");
    let e = m.err("execute_command", json!({"command": "home.bolt", "params": {"range": "A1"}}));
    assert_eq!(e["code"], "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c == "home.bold"), "{e}");
    let e = m.err("read_range", json!({"range": "Sheet9!A1:B2"}));
    assert_eq!(e["code"], "not_found");
    assert_eq!(e["sheets"], json!(["Sheet1"]));
    // An unknown tool is an answer the agent sees, naming the closest tools (as in every craft app).
    let e = m.err("reed_range", json!({}));
    assert_eq!(e["code"], "not_found");
    assert!(e["closest"].as_array().unwrap().iter().any(|c| c == "read_range"), "{e}");
    // The text block: `[code] reason` and the next step.
    let r = m.call("reed_range", json!({}));
    let text = r["content"][0]["text"].as_str().unwrap();
    assert!(text.starts_with("[not_found] ") && text.contains("\nnext_step: "), "{text}");
    // A protocol error carries the same shape in `data`.
    let v = m.request("nope/nope", json!({}));
    assert!(v["error"]["data"]["next_step"].is_string(), "{v}");
    assert!(v["error"]["data"]["code"].is_string() && v["error"]["data"]["reason"].is_string(), "{v}");
    // So does a tools/call without a tool name: code, reason, next step.
    let v = m.request("tools/call", json!({"arguments": {}}));
    let e = if v["error"].is_object() { v["error"]["data"].clone() } else { v["result"]["structuredContent"]["error"].clone() };
    assert!(e["code"].is_string() && e["reason"].is_string() && e["next_step"].is_string(), "{v}");
    // An unknown document handle names the open ones.
    let e = m.err("read_range", json!({"range": "A1", "document": "doc-999999"}));
    assert_eq!(e["code"], "not_found", "{e}");
    assert!(e["open_documents"].as_array().is_some_and(|d| !d.is_empty()), "{e}");
}

// WHIQ: a supervisor that keeps the backups itself turns the OLD_ copies off.
#[test]
fn old_copies_can_be_turned_off_for_a_supervisor() {
    let dir = folder("no-old");
    std::fs::write(dir.join("report.xlsx"), b"the customer's file").unwrap();
    let mut m = start(&dir, &["--automation-no-old-copies"]);
    assert_eq!(m.ok("server_status", json!({}))["limits"]["old_copies"], false);
    m.ok("set_cell", json!({"cell": "A1", "input": "one"}));
    for path in ["report.xlsx", "report.xlsx", "report.pdf", "report.pdf"] {
        let v = m.ok(if path.ends_with(".pdf") { "export_pdf" } else { "save_workbook" }, json!({"path": path}));
        assert!(v.get("previous_files_renamed").is_none() && v.get("notice").is_none(), "{v}");
    }
    m.ok("build_workbook", json!({"path": "report.xlsx", "sheets": [{"name": "S", "rows": [["two"]]}]}));
    assert_eq!(files_in(&dir), ["report.pdf", "report.xlsx"], "replaced in place, no OLD_ copies");
    assert_eq!(cells_of(&dir.join("report.xlsx"), "A1"), json!([["two"]]));
    drop(m);
    let mut m = start(&dir, &[]);
    assert_eq!(m.ok("server_status", json!({}))["limits"]["old_copies"], true, "on by default");
}

// WHIQ: build / open / new return a document handle; calls name the workbook they act on, so two
// exports in one process never touch each other's document.
#[test]
fn document_handles_keep_two_exports_apart() {
    let dir = folder("handles");
    let mut m = start(&dir, &[]);
    let a = m.ok("build_workbook", json!({"sheets": [{"name": "A", "rows": [["first"]]}]}))["document"].as_str().unwrap().to_string();
    let b = m.ok("build_workbook", json!({"sheets": [{"name": "B", "rows": [["second"]]}]}))["document"].as_str().unwrap().to_string();
    assert!(a.starts_with("doc-") && a != b, "{a} {b}");
    // The second build is active; the first is still reached by its handle.
    m.ok("set_cell", json!({"cell": "A2", "input": "for A", "document": a}));
    m.ok("format_range", json!({"range": "A1", "bold": true, "document": a}));
    assert_eq!(m.ok("read_range", json!({"range": "A1:A2"}))["values"], json!([["second"], [null]]), "B untouched");
    let v = m.ok("save_workbook", json!({"path": "a.xlsx", "document": a}));
    assert!(v["path"].as_str().unwrap().ends_with("a.xlsx"), "{v}");
    m.ok("save_workbook", json!({"path": "b.xlsx"}));
    assert_eq!(cells_of(&dir.join("a.xlsx"), "A1:A2"), json!([["first"], ["for A"]]));
    assert_eq!(cells_of(&dir.join("b.xlsx"), "A1:A2"), json!([["second"], [null]]));
    let st = m.ok("server_status", json!({}));
    let docs: Vec<&str> = st["documents"].as_array().unwrap().iter().filter_map(|d| d["document"].as_str()).collect();
    assert!(docs.contains(&a.as_str()) && docs.contains(&b.as_str()), "{st}");
    assert_eq!(st["documents"][0]["document"], b, "the active one first: {st}");
    let o = m.ok("open_workbook", json!({"path": "a.xlsx"}));
    assert_eq!(o["document"], a, "already open: the same handle");
    // Closing frees one workbook; its handle is gone, the other stays.
    m.ok("set_cell", json!({"cell": "B1", "input": "unsaved", "document": b}));
    let e = m.err("close_workbook", json!({"document": b}));
    assert!(e["reason"].as_str().unwrap().contains("unsaved"), "{e}");
    assert_eq!(m.ok("close_workbook", json!({"document": b, "discard": true}))["closed"], b);
    assert_eq!(m.err("read_range", json!({"range": "A1", "document": b}))["code"], "not_found");
    assert_eq!(m.ok("read_range", json!({"range": "A1", "document": a}))["values"], json!([["first"]]));
}

// 2. Nothing hangs: busy with a retry time, long work as jobs, results that survive timeouts.
#[test]
fn busy_and_jobs() {
    let dir = folder("busy");
    let mut m = start(&dir, &["--job-after-ms", "700"]);
    // A slow call runs past the call timeout: it answers with its job id and carries on.
    let slow = m.begin("tools/call", json!({"name": "debug_sleep", "arguments": {"ms": 2500}}));
    std::thread::sleep(Duration::from_millis(100));
    // Meanwhile: another document call is busy (with a retry time), status answers at once.
    let e = m.err("set_cell", json!({"cell": "A1", "input": "1"}));
    assert_eq!(e["code"], "busy", "{e}");
    assert!(e["retry_after_ms"].as_u64().unwrap() > 0);
    let job = e["job_id"].as_str().unwrap().to_string();
    let st = m.ok("server_status", json!({}));
    assert_eq!(st["current_call"]["tool"], "debug_sleep", "{st}");
    let r = m.wait(&slow);
    let hand = &r["result"]["structuredContent"];
    assert_eq!(hand["status"], "running", "{r}");
    assert_eq!(hand["job_id"], job);
    let running = m.ok("job_status", json!({"job_id": job}));
    assert_eq!(running["status"], "running");
    for k in ["job_id", "tool", "elapsed_ms", "progress", "stage", "steps", "stages", "retry_after_ms", "next_step"] {
        assert!(running.get(k).is_some(), "job_status lacks {k}: {running}");
    }
    let done = m.finish_job(&job, 30);
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["result"]["slept_ms"], 2500);
    assert_eq!(done["progress"], 1.0, "{done}");
    // The result stays available.
    assert_eq!(m.ok("job_status", json!({"job_id": job}))["result"]["slept_ms"], 2500);
    // async: true runs anything as a job from the start.
    let h = m.ok("write_range", json!({"range": "A1", "values": [[1, 2], [3, 4]], "async": true}));
    let job = h["job_id"].as_str().unwrap().to_string();
    let done = m.finish_job(&job, 30);
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(m.ok("read_range", json!({"range": "A1:B2"}))["values"], json!([[1.0, 2.0], [3.0, 4.0]]));
    let e = m.err("job_status", json!({"job_id": "job-999"}));
    assert_eq!(e["code"], "not_found");
    // Every job: list_jobs, and job_status without an id.
    let all = m.ok("list_jobs", json!({}));
    assert_eq!(all["jobs"].as_array().unwrap().len(), 2, "{all}");
    assert_eq!(m.ok("job_status", json!({}))["jobs"], all["jobs"]);
}

// Opening a big file is a job from the start.
#[test]
fn large_open_is_a_job() {
    let dir = folder("bigopen");
    let csv: String = (0..30_000).map(|i| format!("{i},row {i},{}\n", i * 2)).collect();
    std::fs::write(dir.join("big.csv"), csv).unwrap();
    let mut m = start(&dir, &["--open-job-mb", "0.1"]);
    let h = m.ok("open_workbook", json!({"path": "big.csv"}));
    let job = h["job_id"].as_str().expect("a job id").to_string();
    let done = m.finish_job(&job, 60);
    assert_eq!(done["status"], "done", "{done}");
    // Stages and progress, the shape every craft app reports.
    assert!(!done["stages"].as_array().unwrap().is_empty(), "{done}");
    assert_eq!(done["progress"], 1.0, "{done}");
    assert_eq!(m.ok("read_range", json!({"range": "B30000"}))["values"], json!([["row 29999"]]));
}

// 4. Oversized results are trimmed with a continue marker; server_status reports the state.
#[test]
fn oversized_results_are_trimmed() {
    let dir = folder("oversized");
    let mut m = start(&dir, &["--max-read-cells", "500"]);
    let rows: Vec<Value> = (0..120).map(|r| Value::Array((0..10).map(|c| json!(r * 10 + c)).collect())).collect();
    m.ok("write_range", json!({"range": "A1", "values": rows}));
    let mut got: Vec<Value> = vec![];
    let mut next: Option<String> = None;
    for _ in 0..10 {
        let args = match &next {
            Some(r) => json!({"range": r}),
            None => json!({}),
        };
        let v = m.ok("read_range", args);
        got.extend(v["values"].as_array().unwrap().iter().cloned());
        if v["truncated"] == true {
            assert!(v.get("continue_from").is_none(), "{v}");
            assert!(v["next_step"].as_str().unwrap().contains("read_range"));
            next = Some(v["next_range"].as_str().unwrap().to_string());
        } else {
            break;
        }
    }
    assert_eq!(got.len(), 120, "all rows reached through next_range");
    assert_eq!(got[119][9], json!(1199.0));
    // Lists page with offset / limit and say where to continue.
    let p = m.ok("list_functions", json!({"limit": 5}));
    assert_eq!(p["items"].as_array().unwrap().len(), 5);
    assert_eq!(p["truncated"], true);
    assert_eq!(p["continue_from"], 5);
    let st = m.ok("server_status", json!({}));
    for k in [
        "server",
        "version",
        "protocol",
        "pid",
        "uptime_ms",
        "author",
        "memory",
        "fonts",
        "roots",
        "documents",
        "current_call",
        "jobs",
        "recovered_files",
        "evicted",
        "checkpoints",
        "limits",
    ] {
        assert!(st.get(k).is_some(), "server_status has {k}: {st}");
    }
    for k in ["rss_bytes", "committed_bytes", "limit_bytes", "measured", "source"] {
        assert!(st["memory"].get(k).is_some(), "memory has {k}: {st}");
    }
    assert_eq!(st["documents"][0]["cells"], 1200);
    assert_eq!(st["roots"]["write"], dir.to_str().unwrap());
}

// A result too big in bytes (whatever the tool) is cut to fit.
#[test]
fn answer_size_rule_pages_reads_and_saves_the_rest() {
    // WHIQ (item 26): the platform's answer-size rule, 131,072 characters. A read-only tool pages a long answer by size
    // (continue_from); a tool that changes the workbook has its answer cut by structure, the whole answer saved in the write
    // root, and no continue_from (calling it again would repeat its changes).
    let dir = folder("bytes");
    let mut m = start(&dir, &[]);
    // the whole command catalogue fits the rule: it comes whole; a part is read with offset / limit (continue_from too)
    let v = m.ok("list_commands", json!({}));
    assert!(v.get("truncated").is_none_or(|t| t == false) && v.to_string().len() <= 131_072, "{}", v.to_string().len());
    let w = m.ok("list_commands", json!({"offset": 10, "limit": 3}));
    assert_eq!(w["offset"], 10);
    assert_eq!(w["continue_from"], 13);
    let w = m.ok("list_commands", json!({"continue_from": 13, "limit": 3}));
    assert_eq!(w["offset"], 13);
    // a batch of 4,000 writes: its results list shows 25 (the list rule), the count, and the file that holds every one
    let ops: Vec<Value> = (1..=4000).map(|i| json!({"op": "set", "cell": format!("A{i}"), "input": format!("row {i} of the long batch")})).collect();
    let r = m.ok("batch", json!({"ops": ops}));
    assert_eq!(r["results"].as_array().map(Vec::len), Some(25), "{}", r.to_string().chars().take(300).collect::<String>());
    assert_eq!(r["results_total"], 4000);
    assert!(r.get("continue_from").is_none() && r.get("truncated").is_none());
    assert!(r.to_string().len() <= 131_072);
    let note = r["results_note"].as_str().unwrap();
    assert!(note.starts_with("25 of 4000 shown; all 4000 in "), "{note}");
    let path = std::path::PathBuf::from(note.trim_start_matches("25 of 4000 shown; all 4000 in ").trim_end_matches('.'));
    assert!(path.starts_with(&dir), "in the write root: {}", path.display());
    let whole: Value = serde_json::from_str(&std::fs::read_to_string(&path).unwrap()).unwrap();
    assert_eq!(whole["results"].as_array().map(Vec::len), Some(4000));
    // the size knob and the shared max_bytes are gone; export_markdown's page size is refused outside 1024..131072
    let e = m.err("export_markdown", json!({"max_bytes": 500_000}));
    assert_eq!(e["code"], "invalid_argument", "{e}");
}

// 3. Eviction: the work is auto-saved inside the write root and the next call names it.
#[test]
fn evicted_work_is_recovered() {
    let dir = folder("evict");
    let mut m = start(&dir, &["--idle-evict-secs", "1"]);
    m.ok("write_range", json!({"range": "A1", "values": [["kept", 42]]}));
    std::thread::sleep(Duration::from_millis(2200));
    let e = m.err("read_range", json!({"range": "A1:B1"}));
    assert_eq!(e["code"], "evicted", "{e}");
    let recovered = e["recovered_path"].as_str().unwrap().to_string();
    assert!(recovered.starts_with(dir.to_str().unwrap()), "inside the write root: {recovered}");
    assert!(recovered.contains(".recovered-") && recovered.ends_with(".xlsx"));
    assert!(e["next_step"].as_str().unwrap().contains(&recovered));
    assert!(m.ok("server_status", json!({}))["evicted"][0]["recovered_path"].is_string());
    m.ok("open_workbook", json!({"path": recovered}));
    assert_eq!(m.ok("read_range", json!({"range": "A1:B1"}))["values"], json!([["kept", 42.0]]));
    // `evict` (for a supervisor) does the same on demand.
    m.ok("set_cell", json!({"cell": "C1", "input": "more"}));
    let r = m.ok("evict", json!({}));
    let rec = r["evicted"][0]["recovered_path"].as_str().unwrap().to_string();
    assert!(std::path::Path::new(&rec).exists(), "{r}");
    assert_eq!(m.err("get_cell", json!({"cell": "C1"}))["code"], "evicted");
}

/// WHIQ (item 26, fault A21): an auto-save that fails is said — the work is lost — and a save after it is refused, never a copy
/// of the unchanged original reported as saved.
#[test]
fn a_failed_auto_save_says_the_work_is_lost_and_refuses_the_save() {
    let dir = folder("evict-lost");
    let mut m = start(&dir, &["--idle-evict-secs", "1"]);
    m.ok("write_range", json!({"range": "A1", "values": [["not saved anywhere"]]}));
    // the write root is no longer a folder: the auto-save cannot be written
    std::fs::remove_dir_all(&dir).unwrap();
    std::fs::write(&dir, b"a file where the folder was").unwrap();
    std::thread::sleep(Duration::from_millis(2200));
    let e = m.err("read_range", json!({"range": "A1"}));
    assert_eq!(e["code"], "evicted", "{e}");
    let reason = e["reason"].as_str().unwrap();
    assert!(reason.contains("could NOT be auto-saved") && reason.contains("are lost") && !reason.contains("no unsaved changes"), "{e}");
    assert!(e.get("recovered_path").is_none(), "{e}");
    assert!(e["next_step"].as_str().unwrap().starts_with("tell the user"), "{e}");
    let out = std::env::temp_dir().join(format!("gc-lost-{}.xlsx", std::process::id()));
    let s = m.err("save_workbook", json!({"path": out.to_str().unwrap()}));
    assert_eq!(s["code"], "evicted", "{s}");
    assert!(s["reason"].as_str().unwrap().starts_with("not saved: the server closed this workbook"), "{s}");
    assert!(!out.exists());
    std::fs::remove_file(&dir).ok();
}

/// WHIQ (item 26, fault A31): a password-protected workbook opens with open_workbook's `password`, the next step its refusal names.
#[test]
fn a_password_protected_workbook_opens_with_its_password() {
    let dir = folder("password");
    let mut s = gridcraft_engine::Session::new();
    s.new_workbook();
    s.run("cell.set", json!({"cell": "A1", "input": "42"})).unwrap();
    let xlsx = gridcraft_engine::io::save_bytes(&s.active().unwrap().wb, "book.xlsx").unwrap();
    let path = dir.join("Secret.xlsx");
    std::fs::write(&path, gridcraft_engine::xlsx::testing::encrypt_workbook(&xlsx, "synthetic-pw-8842")).unwrap();
    let p = path.to_str().unwrap();
    let mut m = start(&dir, &[]);
    let e = m.err("open_workbook", json!({"path": p}));
    assert_eq!(e["code"], "password_required", "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("`password`"), "{e}");
    assert_eq!(m.err("open_workbook", json!({"path": p, "password": "wrong"}))["code"], "wrong_password");
    m.ok("open_workbook", json!({"path": p, "password": "synthetic-pw-8842"}));
    assert_eq!(m.ok("read_range", json!({"range": "A1"}))["values"], json!([[42.0]]));
}

/// WHIQ (item 26, faults B50, B15, B17, A28, B54): the arguments a tool reads say what is done, or are refused in words.
#[test]
fn item26_arguments_do_what_they_say_or_are_refused() {
    let dir = folder("item26-args");
    let mut m = start(&dir, &[]);
    m.ok("build_workbook", json!({"sheets": [{"name": "Main", "rows": [["m1"], ["m2"], ["m3"]]}, {"name": "Data", "rows": [["d1"], ["d2"], ["d3"]]}]}));
    // B50: a sheet prefix in rows names the sheet (it deleted rows of the active sheet)
    m.ok("delete_rows", json!({"rows": "Data!2:2"}));
    assert_eq!(m.ok("read_range", json!({"range": "Data!A1:A2"}))["values"], json!([["d1"], ["d3"]]));
    assert_eq!(m.ok("read_range", json!({"range": "Main!A1:A3"}))["values"], json!([["m1"], ["m2"], ["m3"]]));
    let e = m.err("delete_rows", json!({"rows": "Data!1:1", "sheet": "Main"}));
    assert!(e["reason"].as_str().unwrap().contains("give one sheet"), "{e}");
    // B15: header left out is true, as the tool says (a first row holding one number was guessed headerless: data pushed down)
    m.ok("write_range", json!({"range": "Main!C1", "values": [["Name", 2026], ["a", 1], ["b", 2]]}));
    m.ok("create_table", json!({"range": "Main!C1:D3"}));
    // (a table's header cells are text, as in Excel; the data stays where it was)
    assert_eq!(m.ok("read_range", json!({"range": "Main!C1:D2"}))["values"], json!([["Name", "2026"], ["a", 1.0]]));
    // B17: a sort key that names no column, or an order that is not asc / desc, is refused (it sorted nothing and answered ok)
    let e = m.err("sort_range", json!({"range": "Main!A1:A3", "keys": ["Total"]}));
    assert!(e["reason"].as_str().unwrap().contains("is not a column letter"), "{e}");
    let e = m.err("sort_range", json!({"range": "Main!A1:A3", "keys": ["A xyz"]}));
    assert!(e["reason"].as_str().unwrap().contains("must be asc or desc"), "{e}");
    m.ok("sort_range", json!({"range": "Main!A1:A3", "keys": ["A desc"], "header": false}));
    assert_eq!(m.ok("read_range", json!({"range": "Main!A1:A3"}))["values"], json!([["m3"], ["m2"], ["m1"]]));
    // A28: the schema's bounds are checked (a negative count was read as absent, a font size past 409 clamped)
    let e = m.err("format_range", json!({"range": "Main!A1", "fontSize": 500}));
    assert!(e["reason"].as_str().unwrap().contains("must be 409 or less"), "{e}");
    let e = m.err("sort_range", json!({"range": "Main!A1:A3", "keys": []}));
    assert!(e["reason"].as_str().unwrap().contains("at least 1 item"), "{e}");
    // B54: through execute_command a parameter of the wrong type, or one the command never reads, is refused and nothing changes
    let e = m.err("execute_command", json!({"command": "home.bold", "params": {"range": "Main!A1", "on": "yes"}}));
    assert!(e["reason"].as_str().unwrap().contains("`on` must be true or false"), "{e}");
    let e = m.err("execute_command", json!({"command": "cell.set", "params": {"cell": "Main!B1", "input": "x", "colour": "red"}}));
    assert!(e["reason"].as_str().unwrap().contains("`colour` is not read by `cell.set`"), "{e}");
    assert_eq!(m.ok("read_range", json!({"range": "Main!B1"}))["values"], json!([[null]]));
    m.ok("execute_command", json!({"command": "cell.set", "params": {"cell": "Main!B1", "input": "x"}}));
    // B52: a delete names what it deletes (it deleted the active sheet, or the last chart)
    assert!(m.err("delete_sheet", json!({}))["reason"].as_str().unwrap().contains("`sheet`"));
    // B10: the chart's place and size are in the answer (Excel's defaults when not given)
    m.ok("write_range", json!({"range": "Main!F1", "values": [["a", 1], ["b", 2]]}));
    let c = m.ok("insert_chart", json!({"range": "Main!F1:G2", "type": "column"}));
    assert!(c["at"].is_string() && c["width"].as_f64() == Some(480.0) && c["height"].as_f64() == Some(288.0), "{c}");
    assert!(m.err("delete_chart", json!({"sheet": "Main"}))["reason"].as_str().unwrap().contains("say which chart"));
}

/// WHIQ (item 26, fault A8): a read_range part larger than the answer-size rule is cut to whole rows, and the next range starts
/// right after them: following next_range reads every row once.
#[test]
fn a_size_cut_read_range_names_the_rows_it_holds_and_skips_none() {
    let dir = folder("item26-read");
    let mut m = start(&dir, &[]);
    let rows: Vec<Value> = (1..=300).map(|i| json!([format!("{i:04} {}", "x".repeat(1000))])).collect();
    m.ok("write_range", json!({"range": "A1", "values": rows}));
    let mut seen: Vec<String> = Vec::new();
    let mut range = "A1:A300".to_string();
    for _ in 0..20 {
        let r = m.ok("read_range", json!({"range": range}));
        assert!(r.to_string().len() <= 131_072, "{}", r.to_string().len());
        for row in r["values"].as_array().unwrap() {
            seen.push(row[0].as_str().unwrap()[..4].to_string());
        }
        match r["next_range"].as_str() {
            Some(n) => range = n.to_string(),
            None => break,
        }
    }
    let want: Vec<String> = (1..=300).map(|i| format!("{i:04}")).collect();
    assert_eq!(seen, want);
}

/// Reads `range` the way the answers say — next_range, and after a row cut then_range — and answers every cell read by its
/// address, with the answers that cut a cell.
fn read_all(m: &mut Mcp, range: &str) -> (std::collections::BTreeMap<String, Value>, Vec<Value>) {
    let mut cells = std::collections::BTreeMap::new();
    let mut cut = Vec::new();
    let mut after: Vec<String> = Vec::new();
    let mut next = Some(range.to_string());
    for _ in 0..200 {
        let Some(range) = next.take() else { break };
        let r = m.ok("read_range", json!({"range": range}));
        assert!(r.to_string().len() <= 131_072, "{}", r.to_string().len());
        let part = r["range"].as_str().map(str::to_string).unwrap_or_else(|| range.rsplit('!').next().unwrap().to_string());
        let start = part.split(':').next().unwrap();
        let col0 = start.chars().take_while(char::is_ascii_alphabetic).fold(0u32, |n, c| n * 26 + (c as u32 - 64)) - 1;
        let row0: u32 = start.trim_start_matches(|c: char| c.is_ascii_alphabetic()).parse().unwrap();
        for (i, row) in r["values"].as_array().unwrap().iter().enumerate() {
            for (j, v) in row.as_array().unwrap().iter().enumerate() {
                let (c, n) = (col0 + j as u32, row0 + i as u32);
                let name = if c < 26 { format!("{}{n}", (b'A' + c as u8) as char) } else { format!("A{}{n}", (b'A' + (c - 26) as u8) as char) };
                assert!(cells.insert(name.clone(), v.clone()).is_none(), "{name} read twice");
            }
        }
        if r.get("cell_cut").is_some() {
            cut.push(r["cell_cut"].clone());
        }
        if r["truncated"] == true {
            if let Some(t) = r["then_range"].as_str() {
                after.push(t.to_string());
            }
            next = Some(r["next_range"].as_str().unwrap().to_string());
        } else {
            next = after.pop();
        }
    }
    (cells, cut)
}

/// WHIQ (item 26, fault A8): a row cut — wider than one read, or larger than one answer — names the rest of that row and then
/// the rows after it; following them reads every cell once. A cell larger than one answer comes as the start of its text, said.
#[test]
fn a_cut_row_names_the_rows_after_it_and_a_cut_cell_is_said() {
    let dir = folder("item26-row-cut");
    // wider than one read: 3 rows of 8 cells, 5 cells a read
    let mut m = start(&dir, &["--max-read-cells", "5"]);
    let rows: Vec<Value> = (1..=3).map(|r| Value::Array((0..8).map(|c| json!(format!("r{r}c{c}"))).collect())).collect();
    m.ok("write_range", json!({"range": "A1", "values": rows}));
    let first = m.ok("read_range", json!({"range": "A1:H3"}));
    assert_eq!(first["range"], "A1:E1", "{first}");
    assert_eq!(first["next_range"], "Sheet1!F1:H1");
    assert_eq!(first["then_range"], "Sheet1!A2:H3");
    let (cells, cut) = read_all(&mut m, "A1:H3");
    assert_eq!(cells.len(), 24, "{cells:?}");
    assert_eq!(cells["H3"], "r3c7");
    assert!(cut.is_empty());
    drop(m);
    // larger than one answer: row 1 of three 60,000-character cells, a 200,000-character cell in row 2, a short row 3
    let mut m = start(&dir, &[]);
    let long = |ch: char, n: usize| ch.to_string().repeat(n);
    m.ok("write_range", json!({"range": "A1", "values": [[long('a', 60_000), long('b', 60_000), long('c', 60_000)], [long('d', 200_000), "e", "f"], ["g", "h", "i"]]}));
    let first = m.ok("read_range", json!({"range": "A1:C3"}));
    assert_eq!(first["range"], "A1:B1", "{}", first.to_string().chars().take(300).collect::<String>());
    assert_eq!(first["next_range"], "Sheet1!C1");
    assert_eq!(first["then_range"], "Sheet1!A2:C3");
    let (cells, cut) = read_all(&mut m, "A1:C3");
    assert_eq!(cells.len(), 9, "{:?}", cells.keys().collect::<Vec<_>>());
    assert_eq!(cells["C1"].as_str().map(str::len), Some(60_000));
    assert_eq!((cells["B2"].clone(), cells["C3"].clone()), (json!("e"), json!("i")), "the cells after the cut cell are read");
    assert_eq!(cut.len(), 1);
    assert_eq!(cut[0]["cell"], "Sheet1!A2");
    assert_eq!(cut[0]["characters"], 200_000);
    let shown = cut[0]["shown"].as_u64().unwrap() as usize;
    assert!(shown > 100_000 && cells["A2"].as_str().map(str::len) == Some(shown), "{shown}");
    assert!(cut[0]["note"].as_str().unwrap().contains("export_range writes the whole cell"));
}

#[test]
fn memory_limit_evicts_with_recovery() {
    let dir = folder("memory");
    let mut m = start(&dir, &["--max-memory-mb", "1"]);
    m.ok("set_cell", json!({"cell": "A1", "input": "precious"}));
    let e = m.err("get_cell", json!({"cell": "A1"}));
    assert_eq!(e["code"], "evicted", "{e}");
    assert!(e["reason"].as_str().unwrap().contains("MB"), "{e}");
    let path = std::path::PathBuf::from(e["recovered_path"].as_str().unwrap());
    assert_eq!(cells_of(&path, "A1"), json!([["precious"]]));
}

/// WHIQ (owner rule: a save is never interrupted by eviction): the call after which memory was
/// over the limit says the workbook was closed and where its work is; the save that comes next
/// writes that work where it was asked, byte for byte.
#[test]
fn a_save_after_a_memory_eviction_writes_the_auto_saved_work() {
    let dir = folder("memory-save");
    let mut m = start(&dir, &["--max-memory-mb", "1"]);
    let r = m.ok("set_cell", json!({"cell": "A1", "input": "precious"}));
    let note = &r["closed_after_this_call"];
    let rec = note["recovered_path"].as_str().unwrap_or_else(|| panic!("the answer says the workbook was closed: {r}")).to_string();
    assert!(note["next_step"].as_str().unwrap().contains("save_workbook"), "{r}");
    let out = dir.join("after-eviction.xlsx");
    let s = m.ok("save_workbook", json!({"path": out.to_str().unwrap()}));
    let name = std::path::Path::new(&rec).file_name().unwrap().to_str().unwrap().to_string();
    assert!(s["saved_from"].as_str().unwrap().ends_with(&name), "{s}");
    assert_eq!(std::fs::read(&out).unwrap(), std::fs::read(&rec).unwrap());
    assert_eq!(cells_of(&out, "A1"), json!([["precious"]]));
}

/// WHIQ: a save after an eviction saves the workbook the call NAMES, not whichever was evicted first.
#[test]
fn a_save_after_an_eviction_saves_the_workbook_it_names() {
    let dir = folder("evict-names");
    let mut m = start(&dir, &[]);
    let a = m.ok("build_workbook", json!({"sheets": [{"name": "A", "rows": [["work A"]]}]}))["document"].as_str().unwrap().to_string();
    let b = m.ok("build_workbook", json!({"sheets": [{"name": "B", "rows": [["work B"]]}]}))["document"].as_str().unwrap().to_string();
    m.ok("set_cell", json!({"cell": "B1", "input": "more A", "document": a}));
    m.ok("set_cell", json!({"cell": "B1", "input": "more B", "document": b}));
    m.ok("evict", json!({}));
    let (out_b, out_a) = (dir.join("b-saved.xlsx"), dir.join("a-saved.xlsx"));
    let s = m.ok("save_workbook", json!({"path": out_b.to_str().unwrap(), "document": b}));
    assert!(s["saved_from"].is_string(), "{s}");
    assert_eq!(cells_of(&out_b, "A1:B1"), json!([["work B", "more B"]]));
    m.ok("save_workbook", json!({"path": out_a.to_str().unwrap(), "document": a}));
    assert_eq!(cells_of(&out_a, "A1:B1"), json!([["work A", "more A"]]));
    // A handle the eviction did not close is an error, not another workbook's work.
    let e = m.err("save_workbook", json!({"path": dir.join("c.xlsx").to_str().unwrap(), "document": "doc-999"}));
    assert_eq!(e["code"], "failed", "{e}");
}

/// WHIQ: a whole range in one call, as a CSV file (read_range answers a window per call).
#[test]
fn export_range_writes_a_whole_range_as_csv() {
    let dir = folder("export-range");
    let mut m = start(&dir, &[]);
    m.ok("write_range", json!({"range": "A1", "values": [["a", 1], ["b", 2], ["c", 3]]}));
    let out = dir.join("range.csv");
    let r = m.ok("export_range", json!({"path": out.to_str().unwrap(), "range": "A1:B3"}));
    assert_eq!(r["rows"], 3, "{r}");
    assert_eq!(std::fs::read_to_string(&out).unwrap(), "a,1\r\nb,2\r\nc,3\r\n");
}

// 3. SIGTERM: the open document is auto-saved; the next process names the file.
/// WHIQ (owner rule: never lose user work): every open workbook with unsaved changes is
/// auto-saved, each to its own recovered file, on SIGTERM, on hang-up and on eviction.
#[cfg(unix)]
#[test]
fn every_dirty_workbook_is_recovered() {
    let two = |m: &mut Mcp| {
        let a = m.ok("build_workbook", json!({"sheets": [{"name": "A", "rows": [["work A"]]}]}))["document"].as_str().unwrap().to_string();
        m.ok("build_workbook", json!({"sheets": [{"name": "B", "rows": [["work B"]]}]}));
        m.ok("set_cell", json!({"cell": "B1", "input": "more A", "document": a}));
    };
    let recovered = |dir: &std::path::Path| -> Vec<Value> {
        let mut v: Vec<Value> = files_in(dir).into_iter().filter(|f| f.contains(".recovered-")).map(|f| cells_of(&dir.join(f), "A1:B1")).collect();
        v.sort_by_key(|x| x.to_string());
        v
    };
    let both = json!([[["work A", "more A"]], [["work B", null]]]);
    // SIGTERM: two workbooks, two recovered files.
    let dir = folder("sigterm-two");
    let mut m = start(&dir, &[]);
    two(&mut m);
    let st = m.terminate();
    assert!(!st.success() || st.code() == Some(143), "{st:?}");
    assert_eq!(Value::Array(recovered(&dir)), both, "{:?}", files_in(&dir));
    // The client hangs up (stdin EOF): the same.
    let dir = folder("hangup-two");
    let mut m = start(&dir, &[]);
    two(&mut m);
    m.close();
    assert_eq!(Value::Array(recovered(&dir)), both, "{:?}", files_in(&dir));
    // Eviction names every file it wrote.
    let dir = folder("evict-two");
    let mut m = start(&dir, &[]);
    two(&mut m);
    let r = m.ok("evict", json!({}));
    let paths = r["evicted"][0]["recovered_paths"].as_array().unwrap().clone();
    assert_eq!(paths.len(), 2, "{r}");
    assert_eq!(Value::Array(recovered(&dir)), both);
    let e = m.err("read_range", json!({"range": "A1"}));
    assert_eq!(e["code"], "evicted");
    assert_eq!(e["recovered_paths"].as_array().map(Vec::len), Some(2), "{e}");
    // A saved workbook is not auto-saved: only the dirty one is.
    let dir = folder("hangup-one-saved");
    let mut m = start(&dir, &[]);
    two(&mut m);
    m.ok("save_workbook", json!({"path": "b.xlsx"}));
    m.close();
    assert_eq!(Value::Array(recovered(&dir)), json!([[["work A", "more A"]]]), "{:?}", files_in(&dir));
    // A hard kill (no handler runs): each workbook's checkpoint file holds its work.
    let dir = folder("kill-two");
    let mut m = start(&dir, &["--checkpoint-edits", "1"]);
    two(&mut m);
    let deadline = Instant::now() + Duration::from_secs(10);
    while m.ok("server_status", json!({}))["checkpoints"]["files"].as_array().map_or(0, Vec::len) < 2 {
        assert!(Instant::now() < deadline, "two checkpoint files");
        std::thread::sleep(Duration::from_millis(100));
    }
    m.kill();
    assert_eq!(Value::Array(recovered(&dir)), both, "{:?}", files_in(&dir));
}

#[cfg(unix)]
#[test]
fn sigterm_autosaves_and_the_next_start_names_it() {
    let dir = folder("sigterm");
    let mut m = start(&dir, &[]);
    m.ok("write_range", json!({"range": "A1", "values": [["unsaved", 7]]}));
    let st = m.terminate();
    assert!(!st.success() || st.code() == Some(143), "{st:?}");
    let rec: Vec<String> = files_in(&dir).into_iter().filter(|f| f.contains(".recovered-")).collect();
    assert_eq!(rec.len(), 1, "{:?}", files_in(&dir));
    let path = dir.join(&rec[0]);
    assert_eq!(cells_of(&path, "A1:B1"), json!([["unsaved", 7.0]]));
    let mut m = start(&dir, &[]);
    let v = m.ok("inspect_workbook", json!({}));
    assert!(v["recovered_files"].to_string().contains(&rec[0]), "the next call names the recovered file: {v}");
    assert_eq!(v["recovered_files"][0]["reason"], "found");
    assert!(m.ok("inspect_workbook", json!({})).get("recovered_files").is_none(), "named once");
    assert!(m.ok("server_status", json!({}))["recovered_files"].to_string().contains(&rec[0]));
}

// 3. A panic inside a call loses nothing: an error with a recovery copy, and the server goes on.
#[test]
fn panic_is_an_answer_not_a_crash() {
    let dir = folder("panic");
    let mut m = start(&dir, &[]);
    m.ok("set_cell", json!({"cell": "B2", "input": "=6*7"}));
    let e = m.err("debug_panic", json!({}));
    assert_eq!(e["code"], "internal", "{e}");
    let path = std::path::PathBuf::from(e["recovered_path"].as_str().unwrap());
    assert_eq!(cells_of(&path, "B2"), json!([[42.0]]));
    assert_eq!(m.ok("get_cell", json!({"cell": "B2"}))["value"], 42.0);
}

// WHIQ (owner, 2026-11-05 "Undo the call"): a panic half way through a batch undoes the call:
// the live workbook and the recovered file are both as they were before it.
#[test]
fn panic_half_way_through_a_batch_undoes_the_call() {
    let dir = folder("panic-batch");
    let mut m = start(&dir, &[]);
    m.ok("set_cell", json!({"cell": "A1", "input": "before"}));
    for transactional in [true, false] {
        let e = m.err(
            "batch",
            json!({"ops": [{"op": "set", "cell": "A1", "input": "changed"}, {"op": "write", "range": "B1", "values": [[1, 2]]}, {"tool": "debug_panic"}], "transactional": transactional}),
        );
        assert_eq!(e["code"], "internal", "{e}");
        let path = std::path::PathBuf::from(e["recovered_path"].as_str().unwrap());
        assert_eq!(cells_of(&path, "A1"), json!([["before"]]), "{transactional}");
        assert_eq!(m.ok("get_cell", json!({"cell": "A1"}))["value"], "before", "{transactional}");
        assert!(m.ok("get_cell", json!({"cell": "B1"}))["value"].is_null(), "{transactional}");
    }
    // The next call starts clean: a batch after it is undone on its own failure only.
    m.ok("batch", json!({"ops": [{"op": "set", "cell": "C1", "input": "kept"}]}));
    assert_eq!(m.ok("get_cell", json!({"cell": "C1"}))["value"], "kept");
}

// Hanging up with unsaved changes keeps them.
#[test]
fn hang_up_keeps_unsaved_work() {
    let dir = folder("hangup");
    let mut m = start(&dir, &[]);
    m.ok("set_cell", json!({"cell": "A1", "input": "left behind"}));
    m.close();
    let rec: Vec<String> = files_in(&dir).into_iter().filter(|f| f.contains(".recovered-")).collect();
    assert_eq!(rec.len(), 1, "{:?}", files_in(&dir));
    assert_eq!(cells_of(&dir.join(&rec[0]), "A1"), json!([["left behind"]]));
}

// 6. Saves never overwrite silently: OLD_<name>, or overwrite: true.
#[test]
fn old_rename() {
    let dir = folder("oldrename");
    std::fs::write(dir.join("report.xlsx"), b"the customer's file").unwrap();
    let mut m = start(&dir, &[]);
    m.ok("set_cell", json!({"cell": "A1", "input": "new"}));
    let v = m.ok("save_workbook", json!({"path": "report.xlsx"}));
    assert_eq!(std::fs::read(dir.join("OLD_report.xlsx")).unwrap(), b"the customer's file");
    let old = dir.join("OLD_report.xlsx");
    assert_eq!(v["previous_files_renamed"][0]["previous"], old.to_str().unwrap(), "{v}");
    assert!(v["notice"].as_str().unwrap().contains("OLD_report.xlsx"), "{v}");
    assert!(v["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("OLD_report.xlsx")));
    assert_eq!(cells_of(&dir.join("report.xlsx"), "A1"), json!([["new"]]));
    // Saving again keeps our own earlier save too (OLD_2_), as in every craft app.
    m.ok("set_cell", json!({"cell": "A1", "input": "newer"}));
    let v = m.ok("save_workbook", json!({}));
    assert!(v["previous_files_renamed"][0]["previous"].as_str().unwrap().ends_with("OLD_2_report.xlsx"), "{v}");
    assert_eq!(cells_of(&dir.join("OLD_2_report.xlsx"), "A1"), json!([["new"]]));
    // overwrite: true replaces in place.
    std::fs::write(dir.join("third.xlsx"), b"y").unwrap();
    let v = m.ok("save_workbook", json!({"path": "third.xlsx", "overwrite": true}));
    assert!(v.get("previous_files_renamed").is_none(), "{v}");
    assert!(!dir.join("OLD_third.xlsx").exists());
}

// 6. Paths are confined to the roots; links out are refused; relative paths land inside.
#[test]
fn confinement() {
    let base = folder("confine");
    let chat = base.join("chat");
    let outside = base.join("outside");
    std::fs::create_dir_all(&chat).unwrap();
    std::fs::create_dir_all(&outside).unwrap();
    std::fs::write(outside.join("secret.csv"), "1,2\n").unwrap();
    let mut m = start(&chat, &[]);
    let e = m.err("open_workbook", json!({"path": outside.join("secret.csv").to_str().unwrap()}));
    assert_eq!(e["code"], "outside_root", "{e}");
    assert_eq!(e["read_root"], chat.to_str().unwrap());
    let e = m.err("save_workbook", json!({"path": outside.join("leak.xlsx").to_str().unwrap()}));
    assert_eq!(e["code"], "outside_root");
    assert!(e["next_step"].as_str().unwrap().contains("leak.xlsx"), "{e}");
    let e = m.err("save_workbook", json!({"path": "../outside/leak.xlsx"}));
    assert_eq!(e["code"], "outside_root", "{e}");
    assert!(!outside.join("leak.xlsx").exists());
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(outside.join("secret.csv"), chat.join("link.csv")).unwrap();
        let e = m.err("open_workbook", json!({"path": "link.csv"}));
        assert_eq!(e["code"], "outside_root", "a symlink escape is refused: {e}");
        std::os::unix::fs::symlink(&outside, chat.join("away")).unwrap();
        let e = m.err("save_workbook", json!({"path": "away/x.xlsx"}));
        assert_eq!(e["code"], "outside_root");
    }
    m.ok("set_cell", json!({"cell": "A1", "input": "in"}));
    m.ok("save_workbook", json!({"path": "inside.xlsx"}));
    assert!(chat.join("inside.xlsx").exists());
    let e = m.err("execute_command", json!({"command": "file.exportCsv", "params": {"path": outside.join("x.csv").to_str().unwrap()}}));
    assert_eq!(e["code"], "outside_root");
}

// 8. Annotations say what each tool does; structuredContent on every result.
#[test]
fn annotations() {
    let dir = folder("annotations");
    let mut m = start(&dir, &[]);
    let v = m.request("tools/list", json!({}));
    let tools = v["result"]["tools"].as_array().unwrap();
    let get = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap_or_else(|| panic!("{n}"))["annotations"].clone();
    for t in tools {
        let a = &t["annotations"];
        assert!(
            a["readOnlyHint"].is_boolean()
                && a["destructiveHint"].is_boolean()
                && a["idempotentHint"].is_boolean()
                && a["openWorldHint"].is_boolean(),
            "{t}"
        );
        assert!(t["inputSchema"]["properties"]["caller"].is_object(), "every tool takes caller: {}", t["name"]);
    }
    for r in ["read_range", "get_cell", "inspect_workbook", "server_status", "job_status", "list_jobs", "list_functions", "roundtrip_check"] {
        assert_eq!(get(r)["readOnlyHint"], true, "{r}");
        assert_eq!(get(r)["destructiveHint"], false, "{r}");
    }
    for w in ["save_workbook", "write_range", "set_cell", "execute_command"] {
        assert_eq!(get(w)["readOnlyHint"], false, "{w}");
        assert_eq!(get(w)["destructiveHint"], true, "{w}");
    }
    assert_eq!(get("insert_chart")["destructiveHint"], false);
    for n in ["cancel", "evict"] {
        assert_eq!(
            (get(n)["readOnlyHint"].as_bool(), get(n)["destructiveHint"].as_bool(), get(n)["idempotentHint"].as_bool()),
            (Some(false), Some(false), Some(true)),
            "{n}"
        );
    }
    let save = tools.iter().find(|t| t["name"] == "save_workbook").unwrap();
    assert!(save["inputSchema"]["properties"]["overwrite"].is_object());
    // `_meta["whiq/files"]`: the arguments naming files a call reads, and whether it opens the
    // document (the field WordCraft and DeckCraft carry).
    let files = |n: &str| tools.iter().find(|t| t["name"] == n).unwrap()["_meta"]["whiq/files"].clone();
    assert_eq!(files("open_workbook"), json!({"read": ["path"], "opens": true}));
    assert_eq!(files("new_workbook"), json!({"read": [], "opens": true}));
    assert_eq!(files("build_workbook"), json!({"read": [], "opens": true}));
    assert_eq!(files("roundtrip_check"), json!({"read": ["path"], "opens": false}));
    assert!(files("save_workbook").is_null() && files("export_pdf").is_null(), "a save's path is written, not read");
    for t in tools {
        for k in t["_meta"]["whiq/files"]["read"].as_array().into_iter().flatten() {
            assert!(t["inputSchema"]["properties"][k.as_str().unwrap()].is_object(), "{}: `{k}` is in its schema", t["name"]);
        }
    }
    // Document tools name the workbook they act on.
    for n in ["read_range", "set_cell", "format_range", "save_workbook", "export_pdf", "close_workbook", "execute_command"] {
        let t = tools.iter().find(|t| t["name"] == n).unwrap();
        assert!(t["inputSchema"]["properties"]["document"].is_object(), "{n} takes document");
    }
    assert_eq!(get("close_workbook")["destructiveHint"], true);
    // structuredContent everywhere (the harness asserts it on each call).
    m.ok("inspect_workbook", json!({}));
    m.ok("list_commands", json!({"search": "bold"}));
    m.ok("evaluate_formula", json!({"formula": "1+1"}));
}

// 5. Forgiving inputs.
#[test]
fn forgiving_inputs() {
    let dir = folder("forgiving");
    let mut m = start(&dir, &[]);
    // names in other spellings are read and said; a value of the wrong type or shape is refused in words (WHIQ item 26: a flat
    // list was one row, "false" was false)
    let e = m.err("write_range", json!({"range": "a1", "values": [1, 2, 3]}));
    assert!(e["reason"].as_str().unwrap().contains("is not a list of cells"), "{e}");
    let v = m.ok("WriteRange", json!({"Range": "a1", "data": [[1, 2, 3]], "colour": "red"}));
    let w: Vec<String> = v["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect();
    assert!(w.iter().any(|w| w.contains("WriteRange")), "{w:?}");
    assert!(w.iter().any(|w| w.contains("colour")), "{w:?}");
    let e = m.err("read-range", json!({"range": "A1:C1", "formulas": "false"}));
    assert!(e["reason"].as_str().unwrap().contains("send it without quotes"), "{e}");
    assert_eq!(m.ok("read-range", json!({"range": "A1:C1", "formulas": false}))["values"], json!([[1.0, 2.0, 3.0]]));
    m.ok("execute_command", json!({"command": "sheet.rename", "params": {"name": "Data"}}));
    // Sheet names and command ids in any case.
    assert_eq!(m.ok("read_range", json!({"range": "data!A1"}))["values"], json!([[1.0]]));
    assert_eq!(m.ok("read_range", json!({"range": "A1", "sheet": "DATA"}))["values"], json!([[1.0]]));
    let v = m.ok("execute_command", json!({"command": "HOME.BOLD", "params": {"range": "A1"}}));
    assert!(v["warnings"].as_array().unwrap().iter().any(|w| w.as_str().unwrap().contains("home.bold")), "{v}");
    assert_eq!(m.ok("get_cell", json!({"address": "A1"}))["style"]["font"]["bold"], true);
    m.ok("insert_chart", json!({"range": "A1:C1", "type": "Column"}));
}

// 7. Caller identity: --author, and caller per call, on notes and the saved file.
#[test]
fn caller_identity() {
    let dir = folder("caller");
    let mut m = start(&dir, &["--author", "Platform Agent"]);
    m.ok("set_cell", json!({"cell": "A1", "input": "x"}));
    m.ok("execute_command", json!({"command": "review.newNote", "params": {"cell": "A1", "text": "default author"}}));
    m.ok("execute_command", json!({"command": "review.newNote", "params": {"cell": "B1", "text": "by caller"}, "caller": "Mona"}));
    assert_eq!(m.ok("get_cell", json!({"cell": "A1"}))["comment"]["author"], "Platform Agent");
    assert_eq!(m.ok("get_cell", json!({"cell": "B1"}))["comment"]["author"], "Mona");
    m.ok("save_workbook", json!({"path": "who.xlsx", "caller": "Mona"}));
    let core = crate::harness::zip_text(&dir.join("who.xlsx"), "docProps/core.xml");
    assert!(core.contains("<cp:lastModifiedBy>Mona</cp:lastModifiedBy>"), "{core}");
    assert!(core.contains("<dc:creator>Mona</dc:creator>"), "{core}");
}

// Round 2: `cancel` stops the call in progress or a job at a safe point; the document stays as
// it was before that call.
#[test]
fn cancel_stops_calls_and_jobs_and_keeps_the_document() {
    let dir = folder("cancel");
    let mut m = start(&dir, &["--job-after-ms", "300"]);
    // Nothing to cancel.
    let c = m.ok("cancel", json!({}));
    assert_eq!((c["cancelled"].as_bool(), c["status"].as_str()), (Some(false), Some("not_running")), "{c}");
    assert_eq!(m.err("cancel", json!({"job_id": "job-77"}))["code"], "not_found");
    // A long call, cancelled while it runs (no job id: the call in progress).
    let t0 = Instant::now();
    let slow = m.begin("tools/call", json!({"name": "debug_sleep", "arguments": {"ms": 20000}}));
    std::thread::sleep(Duration::from_millis(100));
    let c = m.ok("cancel", json!({}));
    assert_eq!((c["cancelled"].as_bool(), c["status"].as_str()), (Some(true), Some("cancelling")), "{c}");
    let job = c["job_id"].as_str().unwrap().to_string();
    let r = m.wait_for(&slow, 30);
    let res = &r["result"];
    // Either the call's own answer (cancelled) or its hand-off as a job, then cancelled.
    let status = if res["isError"] == true {
        assert_eq!(res["structuredContent"]["error"]["code"], "cancelled", "{r}");
        m.ok("job_status", json!({"job_id": job}))["status"].clone()
    } else {
        m.finish_job(&job, 30)["status"].clone()
    };
    assert_eq!(status, "cancelled");
    assert!(t0.elapsed() < Duration::from_secs(10), "stopped long before the 20 s sleep ended");
    // The job is over: cancelling it again says so.
    let c = m.ok("cancel", json!({"job_id": job}));
    assert_eq!((c["cancelled"].as_bool(), c["status"].as_str()), (Some(false), Some("cancelled")), "{c}");

    // A big edit run as a job, cancelled part-way: nothing of it stays.
    m.ok("set_cell", json!({"cell": "D1", "input": "keep me"}));
    let rows: Vec<Value> = (1..=150_000).map(|i| json!([i, format!("=A{i}*2"), format!("=SUM($A$1:A{i})")])).collect();
    let h = m.ok("write_range", json!({"range": "A1", "values": rows, "async": true}));
    let job = h["job_id"].as_str().unwrap().to_string();
    let c = m.ok("cancel", json!({"job_id": job}));
    assert_eq!(c["status"], "cancelling");
    let done = m.finish_job(&job, 60);
    assert_eq!(done["status"], "cancelled", "{done}");
    assert_eq!(done["error"]["code"], "cancelled");
    // The workbook is as it was: D1 kept, nothing written, and it keeps working.
    assert_eq!(m.ok("read_range", json!({"range": "D1"}))["values"], json!([["keep me"]]));
    let a = m.ok("read_range", json!({"range": "A1:C2"}));
    assert!(a["values"].as_array().unwrap().iter().flat_map(|r| r.as_array().unwrap()).all(|v| v.is_null() || v == ""), "{a}");
    m.ok("write_range", json!({"range": "A1", "values": [[2, "=A1*3"]]}));
    assert_eq!(m.ok("read_range", json!({"range": "B1"}))["values"], json!([[6.0]]));
    assert_eq!(m.ok("recalculate", json!({}))["errorCount"], 0);

    // The client giving up on its request (notifications/cancelled, which clients also send on
    // their own timeouts): the call carries on as a job, its result kept for job_status.
    let slow = m.begin("tools/call", json!({"name": "debug_sleep", "arguments": {"ms": 1500}}));
    std::thread::sleep(Duration::from_millis(100));
    m.send(&json!({"jsonrpc": "2.0", "method": "notifications/cancelled", "params": {"requestId": slow, "reason": "client timeout"}}));
    let job = m.ok("server_status", json!({}))["current_call"]["job_id"].as_str().unwrap().to_string();
    let done = m.finish_job(&job, 30);
    assert_eq!(done["status"], "done", "{done}");
    assert_eq!(done["result"]["slept_ms"], 1500);
}

// Round 2: periodic checkpoints — every N changing calls, or when unsaved changes are T seconds
// old — keep the recovered file current, so a hard kill (no handler runs) loses little.
#[test]
fn checkpoints_survive_a_hard_kill() {
    let dir = folder("checkpoint");
    let mut m = start(&dir, &["--checkpoint-edits", "3", "--checkpoint-secs", "0"]);
    m.ok("set_cell", json!({"cell": "A1", "input": "one"}));
    m.ok("set_cell", json!({"cell": "A2", "input": "two"}));
    std::thread::sleep(Duration::from_millis(600));
    assert!(files_in(&dir).iter().all(|f| !f.contains(".recovered-")), "not due after 2 changes: {:?}", files_in(&dir));
    assert_eq!(m.ok("server_status", json!({}))["checkpoints"]["pending_edits"], 2);
    m.ok("set_cell", json!({"cell": "A3", "input": "three"}));
    let wait_for_file = |dir: &std::path::Path| {
        (0..50).find_map(|_| {
            std::thread::sleep(Duration::from_millis(100));
            files_in(dir).into_iter().find(|f| f.contains(".recovered-") && f.ends_with(".xlsx"))
        })
    };
    let file = wait_for_file(&dir).expect("a checkpoint after 3 changes");
    let st = (0..50)
        .find_map(|_| {
            let st = m.ok("server_status", json!({}));
            if st["checkpoints"]["writes"] == 1 {
                return Some(st);
            }
            std::thread::sleep(Duration::from_millis(100));
            None
        })
        .expect("the checkpoint is recorded");
    assert_eq!(st["checkpoints"]["pending_edits"], 0, "{st}");
    assert!(st["checkpoints"]["files"][0]["path"].as_str().unwrap().ends_with(&file), "{st}");
    // Three more: the same file is brought up to date (not a second one).
    for c in ["A4", "A5", "A6"] {
        m.ok("set_cell", json!({"cell": c, "input": c}));
    }
    std::thread::sleep(Duration::from_millis(800));
    assert_eq!(files_in(&dir).iter().filter(|f| f.contains(".recovered-") && f.ends_with(".xlsx")).count(), 1, "{:?}", files_in(&dir));
    // Killed outright: no handler runs, the checkpoint is what is left.
    m.kill();
    assert_eq!(cells_of(&dir.join(&file), "A1:A6"), json!([["one"], ["two"], ["three"], ["A4"], ["A5"], ["A6"]]));
    // The next server announces it.
    let mut m = start(&dir, &[]);
    let st = m.ok("server_status", json!({}));
    assert!(st["recovered_files"].as_array().unwrap().iter().any(|f| f["path"].as_str().unwrap().ends_with(&file)), "{st}");
    drop(m);

    // Time-based: one change, older than the interval, is written; saving removes the checkpoint.
    let dir = folder("checkpoint-time");
    let mut m = start(&dir, &["--checkpoint-edits", "0", "--checkpoint-secs", "1"]);
    m.ok("set_cell", json!({"cell": "B2", "input": "42"}));
    let file = wait_for_file(&dir).expect("a checkpoint after a second");
    m.ok("save_workbook", json!({"path": "saved.xlsx"}));
    std::thread::sleep(Duration::from_millis(300));
    assert!(!dir.join(&file).exists(), "the saved file supersedes the checkpoint");
    let st = m.ok("server_status", json!({}));
    assert!(st["memory"]["rss_bytes"].as_u64().unwrap() > 1_000_000, "real memory measurement: {st}");
}

// The round-trip check (as in WordCraft): open + save in memory, report what would be lost.
#[test]
fn roundtrip_check_reports_what_a_save_would_keep() {
    let dir = folder("roundtrip");
    let mut m = start(&dir, &[]);
    m.ok("set_cell", json!({"cell": "A1", "input": "kept"}));
    m.ok("save_workbook", json!({"path": "plain.xlsx"}));
    let r = m.ok("roundtrip_check", json!({"path": "plain.xlsx"}));
    assert_eq!(r["lossless"], true, "{r}");
    for k in ["missingParts", "addedParts", "changedBinaryParts", "missingRelationships", "stories", "problems"] {
        assert!(r.get(k).is_some(), "{k}: {r}");
    }
    let e = m.err("roundtrip_check", json!({"path": "missing.xlsx"}));
    assert_eq!(e["code"], "not_found", "{e}");
}

// Progress notifications for a call that asks for them (`_meta.progressToken`), as in every craft
// app: the value only grows; the message names the stage.
#[test]
fn a_call_with_a_progress_token_gets_progress_notifications() {
    let dir = folder("progress");
    let mut m = start(&dir, &[]);
    let id = m.begin("tools/call", json!({"name": "debug_sleep", "arguments": {"ms": 1500}, "_meta": {"progressToken": "tok-1"}}));
    let r = m.wait(&id);
    assert_eq!(r["result"]["isError"], false, "{r}");
    let notes: Vec<&Value> = m.notifications.iter().filter(|n| n["params"]["progressToken"] == "tok-1").collect();
    assert!(notes.len() >= 2, "{:?}", m.notifications);
    let values: Vec<f64> = notes.iter().map(|n| n["params"]["progress"].as_f64().unwrap()).collect();
    assert!(values.windows(2).all(|w| w[1] > w[0]), "progress only grows: {values:?}");
    assert!(notes.last().unwrap()["params"]["message"].as_str().unwrap().contains("sleeping"), "{:?}", notes);
}

// WHIQ 0.7.6 (G1): a large sheet picture sends progress notifications while it draws, so a
// caller can tell a long render from a hung one.
#[test]
fn a_large_render_reports_its_progress() {
    let dir = folder("render-progress");
    let mut m = start(&dir, &[]);
    let rows: Vec<Value> = (0..260).map(|r| json!((0..250).map(|c| json!(r * 1000 + c)).collect::<Vec<_>>())).collect();
    let id = m.begin("tools/call", json!({"name": "build_workbook", "arguments": {"sheets": [{"name": "Big", "rows": rows}]}}));
    assert_eq!(m.wait(&id)["result"]["isError"], false);
    let png = dir.join("big.png");
    let id = m.begin(
        "tools/call",
        json!({"name": "render_sheet", "arguments": {"sheet": "Big", "path": png.to_str().unwrap(), "scale": 1}, "_meta": {"progressToken": "r-1"}}),
    );
    let r = m.wait_for(&id, 600);
    assert_eq!(r["result"]["isError"], false, "{r}");
    let notes: Vec<&Value> = m.notifications.iter().filter(|n| n["params"]["progressToken"] == "r-1").collect();
    assert!(notes.iter().any(|n| n["params"]["message"].as_str().unwrap_or("").contains("drawing cells")), "{:?}", notes);
}
