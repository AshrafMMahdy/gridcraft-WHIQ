//! WHIQ: the tools the platform needs on top of upstream's, and the contract pipeline every
//! `tools/call` goes through ([`run_tool`]): forgiving names and arguments, `caller` identity,
//! `overwrite`, eviction, errors with a code / reason / next step, trimmed results and
//! `structuredContent`.

use std::path::{Path, PathBuf};

use gridcraft_engine::core::{CellRef, RangeRef};
use serde_json::{Map, Value, json};

use crate::backend::{Backend, DocSnapshot};
use crate::contract::{self, ToolError};
use crate::tools::{Args, Hints, ToolResult, check_arguments, exec, pick};

// ---------------------------------------------------------------- settings and state

/// Limits and identity for one server.
#[derive(Clone, Debug)]
pub struct Settings {
    /// `--author NAME`: who the work is done for, unless a call names its own `caller`.
    pub author: Option<String>,
    /// Results above this many bytes of JSON are trimmed with a continue marker.
    pub max_result_bytes: usize,
    /// read_range returns at most this many cells per call (the rest via `continue_from`).
    pub max_read_cells: u64,
    /// `GRIDCRAFT_MCP_TEST_HOOKS=1`: the `debug_sleep` / `debug_panic` tools exist (contract tests).
    pub test_hooks: bool,
}

impl Default for Settings {
    fn default() -> Self {
        Self { author: None, max_result_bytes: contract::MAX_RESULT_BYTES, max_read_cells: 10_000, test_hooks: test_hooks_from_env() }
    }
}

/// `GRIDCRAFT_MCP_TEST_HOOKS=1` turns on the tools the contract tests use to provoke a slow call
/// or a panic.
pub fn test_hooks_from_env() -> bool {
    std::env::var("GRIDCRAFT_MCP_TEST_HOOKS").is_ok_and(|v| v == "1")
}

/// Why the open document was closed, and where its work went.
#[derive(Clone, Debug, PartialEq)]
pub struct Eviction {
    pub reason: String,
    /// The auto-saved copy (`<name>.recovered-<stamp>.xlsx`), when there was unsaved work.
    pub recovered: Option<PathBuf>,
    /// The file the document came from, if any.
    pub original: Option<String>,
}

impl Eviction {
    fn error(&self) -> ToolError {
        let (reason, next) = match (&self.recovered, &self.original) {
            (Some(r), _) => (
                format!("{}; your work is auto-saved at {}", self.reason, r.display()),
                format!("call open_workbook with path \"{}\" and carry on", r.display()),
            ),
            (None, Some(o)) => (format!("{}; it had no unsaved changes", self.reason), format!("call open_workbook with path \"{o}\" and carry on")),
            (None, None) => (format!("{}; it had no unsaved changes", self.reason), "call new_workbook or open_workbook to carry on".to_string()),
        };
        let mut e = ToolError::new("evicted", reason, next);
        if let Some(r) = &self.recovered {
            e = e.with("recovered_path", json!(r.display().to_string()));
        }
        if let Some(o) = &self.original {
            e = e.with("original_path", json!(o));
        }
        e
    }
}

/// What a server keeps between calls.
#[derive(Debug, Default)]
pub struct CallState {
    pub settings: Settings,
    /// Set when the document was evicted; cleared by open_workbook / new_workbook / build_workbook.
    pub evicted: Option<Eviction>,
    /// Said once, on the next successful call (e.g. recovered files found at start).
    pub notices: Vec<String>,
    /// Warnings about this call found before it ran (a tool name read in another spelling).
    pub call_warnings: Vec<String>,
}

// ---------------------------------------------------------------- definitions

