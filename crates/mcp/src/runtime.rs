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
//! - `server_status`, `job_status`, `list_jobs` and `cancel` are answered by the stdio loop
//!   itself, even mid-call. Their shapes (and the error codes, recovered-file names and flags)
//!   are the same in WordCraft, GridCraft and DeckCraft: CONTRACT-PARITY.md.
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
use crate::contract::{self, ToolError, code};
use crate::server::{Server, error_with};
use crate::tools::ToolResult;
use crate::whiq::{CONTROL_TOOLS, Eviction, Settings, all_definitions, status_value};

/// How the runtime behaves.
#[derive(Clone, Debug)]
pub struct RuntimeConfig {
    pub settings: Settings,
    /// `--job-after-ms`: a call still running after this long answers with its job id and
    /// carries on.
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
    /// `--busy-after-ms`: a call arriving while another runs waits this long for it before
    /// answering `busy`.
    pub busy_grace: Duration,
    /// Checkpoint unsaved work to the recovered file after this many changing calls (0: never).
    pub checkpoint_edits: u64,
    /// … and when unsaved changes are this old (`None`: never).
    pub checkpoint_interval: Option<Duration>,
}

impl Default for RuntimeConfig {
    fn default() -> Self {
        Self {
            settings: Settings::default(),
            call_timeout: Duration::from_secs(15),
            job_open_bytes: 20 * 1024 * 1024,
            job_cells: 250_000,
            max_memory_bytes: None,
            idle_evict: None,
            write_root: None,
            read_root: None,
            handle_signals: true,
            busy_grace: Duration::from_millis(300),
            checkpoint_edits: 20,
            checkpoint_interval: Some(Duration::from_secs(60)),
        }
    }
}

#[derive(Clone, Debug, PartialEq)]
enum JobState {
    Running,
    Done,
    Failed,
    /// Stopped by `cancel` (the workbook is as it was before the call).
    Cancelled,
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
    /// The client gave up on the request (`notifications/cancelled`): its answer is kept for
    /// job_status and not written.
    client_cancelled: bool,
    /// The progress the call reached.
    progress: Option<gridcraft_engine::core::progress::Progress>,
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
    /// The JSON-RPC id of the call (`notifications/cancelled` names it).
    id: Value,
    /// The request's `_meta.progressToken` (`notifications/progress`, as WordCraft and
    /// DeckCraft send).
    token: Option<Value>,
}

enum Work {
    Call { msg: Value, job: u64, done: mpsc::Sender<Value> },
    Evict { reason: String },
}

/// WHIQ: the periodic checkpoint of unsaved work (one recovered file per document, rewritten).
#[derive(Debug, Default)]
struct Checkpoint {
    /// The document it is for.
    uid: u64,
    path: Option<PathBuf>,
    /// The workbook (its address) last seen, and last written.
    seen: usize,
    written: usize,
    /// Calls that changed the workbook since the last write.
    edits: u64,
    /// When the unsaved changes not yet written began.
    since: Option<Instant>,
    writes: u64,
}

struct Shared {
    out: Mutex<Box<dyn Write + Send>>,
    jobs: Mutex<Jobs>,
    busy: Mutex<Option<Running>>,
    snapshot: Mutex<Option<DocSnapshot>>,
    /// Recovered files made or found (`{path, reason, original, at}`).
    recovered: Mutex<Vec<Value>>,
    /// Recovered files not yet named to the agent: the next tool answer names them, once.
    unannounced: Mutex<Vec<Value>>,
    evicted: Mutex<Option<Eviction>>,
    calls: std::sync::atomic::AtomicU64,
    busy_replies: std::sync::atomic::AtomicU64,
    /// The backend's description (server_status).
    backend: String,
    /// The progress last notified for the running call (job), and when.
    note: Mutex<(u64, f64, Option<Instant>)>,
    last_activity: Mutex<Instant>,
    idle_done: AtomicBool,
    /// The worker thread (what `cancel` stops).
    worker: Mutex<Option<std::thread::ThreadId>>,
    checkpoint: Mutex<Checkpoint>,
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

