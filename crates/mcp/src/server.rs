//! JSON-RPC 2.0 framing and the MCP lifecycle / tools / resources / prompts methods.

use std::io::{BufRead, Write};

use serde_json::{Value, json};

use crate::backend::Backend;
use crate::contract::{self, ToolError};
use crate::whiq::{CallState, Settings, all_definitions, run_tool};

/// The MCP revision we implement.
pub const PROTOCOL_VERSION: &str = "2025-06-18";
/// Revisions we can speak; an older client gets its own version echoed back.
pub const SUPPORTED_VERSIONS: &[&str] = &[PROTOCOL_VERSION, "2025-03-26", "2024-11-05"];

const PARSE_ERROR: i64 = -32700;
const INVALID_REQUEST: i64 = -32600;
const METHOD_NOT_FOUND: i64 = -32601;
const INVALID_PARAMS: i64 = -32602;
const INTERNAL_ERROR: i64 = -32603;
const RESOURCE_NOT_FOUND: i64 = -32002;

/// Longest accepted message line (16 MB): bigger payloads belong in files.
const MAX_LINE: usize = 16 * 1024 * 1024;

const INSTRUCTIONS: &str = "GridCraft is a spreadsheet app (an Excel clone). Cells use A1 notation (\"B2\", \"A1:D10\", \
\"Sheet2!A1:B3\"). Every action is a command: find ids and parameters with list_commands and run any of them with \
execute_command. Convenience tools cover the common flow: write_range / set_cell to enter values and formulas (strings \
starting with = are formulas), read_range / get_cell to read computed values back, format_range, create_table, \
insert_chart, sort_range, filter, evaluate_formula (try a formula without changing the sheet), undo / redo, \
open_workbook / save_workbook. inspect_workbook summarizes sheets, used ranges, tables, charts and names. \
list_functions lists the worksheet functions. WHIQ: build_workbook makes a whole workbook (sheets, formats, tables, native \
charts) from JSON; render_range / render_sheet give PNG pictures and export_pdf a PDF with page setup (Arabic shaped, \
fonts embedded); recalculate recomputes every formula; insert_rows, delete_columns, rename_sheet, conditional_format, \
data_validation edit in place. Every failure says what to do next (`next_step`); `busy` means retry after \
`retry_after_ms`; long calls answer with a `job_id` to poll with job_status; server_status always answers. Saves never \
replace an existing file silently (it becomes OLD_<name>; pass overwrite: true to replace it). open_workbook, \
new_workbook and build_workbook answer with a `document` handle: pass it as `document` to act on that workbook while \
others are open; close_workbook frees one.";

/// Resource URIs.
pub const WORKBOOK_URI: &str = "gridcraft://workbook";
pub const COMMANDS_URI: &str = "gridcraft://commands";
pub const FUNCTIONS_URI: &str = "gridcraft://functions";

/// An MCP server bound to one backend.
pub struct Server {
    backend: Box<dyn Backend>,
    initialized: bool,
    /// WHIQ: settings, eviction and notices kept between calls.
    state: CallState,
}

fn response(id: Value, result: Value) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "result": result})
}

fn error(id: Value, code: i64, message: impl Into<String>) -> Value {
    let message = message.into();
    // WHIQ: protocol errors carry the contract's code / reason / next step too.
    let kind = match code {
        PARSE_ERROR => "invalid_request",
        INVALID_REQUEST => "invalid_request",
        METHOD_NOT_FOUND => "not_found",
        RESOURCE_NOT_FOUND => "not_found",
        INVALID_PARAMS => "invalid_argument",
        _ => "internal",
    };
    let next = match code {
        PARSE_ERROR | INVALID_REQUEST => {
            "send one JSON-RPC 2.0 object per line: {\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{...}}"
        }
        METHOD_NOT_FOUND => "use initialize, tools/list, tools/call, resources/list, resources/read or ping",
        RESOURCE_NOT_FOUND => "resources/list names the resources",
        _ => "check the request and send it again",
    };
    let data = ToolError::new(kind, message.clone(), next).to_value();
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": message, "data": data}})
}

/// A JSON-RPC error with a contract error as its data.
pub(crate) fn error_with(id: Value, code: i64, e: &ToolError) -> Value {
    json!({"jsonrpc": "2.0", "id": id, "error": {"code": code, "message": e.reason, "data": e.to_value()}})
}