fn string(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn boolean(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}
#[allow(dead_code)]
fn number(desc: &str) -> Value {
    json!({"type": "number", "description": desc})
}
fn int(desc: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": desc})
}
fn obj(props: Value, required: &[&str]) -> Value {
    let mut o = json!({"type": "object", "properties": props, "additionalProperties": false});
    if !required.is_empty() {
        o["required"] = json!(required);
    }
    o
}
fn tool(name: &str, title: &str, desc: &str, schema: Value, hints: Hints) -> Value {
    json!({
        "name": name,
        "title": title,
        "description": desc,
        "inputSchema": schema,
        "annotations": {
            "title": title,
            "readOnlyHint": hints.read_only,
            "destructiveHint": hints.destructive,
            "idempotentHint": hints.idempotent,
            "openWorldHint": false,
        },
    })
}

/// Tools that write files: they take `overwrite`.
pub(crate) const WRITES_FILES: &[&str] = &["save_workbook", "export_pdf", "render_range", "render_sheet", "build_workbook", "screenshot"];

/// Tools that work without an open document (they stay available after an eviction).
const NEEDS_NO_DOCUMENT: &[&str] =
    &["open_workbook", "new_workbook", "build_workbook", "server_status", "job_status", "list_commands", "list_functions", "debug_sleep"];

/// Tools that run on the dispatcher thread, never queued behind document work.
pub const CONTROL_TOOLS: &[&str] = &["server_status", "job_status"];

/// The WHIQ tools (definitions only; the global arguments are added by [`with_globals`]).
pub fn definitions() -> Vec<Value> {
    let mut v = vec![
        tool(
            "server_status",
            "Server status",
            "Health of this GridCraft server: version, memory in use, the open document (title, path, unsaved changes, sheets, \
             cells), jobs running or finished, whether a call is in progress, the read/write roots, the author, and any \
             recovered (auto-saved) files waiting to be reopened. Always answers at once, even while a long call runs.",
            obj(json!({}), &[]),
            crate::tools::READ,
        ),
        tool(
            "job_status",
            "Job status",
            "Progress and result of a job. Long work (opening a very large file, a full recalculation, exporting or rendering many \
             pages, or any call that runs past the call timeout) answers with a job_id at once; poll this tool with it. When the \
             job is done the result is here (it survives client timeouts). Without job_id: every job this server knows.",
            obj(json!({"job_id": string("The job id a call returned, e.g. \"job-3\"")}), &[]),
            crate::tools::READ,
        ),
    ];
    v.extend(more_definitions());
    v
}

/// The authoring, rendering and editing tools (later WHIQ steps).
fn more_definitions() -> Vec<Value> {
    vec![]
}

/// The WHIQ tools that upstream's dispatcher doesn't know.
pub(crate) fn dispatch_extra(_b: &mut dyn Backend, name: &str, _a: &Args) -> Result<Value, String> {
    Err(format!("unknown tool `{name}`"))
}

/// Upstream's and the WHIQ tools with the test hooks when enabled.
pub fn all_definitions() -> Vec<Value> {
    let mut defs = crate::tools::tool_definitions();
    if test_hooks_from_env() {
        defs.extend(with_globals(vec![
            tool(
                "debug_sleep",
                "Debug: sleep",
                "Contract-test hook: hold the document for `ms` milliseconds.",
                obj(json!({"ms": int("Milliseconds")}), &[]),
                crate::tools::READ,
            ),
            tool("debug_panic", "Debug: panic", "Contract-test hook: panic inside a call.", obj(json!({}), &[]), crate::tools::READ),
        ]));
    }
    defs
}

/// Adds `caller` and `async` to every tool, `overwrite` to the ones that write files, and
/// `offset`/`limit` to the list tools.
pub fn with_globals(mut tools: Vec<Value>) -> Vec<Value> {
    for t in &mut tools {
        let name = t["name"].as_str().unwrap_or("").to_string();
        let Some(props) = t["inputSchema"]["properties"].as_object_mut() else { continue };
        props.insert(
            "caller".into(),
            string("Who this call is for (a person or agent name); tracked changes, comments and the file's last-modified-by carry it. Default: --author"),
        );
        if !CONTROL_TOOLS.contains(&name.as_str()) {
            props
                .insert("async".into(), boolean("Run as a job: answer at once with a job_id and poll job_status (default: automatic for long work)"));
        }
        if WRITES_FILES.contains(&name.as_str()) {
            props.insert(
                "overwrite".into(),
                boolean("Replace an existing file at the destination. Default false: the existing file is renamed OLD_<name> first and the result says so"),
            );
        }
        if matches!(name.as_str(), "list_commands" | "list_functions") {
            props.insert("offset".into(), int("Skip this many items (continue_from of a trimmed result)"));
            props.insert("limit".into(), int("Return at most this many items"));
        }
        if name == "read_range" {
            props.insert("max_cells".into(), int("Most cells to return in one call (default 10000); the rest is reached with continue_from"));
        }
    }
    tools
}

// ---------------------------------------------------------------- the pipeline

/// Runs one tool through the whole contract. `name` must be a known tool (resolve it with
/// [`contract::normalize_tool_name`] first).
pub fn run_tool(b: &mut dyn Backend, st: &mut CallState, name: &str, args: &Value) -> ToolResult {
    let mut warnings = std::mem::take(&mut st.call_warnings);
    let defs = all_definitions();
    let Some(def) = defs.iter().find(|d| d["name"] == name) else {
        let names: Vec<&str> = defs.iter().filter_map(|d| d["name"].as_str()).collect();
        let near = contract::closest(name, names.iter().copied(), 3);
        return ToolResult::failure(
            &ToolError::new(
                "not_found",
                format!("there is no tool `{name}`"),
                "call tools/list to see the tools; the closest are listed in `closest`",
            )
            .with("closest", json!(near)),
        );
    };
    let empty = Map::new();
    let raw = match args {
        Value::Object(o) => o,
        Value::Null => &empty,
        _ => {
            return ToolResult::failure(&ToolError::new(
                "invalid_argument",
                "tool arguments must be a JSON object",
                format!("pass the arguments as an object, e.g. {}", example_args(def)),
            ));
        }
    };
    let (a, mut fixes) = contract::normalize_args(&def["inputSchema"], raw);
    warnings.append(&mut fixes);
    if let Err(e) = check_arguments(name, &Value::Object(a.clone())) {
        return ToolResult::failure(&invalid_argument(def, &e));
    }
    if let Some(ev) = &st.evicted
        && !NEEDS_NO_DOCUMENT.contains(&name)
    {
        return ToolResult::failure(&ev.error());
    }
    let caller = raw.get("caller").and_then(Value::as_str).map(str::to_string).or_else(|| st.settings.author.clone());
    let overwrite = raw.get("overwrite").is_some_and(|v| v.as_bool() == Some(true) || v.as_str().is_some_and(|s| s.eq_ignore_ascii_case("true")));
    let previous_user = caller.as_deref().and_then(|c| b.set_user(c));
    if let Some(c) = &caller
        && WRITES_FILES.contains(&name)
    {
        b.stamp_author(c);
    }
    let _ = gridcraft_engine::io::save_policy::take_reports();
    let r = gridcraft_engine::io::save_policy::allow_overwrite(overwrite, || dispatch(b, st, name, &a, &mut warnings));
    if let Some(p) = previous_user {
        b.set_user(&p);
    }
    let reports = gridcraft_engine::io::save_policy::take_reports();
    match r {
        Ok(v) => {
            if matches!(name, "open_workbook" | "new_workbook" | "build_workbook") {
                st.evicted = None;
            }
            let mut v = contract::trim_result(v, st.settings.max_result_bytes);
            warnings.append(&mut st.notices);
            if !v.is_object() && (!reports.is_empty() || !warnings.is_empty()) {
                v = json!({"result": v});
            }
            if !reports.is_empty() {
                let saved: Vec<Value> = reports
                    .iter()
                    .map(|r| json!({"path": r.path.display().to_string(), "previous_renamed_to": r.previous_renamed_to.as_ref().map(|p| p.display().to_string())}))
                    .collect();
                for r in &reports {
                    if let Some(old) = &r.previous_renamed_to {
                        warnings.push(format!(
                            "{} already existed: it was renamed to {} (pass overwrite: true to replace a file instead)",
                            r.path.display(),
                            old.display()
                        ));
                    }
                }
                v["saved"] = Value::Array(saved);
            }
            if !warnings.is_empty() {
                match v.get_mut("warnings").and_then(Value::as_array_mut) {
                    Some(list) => list.extend(warnings.into_iter().map(Value::String)),
                    None => v["warnings"] = json!(warnings),
                }
            }
            ToolResult::json(&v)
        }
        Err(e) => ToolResult::failure(&e),
    }
}

/// An example of a tool's arguments from its schema: the required ones with placeholders.
fn example_args(def: &Value) -> String {
    let schema = &def["inputSchema"];
    let mut o = Map::new();
    for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        let p = &schema["properties"][r];
        let ex = match p["type"].as_str() {
            Some("array") => json!([]),
            Some("boolean") => json!(true),
            Some("number" | "integer") => json!(1),
            Some("object") => json!({}),
            _ => match p["enum"].as_array().and_then(|e| e.first()) {
                Some(first) => first.clone(),
                None => json!(format!("<{r}>")),
            },
        };
        o.insert(r.to_string(), ex);
    }
    Value::Object(o).to_string()
}