    /// Recovered files not yet named to the agent go into this tool answer (`recovered_files`,
    /// once), as in every craft app.
    fn announce(&self, reply: &mut Value) {
        let Some(sc) = reply.pointer_mut("/result/structuredContent").and_then(Value::as_object_mut) else { return };
        let news = std::mem::take(&mut *lock(&self.unannounced));
        if news.is_empty() {
            return;
        }
        let paths: Vec<String> = news.iter().filter_map(|e| e["path"].as_str().map(str::to_string)).collect();
        sc.insert("recovered_files".into(), Value::Array(news));
        let note = format!("work was auto-saved to recovered files: {} (open_workbook on one to continue that work)", paths.join(", "));
        if let Some(content) = reply.pointer_mut("/result/content").and_then(Value::as_array_mut) {
            content.push(json!({"type": "text", "text": note}));
        }
    }

    /// A `notifications/progress` for the running call when its request carries a progress token
    /// and there is news (a step done, or a second gone by): the value only ever grows, `total`
    /// when the steps are counted, a message with the stage and the time (as in every craft app).
    fn progress_notification(&self) -> Option<Value> {
        let (job, token, tool, started) = {
            let b = lock(&self.busy);
            let r = b.as_ref()?;
            (r.job, r.token.clone()?, r.tool.clone(), r.started)
        };
        let p = gridcraft_engine::core::progress::current().unwrap_or_default();
        let mut note = lock(&self.note);
        if note.0 != job {
            *note = (job, 0.0, None);
        }
        let now = Instant::now();
        let counted = p.total > 0 && (p.done as f64) > note.1;
        let beat = note.2.is_none_or(|t| now.duration_since(t) >= Duration::from_secs(1));
        if !counted && !beat {
            return None;
        }
        let value = if counted { p.done as f64 } else { note.1 + 0.001 };
        note.1 = value;
        note.2 = Some(now);
        let stage = if p.stage.is_empty() { tool } else { p.stage.clone() };
        let secs = started.elapsed().as_secs();
        let mut params = json!({"progressToken": token, "progress": value, "message": if p.total > 0 {
            format!("{stage}: {} of {} ({secs} s)", p.done, p.total)
        } else {
            format!("{stage} ({secs} s)")
        }});
        if p.total > 0 {
            params["total"] = json!(p.total);
        }
        Some(json!({"jsonrpc": "2.0", "method": "notifications/progress", "params": params}))
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
        // The document's checkpoint file, when it has one, is brought up to date rather than
        // joined by another file.
        let existing = {
            let cp = lock(&self.checkpoint);
            cp.path.clone().filter(|_| cp.uid == snap.uid)
        };
        let path = existing.clone().unwrap_or_else(|| {
            let base = format!("{}.recovered-{}", stem.trim(), stamp());
            let mut p = dir.join(format!("{base}.xlsx"));
            let mut n = 2;
            while p.exists() && n < 1000 {
                p = dir.join(format!("{base}-{n}.xlsx"));
                n += 1;
            }
            p
        });
        let text = path.to_string_lossy().into_owned();
        // The recovered file is the server's own copy: rewriting it never keeps an OLD_ one.
        let saved = gridcraft_engine::io::save_bytes(&snap.wb, &text)
            .and_then(|bytes| gridcraft_engine::io::save_policy::allow_overwrite(true, || gridcraft_engine::io::write_file(&text, &bytes)));
        match saved {
            Ok(()) => {
                eprintln!("gridcraft-cli: {why}: unsaved work auto-saved to {text}");
                {
                    let mut cp = lock(&self.checkpoint);
                    if cp.uid == snap.uid {
                        cp.path = Some(path.clone());
                        cp.written = Arc::as_ptr(&snap.wb) as usize;
                        cp.edits = 0;
                        cp.since = None;
                    }
                }
                // A checkpoint is listed under server_status.checkpoints; any other auto-save is
                // work the agent must hear about.
                if why != "checkpoint" {
                    let entry = recovered_entry(&text, why, snap.path.as_deref());
                    let mut rec = lock(&self.recovered);
                    rec.retain(|r| r["path"] != entry["path"]);
                    rec.push(entry.clone());
                    let mut un = lock(&self.unannounced);
                    un.retain(|r| r["path"] != entry["path"]);
                    un.push(entry);
                }
                Some(path)
            }
            Err(e) => {
                eprintln!("gridcraft-cli: {why}: auto-save to {text} failed: {e}");
                None
            }
        }
    }