impl Server {
    pub fn new(backend: Box<dyn Backend>) -> Self {
        Self { backend, initialized: false, state: CallState::default() }
    }

    /// WHIQ: a server with its limits and author.
    pub fn with_settings(backend: Box<dyn Backend>, settings: Settings) -> Self {
        Self { backend, initialized: false, state: CallState { settings, ..CallState::default() } }
    }

    /// WHIQ: eviction state, notices and settings.
    pub fn state_mut(&mut self) -> &mut CallState {
        &mut self.state
    }

    pub fn backend(&mut self) -> &mut dyn Backend {
        self.backend.as_mut()
    }

    /// Whether the client has sent `notifications/initialized`.
    pub fn is_initialized(&self) -> bool {
        self.initialized
    }

    /// Serve newline-delimited JSON-RPC until `input` closes. Logs go to stderr only (stdout is
    /// the protocol stream).
    pub fn serve(&mut self, mut input: impl BufRead, mut output: impl Write) -> std::io::Result<()> {
        let mut buf = Vec::new();
        loop {
            buf.clear();
            let n = input.read_until(b'\n', &mut buf)?;
            if n == 0 {
                return Ok(());
            }
            let reply = match std::str::from_utf8(&buf) {
                Ok(line) => self.handle_line(line),
                Err(_) => Some(error(Value::Null, PARSE_ERROR, "message is not valid UTF-8").to_string()),
            };
            if let Some(reply) = reply {
                output.write_all(reply.as_bytes())?;
                output.write_all(b"\n")?;
                output.flush()?;
            }
        }
    }

    /// Handle one line; returns the reply line (None for notifications and blank lines).
    pub fn handle_line(&mut self, line: &str) -> Option<String> {
        let line = line.trim();
        if line.is_empty() {
            return None;
        }
        if line.len() > MAX_LINE {
            return Some(error(Value::Null, INVALID_REQUEST, "message too large (16 MB max)").to_string());
        }
        let reply = match serde_json::from_str::<Value>(line) {
            Ok(Value::Array(batch)) => {
                // Batches were removed in 2025-06-18; still answer older clients sensibly.
                if batch.is_empty() {
                    Some(error(Value::Null, INVALID_REQUEST, "empty batch"))
                } else {
                    let replies: Vec<Value> = batch.into_iter().filter_map(|m| self.handle(m)).collect();
                    (!replies.is_empty()).then_some(Value::Array(replies))
                }
            }
            Ok(msg) => self.handle(msg),
            Err(e) => Some(error(Value::Null, PARSE_ERROR, format!("parse error: {e}"))),
        };
        reply.map(|r| r.to_string())
    }

    /// Handle one JSON-RPC message; `None` for notifications and responses.
    pub fn handle(&mut self, msg: Value) -> Option<Value> {
        let Value::Object(o) = &msg else { return Some(error(Value::Null, INVALID_REQUEST, "message must be an object")) };
        let id = o.get("id").cloned();
        let Some(method) = o.get("method").and_then(Value::as_str) else {
            // A response to a server→client request (we send none): ignore. Anything else is invalid.
            if o.contains_key("result") || o.contains_key("error") {
                return None;
            }
            return Some(error(id.unwrap_or(Value::Null), INVALID_REQUEST, "missing `method`"));
        };
        let params = o.get("params").cloned().unwrap_or(Value::Null);
        let Some(id) = id else {
            self.notification(method);
            return None;
        };
        if !(id.is_string() || id.is_number()) {
            return Some(error(Value::Null, INVALID_REQUEST, "`id` must be a string or number"));
        }
        if !(params.is_object() || params.is_null()) {
            return Some(error(id, INVALID_PARAMS, "`params` must be an object"));
        }
        if method == "tools/call" {
            return Some(self.tools_call(id, &params));
        }
        Some(match self.request(method, &params) {
            Ok(r) => response(id, r),
            Err((code, m)) => error(id, code, m),
        })
    }