fn invalid_argument(def: &Value, msg: &str) -> ToolError {
    let props: Vec<&str> = def["inputSchema"]["properties"].as_object().map(|p| p.keys().map(String::as_str).collect()).unwrap_or_default();
    ToolError::new(
        "invalid_argument",
        msg,
        format!("call {} again with arguments like {}", def["name"].as_str().unwrap_or("the tool"), example_args(def)),
    )
    .with("accepted_arguments", json!(props))
}

fn dispatch(b: &mut dyn Backend, st: &mut CallState, name: &str, a: &Args, warnings: &mut Vec<String>) -> Result<Value, ToolError> {
    match name {
        "server_status" => Ok(status_value(b.snapshot().as_ref(), b.roots(), &st.settings, json!({"mode": "synchronous", "jobs": []}))),
        "job_status" => Err(ToolError::new(
            "not_found",
            "this server runs calls synchronously and has no jobs",
            "the result of every call is in its own answer; jobs exist under `gridcraft-cli mcp`",
        )),
        "debug_sleep" => {
            let ms = a.get("ms").and_then(Value::as_u64).unwrap_or(1000).min(600_000);
            std::thread::sleep(std::time::Duration::from_millis(ms));
            Ok(json!({"slept_ms": ms}))
        }
        "debug_panic" => provoked_panic(),
        "read_range" => read_range(b, st, a),
        "list_commands" => {
            let v = b.call("engine.commands", pick(a, &["search"])).map_err(|e| classify(b, name, a, &e))?;
            Ok(paged(v, a))
        }
        "list_functions" => {
            let v = exec(b, "formulas.functions", pick(a, &["search", "category"])).map_err(|e| classify(b, name, a, &e))?;
            Ok(paged(v, a))
        }
        "execute_command" => execute_command(b, a, warnings),
        _ => crate::tools::dispatch(b, name, a).map_err(|e| classify(b, name, a, &e)),
    }
}

