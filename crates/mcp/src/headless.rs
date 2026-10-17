//! An in-process engine session that answers the engine-level control-channel methods itself.

use std::sync::Arc;

use gridcraft_engine::Session;
use gridcraft_engine::file_access::{self, AutomationRoots};
use serde_json::{Value, json};

use crate::backend::Backend;

/// Error for methods that need a window.
pub(crate) const NO_UI: &str = "no UI in headless mode: start the app with `gridcraft --control 7979` and run the MCP server \
with `gridcraft-cli mcp --connect 7979`";

/// Headless backend: a [`Session`] with one blank workbook to start with.
pub struct Headless {
    pub session: Session,
    /// WHIQ: the folders this session's commands may read and write (`--automation-read-root`,
    /// `--automation-write-root`); `None` confines nothing.
    roots: Option<Arc<AutomationRoots>>,
}

impl Default for Headless {
    fn default() -> Self {
        Self::new()
    }
}

impl Headless {
    /// A session with a blank workbook (`Book1`) open.
    pub fn new() -> Self {
        let mut session = Session::new();
        session.new_workbook();
        Self { session, roots: None }
    }

    /// A session wrapping an existing one (e.g. with a file already open).
    pub fn with_session(session: Session) -> Self {
        Self { session, roots: None }
    }

    /// Confine every file the session's commands read and write to `roots`.
    pub fn with_automation_roots(mut self, roots: Option<Arc<AutomationRoots>>) -> Self {
        self.roots = roots;
        self
    }

    /// The roots in force, if any.
    pub fn roots(&self) -> Option<&Arc<AutomationRoots>> {
        self.roots.as_ref()
    }

    fn execute(&mut self, id: &str, params: Value) -> Result<Value, String> {
        // Programmatic calls never open dialogs; anything a command queued for a UI is dropped.
        self.session.execute_headless(id, params).map_err(|e| e.to_string())
    }

    fn commands(&self, params: &Value) -> Value {
        let q = params.get("search").and_then(Value::as_str).map(str::to_ascii_lowercase);
        let list: Vec<Value> = self
            .session
            .commands()
            .into_iter()
            .filter(|c| {
                q.as_ref().is_none_or(|q| {
                    c.id.to_ascii_lowercase().contains(q.as_str())
                        || c.label.to_ascii_lowercase().contains(q.as_str())
                        || c.menu.iter().any(|m| m.to_ascii_lowercase().contains(q.as_str()))
                })
            })
            .map(|c| serde_json::to_value(c).unwrap_or_default())
            .collect();
        Value::Array(list)
    }
}

impl Backend for Headless {
    fn call(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let roots = self.roots.clone();
        file_access::confine(roots.as_ref(), || self.answer(method, params))
    }

    fn has_ui(&self) -> bool {
        false
    }

    fn describe(&self) -> String {
        "headless (in-process engine, no window)".to_string()
    }

    fn snapshot(&self) -> Option<crate::backend::DocSnapshot> {
        let d = self.session.active()?;
        Some(crate::backend::DocSnapshot {
            wb: d.wb.clone(),
            title: d.display_title(),
            path: d.path.clone(),
            dirty: d.is_dirty(),
            cells: d.wb.sheets.iter().map(|s| s.cells.len() as u64).sum(),
            sheets: d.wb.sheets.iter().map(|s| s.name.clone()).collect(),
        })
    }

    fn set_user(&mut self, name: &str) -> Option<String> {
        Some(std::mem::replace(&mut self.session.prefs.user_name, name.to_string()))
    }

    fn stamp_author(&mut self, name: &str) {
        if let Ok(d) = self.session.doc_mut()
            && (d.wb.props.last_modified_by != name || d.wb.props.author.is_empty())
        {
            let wb = std::sync::Arc::make_mut(&mut d.wb);
            wb.props.last_modified_by = name.to_string();
            if wb.props.author.is_empty() {
                wb.props.author = name.to_string();
            }
        }
    }

    fn reset(&mut self) {
        let prefs = self.session.prefs.clone();
        self.session = Session::new();
        self.session.prefs = prefs;
        self.session.new_workbook();
    }

    fn roots(&self) -> (Option<std::path::PathBuf>, Option<std::path::PathBuf>) {
        match &self.roots {
            Some(r) => (r.read_root().map(Into::into), r.write_root().map(Into::into)),
            None => (None, None),
        }
    }
}

impl Headless {
    fn answer(&mut self, method: &str, params: Value) -> Result<Value, String> {
        let params = if params.is_null() { json!({}) } else { params };
        match method {
            "ping" => Ok(json!("pong")),
            "engine.execute" => {
                let id = params.get("command").and_then(Value::as_str).ok_or("engine.execute: missing `command`")?.to_string();
                let p = params.get("params").cloned().unwrap_or_else(|| json!({}));
                if !(p.is_object() || p.is_null()) {
                    return Err(format!("engine.execute: `params` of `{id}` must be an object"));
                }
                self.execute(&id, p)
            }
            "engine.commands" => Ok(self.commands(&params)),
            "engine.journal" => Ok(Value::Array(self.session.journal.iter().map(|(id, p)| json!({"command": id, "params": p})).collect())),
            m if m.starts_with("ui.") || m.starts_with("app.screenshot") => Err(format!("{m}: {NO_UI}")),
            // Read-only engine methods (`document.inspect`, `sheet.read`, `cell.get`, …) are
            // commands with the same id.
            m if gridcraft_engine::find_command(m).is_some() => self.execute(m, params),
            other => Err(format!("unknown method `{other}`")),
        }
    }
}