    /// Notes the document after a call: counts changes, forgets the checkpoint once saved.
    fn note_call(&self, snap: Option<&DocSnapshot>) {
        let Some(s) = snap else { return };
        let ptr = Arc::as_ptr(&s.wb) as usize;
        let mut cp = lock(&self.checkpoint);
        if cp.uid != s.uid {
            // Another document: the old checkpoint file stays (it holds that document's work).
            *cp = Checkpoint { uid: s.uid, seen: ptr, written: ptr, ..Checkpoint::default() };
        }
        if ptr != cp.seen {
            cp.seen = ptr;
            if s.dirty {
                cp.edits += 1;
                cp.since.get_or_insert_with(Instant::now);
            }
        }
        if !s.dirty {
            // Saved (or untouched): the checkpoint is out of date and the file on disk is newer.
            if let Some(p) = cp.path.take() {
                let text = p.to_string_lossy().into_owned();
                // Only a checkpoint nobody was told about goes; a reported recovered file stays.
                if !lock(&self.recovered).iter().any(|r| r["path"] == text.as_str()) {
                    let _ = std::fs::remove_file(&p);
                }
            }
            cp.edits = 0;
            cp.since = None;
            cp.written = ptr;
        }
    }

    /// Writes the checkpoint when it is due: `checkpoint_edits` changing calls, or unsaved
    /// changes older than `checkpoint_interval`, not yet written.
    fn checkpoint_if_due(&self) {
        let snap = lock(&self.snapshot).clone();
        let Some(snap) = snap.filter(|s| s.dirty) else { return };
        let due = {
            let cp = lock(&self.checkpoint);
            let n = self.config.checkpoint_edits;
            cp.uid == snap.uid
                && Arc::as_ptr(&snap.wb) as usize != cp.written
                && cp.edits > 0
                && ((n > 0 && cp.edits >= n) || self.config.checkpoint_interval.is_some_and(|i| cp.since.is_some_and(|t| t.elapsed() >= i)))
        };
        if due && self.autosave(&snap, "checkpoint").is_some() {
            lock(&self.checkpoint).writes += 1;
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
        let state = job_state_name(&j.state);
        let live = (j.state == JobState::Running).then(gridcraft_engine::core::progress::current).flatten();
        let p = if j.state == JobState::Running { live.as_ref() } else { j.progress.as_ref() };
        let mut v = json!({"job_id": job_name(n), "tool": j.tool, "status": state, "elapsed_ms": elapsed});
        if let (Some(o), Value::Object(f)) = (v.as_object_mut(), progress_fields(p, &j.tool, j.state == JobState::Done)) {
            o.extend(f);
        }
        v
    }

    fn status(&self) -> Value {
        let snap = lock(&self.snapshot).clone();
        let busy = lock(&self.busy).clone();
        let jobs: Vec<Value> = {
            let j = lock(&self.jobs);
            j.map.iter().filter(|(_, x)| x.handed_off || x.state == JobState::Running).map(|(n, x)| self.job_summary(*n, x)).collect()
        };
        let evicted = lock(&self.evicted).clone();
        let checkpoint = {
            let cp = lock(&self.checkpoint);
            json!({
                "every_edits": self.config.checkpoint_edits,
                "every_secs": self.config.checkpoint_interval.map(|d| d.as_secs()).unwrap_or(0),
                "pending_edits": cp.edits,
                "writes": cp.writes,
                "files": cp.path.as_ref().map(|p| vec![json!({"path": p.display().to_string(), "document": cp.uid})]).unwrap_or_default(),
            })
        };
        let current = busy.map(|b| {
            let mut v = json!({"tool": b.tool, "job_id": job_name(b.job), "elapsed_ms": b.started.elapsed().as_millis() as u64,
                "cancelling": gridcraft_engine::core::cancel::pending()});
            if let (Some(o), Value::Object(f)) =
                (v.as_object_mut(), progress_fields(gridcraft_engine::core::progress::current().as_ref(), &b.tool, false))
            {
                o.extend(f);
            }
            v
        });
        let idle = lock(&self.last_activity).elapsed();
        let extra = json!({
            "backend": self.backend,
            "uptime_ms": self.started.elapsed().as_millis() as u64,
            "current_call": current,
            "jobs": jobs,
            "recovered_files": lock(&self.recovered).clone(),
            "checkpoints": checkpoint,
            "evicted": evicted.map(|e| vec![json!({"name": e.original.as_deref().and_then(|o| Path::new(o).file_stem()).map(|s| s.to_string_lossy().into_owned()), "reason": e.reason, "recovered_path": e.recovered.map(|p| p.display().to_string()), "original_path": e.original})]).unwrap_or_default(),
            "idle_ms": idle.as_millis() as u64,
            "calls": self.calls.load(Ordering::Relaxed),
            "busy_replies": self.busy_replies.load(Ordering::Relaxed),
            "limits": {
                "max_result_bytes": self.config.settings.max_result_bytes,
                "max_read_cells": self.config.settings.max_read_cells,
                "max_memory_mb": self.config.max_memory_bytes.map(|b| b / (1024 * 1024)),
                "busy_after_ms": self.config.busy_grace.as_millis() as u64,
                "job_after_ms": self.config.call_timeout.as_millis() as u64,
                "open_job_bytes": self.config.job_open_bytes,
                "job_cells": self.config.job_cells,
                "idle_evict_secs": self.config.idle_evict.map(|d| d.as_secs()),
            },
        });
        let mut v = status_value(snap.as_ref(), (self.config.read_root.clone(), self.config.write_root.clone()), &self.config.settings, extra);
        v["memory"]["limit_bytes"] = json!(self.config.max_memory_bytes);
        if v["memory"]["rss_bytes"].is_null()
            && let Some(s) = &snap
        {
            // No RSS reading on this platform: an estimate from the cell count.
            v["memory"]["estimated_bytes"] = json!(s.cells.saturating_mul(160));
        }
        v
    }

    /// `cancel`: asks the call in progress (or the job named, when it is the one running) to stop
    /// at its next safe point. The answer has the same shape in every craft app:
    /// `{cancelled, status, job_id, tool, next_step}`.
    fn cancel(&self, args: &Value) -> ToolResult {
        let asked = args.get("job_id").or(args.get("id")).or(args.get("job")).map(|v| match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        });
        let busy = lock(&self.busy);
        let target = match (&asked, busy.as_ref()) {
            (None, Some(b)) => Some(b.clone()),
            (Some(t), Some(b)) if parse_job(t) == Some(b.job) => Some(b.clone()),
            _ => None,
        };
        if let Some(b) = target {
            if let Some(t) = *lock(&self.worker) {
                gridcraft_engine::core::cancel::request(t);
            }
            drop(busy);
            if let Some(j) = lock(&self.jobs).map.get_mut(&b.job) {
                j.handed_off = true;
            }
            return ToolResult::json(&json!({
                "cancelled": true,
                "job_id": job_name(b.job),
                "tool": b.tool,
                "status": "cancelling",
                "retry_after_ms": 500,
                "next_step": format!(
                    "it stops at its next safe point and changes nothing; job_status with job_id \"{}\" then says cancelled",
                    job_name(b.job)
                ),
            }));
        }
        drop(busy);
        let jobs = lock(&self.jobs);
        match asked {
            None => ToolResult::json(&json!({"cancelled": false, "status": "not_running", "next_step": "no call is running: nothing to stop"})),
            Some(t) => match parse_job(&t).and_then(|n| jobs.map.get(&n).map(|j| (n, j))) {
                Some((n, j)) => ToolResult::json(&json!({
                    "cancelled": false,
                    "status": job_state_name(&j.state),
                    "job_id": job_name(n),
                    "tool": j.tool,
                    "next_step": if j.state == JobState::Running { "server_status shows the running call".to_string() } else { format!("the job had ended; job_status with job_id \"{}\" has its result", job_name(n)) },
                })),
                None => {
                    let ids: Vec<String> = jobs.map.keys().map(|n| job_name(*n)).collect();
                    ToolResult::failure(
                        &ToolError::new(code::NOT_FOUND, format!("there is no job `{t}`"), "use a job_id a call returned; list_jobs lists the jobs")
                            .with("closest", json!(contract::closest(&t, ids.iter().map(String::as_str), 5))),
                    )
                }
            },
        }
    }