/// The contract tests' way to prove a panic inside a call loses no work (test hooks only).
#[allow(clippy::panic)]
fn provoked_panic() -> Result<Value, ToolError> {
    panic!("debug_panic: provoked by the contract tests")
}

fn paged(v: Value, a: &Args) -> Value {
    let offset = a.get("offset").and_then(Value::as_u64).unwrap_or(0) as usize;
    let limit = a.get("limit").and_then(Value::as_u64).map(|l| l as usize);
    contract::page(v, offset, limit)
}

/// execute_command with the command id matched in any spelling.
fn execute_command(b: &mut dyn Backend, a: &Args, warnings: &mut Vec<String>) -> Result<Value, ToolError> {
    let cmd = a.get("command").and_then(Value::as_str).unwrap_or("").to_string();
    let p = match a.get("params") {
        None | Some(Value::Null) => json!({}),
        Some(v @ Value::Object(_)) => v.clone(),
        Some(_) => return Err(ToolError::new("invalid_argument", "`params` must be an object", "pass params as {\"key\": value}")),
    };
    match exec(b, &cmd, p.clone()) {
        Ok(v) => Ok(v),
        Err(e) if e.contains("unknown command") => {
            let ids = command_ids(b);
            let sq = contract::squash(&cmd);
            if let Some(id) = ids.iter().find(|i| contract::squash(i) == sq) {
                warnings.push(format!("command `{cmd}` was read as `{id}`"));
                let id = id.clone();
                return exec(b, &id, p).map_err(|e| classify(b, "execute_command", a, &e));
            }
            Err(classify(b, "execute_command", a, &e))
        }
        Err(e) => Err(classify(b, "execute_command", a, &e)),
    }
}

fn command_ids(b: &mut dyn Backend) -> Vec<String> {
    b.call("engine.commands", json!({}))
        .ok()
        .and_then(|v| v.as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|c| c["id"].as_str().map(str::to_string))
        .collect()
}

