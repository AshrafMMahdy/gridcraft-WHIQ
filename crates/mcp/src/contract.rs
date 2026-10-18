//! WHIQ: the "no dead stops" contract for tool calls.
//!
//! - Every error is a [`ToolError`]: a `code`, a one-line `reason` and a `next_step` an agent can
//!   act on, plus extra fields (`retry_after_ms`, `closest`, `recovered_path`…). It reaches the
//!   caller as an `isError` tool result whose `structuredContent` is `{"error": {…}}`.
//! - Inputs are forgiving where that is safe ([`normalize_tool_name`], [`normalize_args`]): tool
//!   names and argument keys match case-insensitively in any spelling (`readRange`,
//!   `read-range`), obvious synonyms are accepted (`file` for `path`), scalar types are coerced
//!   (`"true"`, `"12"`), enum values match without regard to case, and unknown arguments are
//!   dropped with a warning instead of failing the call.
//! - Oversized results are trimmed ([`trim_result`]) with `truncated: true` and a `continue_from`
//!   the agent passes back to get the rest.

use serde_json::{Map, Value, json};

/// Arguments every tool accepts besides its own: the same in every craft app (CONTRACT-PARITY.md).
pub const GLOBAL_ARGS: &[&str] = &["caller", "async", "overwrite", "continue_from", "max_bytes"];

/// The error codes an agent can branch on: the same set in WordCraft, GridCraft and DeckCraft
/// (CONTRACT-PARITY.md). Every error a tool answers carries one of these.
pub mod code {
    pub const BUSY: &str = "busy";
    pub const CANCELLED: &str = "cancelled";
    pub const DISK_FULL: &str = "disk_full";
    pub const EVICTED: &str = "evicted";
    pub const FAILED: &str = "failed";
    pub const INTERNAL: &str = "internal";
    pub const INVALID_ARGUMENT: &str = "invalid_argument";
    pub const INVALID_REQUEST: &str = "invalid_request";
    pub const LOCKED: &str = "locked";
    pub const NO_DOCUMENT: &str = "no_document";
    pub const NOT_AVAILABLE: &str = "not_available";
    pub const NOT_FOUND: &str = "not_found";
    pub const OUT_OF_RANGE: &str = "out_of_range";
    pub const OUTSIDE_ROOT: &str = "outside_root";
    pub const PASSWORD_REQUIRED: &str = "password_required";
    pub const TOO_LARGE: &str = "too_large";
    pub const UNSUPPORTED: &str = "unsupported";
    pub const WRONG_PASSWORD: &str = "wrong_password";
    /// Every code, sorted.
    pub const ALL: &[&str] = &[
        BUSY,
        CANCELLED,
        DISK_FULL,
        EVICTED,
        FAILED,
        INTERNAL,
        INVALID_ARGUMENT,
        INVALID_REQUEST,
        LOCKED,
        NO_DOCUMENT,
        NOT_AVAILABLE,
        NOT_FOUND,
        OUT_OF_RANGE,
        OUTSIDE_ROOT,
        PASSWORD_REQUIRED,
        TOO_LARGE,
        UNSUPPORTED,
        WRONG_PASSWORD,
    ];
}

/// Results above this many bytes of JSON are trimmed.
pub const MAX_RESULT_BYTES: usize = 256 * 1024;

/// A tool failure the caller can act on.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolError {
    pub code: String,
    pub reason: String,
    pub next_step: String,
    pub extra: Map<String, Value>,
}

impl ToolError {
    pub fn new(code: &str, reason: impl Into<String>, next_step: impl Into<String>) -> Self {
        Self { code: code.to_string(), reason: one_line(&reason.into()), next_step: next_step.into(), extra: Map::new() }
    }

    /// Adds an extra field (`retry_after_ms`, `closest`, `recovered_path`…).
    pub fn with(mut self, key: &str, value: Value) -> Self {
        self.extra.insert(key.to_string(), value);
        self
    }

    pub fn to_value(&self) -> Value {
        let mut o = Map::new();
        o.insert("code".into(), json!(self.code));
        o.insert("reason".into(), json!(self.reason));
        o.insert("next_step".into(), json!(self.next_step));
        for (k, v) in &self.extra {
            o.insert(k.clone(), v.clone());
        }
        Value::Object(o)
    }

