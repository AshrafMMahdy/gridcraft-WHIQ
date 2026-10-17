//! WHIQ: the server `gridcraft-cli mcp` runs — nothing hangs, work is never lost.
//!
//! - **One worker thread** owns the document ([`Server`] with its backend). The stdio loop never
//!   waits for it: a call that arrives while another runs is answered `busy` at once, with
//!   `retry_after_ms` and the running job's id.
//! - **Jobs.** Every document call is a job. A call that is done within `call_timeout` answers
//!   normally; one that isn't answers `{job_id, status: "running"}` and keeps going. Long work
//!   (`async: true`, a very large file to open, recalculating/exporting/rendering a big
//!   workbook) answers with its job id straight away. `job_status` reports progress and keeps the
//!   result, so it survives client timeouts.
//! - **Auto-save.** After every call the worker publishes a snapshot of the document. On eviction
//!   (idle timeout, memory over the limit), a panic inside a call, SIGTERM/SIGINT/SIGHUP (Ctrl-C,
//!   Ctrl-Break and console close on Windows) or the client hanging up with unsaved changes, the
//!   snapshot is written to `<name>.recovered-<stamp>.xlsx` inside the write root; the next call
//!   names the file. Recovered files found in the write root at start are announced too.
//! - `server_status` and `job_status` are answered by the stdio loop itself, even mid-call.
//!
//! The MCP server is native only (stdio, threads): it never runs in the browser, so the clock
//! is read freely here.
#![allow(clippy::disallowed_methods)]

use std::collections::BTreeMap;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

use crate::backend::{Backend, DocSnapshot};
use crate::contract::{self, ToolError};
use crate::server::{Server, error_with};
use crate::tools::ToolResult;
use crate::whiq::{CONTROL_TOOLS, Eviction, Settings, all_definitions, status_value};

/// How the runtime behaves.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub settings: Settings,
    /// A call still running after this long answers with its job id and carries on.
    pub call_timeout: Duration,
    /// open_workbook of a file larger than this runs as a job from the start.
    pub job_open_bytes: u64,
    /// recalculate / export / render of a workbook with more cells than this runs as a job.
    pub job_cells: u64,
    /// Evict (auto-save and close) the document when the process uses more memory than this.
    pub max_memory_bytes: Option<u64>,
    /// Evict the document after this long without calls.
    pub idle_evict: Option<Duration>,
    /// Where recovered files go (the automation write root).
    pub write_root: Option<PathBuf>,
    pub read_root: Option<PathBuf>,
    /// Handle SIGTERM / SIGINT / SIGHUP (and the Windows console events) by auto-saving first.
    pub handle_signals: bool,
    /// A call arriving while another runs waits this long for it before answering `busy`.
    pub busy_grace: Duration,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            call_timeout: Duration::from_secs(20),
            job_open_bytes: 20 * 1024 * 1024,
            job_cells: 250_000,
            max_memory_bytes: None,
            idle_evict: None,
            write_root: None,
            read_root: None,
            handle_signals: true,
            busy_grace: Duration::from_millis(300),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum JobState {
    Running,
    Done,
    Failed,
}

#[derive(Debug)]
struct Job {
    tool: String,
    state: JobState,
    started: Instant,
    finished: Option<Instant>,
    /// The tool result (`{content, isError, structuredContent}`) once done.
    result: Option<Value>,
    /// The caller was told the job id: the result waits here for job_status.
    handed_off: bool,
}

#[derive(Debug, Default)]
struct Jobs {
    next: u64,
    map: BTreeMap<u64, Job>,
}

#[derive(Clone, Debug)]
struct Running {
    tool: String,
    job: u64,
    started: Instant,
}

enum Work {
    Call { msg: Value, job: u64, done: mpsc::Sender<Value> },
    Evict { reason: String },
}

struct Shared {
    out: Mutex<Box<dyn Write + Send>>,
    jobs: Mutex<Jobs>,
    busy: Mutex<Option<Running>>,
    snapshot: Mutex<Option<DocSnapshot>>,
    recovered: Mutex<Vec<String>>,
    evicted: Mutex<Option<Eviction>>,
    last_activity: Mutex<Instant>,
    idle_done: AtomicBool,
    config: RuntimeConfig,
    started: Instant,
}

/// A poisoned lock still holds usable data: a panic elsewhere must not stop the server.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

fn job_name(n: u64) -> String {
    format!("job-{n}")
}