fn sheet_names(b: &mut dyn Backend) -> Vec<String> {
    b.call("document.inspect", json!({}))
        .ok()
        .and_then(|v| v["sheets"].as_array().cloned())
        .unwrap_or_default()
        .iter()
        .filter_map(|s| s["name"].as_str().map(str::to_string))
        .collect()
}

/// `Sheet2!A1:B3` → (`Some("Sheet2")`, `"A1:B3"`); quotes are removed from the sheet name.
pub(crate) fn split_sheet(r: &str) -> (Option<String>, &str) {
    match r.rsplit_once('!') {
        Some((s, rest)) => {
            let s = s.trim();
            let s = s.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')).map(|x| x.replace("''", "'")).unwrap_or_else(|| s.to_string());
            (Some(s), rest)
        }
        None => (None, r),
    }
}

/// `Sheet 2` → `'Sheet 2'`; plain names as they are.
pub(crate) fn quote_sheet(s: &str) -> String {
    if s.chars().all(|c| c.is_alphanumeric() || c == '_' || c == '.') && !s.chars().next().is_some_and(|c| c.is_ascii_digit()) {
        s.to_string()
    } else {
        format!("'{}'", s.replace('\'', "''"))
    }
}

/// read_range that never floods the caller: at most `max_cells` cells per call; the answer says
/// `truncated`, `continue_from` (the first cell not returned) and `next_range` to ask for next.
fn read_range(b: &mut dyn Backend, st: &CallState, a: &Args) -> Result<Value, ToolError> {
    let cap = a.get("max_cells").and_then(Value::as_u64).map(|n| n.clamp(1, 1_000_000)).unwrap_or(st.settings.max_read_cells).max(1);
    let mut p = pick(a, &["range", "sheet", "formulas", "formatted"]);
    let given = a.get("range").and_then(Value::as_str).map(str::to_string);
    let (prefix, rest) = match &given {
        Some(g) => {
            let (s, r) = split_sheet(g);
            (s, r.to_string())
        }
        None => (None, String::new()),
    };
    let info = b.call("document.inspect", json!({})).map_err(|e| classify(b, "read_range", a, &e))?;
    let sheets = info["sheets"].as_array().cloned().unwrap_or_default();
    let wanted = prefix.clone().or_else(|| a.get("sheet").and_then(Value::as_str).map(str::to_string));
    let sheet = match &wanted {
        Some(w) => sheets.iter().find(|s| s["name"].as_str().is_some_and(|n| n.eq_ignore_ascii_case(w))),
        None => info["activeSheet"].as_u64().and_then(|i| sheets.get(i as usize)),
    };
    let Some(sheet) = sheet else {
        let names: Vec<&str> = sheets.iter().filter_map(|s| s["name"].as_str()).collect();
        return Err(ToolError::new(
            "not_found",
            format!("there is no sheet `{}`", wanted.unwrap_or_default()),
            "use one of the sheets in `closest` (inspect_workbook lists them all)",
        )
        .with("closest", json!(contract::closest(&prefix.unwrap_or_default(), names.iter().copied(), 5)))
        .with("sheets", json!(names)));
    };
    let sheet_name = sheet["name"].as_str().unwrap_or("").to_string();
    let used = sheet["usedRange"].as_str().and_then(RangeRef::parse);
    let requested = if given.is_some() { RangeRef::parse(&rest) } else { used };
    let Some(mut r) = requested else {
        // Unparseable (the engine words the error) or an empty sheet.
        return b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e));
    };
    if r.count() > cap
        && let Some(u) = used
    {
        r = match r.intersection(&u) {
            Some(x) => x,
            None => return Ok(json!({"range": r.a1(), "sheet": sheet_name, "values": [], "note": "the range holds no data"})),
        };
    }
    if r.count() <= cap {
        return b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e));
    }
    let width = u64::from(r.width());
    let part = if width > cap {
        RangeRef::new(r.start, CellRef::new(r.start.row, r.start.col.saturating_add(cap as u32 - 1)))
    } else {
        let rows = (cap / width).max(1) as u32;
        RangeRef::new(r.start, CellRef::new(r.start.row.saturating_add(rows - 1).min(r.end.row), r.end.col))
    };
    let (next_start, next_range) = if width > cap {
        let c = CellRef::new(part.start.row, part.end.col + 1);
        (c, RangeRef::new(c, CellRef::new(part.start.row, r.end.col)))
    } else {
        let c = CellRef::new(part.end.row + 1, r.start.col);
        (c, RangeRef::new(c, r.end))
    };
    let q = quote_sheet(&sheet_name);
    p["range"] = json!(format!("{q}!{}", part.a1()));
    if let Some(o) = p.as_object_mut() {
        o.remove("sheet");
    }
    let mut v = b.call("sheet.read", p).map_err(|e| classify(b, "read_range", a, &e))?;
    let next = format!("{q}!{}", next_range.a1());
    v["truncated"] = json!(true);
    v["total_cells"] = json!(r.count());
    v["returned_cells"] = json!(part.count());
    v["continue_from"] = json!(format!("{q}!{}", next_start.a1()));
    v["next_range"] = json!(next);
    v["next_step"] = json!(format!("call read_range with range \"{next}\" for the next part"));
    Ok(v)
}