    /// The text block shown to the model (the same form in every craft app).
    pub fn text(&self) -> String {
        format!("[{}] {}\nnext_step: {}", self.code, self.reason, self.next_step)
    }
}

/// A message on one line (errors from the engine can span several).
fn one_line(s: &str) -> String {
    let t: Vec<&str> = s.split(['\n', '\r']).map(str::trim).filter(|l| !l.is_empty()).collect();
    t.join(" ")
}

// ---------------------------------------------------------------- matching

/// Lower-case ASCII letters and digits only: `Read-Range` and `read_range` both give `readrange`.
pub fn squash(s: &str) -> String {
    s.chars().filter(char::is_ascii_alphanumeric).map(|c| c.to_ascii_lowercase()).collect()
}

/// Edit distance (insertions, deletions, substitutions), capped for long inputs.
pub fn distance(a: &str, b: &str) -> usize {
    let a: Vec<char> = a.chars().take(64).collect();
    let b: Vec<char> = b.chars().take(64).collect();
    let mut prev: Vec<usize> = (0..=b.len()).collect();
    for (i, ca) in a.iter().enumerate() {
        let mut cur = vec![i + 1; b.len() + 1];
        for (j, cb) in b.iter().enumerate() {
            let sub = prev.get(j).copied().unwrap_or(0) + usize::from(ca != cb);
            let del = prev.get(j + 1).copied().unwrap_or(0) + 1;
            let ins = cur.get(j).copied().unwrap_or(0) + 1;
            if let Some(slot) = cur.get_mut(j + 1) {
                *slot = sub.min(del).min(ins);
            }
        }
        prev = cur;
    }
    prev.last().copied().unwrap_or(0)
}

/// Up to `n` of `candidates` closest to `wanted` (substring matches first, then edit distance).
pub fn closest<'a>(wanted: &str, candidates: impl IntoIterator<Item = &'a str>, n: usize) -> Vec<String> {
    let w = squash(wanted);
    let mut scored: Vec<(usize, String)> = candidates
        .into_iter()
        .map(|c| {
            let s = squash(c);
            let score = if s == w {
                0
            } else if !w.is_empty() && (s.contains(&w) || w.contains(&s)) {
                1
            } else {
                2 + distance(&w, &s)
            };
            (score, c.to_string())
        })
        .collect();
    scored.sort();
    let limit = (w.len() / 2).max(3) + 2;
    scored.into_iter().filter(|(s, _)| *s <= limit).take(n).map(|(_, c)| c).collect()
}

/// Other names agents use for a tool.
const TOOL_ALIASES: &[(&str, &str)] = &[
    ("status", "server_status"),
    ("serverinfo", "server_status"),
    ("health", "server_status"),
    ("job", "job_status"),
    ("getjob", "job_status"),
    ("jobresult", "job_status"),
    ("open", "open_workbook"),
    ("openfile", "open_workbook"),
    ("load", "open_workbook"),
    ("save", "save_workbook"),
    ("saveas", "save_workbook"),
    ("savefile", "save_workbook"),
    ("new", "new_workbook"),
    ("read", "read_range"),
    ("readcells", "read_range"),
    ("getrange", "read_range"),
    ("getvalues", "read_range"),
    ("write", "write_range"),
    ("writecells", "write_range"),
    ("setrange", "write_range"),
    ("setvalues", "write_range"),
    ("setcells", "write_range"),
    ("setvalue", "set_cell"),
    ("command", "execute_command"),
    ("run", "execute_command"),
    ("runcommand", "execute_command"),
    ("inspect", "inspect_workbook"),
    ("workbookinfo", "inspect_workbook"),
    ("chart", "insert_chart"),
    ("addchart", "insert_chart"),
    ("createchart", "insert_chart"),
    ("table", "create_table"),
    ("sort", "sort_range"),
    ("format", "format_range"),
    ("eval", "evaluate_formula"),
    ("evaluate", "evaluate_formula"),
    ("pdf", "export_pdf"),
    ("exportpdf", "export_pdf"),
    ("savepdf", "export_pdf"),
    ("png", "render_range"),
    ("render", "render_range"),
    ("renderpng", "render_range"),
    ("screenshotrange", "render_range"),
    ("rendersheetpng", "render_sheet"),
    ("build", "build_workbook"),
    ("createworkbook", "build_workbook"),
    ("recalc", "recalculate"),
    ("calculate", "recalculate"),
    ("calculatenow", "recalculate"),
    ("insertrow", "insert_rows"),
    ("insertcolumn", "insert_columns"),
    ("insertcols", "insert_columns"),
    ("deleterow", "delete_rows"),
    ("deletecolumn", "delete_columns"),
    ("deletecols", "delete_columns"),
    ("renamesheet", "rename_sheet"),
    ("addsheet", "add_sheet"),
    ("insertsheet", "add_sheet"),
    ("newsheet", "add_sheet"),
    ("conditionalformat", "conditional_format"),
    ("conditionalformatting", "conditional_format"),
    ("validation", "data_validation"),
    ("datavalidation", "data_validation"),
];