    fn list_jobs(&self) -> ToolResult {
        let jobs = lock(&self.jobs);
        let list: Vec<Value> =
            jobs.map.iter().filter(|(_, x)| x.handed_off || x.state == JobState::Running).map(|(n, x)| self.job_summary(*n, x)).collect();
        ToolResult::json(&json!({"jobs": list}))
    }

    fn job_status(&self, args: &Value) -> ToolResult {
        let Some(asked) = args.get("job_id").or(args.get("id")).or(args.get("job")) else {
            return self.list_jobs();
        };
        let jobs = lock(&self.jobs);
        let text = match asked {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        let Some((n, j)) = parse_job(&text).and_then(|n| jobs.map.get(&n).map(|j| (n, j))) else {
            let ids: Vec<String> = jobs.map.keys().map(|n| job_name(*n)).collect();
            return ToolResult::failure(
                &ToolError::new(code::NOT_FOUND, format!("there is no job `{text}`"), "use a job_id a call returned; list_jobs lists the jobs")
                    .with("closest", json!(contract::closest(&text, ids.iter().map(String::as_str), 5))),
            );
        };
        let mut v = self.job_summary(n, j);
        let mut pictures = Vec::new();
        match (&j.state, &j.result) {
            (JobState::Running, _) => {
                let elapsed = j.started.elapsed().as_millis() as u64;
                v["retry_after_ms"] = json!(retry_after(elapsed));
                v["cancelling"] = json!(gridcraft_engine::core::cancel::pending());
                v["next_step"] = json!(format!("the job is still running; call job_status with job_id \"{}\" again", job_name(n)));
            }
            (state, Some(r)) => {
                let s = r["structuredContent"].clone();
                if r["isError"] == true {
                    v["error"] = s["error"].clone();
                    v["next_step"] = json!(if *state == JobState::Cancelled {
                        "the job was cancelled at a safe point; the workbook is as it was before it"
                    } else {
                        "the job failed; see error.next_step"
                    });
                } else {
                    v["result"] = s;
                    v["next_step"] = json!("the job is finished; its result is in `result`");
                }
                pictures = r["content"].as_array().map(|c| c.iter().filter(|b| b["type"] != "text").cloned().collect()).unwrap_or_default();
            }
            _ => {}
        }
        let mut out = ToolResult::json(&v);
        out.content.extend(pictures);
        out
    }
}

/// The progress fields every craft app reports for a call or job: `progress` (0..1, never
/// going back, 1 only once done), `stage`, `steps` `{done, total}` and the `stages` passed.
fn progress_fields(p: Option<&gridcraft_engine::core::progress::Progress>, tool: &str, done: bool) -> Value {
    let p = p.cloned().unwrap_or_default();
    let stage = if p.stage.is_empty() { tool.to_string() } else { p.stage.clone() };
    let mut stages = p.stages.clone();
    if stages.is_empty() {
        stages.push(stage.clone());
    }
    let progress = if done { 1.0 } else { (p.overall.min(0.99) * 1000.0).round() / 1000.0 };
    json!({
        "progress": progress,
        "stage": stage,
        "steps": {"done": if done && p.total > 0 { p.total } else { p.done }, "total": p.total},
        "stages": stages,
    })
}

fn job_state_name(s: &JobState) -> &'static str {
    match s {
        JobState::Running => "running",
        JobState::Done => "done",
        JobState::Failed => "failed",
        JobState::Cancelled => "cancelled",
    }
}