// ---------------------------------------------------------------- errors

/// Turns an engine or backend message into an error the agent can act on.
pub fn classify(b: &mut dyn Backend, tool: &str, a: &Args, msg: &str) -> ToolError {
    let m = msg.to_ascii_lowercase();
    let msg = format!("{tool}: {msg}");
    let path = a.get("path").and_then(Value::as_str).unwrap_or("");
    let (read_root, write_root) = b.roots();
    if m.contains("automation path rejected") || m.contains("automation filesystem access is not granted") {
        let root = if m.contains("read") { read_root.clone().or(write_root.clone()) } else { write_root.clone().or(read_root.clone()) };
        let name = Path::new(path).file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_else(|| "book.xlsx".into());
        let next = match &root {
            Some(r) => format!("use a path inside {}: e.g. the relative name \"{name}\" resolves to {}", r.display(), r.join(&name).display()),
            None => "this server has no root for that kind of access; ask the operator".to_string(),
        };
        let mut e = ToolError::new("outside_root", msg, next);
        if let Some(r) = read_root {
            e = e.with("read_root", json!(r.display().to_string()));
        }
        if let Some(w) = write_root {
            e = e.with("write_root", json!(w.display().to_string()));
        }
        return e;
    }
    if m.contains("no ui in headless mode") {
        return ToolError::new("no_ui", msg, "use render_range or render_sheet for a picture of the cells, or connect to the desktop app");
    }
    if m.contains(&gridcraft_engine::io::WRONG_PASSWORD.to_ascii_lowercase()) {
        return ToolError::new("wrong_password", msg, "call open_workbook again with the right `password`");
    }
    if m.contains("password-protected") {
        return ToolError::new("password_required", msg, "call open_workbook again with `password`");
    }
    if m.contains("unknown command") {
        let wanted = a.get("command").and_then(Value::as_str).unwrap_or("");
        let ids = command_ids(b);
        let near = contract::closest(wanted, ids.iter().map(String::as_str), 5);
        return ToolError::new("not_found", msg, "use one of the command ids in `closest`, or list_commands with a search word")
            .with("closest", json!(near));
    }
    if m.contains("unknown sheet") || m.contains("no sheet") || (m.contains("sheet") && m.contains("not found")) {
        let names = sheet_names(b);
        let wanted = a
            .get("sheet")
            .and_then(Value::as_str)
            .map(str::to_string)
            .or_else(|| a.get("range").or(a.get("cell")).and_then(Value::as_str).and_then(|r| split_sheet(r).0))
            .unwrap_or_default();
        let near = contract::closest(&wanted, names.iter().map(String::as_str), 5);
        return ToolError::new("not_found", msg, "use one of the sheet names in `sheets`").with("closest", json!(near)).with("sheets", json!(names));
    }
    if m.contains("no such file") || m.contains("os error 2") || m.contains("cannot find the file") || m.contains("cannot find the path") {
        let near = nearby_files(path);
        return ToolError::new(
            "not_found",
            msg,
            if near.is_empty() {
                "check the path; relative paths resolve inside the read root".to_string()
            } else {
                "did you mean one of the files in `closest`?".to_string()
            },
        )
        .with("closest", json!(near));
    }
    if m.contains("no workbook open") {
        return ToolError::new("no_document", msg, "call new_workbook, open_workbook or build_workbook first");
    }
    if m.contains("is not available right now") {
        return ToolError::new("not_available", msg, "inspect_workbook shows the current state (selection, sheets); change it and try again");
    }
    if m.contains("too large") || m.contains("too many") || m.contains("too tall") || m.contains("larger than") || m.contains("read it in parts") {
        return ToolError::new("too_large", msg, "ask for a smaller part: a range like A1:D500, fewer rows, or fewer pages");
    }
    if m.contains("internal error") {
        return ToolError::new(
            "internal_error",
            msg,
            "the workbook was kept as it was before this call; try again with simpler input or another way",
        );
    }
    if m.contains("not reachable") || m.contains("control channel closed") {
        return ToolError::new("unavailable", msg, "start the app with `gridcraft --control PORT`, or use the headless server");
    }
    if m.contains("invalid parameters")
        || m.contains("missing")
        || m.contains("must be")
        || m.contains("invalid")
        || m.contains("expected")
        || m.contains("not a valid")
        || m.contains("unknown")
        || m.contains("bad ")
    {
        let next = if tool == "execute_command" {
            let cmd = a.get("command").and_then(Value::as_str).unwrap_or("");
            format!("list_commands with search \"{cmd}\" shows the command's parameters; fix them and call again")
        } else {
            "fix the arguments named in the reason and call again (tools/list has each tool's schema)".to_string()
        };
        return ToolError::new("invalid_argument", msg, next);
    }
    ToolError::new("failed", msg, "check the arguments against the tool's schema; inspect_workbook shows the current state")
}