/// The tool `name` means: exact, then case/spelling-insensitive, then an alias.
pub fn normalize_tool_name(name: &str, known: &[String]) -> Option<String> {
    if known.iter().any(|k| k == name) {
        return Some(name.to_string());
    }
    let s = squash(name);
    if let Some(k) = known.iter().find(|k| squash(k) == s) {
        return Some(k.clone());
    }
    TOOL_ALIASES.iter().find(|(a, _)| *a == s).map(|(_, t)| t.to_string()).filter(|t| known.iter().any(|k| k == t))
}

/// Other names agents give arguments, by squashed name.
const ARG_SYNONYMS: &[(&str, &[&str])] = &[
    ("path", &["file", "filename", "filepath", "file_path", "output", "out", "outputpath", "outpath", "destination", "dest", "target"]),
    ("range", &["address", "ref", "reference", "cells", "area", "rangeaddress", "a1"]),
    ("cell", &["address", "ref", "reference", "celladdress", "at"]),
    ("sheet", &["sheetname", "worksheet", "tab", "sheetid"]),
    ("values", &["data", "rows", "matrix", "cells", "value"]),
    ("input", &["value", "formula", "text", "content"]),
    ("formula", &["expression", "expr", "input"]),
    ("command", &["id", "commandid", "name", "cmd"]),
    ("params", &["parameters", "args", "arguments", "options"]),
    ("type", &["charttype", "kind"]),
    ("job_id", &["id", "job", "jobid"]),
    ("offset", &["continuefrom", "skip", "start"]),
    ("name", &["newname", "title"]),
    ("search", &["query", "filter", "q"]),
];

/// A JSON-Schema property's accepted types.
fn types_of(p: &Value) -> Vec<String> {
    match &p["type"] {
        Value::String(t) => vec![t.clone()],
        Value::Array(ts) => ts.iter().filter_map(Value::as_str).map(str::to_string).collect(),
        _ => vec![],
    }
}