/// A recovered file as every craft app reports it: `{path, reason, original, at}`.
fn recovered_entry(path: &str, reason: &str, original: Option<&str>) -> Value {
    json!({"path": path, "reason": reason, "original": original, "at": rfc3339_now()})
}

/// `2026-10-17T18:45:12Z` (UTC).
fn rfc3339_now() -> String {
    let s = stamp();
    match (s.get(0..4), s.get(4..6), s.get(6..8), s.get(9..11), s.get(11..13), s.get(13..15)) {
        (Some(y), Some(mo), Some(d), Some(h), Some(mi), Some(se)) => format!("{y}-{mo}-{d}T{h}:{mi}:{se}Z"),
        _ => s,
    }
}

fn retry_after(elapsed_ms: u64) -> u64 {
    (elapsed_ms / 4).clamp(250, 5000)
}

/// `20261017-184512` (UTC) for recovered file names: `<name>.recovered-<stamp>.xlsx`, the same
/// form in every craft app.
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
    format!("{y:04}{m:02}{d:02}-{:02}{:02}{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// Recovered files already in `dir` (from an earlier process), as entries with reason `found`.
fn find_recovered(dir: &Path) -> Vec<Value> {
    let mut v: Vec<String> = std::fs::read_dir(dir)
        .map(|it| {
            it.filter_map(|e| e.ok())
                .filter(|e| {
                    let n = e.file_name().to_string_lossy().into_owned();
                    // Not an atomic write's temporary file a crash left (`<name>.sctmp`).
                    n.contains(".recovered-") && !n.starts_with("OLD_") && !n.ends_with(".sctmp") && e.path().is_file()
                })
                .map(|e| e.path().display().to_string())
                .collect()
        })
        .unwrap_or_default();
    v.sort();
    v.into_iter()
        .map(|p| {
            let mut e = recovered_entry(&p, "found", None);
            e["at"] = Value::Null;
            e
        })
        .collect()
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
        unannounced: Mutex::new(config.write_root.as_deref().map(find_recovered).unwrap_or_default()),
        evicted: Mutex::new(None),
        calls: std::sync::atomic::AtomicU64::new(0),
        busy_replies: std::sync::atomic::AtomicU64::new(0),
        backend: describe.clone(),
        note: Mutex::new((0, 0.0, None)),
        last_activity: Mutex::new(Instant::now()),
        idle_done: AtomicBool::new(false),
        worker: Mutex::new(None),
        checkpoint: Mutex::new(Checkpoint::default()),
        config: config.clone(),
        started: Instant::now(),
    });
    if config.handle_signals {
        let s = shared.clone();
        let installed = ctrlc::set_handler(move || {
            s.autosave_latest("signal");
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
        *lock(&ws.worker) = Some(std::thread::current().id());
        gridcraft_engine::core::progress::watch_this_thread();
        let mut server = Server::with_settings(factory(), settings);
        let snap = server.backend().snapshot();
        ws.note_call(snap.as_ref());
        *lock(&ws.snapshot) = snap;
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

    // WHIQ: periodic checkpoints of unsaved work (a hard kill then loses little).
    if config.checkpoint_edits > 0 || config.checkpoint_interval.is_some() {
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("gridcraft-checkpoint".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(200));
                s.checkpoint_if_due();
            }
        });
    }

    // WHIQ parity pass: progress notifications for a call whose request asked for them.
    {
        let s = shared.clone();
        let _ = std::thread::Builder::new().name("gridcraft-progress".into()).spawn(move || {
            loop {
                std::thread::sleep(Duration::from_millis(100));
                if let Some(n) = s.progress_notification() {
                    s.write(&n);
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
                        let r = match name.as_str() {
                            "server_status" => ToolResult::json(&shared.status()),
                            "cancel" => {
                                let mut r = shared.cancel(&args);
                                // Nothing waits in a queue here (a waiting call is answered busy).
                                if args.get("queued").is_some_and(|q| q == true || q == "true")
                                    && let Some(sc) = r.structured.as_mut()
                                {
                                    sc["dropped_from_queue"] = json!(0);
                                }
                                r
                            }
                            "list_jobs" => shared.list_jobs(),
                            _ => shared.job_status(&args),
                        };
                        shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": r.to_value()}));
                    }
                    Some(name) => submit(&shared, &work_tx, msg, id, &name, &args),
                    // An unknown tool: an answer the agent sees, with the closest names.
                    None => {
                        let near = contract::closest(&asked, names.iter().map(String::as_str), 3);
                        let e =
                            ToolError::new(code::NOT_FOUND, format!("unknown tool `{asked}`"), "call tools/list; the closest tools are in `closest`")
                                .with("closest", json!(near));
                        shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::failure(&e).to_value()}));
                    }
                }
            }
            ("resources/read", Some(id)) => submit(&shared, &work_tx, msg, id, "resources/read", &Value::Null),
            // The client gave up on a request (clients also send this on their own timeouts): the
            // call carries on as a job, its result kept for job_status (`cancel` stops work).
            ("notifications/cancelled", None) => {
                let asked = msg["params"]["requestId"].clone();
                let busy = lock(&shared.busy);
                if let Some(b) = busy.as_ref().filter(|b| !asked.is_null() && b.id == asked)
                    && let Some(j) = lock(&shared.jobs).map.get_mut(&b.job)
                {
                    j.handed_off = true;
                    j.client_cancelled = true;
                }
            }
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
    shared.autosave_latest("disconnect");
    Ok(())
}