fn parse_job(s: &str) -> Option<u64> {
    s.trim().trim_start_matches("job-").trim_start_matches("job").trim_start_matches(['-', '_', ' ']).parse().ok()
}

impl Shared {
    fn write(&self, v: &Value) {
        let mut out = lock(&self.out);
        let _ = out.write_all(v.to_string().as_bytes());
        let _ = out.write_all(b"\n");
        let _ = out.flush();
    }

    fn touch(&self) {
        *lock(&self.last_activity) = Instant::now();
        self.idle_done.store(false, Ordering::SeqCst);
    }

    /// Writes the snapshot's workbook to `<name>.recovered-<stamp>.xlsx` (write root first).
    fn autosave(&self, snap: &DocSnapshot, why: &str) -> Option<PathBuf> {
        let dir = self
            .config
            .write_root
            .clone()
            .or_else(|| snap.path.as_deref().and_then(|p| Path::new(p).parent().map(Path::to_path_buf)).filter(|d| !d.as_os_str().is_empty()))
            .or_else(|| std::env::current_dir().ok())?;
        let stem = Path::new(&snap.title).file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Book".into());
        let stem: String = stem.chars().map(|c| if c.is_alphanumeric() || " -_.()".contains(c) { c } else { '_' }).collect();
        let path = dir.join(format!("{}.recovered-{}.xlsx", stem.trim(), stamp()));
        let text = path.to_string_lossy().into_owned();
        let saved = gridcraft_engine::io::save_bytes(&snap.wb, &text).and_then(|bytes| gridcraft_engine::io::write_file(&text, &bytes));
        match saved {
            Ok(()) => {
                eprintln!("gridcraft-cli: {why}: unsaved work auto-saved to {text}");
                lock(&self.recovered).push(text);
                Some(path)
            }
            Err(e) => {
                eprintln!("gridcraft-cli: {why}: auto-save to {text} failed: {e}");
                None
            }
        }
    }

    /// Auto-saves the latest snapshot when it has unsaved changes.
    fn autosave_latest(&self, why: &str) -> Option<PathBuf> {
        let snap = lock(&self.snapshot).clone()?;
        if !snap.dirty {
            return None;
        }
        self.autosave(&snap, why)
    }

    fn job_summary(&self, n: u64, j: &Job) -> Value {
        let elapsed = j.finished.unwrap_or_else(Instant::now).duration_since(j.started).as_millis() as u64;
        let state = match j.state {
            JobState::Running => "running",
            JobState::Done => "done",
            JobState::Failed => "failed",
        };
        json!({"job_id": job_name(n), "tool": j.tool, "status": state, "elapsed_ms": elapsed})
    }

    fn status(&self) -> Value {
        let snap = lock(&self.snapshot).clone();
        let busy = lock(&self.busy).clone();
        let jobs: Vec<Value> = {
            let j = lock(&self.jobs);
            j.map.iter().filter(|(_, x)| x.handed_off || x.state == JobState::Running).map(|(n, x)| self.job_summary(*n, x)).collect()
        };
        let evicted = lock(&self.evicted).clone();
        let extra = json!({
            "mode": "concurrent",
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "busy": busy.map(|b| json!({"tool": b.tool, "job_id": job_name(b.job), "elapsed_ms": b.started.elapsed().as_millis() as u64})),
            "jobs": jobs,
            "recovered_files": lock(&self.recovered).clone(),
            "evicted": evicted.map(|e| json!({"reason": e.reason, "recovered_path": e.recovered.map(|p| p.display().to_string()), "original_path": e.original})),
            "limits": {
                "call_timeout_ms": self.config.call_timeout.as_millis() as u64,
                "job_open_bytes": self.config.job_open_bytes,
                "job_cells": self.config.job_cells,
                "max_memory_bytes": self.config.max_memory_bytes,
                "idle_evict_ms": self.config.idle_evict.map(|d| d.as_millis() as u64),
            },
        });
        let mut v = status_value(snap.as_ref(), (self.config.read_root.clone(), self.config.write_root.clone()), &self.config.settings, extra);
        if v["memory"]["rss_bytes"].is_null()
            && let Some(s) = &snap
        {
            // No RSS reading on this platform: an estimate from the cell count.
            v["memory"]["estimated_bytes"] = json!(s.cells.saturating_mul(160));
        }
        v
    }

