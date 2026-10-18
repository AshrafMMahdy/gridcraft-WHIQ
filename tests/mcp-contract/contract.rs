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
            assert!(v["continue_from"].is_string(), "{v}");
            assert!(v["next_step"].as_str().unwrap().contains("read_range"));
            next = Some(v["next_range"].as_str().unwrap().to_string());
        } else {
            break;
        }
    }
    assert_eq!(got.len(), 120, "all rows reached through continue_from");
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
fn byte_budget_trims_any_result() {
    let dir = folder("bytes");
    let mut m = start(&dir, &["--max-result-bytes", "8192"]);
    let v = m.ok("list_commands", json!({}));
    assert_eq!(v["truncated"], true, "{}", v.to_string().len());
    assert!(v.to_string().len() < 9 * 1024);
    let from = v["continue_from"].as_u64().unwrap();
    let w = m.ok("list_commands", json!({"offset": from, "limit": 3}));
    assert_eq!(w["offset"], from);
    // `continue_from` passed back works as well (the shared continuation).
    let w = m.ok("list_commands", json!({"continue_from": from, "limit": 3}));
    assert_eq!(w["offset"], from);
    // A per-call limit.
    let small = m.ok("list_functions", json!({"max_bytes": 2048}));
    assert_eq!(small["truncated"], true);
    assert!(small.to_string().len() < 3 * 1024, "{}", small.to_string().len());
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

// 3. SIGTERM: the open document is auto-saved; the next process names the file.
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
    let v = m.ok("WriteRange", json!({"Range": "a1", "data": [1, 2, 3], "colour": "red"}));
    let w: Vec<String> = v["warnings"].as_array().unwrap().iter().map(|w| w.as_str().unwrap().to_string()).collect();
    assert!(w.iter().any(|w| w.contains("WriteRange")), "{w:?}");
    assert!(w.iter().any(|w| w.contains("colour")), "{w:?}");
    assert_eq!(m.ok("read-range", json!({"range": "A1:C1", "formulas": "false"}))["values"], json!([[1.0, 2.0, 3.0]]));
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