/// Should this call run as a job from the start?
fn starts_as_job(shared: &Shared, name: &str, args: &Value) -> bool {
    match args.get("async").or_else(|| args.get("background")) {
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
            gridcraft_engine::file_access::check_read(&p).is_ok() && std::fs::metadata(&p).is_ok_and(|m| m.len() >= shared.config.job_open_bytes)
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
            .with("job_id", json!(job_name(r.job)))
            .with("running", json!(r.tool));
            drop(busy);
            shared.busy_replies.fetch_add(1, Ordering::Relaxed);
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
            Job {
                tool: name.to_string(),
                state: JobState::Running,
                started: Instant::now(),
                finished: None,
                result: None,
                handed_off: false,
                client_cancelled: false,
                progress: None,
            },
        );
        // Old finished jobs are forgotten (the newest 200 stay).
        while jobs.map.len() > 200 {
            let Some(oldest) = jobs.map.iter().find(|(_, j)| j.state != JobState::Running).map(|(k, _)| *k) else { break };
            jobs.map.remove(&oldest);
        }
        let token = msg.pointer("/params/_meta/progressToken").filter(|t| t.is_string() || t.is_number()).cloned();
        *busy = Some(Running { tool: name.to_string(), job: n, started: Instant::now(), id: id.clone(), token });
        shared.calls.fetch_add(1, Ordering::Relaxed);
        n
    };
    let as_job = name != "resources/read" && starts_as_job(shared, name, args);
    let (done_tx, done_rx) = mpsc::channel::<Value>();
    if work_tx.send(Work::Call { msg, job, done: done_tx }).is_err() {
        *lock(&shared.busy) = None;
        let saved = shared.autosave_latest("panic");
        let mut e = ToolError::new(code::INTERNAL, "the document worker stopped", "restart the server and open the recovered file");
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
        Ok(mut reply) => {
            // A job someone asked about (cancel names it) keeps its record for job_status.
            let mut jobs = lock(&s.jobs);
            let quiet = jobs.map.get(&job).is_some_and(|j| j.client_cancelled);
            if !jobs.map.get(&job).is_some_and(|j| j.handed_off) {
                jobs.map.remove(&job);
            }
            drop(jobs);
            if !quiet {
                s.announce(&mut reply);
                s.write(&reply);
            }
        }
        Err(_) => {
            let mut jobs = lock(&s.jobs);
            let finished = jobs.map.get(&job).and_then(|j| j.result.clone().map(|r| (r, j.state.clone())));
            match finished {
                Some((r, _)) => {
                    jobs.map.remove(&job);
                    drop(jobs);
                    let mut reply = json!({"jsonrpc": "2.0", "id": id, "result": r});
                    s.announce(&mut reply);
                    s.write(&reply);
                }
                None => {
                    let quiet = jobs.map.get(&job).is_some_and(|j| j.client_cancelled);
                    if let Some(j) = jobs.map.get_mut(&job) {
                        j.handed_off = true;
                    }
                    drop(jobs);
                    if !quiet {
                        handoff(&s, id, job, &tool, "the call is taking longer than --job-after-ms; it carries on as a job");
                    }
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
        "next_step": format!("poll job_status with job_id \"{}\" until status is not \"running\"; the result will be there", job_name(job)),
    });
    shared.write(&json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::json(&v).to_value()}));
}