    fn job_status(&self, args: &Value) -> ToolResult {
        let jobs = lock(&self.jobs);
        let Some(asked) = args.get("job_id").or(args.get("id")).or(args.get("job")) else {
            let list: Vec<Value> =
                jobs.map.iter().filter(|(_, x)| x.handed_off || x.state == JobState::Running).map(|(n, x)| self.job_summary(*n, x)).collect();
            return ToolResult::json(&json!({"jobs": list}));
        };
        let text = match asked {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let Some((n, j)) = parse_job(&text).and_then(|n| jobs.map.get(&n).map(|j| (n, j))) else {
            let ids: Vec<String> = jobs.map.keys().map(|n| job_name(*n)).collect();
            return ToolResult::failure(
                &ToolError::new("not_found", format!("there is no job `{text}`"), "use a job_id a call returned; server_status lists the jobs")
                    .with("closest", json!(contract::closest(&text, ids.iter().map(String::as_str), 5))),
            );
        };
        let mut v = self.job_summary(n, j);
        match (&j.state, &j.result) {
            (JobState::Running, _) => {
                let elapsed = j.started.elapsed().as_millis() as u64;
                v["retry_after_ms"] = json!(retry_after(elapsed));
                v["next_step"] = json!(format!("the job is still running; call job_status with job_id \"{}\" again", job_name(n)));
            }
            (_, Some(r)) => {
                v["is_error"] = r["isError"].clone();
                let s = r["structuredContent"].clone();
                if r["isError"] == true {
                    v["error"] = s["error"].clone();
                } else {
                    v["result"] = s;
                }
            }
            _ => {}
        }
        ToolResult::json(&v)
    }
}

fn retry_after(elapsed_ms: u64) -> u64 {
    (elapsed_ms / 4).clamp(250, 5000)
}

/// `20261017-184512-123` (UTC) for recovered file names.
fn stamp() -> String {
    let now = SystemTime::now().duration_since(UNIX_EPOCH).unwrap_or_default();
    let secs = now.as_secs() as i64;
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    // Days since 1970-01-01 to a civil date (Howard Hinnant's algorithm).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}-{:03}", rem / 3600, rem % 3600 / 60, rem % 60, now.subsec_millis())
}

/// Recovered files already in `dir` (from an earlier process).
fn find_recovered(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| e.file_name().to_string_lossy().contains(".recovered-"))
                .map(|e| e.path().display().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v
}

/// A backend that only describes itself: the stdio loop's own server answers the protocol
/// methods (initialize, tools/list, ping…) with it.
struct Describe(String);

impl Backend for Describe {
    fn call(&mut self, method: &str, _params: Value) -> Result<Value, String> {
        Err(format!("`{method}` is answered by the document worker"))
    }
    fn has_ui(&self) -> bool {
        false
    }
    fn describe(&self) -> String {
        self.0.clone()
    }
}

/// Builds the backend inside the worker thread.
pub type BackendFactory = Box<dyn FnOnce() -> Box<dyn Backend> + Send>;

/// Serves MCP on `input`/`output` until `input` closes. `describe` is the backend's description
/// (for `initialize`); `factory` builds the backend on the worker thread.
pub fn serve(
    factory: BackendFactory,
    describe: String,
    config: RuntimeConfig,
    input: impl BufRead + Send + 'static,
    output: impl Write + Send + 'static,
) -> std::io::Result<()> {
    let shared = Arc::new(Shared {
        out: Mutex::new(Box::new(output)),
        jobs: Mutex::new(Jobs::default()),
        busy: Mutex::new(None),
        snapshot: Mutex::new(None),
        recovered: Mutex::new(config.write_root.as_deref().map(find_recovered).unwrap_or_default()),
        evicted: Mutex::new(None),
        last_activity: Mutex::new(Instant::now()),
        idle_done: AtomicBool::new(false),
        config: config.clone(),
        started: Instant::now(),
    });
    if config.handle_signals {
        let s = shared.clone();
        let installed = ctrlc::set_handler(move || {
            s.autosave_latest("stopped by a signal");
            std::process::exit(143);
        });
        if let Err(e) = installed {
            eprintln!("gridcraft-cli: signals are not handled ({e}); unsaved work is still auto-saved on eviction and hang-up");
        }
    }

    // The worker owns the document.
    let (work_tx, work_rx) = mpsc::channel::<Work>();
    let ws = shared.clone();
    let settings = config.settings.clone();
    let worker = std::thread::Builder::new().name("gridcraft-worker".into()).spawn(move || {
        let mut server = Server::with_settings(factory(), settings);
        let found = lock(&ws.recovered).clone();
        if !found.is_empty() {
            server.state_mut().notices.push(format!(
                "work from an earlier session was auto-saved and is waiting to be reopened: {} (open_workbook it to continue)",
                found.join(", ")
            ));
        }
        *lock(&ws.snapshot) = server.backend().snapshot();
        worker_loop(&mut server, &ws, work_rx);
    })?;

    // Idle eviction.
    if let Some(idle) = config.idle_evict {
        let s = shared.clone();
        let tx = work_tx.clone();
        let _ = std::thread::Builder::new().name("gridcraft-idle".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                let idle_for = lock(&s.last_activity).elapsed();
                if idle_for < idle || lock(&s.busy).is_some() || s.idle_done.load(Ordering::SeqCst) {
                    continue;
                }
                let open = lock(&s.snapshot).as_ref().is_some_and(|d| d.dirty || d.path.is_some() || d.cells > 0);
                if !open || lock(&s.evicted).is_some() {
                    continue;
                }
                s.idle_done.store(true, Ordering::SeqCst);
                let reason = format!("the workbook was closed after {} s without calls", idle.as_secs_f32());
                if tx.send(Work::Evict { reason }).is_err() {
                    return;
                }
            }
        });
    }

    // Lines are read on their own thread so the loop can answer while it waits.
    let (line_tx, line_rx) = mpsc::channel::<Option<String>>();
    let _ = std::thread::Builder::new().name("gridcraft-stdin".into()).spawn(move || {
        let mut input = input;
        let mut buf = Vec::new();
        loop {
            buf.clear();
            match input.read_until(b'\n', &mut buf) {
                Ok(0) | Err(_) => {
                    let _ = line_tx.send(None);
                    return;
                }
                Ok(_) => {
                    let line = String::from_utf8(buf.clone()).unwrap_or_else(|_| "\u{0}invalid-utf8".into());
                    if line_tx.send(Some(line)).is_err() {
                        return;
                    }
                }
            }
        }
    });

    let mut local = Server::with_settings(Box::new(Describe(describe)), config.settings.clone());
    let names: Vec<String> = all_definitions().iter().filter_map(|d| d["name"].as_str().map(str::to_string)).collect();
    while let Ok(Some(line)) = line_rx.recv() {
        if line.starts_with('\u{0}') {
            shared.write(&json!({"jsonrpc": "2.0", "id": null, "error": {"code": -32700, "message": "message is not valid UTF-8"}}));
            continue;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(trimmed) {
            Ok(m) => m,
            Err(_) => {
                if let Some(r) = local.handle_line(trimmed) {
                    let _ = lock(&shared.out).write_all(format!("{r}\n").as_bytes());
                    let _ = lock(&shared.out).flush();
                }
                continue;
            }
        };
        let method = msg.get("method").and_then(Value::as_str).unwrap_or("").to_string();
        let id = msg.get("id").cloned();
        match (method.as_str(), id) {
            ("tools/call", Some(id)) => {
                let asked = msg["params"]["name"].as_str().unwrap_or("").to_string();
                let args = msg["params"]["arguments"].clone();
                match contract::normalize_tool_name(&asked, &names) {
                    Some(name) if CONTROL_TOOLS.contains(&name.as_str()) => {
                        let r = if name == "server_status" { ToolResult::json(&shared.status()) } else { shared.job_status(&args) };
                        shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": r.to_value()}));
                    }
                    Some(name) => submit(&shared, &work_tx, msg, id, &name, &args),
                    None => {
                        if let Some(r) = local.handle(msg) {
                            shared.write(&r);
                        }
                    }
                }
            }
            ("resources/read", Some(id)) => submit(&shared, &work_tx, msg, id, "resources/read", &Value::Null),
            _ => {
                if let Some(r) = local.handle(msg) {
                    shared.write(&r);
                }
            }
        }
    }
    // The client hung up: finish the call in progress (for a while), then keep any unsaved work.
    let deadline = Instant::now() + Duration::from_secs(60);
    while lock(&shared.busy).is_some() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(work_tx);
    if lock(&shared.busy).is_none() {
        let _ = worker.join();
    }
    shared.autosave_latest("the client closed the connection");
    Ok(())
}

