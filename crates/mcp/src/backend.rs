//! Where MCP tool calls end up: a control-channel method call.

use std::io::{BufRead, BufReader, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::time::Duration;

use serde_json::{Value, json};

/// WHIQ: the state of the open document, kept after every call so it can be auto-saved from
/// another thread (SIGTERM, eviction) without asking the busy worker.
#[derive(Clone, Debug)]
pub struct DocSnapshot {
    /// Identifies the document (a new or reopened one has another).
    pub uid: u64,
    pub wb: std::sync::Arc<gridcraft_engine::model::Workbook>,
    pub title: String,
    pub path: Option<String>,
    pub dirty: bool,
    pub cells: u64,
    pub sheets: Vec<String>,
    /// WHIQ: the document handle calls name it by (`document: "doc-7"`).
    pub handle: String,
    /// WHIQ: every open document (the active one included), for server_status.
    pub open: Vec<OpenDoc>,
    /// WHIQ: the other open documents as they stand (their own `others` empty), so auto-save
    /// covers every workbook with unsaved work, not only the active one.
    pub others: Vec<DocSnapshot>,
}

impl DocSnapshot {
    /// The active document and every other open one.
    pub fn all(&self) -> impl Iterator<Item = &DocSnapshot> {
        std::iter::once(self).chain(self.others.iter())
    }
}

/// WHIQ: an open document as server_status lists it.
#[derive(Clone, Debug)]
pub struct OpenDoc {
    pub handle: String,
    pub title: String,
    pub path: Option<String>,
    pub dirty: bool,
    pub active: bool,
}

/// WHIQ: the handle of the document with this uid.
pub fn handle_of(uid: u64) -> String {
    format!("doc-{uid}")
}

/// Something that answers control-channel methods (`engine.execute`, `engine.commands`,
/// `document.inspect`, `sheet.read`, `ui.screenshot`, …).
pub trait Backend {
    /// Call one method. `Ok` carries the `result`, `Err` the error message.
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String>;
    /// True when a real UI is attached (`ui.screenshot` and dialogs work).
    fn has_ui(&self) -> bool;
    /// Short human description ("headless", "connected to 127.0.0.1:7979").
    fn describe(&self) -> String;
    /// WHIQ: the active document as it stands (`None` when the backend can't say).
    fn snapshot(&self) -> Option<DocSnapshot> {
        None
    }
    /// WHIQ: the user name comments, notes and saves carry from now on; returns the previous one.
    fn set_user(&mut self, _name: &str) -> Option<String> {
        None
    }
    /// WHIQ: record `name` as the workbook's last modifier (and author when it has none) before
    /// a save.
    fn stamp_author(&mut self, _name: &str) {}
    /// WHIQ: close every document and start over with a blank one (eviction frees the memory).
    fn reset(&mut self) {}
    /// WHIQ: whether documents have handles here (the headless server; a running app has none).
    fn has_handles(&self) -> bool {
        false
    }
    /// WHIQ: the handle of the active document (`None` when the backend has no handles).
    fn active_document(&self) -> Option<String> {
        None
    }
    /// WHIQ: make the document with this handle the active one. `Err` carries the handles that
    /// are open (empty when the backend has no handles).
    fn select_document(&mut self, _handle: &str) -> Result<(), Vec<String>> {
        Err(vec![])
    }
    /// WHIQ: the automation read and write roots in force.
    fn roots(&self) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        (None, None)
    }
}

/// Read/write timeout on the control connection.
const IO_TIMEOUT: Duration = Duration::from_secs(30);
/// Longest reply line accepted from the app (guards against a runaway peer).
const MAX_REPLY: u64 = 256 * 1024 * 1024;

/// A running GridCraft app, reached through its loopback control port.
pub struct Remote {
    addr: String,
    conn: Option<(BufReader<TcpStream>, TcpStream)>,
    next_id: u64,
}

impl Remote {
    /// Connect to `addr` (`127.0.0.1:7979`), failing fast when nothing is listening.
    pub fn connect(addr: &str) -> std::io::Result<Self> {
        let mut r = Self { addr: addr.to_string(), conn: None, next_id: 1 };
        r.reconnect()?;
        Ok(r)
    }

    pub fn addr(&self) -> &str {
        &self.addr
    }

    fn reconnect(&mut self) -> std::io::Result<()> {
        self.conn = None;
        let mut last = std::io::Error::new(std::io::ErrorKind::NotFound, format!("cannot resolve {}", self.addr));
        for sa in self.addr.to_socket_addrs()? {
            match TcpStream::connect_timeout(&sa, Duration::from_secs(2)) {
                Ok(s) => {
                    s.set_nodelay(true).ok();
                    s.set_read_timeout(Some(IO_TIMEOUT)).ok();
                    s.set_write_timeout(Some(IO_TIMEOUT)).ok();
                    let read = s.try_clone()?;
                    self.conn = Some((BufReader::new(read), s));
                    return Ok(());
                }
                Err(e) => last = e,
            }
        }
        Err(last)
    }

    fn roundtrip(&mut self, line: &str) -> std::io::Result<String> {
        if self.conn.is_none() {
            self.reconnect()?;
        }
        let Some((reader, writer)) = self.conn.as_mut() else {
            return Err(std::io::Error::new(std::io::ErrorKind::NotConnected, "not connected"));
        };
        writer.write_all(line.as_bytes())?;
        writer.write_all(b"\n")?;
        writer.flush()?;
        let mut reply = String::new();
        let n = std::io::Read::take(reader, MAX_REPLY).read_line(&mut reply)?;
        if n == 0 {
            return Err(std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "control channel closed"));
        }
        Ok(reply)
    }
}

impl Backend for Remote {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let id = self.next_id;
        self.next_id = self.next_id.wrapping_add(1);
        let line = json!({"id": id, "method": method, "params": params}).to_string();
        // One retry with a fresh connection (the app may have restarted).
        let reply = match self.roundtrip(&line) {
            Ok(r) => r,
            Err(_) => {
                self.conn = None;
                self.roundtrip(&line).map_err(|e| {
                    self.conn = None;
                    format!("GridCraft app at {} is not reachable: {e}", self.addr)
                })?
            }
        };
        let v: Value = serde_json::from_str(reply.trim()).map_err(|e| format!("bad reply from app: {e}"))?;
        if v.get("ok").and_then(Value::as_bool) == Some(true) {
            Ok(v.get("result").cloned().unwrap_or(Value::Null))
        } else {
            Err(v.get("error").and_then(Value::as_str).unwrap_or("unknown error").to_string())
        }
    }

    fn has_ui(&self) -> bool {
        true
    }

    fn describe(&self) -> String {
        format!("connected to the GridCraft app at {}", self.addr)
    }
}