    /// WHIQ: `tools/call` through the contract: the tool name is matched in any spelling (an
    /// unknown one is a JSON-RPC error naming the closest tools); everything else, argument
    /// problems included, is a tool result the model sees.
    fn tools_call(&mut self, id: Value, params: &Value) -> Value {
        let Some(asked) = params.get("name").and_then(Value::as_str) else {
            return error(id, INVALID_PARAMS, "missing tool `name`");
        };
        let args = match params.get("arguments") {
            None | Some(Value::Null) => json!({}),
            Some(v @ Value::Object(_)) => v.clone(),
            Some(_) => return error(id, INVALID_PARAMS, "tool `arguments` must be an object"),
        };
        let names: Vec<String> = all_definitions().iter().filter_map(|d| d["name"].as_str().map(str::to_string)).collect();
        let Some(name) = contract::normalize_tool_name(asked, &names) else {
            let near = contract::closest(asked, names.iter().map(String::as_str), 3);
            let e = ToolError::new("not_found", format!("unknown tool `{asked}`"), "call tools/list; the closest tools are in `closest`")
                .with("closest", json!(near));
            // A tool result the agent sees (as in every craft app), not a protocol error.
            return response(id, crate::tools::ToolResult::failure(&e).to_value());
        };
        if name != asked {
            self.state.call_warnings.push(format!("tool `{asked}` was read as `{name}`"));
        }
        response(id, run_tool(self.backend.as_mut(), &mut self.state, &name, &args).to_value())
    }

    fn notification(&mut self, method: &str) {
        match method {
            "notifications/initialized" => self.initialized = true,
            "notifications/cancelled" | "notifications/progress" | "notifications/roots/list_changed" => {}
            other => log::debug!("ignoring notification {other}"),
        }
    }

    fn request(&mut self, method: &str, params: &Value) -> Result<Value, (i64, String)> {
        match method {
            "initialize" => {
                let asked = params.get("protocolVersion").and_then(Value::as_str).unwrap_or(PROTOCOL_VERSION);
                let version = if SUPPORTED_VERSIONS.contains(&asked) { asked } else { PROTOCOL_VERSION };
                Ok(json!({
                    "protocolVersion": version,
                    "capabilities": {"tools": {"listChanged": false}, "resources": {"listChanged": false, "subscribe": false}, "prompts": {"listChanged": false}},
                    "serverInfo": {"name": "gridcraft", "title": "GridCraft", "version": env!("CARGO_PKG_VERSION")},
                    "instructions": format!("{INSTRUCTIONS} Backend: {}.", self.backend.describe()),
                }))
            }
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({"tools": all_definitions()})),
            "resources/list" => Ok(json!({"resources": [
                {"uri": WORKBOOK_URI, "name": "workbook", "title": "Active workbook", "description": "Sheets, used ranges, tables, charts, names, selection and undo history of the active workbook (document.inspect)", "mimeType": "application/json"},
                {"uri": COMMANDS_URI, "name": "commands", "title": "Commands", "description": "Every command: id, label, ribbon path, shortcut, params, enabled (engine.commands)", "mimeType": "application/json"},
                {"uri": FUNCTIONS_URI, "name": "functions", "title": "Worksheet functions", "description": "Every worksheet function: name, category, signature, description (formulas.functions)", "mimeType": "application/json"},
            ]})),
            "resources/templates/list" => Ok(json!({"resourceTemplates": []})),
            "resources/read" => {
                let uri = params.get("uri").and_then(Value::as_str).ok_or((INVALID_PARAMS, "missing `uri`".to_string()))?;
                let v = match uri {
                    WORKBOOK_URI => self.backend.call("document.inspect", json!({})),
                    COMMANDS_URI => self.backend.call("engine.commands", json!({})),
                    FUNCTIONS_URI => self.backend.call("engine.execute", json!({"command": "formulas.functions", "params": {}})),
                    _ => return Err((RESOURCE_NOT_FOUND, format!("resource not found: {uri}"))),
                }
                .map_err(|e| (INTERNAL_ERROR, e))?;
                Ok(json!({"contents": [{"uri": uri, "mimeType": "application/json", "text": serde_json::to_string_pretty(&v).unwrap_or_default()}]}))
            }
            "prompts/list" => Ok(json!({"prompts": []})),
            "prompts/get" => Err((INVALID_PARAMS, "no prompts are defined".to_string())),
            "logging/setLevel" => Ok(json!({})),
            "completion/complete" => Ok(json!({"completion": {"values": [], "hasMore": false}})),
            other => Err((METHOD_NOT_FOUND, format!("method not found: {other}"))),
        }
    }
}
