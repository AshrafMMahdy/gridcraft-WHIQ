//! Drives `gridcraft-cli mcp` over stdio, as the platform's supervisor does.

use std::collections::HashMap;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, ChildStdin, Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use serde_json::{Value, json};

/// The error codes every craft app answers with (CONTRACT-PARITY.md): no other code may appear.
pub const CODES: &[&str] = &[
    "busy",
    "cancelled",
    "disk_full",
    "evicted",
    "failed",
    "internal",
    "invalid_argument",
    "invalid_request",
    "locked",
    "no_document",
    "not_available",
    "not_found",
    "out_of_range",
    "outside_root",
    "password_required",
    "too_large",
    "unsupported",
    "wrong_password",
];

/// The binary under test: `GRIDCRAFT_CLI` (e.g. a release build), else the one cargo built for
/// this test.
pub fn binary() -> PathBuf {
    std::env::var_os("GRIDCRAFT_CLI").map(PathBuf::from).unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_gridcraft-cli")))
}

/// A fresh folder for one test (never reused, never removed while the test runs).
pub fn folder(name: &str) -> PathBuf {
    let stamp = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let d = std::env::temp_dir().join(format!("gc-mcp-contract-{}-{name}-{stamp}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    plain_path(std::fs::canonicalize(&d).unwrap())
}

pub struct Mcp {
    pub child: Child,
    stdin: Option<ChildStdin>,
    replies: mpsc::Receiver<Value>,
    pending: HashMap<String, Value>,
    next: u64,
    /// Notifications the server sent (progress), in order.
    pub notifications: Vec<Value>,
    pub stderr_path: PathBuf,
}

impl Mcp {
    /// Starts `gridcraft-cli mcp <args>` with the test hooks on and initializes the session.
    pub fn start(args: &[&str]) -> Mcp {
        Self::start_env(args, &[])
    }

    pub fn start_env(args: &[&str], env: &[(&str, &str)]) -> Mcp {
        let err_dir = folder("stderr");
        let stderr_path = err_dir.join("stderr.log");
        let stderr = std::fs::File::create(&stderr_path).unwrap();
        let mut cmd = Command::new(binary());
        cmd.arg("mcp").args(args).env("GRIDCRAFT_MCP_TEST_HOOKS", "1");
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd.stdin(Stdio::piped()).stdout(Stdio::piped()).stderr(Stdio::from(stderr)).spawn().expect("start gridcraft-cli mcp");
        let stdout = child.stdout.take().unwrap();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            for line in BufReader::new(stdout).lines() {
                let Ok(line) = line else { return };
                if let Ok(v) = serde_json::from_str::<Value>(&line)
                    && tx.send(v).is_err()
                {
                    return;
                }
            }
        });
        let stdin = child.stdin.take();
        let mut m = Mcp { child, stdin, replies: rx, pending: HashMap::new(), next: 100, stderr_path, notifications: Vec::new() };
        let init =
            m.request("initialize", json!({"protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": {"name": "contract", "version": "1"}}));
        assert_eq!(init["result"]["serverInfo"]["name"], "gridcraft", "{init}");
        m.notify("notifications/initialized");
        m
    }

    pub fn stderr(&self) -> String {
        std::fs::read_to_string(&self.stderr_path).unwrap_or_default()
    }

    pub fn send(&mut self, v: &Value) {
        let line = format!("{v}\n");
        let stdin = self.stdin.as_mut().expect("stdin open");
        stdin.write_all(line.as_bytes()).unwrap();
        stdin.flush().unwrap();
    }

    pub fn notify(&mut self, method: &str) {
        self.send(&json!({"jsonrpc": "2.0", "method": method}));
    }

    /// Sends a request; returns its id (wait for the reply with [`Mcp::wait`]).
    pub fn begin(&mut self, method: &str, params: Value) -> String {
        self.next += 1;
        let id = format!("r{}", self.next);
        self.send(&json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}));
        id
    }

    /// The reply to request `id` (any order), within `secs`.
    pub fn wait_for(&mut self, id: &str, secs: u64) -> Value {
        if let Some(v) = self.pending.remove(id) {
            return v;
        }
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            let v = self.replies.recv_timeout(left).unwrap_or_else(|_| panic!("no reply to {id} within {secs} s; stderr:\n{}", self.stderr()));
            if v.get("id").is_none() && v.get("method").is_some() {
                self.notifications.push(v);
                continue;
            }
            let got = v["id"].as_str().unwrap_or("").to_string();
            if got == id {
                return v;
            }
            self.pending.insert(got, v);
        }
    }

    pub fn wait(&mut self, id: &str) -> Value {
        self.wait_for(id, 60)
    }

    pub fn request(&mut self, method: &str, params: Value) -> Value {
        let id = self.begin(method, params);
        self.wait(&id)
    }

    /// `tools/call`; returns the tool result (`content`, `isError`, `structuredContent`).
    pub fn call(&mut self, name: &str, args: Value) -> Value {
        let v = self.request("tools/call", json!({"name": name, "arguments": args}));
        assert!(v.get("error").is_none(), "{name}: protocol error {v}");
        let r = v["result"].clone();
        assert!(r["structuredContent"].is_object(), "{name}: every result carries structuredContent: {r}");
        if r["isError"] == true {
            let c = r["structuredContent"]["error"]["code"].as_str().unwrap_or("");
            assert!(CODES.contains(&c), "{name}: `{c}` is not a shared error code: {r}");
        }
        r
    }

    /// A call that must succeed: its structuredContent.
    pub fn ok(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], false, "{name} {args} failed: {r}");
        r["structuredContent"].clone()
    }

    /// A call that must fail: its error object (code, reason, next_step…).
    pub fn err(&mut self, name: &str, args: Value) -> Value {
        let r = self.call(name, args.clone());
        assert_eq!(r["isError"], true, "{name} {args} should fail: {r}");
        let e = r["structuredContent"]["error"].clone();
        for k in ["code", "reason", "next_step"] {
            assert!(e[k].as_str().is_some_and(|s| !s.is_empty()), "{name}: error without {k}: {e}");
        }
        e
    }

    /// Polls job_status until the job is no longer running.
    pub fn finish_job(&mut self, job: &str, secs: u64) -> Value {
        let deadline = Instant::now() + Duration::from_secs(secs);
        loop {
            let s = self.ok("job_status", json!({"job_id": job}));
            if s["status"] != "running" {
                return s;
            }
            assert!(Instant::now() < deadline, "job {job} still running after {secs} s");
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Closes stdin and waits for the process to end.
    pub fn close(mut self) -> std::process::ExitStatus {
        drop(self.stdin.take());
        let deadline = Instant::now() + Duration::from_secs(90);
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s;
            }
            assert!(Instant::now() < deadline, "server did not exit after stdin closed");
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    /// Kills the process outright (SIGKILL on Unix, TerminateProcess on Windows): no handler runs.
    pub fn kill(mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }

    /// Sends SIGTERM (Unix) and waits for the process to end.
    #[cfg(unix)]
    pub fn terminate(mut self) -> std::process::ExitStatus {
        let pid = self.child.id().to_string();
        let st = Command::new("kill").args(["-TERM", &pid]).status().unwrap();
        assert!(st.success());
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            if let Ok(Some(s)) = self.child.try_wait() {
                return s;
            }
            assert!(Instant::now() < deadline, "server did not stop on SIGTERM");
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}

impl Drop for Mcp {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

/// The values of `range` in an .xlsx file, read with the engine.
pub fn cells_of(path: &Path, range: &str) -> Value {
    let mut s = gridcraft_engine::Session::new();
    s.execute_headless("file.open", json!({"path": path.to_str().unwrap()})).unwrap_or_else(|e| panic!("open {}: {e}", path.display()));
    s.execute_headless("sheet.read", json!({"range": range})).unwrap()["values"].clone()
}

/// Every file in `dir` (names only), sorted.
pub fn files_in(dir: &Path) -> Vec<String> {
    let mut v: Vec<String> = std::fs::read_dir(dir).unwrap().filter_map(|e| e.ok()).map(|e| e.file_name().to_string_lossy().into_owned()).collect();
    v.sort();
    v
}

/// The entries of the zip package at `path`.
pub fn zip_entries(path: &Path) -> Vec<String> {
    let f = std::fs::File::open(path).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut v: Vec<String> = (0..z.len()).map(|i| z.by_index(i).unwrap().name().to_string()).collect();
    v.sort();
    v
}

/// One part of the zip package at `path` as text.
pub fn zip_text(path: &Path, name: &str) -> String {
    use std::io::Read;
    let f = std::fs::File::open(path).unwrap();
    let mut z = zip::ZipArchive::new(f).unwrap();
    let mut s = String::new();
    z.by_name(name).unwrap().read_to_string(&mut s).unwrap();
    s
}

/// The canonical path in the form the server reports it: on Windows, canonicalize gives the verbatim `\\?\C:\...` form, while
/// the server (and the person reading its answers) uses the plain `C:\...` path to the same folder. A UNC path keeps its prefix.
pub fn plain_path(p: PathBuf) -> PathBuf {
    if let Some(rest) = p.to_str().and_then(|s| s.strip_prefix(r"\\?\")) {
        if !rest.starts_with(r"UNC\") {
            return PathBuf::from(rest);
        }
    }
    p
}