/// Should this call run as a job from the start?
fn starts_as_job(shared: &Shared, name: &str, args: &Value) -> bool {
    match args.get("async") {
        Some(Value::Bool(b)) => return *b,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("true") => return true,
        Some(Value::String(s)) if s.eq_ignore_ascii_case("false") => return false,
        _ => {}
    }
    let cells = lock(&shared.snapshot).as_ref().map(|s| s.cells).unwrap_or(0);
    match name {
        "open_workbook" => {
            let Some(p) = args.get("path").and_then(Value::as_str) else { return false };
            let p = gridcraft_engine::file_access::resolve_relative(p, gridcraft_engine::file_access::Access::Read);
            gridcraft_engine::file_access::check_read(&p).is_ok() && std::fs::metadata(&p).is_ok_and(|m| m.len() > shared.config.job_open_bytes)
        }
        "recalculate" | "export_pdf" | "render_sheet" => cells > shared.config.job_cells,
        "build_workbook" => {
            let rows: u64 = args["sheets"].as_array().into_iter().flatten().map(|s| s["rows"].as_array().map_or(0, |r| r.len() as u64)).sum();
            rows > shared.config.job_cells / 10
        }
        _ => false,
    }
}

/// Hands a document call to the worker, or answers `busy`.
fn submit(shared: &Arc<Shared>, work_tx: &mpsc::Sender<Work>, msg: Value, id: Value, name: &str, args: &Value) {
    shared.touch();
    // A call that is about to finish is worth a short wait (pipelined requests), never a long one.
    let grace = Instant::now() + shared.config.busy_grace;
    while lock(&shared.busy).is_some() && Instant::now() < grace {
        std::thread::sleep(Duration::from_millis(5));
    }
    let job = {
        let mut busy = lock(&shared.busy);
        if let Some(r) = busy.as_ref() {
            let elapsed = r.started.elapsed().as_millis() as u64;
            let e = ToolError::new(
                "busy",
                format!("another call is running ({} for {} ms, {})", r.tool, elapsed, job_name(r.job)),
                format!("retry after {} ms; server_status and job_status answer meanwhile", retry_after(elapsed)),
            )
            .with("retry_after_ms", json!(retry_after(elapsed)))
            .with("job_id", json!(job_name(r.job)));
            drop(busy);
            let reply = if name == "resources/read" {
                error_with(id, -32000, &e)
            } else {
                json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::failure(&e).to_value()})
            };
            shared.write(&reply);
            return;
        }
        let mut jobs = lock(&shared.jobs);
        jobs.next += 1;
        let n = jobs.next;
        jobs.map.insert(
            n,
            Job { tool: name.to_string(), state: JobState::Running, started: Instant::now(), finished: None, result: None, handed_off: false },
        );
        // Old finished jobs are forgotten (the newest 200 stay).
        while jobs.map.len() > 200 {
            let Some(oldest) = jobs.map.iter().find(|(_, j)| j.state != JobState::Running).map(|(k, _)| *k) else { break };
            jobs.map.remove(&oldest);
        }
        *busy = Some(Running { tool: name.to_string(), job: n, started: Instant::now() });
        n
    };
    let as_job = name != "resources/read" && starts_as_job(shared, name, args);
    let (done_tx, done_rx) = mpsc::channel::<Value>();
    if work_tx.send(Work::Call { msg, job, done: done_tx }).is_err() {
        *lock(&shared.busy) = None;
        let saved = shared.autosave_latest("the document worker stopped");
        let mut e = ToolError::new("internal_error", "the document worker stopped", "restart the server and open the recovered file");
        if let Some(p) = saved {
            e = e.with("recovered_path", json!(p.display().to_string()));
        }
        shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::failure(&e).to_value()}));
        return;
    }
    if as_job {
        if let Some(j) = lock(&shared.jobs).map.get_mut(&job) {
            j.handed_off = true;
        }
        handoff(shared, id, job, name, "long work runs as a job");
        return;
    }
    let s = shared.clone();
    let tool = name.to_string();
    let timeout = shared.config.call_timeout;
    let _ = std::thread::Builder::new().name("gridcraft-wait".into()).spawn(move || match done_rx.recv_timeout(timeout) {
        Ok(reply) => {
            lock(&s.jobs).map.remove(&job);
            s.write(&reply);
        }
        Err(_) => {
            let mut jobs = lock(&s.jobs);
            let finished = jobs.map.get(&job).and_then(|j| j.result.clone().map(|r| (r, j.state.clone())));
            match finished {
                Some((r, _)) => {
                    jobs.map.remove(&job);
                    drop(jobs);
                    s.write(&json!({"jsonrpc": "2.0", "id": id, "result": r}));
                }
                None => {
                    if let Some(j) = jobs.map.get_mut(&job) {
                        j.handed_off = true;
                    }
                    drop(jobs);
                    handoff(&s, id, job, &tool, "the call is taking longer than the call timeout; it carries on as a job");
                }
            }
        }
    });
}