/// Files next to `path` whose names are close to its name (only inside the read root, if any).
fn nearby_files(path: &str) -> Vec<String> {
    if path.is_empty() {
        return vec![];
    }
    let p = Path::new(path);
    let dir = match p.parent() {
        Some(d) if !d.as_os_str().is_empty() => d.to_path_buf(),
        _ => match gridcraft_engine::file_access::current().and_then(|r| r.read_root().map(Path::to_path_buf)) {
            Some(r) => r,
            None => PathBuf::from("."),
        },
    };
    let dir_text = dir.to_string_lossy().into_owned();
    if gridcraft_engine::file_access::check_read(&dir_text).is_err() {
        return vec![];
    }
    let Some(name) = p.file_name().map(|n| n.to_string_lossy().into_owned()) else { return vec![] };
    let names: Vec<String> = std::fs::read_dir(&dir)
        .map(|it| it.filter_map(|e| e.ok()).take(5000).map(|e| e.file_name().to_string_lossy().into_owned()).collect())
        .unwrap_or_default();
    contract::closest(&name, names.iter().map(String::as_str), 5).into_iter().map(|n| dir.join(n).display().to_string()).collect()
}

// ---------------------------------------------------------------- status

/// Resident memory of this process in bytes (Linux), else `None`.
pub fn rss_bytes() -> Option<u64> {
    let text = std::fs::read_to_string("/proc/self/statm").ok()?;
    let pages: u64 = text.split_whitespace().nth(1)?.parse().ok()?;
    Some(pages.saturating_mul(4096))
}

/// What server_status answers. `extra` adds the runtime's part (jobs, busy, limits).
pub fn status_value(snap: Option<&DocSnapshot>, roots: (Option<PathBuf>, Option<PathBuf>), settings: &Settings, extra: Value) -> Value {
    let doc = snap.map(|s| {
        json!({
            "title": s.title,
            "path": s.path,
            "unsaved_changes": s.dirty,
            "sheets": s.sheets,
            "cells": s.cells,
        })
    });
    let (read, write) = roots;
    let mut v = json!({
        "server": "gridcraft",
        "version": env!("CARGO_PKG_VERSION"),
        "whiq": true,
        "memory": {"rss_bytes": rss_bytes()},
        "document": doc,
        "roots": {"read": read.map(|p| p.display().to_string()), "write": write.map(|p| p.display().to_string())},
        "author": settings.author,
        "limits": {"max_result_bytes": settings.max_result_bytes, "max_read_cells": settings.max_read_cells},
    });
    if let (Some(o), Value::Object(e)) = (v.as_object_mut(), extra) {
        for (k, x) in e {
            o.insert(k, x);
        }
    }
    v
}