/// Coerce `v` to the property's type where that is unambiguous; `None` keeps it as is.
fn coerce(p: &Value, v: &Value) -> Option<Value> {
    let types = types_of(p);
    let accepts = |t: &str| types.iter().any(|x| x == t);
    if types.is_empty() || types.iter().any(|t| crate::tools::type_matches(t, v)) {
        // Enums: match without regard to case or spelling.
        if let (Some(allowed), Some(s)) = (p["enum"].as_array(), v.as_str())
            && !allowed.iter().any(|a| a.as_str() == Some(s))
        {
            let sq = squash(s);
            return allowed.iter().find(|a| a.as_str().is_some_and(|a| squash(a) == sq)).cloned();
        }
        return None;
    }
    match v {
        Value::String(s) => {
            let t = s.trim();
            if accepts("boolean") {
                match t.to_ascii_lowercase().as_str() {
                    "true" | "yes" | "on" | "1" => return Some(json!(true)),
                    "false" | "no" | "off" | "0" => return Some(json!(false)),
                    _ => {}
                }
            }
            if accepts("integer")
                && let Ok(i) = t.parse::<i64>()
            {
                return Some(json!(i));
            }
            if accepts("number")
                && let Ok(f) = t.parse::<f64>()
                && f.is_finite()
            {
                return Some(json!(f));
            }
            if accepts("array") {
                // "A,B" for a list of names.
                return Some(Value::Array(t.split(',').map(|x| json!(x.trim())).filter(|x| x != "").collect()));
            }
            None
        }
        Value::Number(n) => {
            if accepts("string") {
                return Some(json!(n.to_string()));
            }
            if accepts("integer")
                && let Some(f) = n.as_f64()
                && f.fract() == 0.0
                && f.abs() < 9.0e15
            {
                return Some(json!(f as i64));
            }
            if accepts("boolean") {
                return n.as_f64().map(|f| json!(f != 0.0));
            }
            if accepts("array") {
                return Some(json!([v]));
            }
            None
        }
        Value::Bool(b) => {
            if accepts("string") {
                return Some(json!(if *b { "true" } else { "false" }));
            }
            if accepts("array") {
                return Some(json!([v]));
            }
            None
        }
        Value::Object(_) if accepts("array") => Some(json!([v])),
        _ => None,
    }
}

/// `values` for write_range-like tools: a 2-D array. A flat list is one row, a scalar one cell.
fn as_rows(v: &Value) -> Option<Value> {
    match v {
        Value::Array(rows) if rows.iter().all(Value::is_array) => None,
        Value::Array(cells) if cells.iter().all(|c| !c.is_array() && !c.is_object()) => Some(json!([cells])),
        Value::Array(_) => None,
        Value::Object(_) => None,
        scalar => Some(json!([[scalar]])),
    }
}