/// The answer to a call that carries on as a job.
fn handoff(shared: &Shared, id: Value, job: u64, name: &str, why: &str) {
    let v = json!({
        "job_id": job_name(job),
        "status": "running",
        "tool": name,
        "reason": why,
        "retry_after_ms": 1000,
        "next_step": format!("poll job_status with job_id \"{}\" until status is done; the result will be there", job_name(job)),
    });
    shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::json(&v).to_value()}));
}

fn worker_loop(server: &mut Server, shared: &Shared, rx: mpsc::Receiver<Work>) {
    while let Ok(work) = rx.recv() {
        match work {
            Work::Call { msg, job, done } => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let call = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(msg)));
                let reply = match call {
                    Ok(Some(r)) => r,
                    Ok(None) => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    Err(_) => {
                        let saved = shared.autosave_latest("a call panicked");
                        let mut e = ToolError::new(
                            "internal_error",
                            "the call failed inside GridCraft (a bug); the workbook is kept as it was before this call",
                            match &saved {
                                Some(p) => format!("carry on; a copy of the work is also auto-saved at {}", p.display()),
                                None => "carry on, or try the call another way".to_string(),
                            },
                        );
                        if let Some(p) = &saved {
                            e = e.with("recovered_path", json!(p.display().to_string()));
                        }
                        json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::failure(&e).to_value()})
                    }
                };
                *lock(&shared.snapshot) = server.backend().snapshot();
                *lock(&shared.evicted) = server.state_mut().evicted.clone();
                {
                    let mut jobs = lock(&shared.jobs);
                    if let Some(j) = jobs.map.get_mut(&job) {
                        let failed = reply["result"]["isError"] == true || reply.get("error").is_some();
                        j.state = if failed { JobState::Failed } else { JobState::Done };
                        j.finished = Some(Instant::now());
                        j.result = Some(match reply.get("result") {
                            Some(r) => r.clone(),
                            None => {
                                let e = ToolError::new("failed", reply["error"]["message"].as_str().unwrap_or("failed"), "check the request");
                                ToolResult::failure(&e).to_value()
                            }
                        });
                    }
                }
                *lock(&shared.busy) = None;
                shared.touch();
                let _ = done.send(reply);
                if let (Some(max), Some(rss)) = (shared.config.max_memory_bytes, crate::whiq::rss_bytes())
                    && rss > max
                {
                    evict(
                        server,
                        shared,
                        format!("the workbook was closed because the server used {} MB, over its {} MB limit", rss >> 20, max >> 20),
                    );
                }
            }
            Work::Evict { reason } => {
                // An eviction waits for no call: the worker is idle when it gets here.
                evict(server, shared, reason);
            }
        }
    }
}

fn evict(server: &mut Server, shared: &Shared, reason: String) {
    let snap = server.backend().snapshot();
    let recovered = snap.as_ref().filter(|s| s.dirty).and_then(|s| shared.autosave(s, "evicted"));
    server.backend().reset();
    let ev = Eviction { reason, recovered, original: snap.and_then(|s| s.path) };
    eprintln!("gridcraft-cli: {}", ev.reason);
    server.state_mut().evicted = Some(ev.clone());
    *lock(&shared.evicted) = Some(ev);
    *lock(&shared.snapshot) = server.backend().snapshot();
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn stamps_and_job_ids() {
        let s = stamp();
        assert_eq!(s.len(), "20261017-184512-123".len(), "{s}");
        assert!(s.starts_with("20"));
        assert_eq!(parse_job("job-12"), Some(12));
        assert_eq!(parse_job("12"), Some(12));
        assert_eq!(parse_job("JOB_3".to_ascii_lowercase().as_str()), Some(3));
        assert_eq!(retry_after(100), 250);
        assert_eq!(retry_after(100_000), 5000);
    }
}