fn worker_loop(server: &mut Server, shared: &Shared, rx: mpsc::Receiver<Work>) {
    while let Ok(work) = rx.recv() {
        match work {
            Work::Call { msg, job, done } => {
                let id = msg.get("id").cloned().unwrap_or(Value::Null);
                let tool = lock(&shared.jobs).map.get(&job).map(|j| j.tool.clone()).unwrap_or_default();
                gridcraft_engine::core::progress::reset(&tool);
                // `evict` (for a supervisor) works on the worker's document like an eviction.
                let call = if tool == "evict" {
                    let reason = msg["params"]["arguments"]["reason"].as_str().unwrap_or("evicted by the supervisor").to_string();
                    let before = lock(&shared.evicted).clone();
                    evict(server, shared, reason);
                    let ev = lock(&shared.evicted).clone().filter(|e| Some(e) != before.as_ref());
                    let list: Vec<Value> = ev
                        .map(|e| vec![json!({"reason": e.reason, "recovered_path": e.recovered.map(|p| p.display().to_string()), "original_path": e.original})])
                        .unwrap_or_default();
                    Ok(Some(json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::json(&json!({"evicted": list})).to_value()})))
                } else {
                    std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| server.handle(msg)))
                };
                let reply = match call {
                    Ok(Some(r)) => r,
                    Ok(None) => json!({"jsonrpc": "2.0", "id": id, "result": {}}),
                    // Stopped by `cancel` between commands: each command is whole, nothing half done.
                    Err(p) if gridcraft_engine::core::cancel::is_cancelled(&*p) => {
                        let e = ToolError::new(
                            code::CANCELLED,
                            "the call was stopped as asked",
                            "the workbook is as it was before the call; run it again when you need it",
                        );
                        json!({"jsonrpc": "2.0", "id": id, "result": ToolResult::failure(&e).to_value()})
                    }
                    Err(_) => {
                        let saved = shared.autosave_latest("panic");
                        if let Some(p) = &saved {
                            // The answer names the file: no separate announcement.
                            let text = p.display().to_string();
                            lock(&shared.unannounced).retain(|r| r["path"] != text.as_str());
                        }
                        let mut e = ToolError::new(
                            code::INTERNAL,
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
                let snap = server.backend().snapshot();
                shared.note_call(snap.as_ref());
                *lock(&shared.snapshot) = snap;
                *lock(&shared.evicted) = server.state_mut().evicted.clone();
                {
                    let mut jobs = lock(&shared.jobs);
                    if let Some(j) = jobs.map.get_mut(&job) {
                        let failed = reply["result"]["isError"] == true || reply.get("error").is_some();
                        let cancelled = reply["result"]["structuredContent"]["error"]["code"] == "cancelled";
                        j.state = if cancelled {
                            JobState::Cancelled
                        } else if failed {
                            JobState::Failed
                        } else {
                            JobState::Done
                        };
                        j.finished = Some(Instant::now());
                        if j.state == JobState::Done {
                            gridcraft_engine::core::progress::finish();
                        }
                        j.progress = gridcraft_engine::core::progress::current();
                        j.result = Some(match reply.get("result") {
                            Some(r) => r.clone(),
                            None => {
                                let e = ToolError::new(code::FAILED, reply["error"]["message"].as_str().unwrap_or("failed"), "check the request");
                                ToolResult::failure(&e).to_value()
                            }
                        });
                    }
                }
                {
                    // A stop asked for this call must not reach the next one.
                    let mut busy = lock(&shared.busy);
                    gridcraft_engine::core::cancel::clear();
                    *busy = None;
                }
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
    let kind = if reason.contains(" MB") {
        "memory"
    } else if reason.contains("without calls") {
        "idle"
    } else {
        "evict"
    };
    let recovered = snap.as_ref().filter(|s| s.dirty).and_then(|s| shared.autosave(s, kind));
    if let Some(p) = &recovered {
        // The `evicted` answer names it: no separate announcement.
        let text = p.display().to_string();
        lock(&shared.unannounced).retain(|r| r["path"] != text.as_str());
    }
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
        assert_eq!(s.len(), "20261017-184512".len(), "{s}");
        assert_eq!(rfc3339_now().len(), "2026-10-17T18:45:12Z".len());
        assert!(s.starts_with("20"));
        assert_eq!(parse_job("job-12"), Some(12));
        assert_eq!(parse_job("12"), Some(12));
        assert_eq!(parse_job("JOB_3".to_ascii_lowercase().as_str()), Some(3));
        assert_eq!(retry_after(100), 250);
        assert_eq!(retry_after(100_000), 5000);
    }
}