/// Fit `args` to the tool's input schema. Returns the arguments to use and the warnings about
/// what was changed (renamed keys, coerced values, dropped unknown keys). Global arguments
/// ([`GLOBAL_ARGS`]) are left out of the result; read them from the original.
pub fn normalize_args(schema: &Value, args: &Map<String, Value>) -> (Map<String, Value>, Vec<String>) {
    let props = schema["properties"].as_object().cloned().unwrap_or_default();
    let mut out = Map::new();
    let mut warnings = Vec::new();
    let by_squash: Vec<(String, String)> = props.keys().map(|k| (squash(k), k.clone())).collect();
    // Exact names first, so a synonym never takes the place of a key given exactly.
    for (k, v) in args {
        if props.contains_key(k) {
            out.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in args {
        if props.contains_key(k) || GLOBAL_ARGS.contains(&k.as_str()) {
            continue;
        }
        let sk = squash(k);
        let target = by_squash.iter().find(|(s, _)| *s == sk).map(|(_, k)| k.clone()).or_else(|| {
            ARG_SYNONYMS
                .iter()
                .filter(|(prop, _)| props.contains_key(*prop))
                .find(|(_, syns)| syns.iter().any(|s| squash(s) == sk))
                .map(|(prop, _)| prop.to_string())
        });
        match target {
            Some(t) if !out.contains_key(&t) => {
                warnings.push(format!("argument `{k}` was read as `{t}`"));
                out.insert(t, v.clone());
            }
            Some(t) => warnings.push(format!("argument `{k}` was ignored: `{t}` is given already")),
            None => {
                let near = closest(k, props.keys().map(String::as_str), 1);
                let hint = near.first().map(|n| format!(" (did you mean `{n}`?)")).unwrap_or_default();
                warnings.push(format!("unknown argument `{k}` was ignored{hint}"));
            }
        }
    }
    for (k, v) in out.iter_mut() {
        let Some(p) = props.get(k) else { continue };
        if k == "values" && p["type"] == "array" && p["items"]["type"] == "array" {
            if let Some(rows) = as_rows(v) {
                warnings.push("`values` was read as rows of cells (a 2-D array)".to_string());
                *v = rows;
            }
            continue;
        }
        if let Some(c) = coerce(p, v) {
            warnings.push(format!("argument `{k}` was read as {c}"));
            *v = c;
        }
    }
    (out, warnings)
}

// ---------------------------------------------------------------- trimming

/// [`trim_result`] resuming at item `from` of the result's largest list (the `continue_from` a
/// trimmed answer gave): the same continuation every craft app offers for any tool.
pub fn trim_from(v: Value, from: usize, max: usize) -> Value {
    if from == 0 {
        return trim_result(v, max);
    }
    let size = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    let (v, total) = match v {
        Value::Array(items) => {
            let total = items.len();
            (Value::Array(items.into_iter().skip(from).collect()), total)
        }
        Value::Object(mut o) => {
            let key = o.iter().filter(|(_, x)| x.is_array()).max_by_key(|(_, x)| size(x)).map(|(k, _)| k.clone());
            let mut total = 0;
            if let Some(k) = key
                && let Some(Value::Array(items)) = o.remove(&k)
            {
                total = items.len();
                o.insert(k, Value::Array(items.into_iter().skip(from).collect()));
            }
            (Value::Object(o), total)
        }
        other => (other, 0),
    };
    let mut t = trim_result(v, max);
    // Offsets in the answer count from the start of the whole list.
    if let Some(o) = t.as_object_mut() {
        if let Some(n) = o.get("continue_from").and_then(Value::as_u64) {
            o.insert("continue_from".into(), json!(n as usize + from));
            o.insert("next_step".into(), json!(format!("call the same tool again with continue_from: {} for the rest", n as usize + from)));
        }
        o.insert("total".into(), json!(total));
        o.entry("truncated").or_insert(json!(false));
    }
    t
}

/// Trims `v` so its JSON stays under `max` bytes: the longest array is cut and the result says
/// `truncated: true`, how many items it holds and where to continue (`continue_from`, an item
/// offset for the `continue_from` / `offset` argument). Objects stay objects.
pub fn trim_result(v: Value, max: usize) -> Value {
    let size = |v: &Value| serde_json::to_string(v).map(|s| s.len()).unwrap_or(0);
    if size(&v) <= max {
        return v;
    }
    match v {
        Value::Array(items) => {
            let total = items.len();
            let kept = fit_items(&items, max.saturating_sub(512));
            json!({
                "items": items.into_iter().take(kept).collect::<Vec<_>>(),
                "truncated": true,
                "total": total,
                "returned": kept,
                "continue_from": kept,
                "next_step": format!("call the same tool again with continue_from: {kept} for the next items"),
            })
        }
        Value::Object(mut o) => {
            // The biggest array member is cut.
            let key = o.iter().filter(|(_, x)| x.is_array()).max_by_key(|(_, x)| size(x)).map(|(k, _)| k.clone());
            let Some(key) = key else {
                return json!({"truncated": true, "reason": "the result was too large to return", "next_step": "ask for a smaller part"});
            };
            let items = match o.remove(&key) {
                Some(Value::Array(items)) => items,
                _ => vec![],
            };
            let rest = size(&Value::Object(o.clone()));
            let total = items.len();
            let kept = fit_items(&items, max.saturating_sub(rest + 512));
            o.insert(key.clone(), Value::Array(items.into_iter().take(kept).collect()));
            o.insert("truncated".into(), json!(true));
            o.insert("total".into(), json!(total));
            o.insert("returned".into(), json!(kept));
            o.entry("continue_from").or_insert(json!(kept));
            o.entry("next_step").or_insert(json!(format!(
                "`{key}` holds the first {kept} of {total}; call the same tool again with continue_from: {kept} for the rest"
            )));
            Value::Object(o)
        }
        Value::String(s) => {
            let cut: String = s.chars().take(max / 2).collect();
            json!({"text": cut, "truncated": true, "total_chars": s.chars().count(), "next_step": "ask for a smaller part"})
        }
        other => other,
    }
}

/// How many leading items fit in `budget` bytes of JSON.
fn fit_items(items: &[Value], budget: usize) -> usize {
    let mut used = 2usize;
    for (i, it) in items.iter().enumerate() {
        used += serde_json::to_string(it).map(|s| s.len()).unwrap_or(0) + 1;
        if used > budget {
            return i;
        }
    }
    items.len()
}

/// `items[offset..offset+limit]` of an array result, with the continue marker when more remain.
pub fn page(v: Value, offset: usize, limit: Option<usize>) -> Value {
    let Value::Array(items) = v else { return v };
    if offset == 0 && limit.is_none_or(|l| l >= items.len()) {
        return Value::Array(items);
    }
    let total = items.len();
    let take = limit.unwrap_or(usize::MAX);
    let part: Vec<Value> = items.into_iter().skip(offset).take(take).collect();
    let next = offset.saturating_add(part.len());
    let mut o = json!({"items": part, "total": total, "offset": offset});
    if next < total {
        o["truncated"] = json!(true);
        o["continue_from"] = json!(next);
        o["next_step"] = json!(format!("call again with continue_from: {next} (or offset) for the next items"));
    }
    o
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    #[test]
    fn names_match_in_any_spelling() {
        let known: Vec<String> = ["read_range", "server_status", "write_range"].iter().map(|s| s.to_string()).collect();
        assert_eq!(normalize_tool_name("readRange", &known).as_deref(), Some("read_range"));
        assert_eq!(normalize_tool_name("READ-RANGE", &known).as_deref(), Some("read_range"));
        assert_eq!(normalize_tool_name("status", &known).as_deref(), Some("server_status"));
        assert_eq!(normalize_tool_name("setValues", &known).as_deref(), Some("write_range"));
        assert_eq!(normalize_tool_name("nope", &known), None);
        assert_eq!(closest("reed_range", known.iter().map(String::as_str), 1), vec!["read_range".to_string()]);
    }

    #[test]
    fn arguments_are_forgiven() {
        let schema = json!({"type": "object", "properties": {
            "range": {"type": "string"}, "formulas": {"type": "boolean"}, "steps": {"type": "integer"},
            "kind": {"type": "string", "enum": ["bar", "line"]},
            "values": {"type": "array", "items": {"type": "array"}},
        }});
        let args = json!({"Range": "A1", "formulas": "true", "steps": "3", "kind": "Bar", "bogus": 1, "values": [1, 2]});
        let (out, w) = normalize_args(&schema, args.as_object().unwrap());
        assert_eq!(Value::Object(out), json!({"range": "A1", "formulas": true, "steps": 3, "kind": "bar", "values": [[1, 2]]}));
        assert!(w.iter().any(|w| w.contains("unknown argument `bogus`")), "{w:?}");
        let (out, _) = normalize_args(&json!({"properties": {"path": {"type": "string"}}}), json!({"file": "a.xlsx"}).as_object().unwrap());
        assert_eq!(out["path"], "a.xlsx");
    }

    #[test]
    fn big_results_are_trimmed_with_a_marker() {
        let big: Vec<Value> = (0..10_000).map(|i| json!({"i": i, "pad": "xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx"})).collect();
        let t = trim_result(json!({"list": big}), 20_000);
        assert_eq!(t["truncated"], true);
        assert!(serde_json::to_string(&t).unwrap().len() <= 20_000);
        let n = t["continue_from"].as_u64().unwrap();
        assert_eq!(t["list"].as_array().unwrap().len() as u64, n);
        let p = page(json!([1, 2, 3, 4, 5]), 1, Some(2));
        assert_eq!(p["items"], json!([2, 3]));
        assert_eq!(p["continue_from"], 3);
    }

    #[test]
    fn errors_have_code_reason_and_next_step() {
        let e = ToolError::new("busy", "working on\nsomething", "retry").with("retry_after_ms", json!(500));
        let v = e.to_value();
        assert_eq!(v["code"], "busy");
        assert_eq!(v["reason"], "working on something");
        assert_eq!(v["retry_after_ms"], 500);
        assert!(e.text().contains("next_step: retry") && e.text().starts_with("[busy] "));
        // Resuming a trimmed list at continue_from.
        let big: Vec<Value> = (0..1000).map(|i| json!({"i": i, "pad": "xxxxxxxxxxxxxxxxxxxx"})).collect();
        let t = trim_from(json!({"list": big}), 100, 4000);
        assert_eq!(t["list"][0]["i"], 100);
        assert_eq!(t["total"], 1000);
        assert!(t["continue_from"].as_u64().unwrap() > 100);
    }
}
