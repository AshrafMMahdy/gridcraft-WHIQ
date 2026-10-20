//! MCP tool definitions (names, descriptions, JSON Schemas) and their implementations on top of a
//! [`Backend`].

use serde_json::{Map, Value, json};

use crate::backend::Backend;
use crate::headless::NO_UI;

/// The result of `tools/call`: MCP content blocks, the `isError` flag and (WHIQ) the
/// `structuredContent` object every result carries.
#[derive(Clone, Debug, PartialEq)]
pub struct ToolResult {
    pub content: Vec<Value>,
    pub is_error: bool,
    pub structured: Option<Value>,
}

impl ToolResult {
    pub fn text(t: impl Into<String>) -> Self {
        Self { content: vec![json!({"type": "text", "text": t.into()})], is_error: false, structured: None }
    }
    /// A successful result: `v` as pretty JSON text and as `structuredContent` (an object:
    /// anything else is wrapped as `{"result": v}`).
    pub fn json(v: &Value) -> Self {
        let structured = if v.is_object() { v.clone() } else { json!({"result": v}) };
        Self { structured: Some(structured), ..Self::text(serde_json::to_string_pretty(v).unwrap_or_default()) }
    }
    pub fn error(t: impl Into<String>) -> Self {
        let t = t.into();
        let e = crate::contract::ToolError::new("failed", t.clone(), "check the arguments against the tool's input schema");
        Self { content: vec![json!({"type": "text", "text": t})], is_error: true, structured: Some(json!({"error": e.to_value()})) }
    }
    /// WHIQ: a failure with its code, reason and next step.
    pub fn failure(e: &crate::contract::ToolError) -> Self {
        Self { content: vec![json!({"type": "text", "text": e.text()})], is_error: true, structured: Some(json!({"error": e.to_value()})) }
    }
    pub fn to_value(&self) -> Value {
        let mut v = json!({"content": self.content, "isError": self.is_error});
        if let Some(s) = &self.structured {
            v["structuredContent"] = s.clone();
        }
        v
    }
}

// ---------- schemas ----------

fn string(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn boolean(desc: &str) -> Value {
    json!({"type": "boolean", "description": desc})
}
fn int(desc: &str) -> Value {
    json!({"type": "integer", "minimum": 0, "description": desc})
}
fn enumeration(desc: &str, values: &[&str]) -> Value {
    json!({"type": "string", "enum": values, "description": desc})
}
fn params_schema() -> Value {
    json!({"type": "object", "description": "Command parameters (see the `params` field of list_commands); omit or {} for defaults"})
}
fn color(desc: &str) -> Value {
    json!({"type": "string", "description": desc})
}
fn obj(props: Value, required: &[&str]) -> Value {
    let mut o = json!({"type": "object", "properties": props, "additionalProperties": false});
    if !required.is_empty() {
        o["required"] = json!(required);
    }
    o
}
/// WHIQ: what a tool does to its environment, for the MCP annotations. `readOnlyHint` only on
/// reads, `destructiveHint` on tools that delete or overwrite (cells, files), `idempotentHint`
/// where calling twice with the same arguments changes nothing more.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Hints {
    pub read_only: bool,
    pub destructive: bool,
    pub idempotent: bool,
}

/// Reads the workbook or the catalogue; changes nothing.
pub(crate) const READ: Hints = Hints { read_only: true, destructive: false, idempotent: true };
/// Adds to the workbook (a chart, a table, a sheet) without destroying anything.
pub(crate) const ADD: Hints = Hints { read_only: false, destructive: false, idempotent: false };
/// Changes state in a way that repeating changes nothing more (formats, selection, filters).
pub(crate) const SET: Hints = Hints { read_only: false, destructive: false, idempotent: true };
/// Overwrites or deletes (cell contents, rows, files), the same result when repeated.
pub(crate) const OVERWRITE: Hints = Hints { read_only: false, destructive: true, idempotent: true };
/// May delete or overwrite anything (`execute_command`).
pub(crate) const ANY: Hints = Hints { read_only: false, destructive: true, idempotent: false };

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

const RANGE: &str = "A1-style range, e.g. \"A1:D10\", \"B:B\", \"Sheet2!A1:C3\"";
const CHART_TYPES: &[&str] = &[
    "column",
    "bar",
    "line",
    "pie",
    "doughnut",
    "area",
    "scatter",
    "bubble",
    "radar",
    "combo",
    "histogram",
    "waterfall",
    "funnel",
    "treemap",
    "sunburst",
    "boxWhisker",
    "stock",
];
const BORDER_PRESETS: &[&str] = &[
    "bottom",
    "top",
    "left",
    "right",
    "none",
    "all",
    "outside",
    "thickOutside",
    "thickBottom",
    "doubleBottom",
    "topBottom",
    "topThickBottom",
    "topDoubleBottom",
    "insideHorizontal",
    "insideVertical",
    "inside",
    "diagonalDown",
    "diagonalUp",
];

/// All tools, in `tools/list` order: upstream's, then the WHIQ ones, each with the global
/// arguments (`caller`, `async`, and `overwrite` on tools that write files).
pub fn tool_definitions() -> Vec<Value> {
    crate::whiq::with_globals(base_definitions().into_iter().chain(crate::whiq::definitions()).collect())
}

/// Upstream's tools.
fn base_definitions() -> Vec<Value> {
    let empty = || obj(json!({}), &[]);
    vec![
        // ----- generic -----
        tool(
            "execute_command",
            "Execute command",
            "Run ANY GridCraft command by id with JSON params — every ribbon/menu action is a command. Find ids and parameter \
             docs with list_commands. Examples: {\"command\":\"cell.set\",\"params\":{\"cell\":\"B2\",\"input\":\"=SUM(A1:A9)\"}}; \
             {\"command\":\"home.bold\",\"params\":{\"range\":\"A1:C1\"}}; {\"command\":\"sheet.insert\"}; \
             {\"command\":\"data.removeDuplicates\",\"params\":{\"range\":\"A1:C20\"}}. Most commands act on the current selection \
             when `range` is omitted. Programmatic calls never open dialogs: missing required params give an error.",
            obj(
                json!({"command": string("Command id, e.g. \"cell.set\", \"home.fillColor\", \"insert.chart\""), "params": params_schema()}),
                &["command"],
            ),
            ANY,
        ),
        tool(
            "list_commands",
            "List commands",
            "List GridCraft commands: id, label, ribbon path, shortcut, a parameter description (`params`) and whether it can run \
             right now. Use it to discover what execute_command can do (e.g. search \"sort\", \"border\", \"chart\", \"sheet\").",
            obj(json!({"search": string("Case-insensitive substring matched against id, label and ribbon path")}), &[]),
            READ,
        ),
        // ----- workbook -----
        tool(
            "inspect_workbook",
            "Inspect workbook",
            "Summary of the active workbook: title, path, dirty flag, sheets (name, used range, cell count, tables, charts, merges, \
             filters, freeze panes), defined names, current selection and active cell, calculation mode and undo/redo history.",
            empty(),
            READ,
        ),
        tool(
            "read_range",
            "Read range",
            "Read cell values from a range as rows (2-D array). Numbers/booleans/strings are returned as JSON values, empty cells as \
             null, errors as {\"error\":\"#DIV/0!\"}. Without `range`, reads the sheet's used range. formulas:true returns formula \
             text (\"=SUM(B2:B4)\") for formula cells; formatted:true returns the displayed text (\"$1,234.00\", \"12%\").",
            obj(
                json!({
                    "range": string(RANGE),
                    "sheet": string("Sheet name (default: the active sheet)"),
                    "formulas": boolean("Return formulas instead of computed values for formula cells"),
                    "formatted": boolean("Return display text with number formats applied"),
                }),
                &[],
            ),
            READ,
        ),
        tool(
            "write_range",
            "Write range",
            "Write a 2-D array of values starting at the top-left cell of `range` (rows of cells). Numbers, booleans, null (clears) \
             and strings are accepted; strings starting with = are formulas (\"=SUM(B2:B4)\"), other strings are parsed like typed \
             input (\"12%\", \"1/2/2024\", \"$5\"). One undo step. Example: {\"range\":\"A1\",\"values\":[[\"Item\",\"Cost\"],[\"Rent\",1200]]}.",
            obj(
                json!({
                    "range": string("Top-left cell or range, e.g. \"A1\" or \"Sheet2!B3\""),
                    "values": {"type": "array", "description": "Rows of cell values", "items": {"type": "array", "items": {"type": ["string", "number", "boolean", "null"]}}},
                }),
                &["range", "values"],
            ),
            OVERWRITE,
        ),
        tool(
            "set_cell",
            "Set cell",
            "Enter one cell exactly as if typed: a number, text, or a formula starting with = (\"=AVERAGE(B2:B9)\"). \
             Dependent cells recalculate. Returns the command result; use get_cell to read the computed value.",
            obj(
                json!({"cell": string("Cell, e.g. \"B7\" or \"Sheet2!C3\""), "input": {"type": ["string", "number", "boolean"], "description": "Typed input: number, text or =formula"}}),
                &["cell", "input"],
            ),
            OVERWRITE,
        ),
        tool(
            "get_cell",
            "Get cell",
            "Details of one cell: computed value and type, formula, display text, raw input, spill range, style, merge, comment and hyperlink.",
            obj(json!({"cell": string("Cell, e.g. \"B7\" or \"Sheet2!C3\"")}), &["cell"]),
            READ,
        ),
        tool(
            "evaluate_formula",
            "Evaluate formula",
            "Evaluate a formula against the active sheet WITHOUT changing any cell, e.g. \"=SUM(B2:B10)/COUNT(B2:B10)\" or \
             \"=VLOOKUP(\\\"x\\\",A:B,2,FALSE)\". `cell` sets the formula's position for relative references and ROW()/COLUMN().",
            obj(
                json!({"formula": string("Formula text, with or without the leading ="), "cell": string("Evaluate as if in this cell (default: active cell)")}),
                &["formula"],
            ),
            READ,
        ),
        tool(
            "select",
            "Select",
            "Set the selection (what commands without a `range` param act on). Multiple areas with commas: \"A1:B2,D4\". \
             A \"Sheet2!A1\" prefix switches sheets.",
            obj(
                json!({"range": string(RANGE), "active": string("Active cell inside the selection (default: top-left of the last area)")}),
                &["range"],
            ),
            SET,
        ),
        tool(
            "format_range",
            "Format range",
            "Apply several formats to a range in one call: bold, italic, underline, font name/size, font color, fill color, number \
             format, horizontal alignment, wrap, borders. Colors are \"#RRGGBB\" (fontColor also \"auto\", fillColor also \"none\"). \
             numberFormat is a name (General, Number, Currency, Accounting, Comma, Percentage, Short Date, Long Date, Time, Fraction, \
             Scientific, Text) or a format code (\"#,##0.00\", \"0%\", \"yyyy-mm-dd\"). Each property runs one command; the result \
             lists them.",
            obj(
                json!({
                    "range": string(RANGE),
                    "bold": boolean("Bold on/off"),
                    "italic": boolean("Italic on/off"),
                    "underline": boolean("Single underline on/off"),
                    "fontName": string("Font family, e.g. \"Calibri\""),
                    "fontSize": {"type": "number", "minimum": 1, "maximum": 409, "description": "Font size in points"},
                    "fontColor": color("Text color \"#RRGGBB\" or \"auto\""),
                    "fillColor": color("Background color \"#RRGGBB\" or \"none\""),
                    "numberFormat": string("Number format name or code"),
                    "horizontalAlign": enumeration("Horizontal alignment", &["left", "center", "right", "justify"]),
                    "wrapText": boolean("Wrap text on/off"),
                    "direction": enumeration("Text direction (reading order): rtl for Arabic, ltr, or context (from the text)", &["context", "ltr", "rtl"]),
                    "borders": {
                        "description": "A border preset name, or {preset, style?, color?}",
                        "oneOf": [
                            enumeration("Border preset", BORDER_PRESETS),
                            obj(json!({
                                "preset": enumeration("Border preset", BORDER_PRESETS),
                                "style": string("thin|medium|thick|dashed|dotted|double|hair…"),
                                "color": color("Border color \"#RRGGBB\""),
                            }), &["preset"]),
                        ],
                    },
                }),
                &["range"],
            ),
            SET,
        ),
        tool(
            "insert_chart",
            "Insert chart",
            "Insert a chart from a data range (first row/column used as series names and categories, like Excel). Returns the chart id. \
             Scatter charts draw each series as a solid straight line with markers (Excel's \"Scatter with Straight Lines and \
             Markers\") unless `line` / `markers` say otherwise; `line_width` and `color` style every series' line.",
            obj(
                json!({
                    "range": string("Source data range including headers, e.g. \"A1:C6\""),
                    "type": enumeration("Chart type", CHART_TYPES),
                    "subtype": enumeration("Variant (optional); scatter: lines = straight lines without markers", &["clustered", "stacked", "stacked100", "markers", "lines"]),
                    "title": string("Chart title"),
                    "at": string("Top-left anchor cell (default: right of the data)"),
                    "line": boolean("Scatter: draw a solid straight line through each series' points (default true; false = markers only)"),
                    "markers": boolean("Scatter: draw a marker at each point (default true; false = the line alone)"),
                    "line_width": {"type": "number", "exclusiveMinimum": 0, "maximum": 20, "description": "Scatter: line width in points (default 2.25)"},
                    "color": color("Scatter: one line/marker color \"#RRGGBB\" for every series (default: the theme's accent colors, one per series)"),
                }),
                &["range", "type"],
            ),
            ADD,
        ),
        tool(
            "create_table",
            "Create table",
            "Format a range as an Excel table (structured references, banded rows, filter buttons). The first row is the header by default.",
            obj(
                json!({
                    "range": string("Range including the header row, e.g. \"A1:D20\""),
                    "style": string("Table style, e.g. \"TableStyleMedium2\" (default), \"TableStyleLight9\""),
                    "header": boolean("First row holds headers (default true)"),
                    "name": string("Table name (default Table1, Table2…)"),
                }),
                &["range"],
            ),
            ADD,
        ),
        tool(
            "sort_range",
            "Sort range",
            "Sort a range by one or more columns. keys: [{\"column\":\"B\",\"order\":\"desc\"}, …] or just [\"B\"]. Without `range` the \
             current region around the selection is sorted; header detection follows Excel unless `header` is given.",
            obj(
                json!({
                    "range": string(RANGE),
                    "keys": {
                        "type": "array",
                        "minItems": 1,
                        "description": "Sort levels, most significant first",
                        "items": {"oneOf": [
                            string("Column letter, e.g. \"B\" (ascending)"),
                            obj(json!({"column": string("Column letter, e.g. \"B\""), "order": enumeration("Sort order", &["asc", "desc"])}), &["column"]),
                        ]},
                    },
                    "header": boolean("The first row is a header (kept in place)"),
                }),
                &["keys"],
            ),
            SET,
        ),
        tool(
            "filter",
            "Filter",
            "Filter rows by a column (turns on AutoFilter on the current region, or `range`, when needed). Give `values` (keep rows whose \
             cell text is one of them), `custom` ({op:\">\", value:\"10\", op2?, value2?, and?}), `top` ({count, bottom?, percent?}), or \
             clear:true to remove this column's filter.",
            obj(
                json!({
                    "column": string("Column letter, e.g. \"C\""),
                    "range": string("Data range to filter (default: the current region of the selection)"),
                    "values": {"type": "array", "items": {"type": ["string", "number", "boolean"]}, "description": "Values to keep"},
                    "custom": {"type": "object", "description": "Custom criteria: {op: \"=\"|\"<>\"|\">\"|\">=\"|\"<\"|\"<=\"|\"beginsWith\"|\"contains\"…, value, op2?, value2?, and?: bool}"},
                    "top": {"type": "object", "description": "Top/bottom N: {count, bottom?: bool, percent?: bool}"},
                    "blanks": boolean("Also keep blank cells"),
                    "clear": boolean("Clear this column's filter"),
                }),
                &["column"],
            ),
            SET,
        ),
        // ----- files -----
        tool(
            "open_workbook",
            "Open workbook",
            "Open a workbook file (.xlsx, .xlsm, .xls, .xlsb, .ods, .csv, .tsv, .txt, .json) and make it active. Excel 97-2003 .xls imports values, formulas and number formats; XLSB and ODS import worksheet data and cached formula values only; check the returned warnings.",
            obj(json!({"path": string("File path")}), &["path"]),
            SET,
        ),
        tool(
            "save_workbook",
            "Save workbook",
            "Save the active workbook. The format follows the extension (.xlsx default; .csv/.tsv save the active sheet; .json; .html). \
             Without `path` saves to the workbook's current file.",
            obj(json!({"path": string("Destination path")}), &[]),
            OVERWRITE,
        ),
        tool(
            "new_workbook",
            "New workbook",
            "Create a new workbook and make it active: blank, or a built-in sample (\"budget\", \"sales\", \"grades\").",
            obj(json!({"sample": enumeration("Sample workbook", &["budget", "sales", "grades"])}), &[]),
            ADD,
        ),
        tool("undo", "Undo", "Undo the last change(s) to the active workbook.", obj(json!({"steps": int("Number of steps (default 1)")}), &[]), ADD),
        tool("redo", "Redo", "Redo the last undone change(s).", obj(json!({"steps": int("Number of steps (default 1)")}), &[]), ADD),
        tool(
            "screenshot",
            "Screenshot",
            "Save a PNG screenshot of the GridCraft window to `path`. Desktop app only (`gridcraft-cli mcp --connect 7979`).",
            obj(json!({"path": string("Output PNG path")}), &["path"]),
            OVERWRITE,
        ),
        tool(
            "list_functions",
            "List functions",
            "List worksheet functions: name, category, signature and description. Filter with `search` (name or description) or `category`.",
            obj(
                json!({"search": string("Substring of the name or description"), "category": string("Category, e.g. \"Math & Trig\", \"Lookup & Reference\"")}),
                &[],
            ),
            READ,
        ),
    ]
}

// ---------- argument validation ----------

pub(crate) fn type_matches(t: &str, v: &Value) -> bool {
    match t {
        "string" => v.is_string(),
        "number" => v.is_number(),
        "integer" => v.is_u64() || v.is_i64(),
        "boolean" => v.is_boolean(),
        "array" => v.is_array(),
        "object" => v.is_object(),
        "null" => v.is_null(),
        _ => true,
    }
}

/// Checks a value against a (sub)schema: `type`, `enum`, `oneOf`, array `items`, and for objects the required and known properties.
fn schema_matches(schema: &Value, v: &Value) -> bool {
    if let Some(t) = schema["type"].as_str()
        && !type_matches(t, v)
    {
        return false;
    }
    if let Some(allowed) = schema["enum"].as_array()
        && !allowed.contains(v)
    {
        return false;
    }
    if let Some(alts) = schema["oneOf"].as_array()
        && !alts.iter().any(|alt| schema_matches(alt, v))
    {
        return false;
    }
    if let Some(xs) = v.as_array()
        && !xs.iter().all(|x| schema_matches(&schema["items"], x))
    {
        return false;
    }
    if let Some(o) = v.as_object() {
        if schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str).any(|r| o.get(r).is_none_or(Value::is_null)) {
            return false;
        }
        if let Some(props) = schema["properties"].as_object() {
            return o.iter().all(|(k, x)| props.get(k).is_some_and(|ps| x.is_null() || schema_matches(ps, x)));
        }
    }
    true
}

/// Validates `args` against the tool's schema: unknown tool, missing required properties,
/// top-level type mismatches and values matching none of a `oneOf` are JSON-RPC "invalid params" errors.
pub(crate) fn check_arguments(name: &str, args: &Value) -> Result<(), String> {
    let defs = crate::whiq::all_definitions();
    let def = defs.iter().find(|d| d["name"] == name).ok_or_else(|| format!("unknown tool `{name}`"))?;
    let schema = &def["inputSchema"];
    let Some(a) = args.as_object() else { return Err("tool arguments must be an object".into()) };
    for r in schema["required"].as_array().into_iter().flatten().filter_map(Value::as_str) {
        if a.get(r).is_none_or(Value::is_null) {
            return Err(format!("{name}: missing required argument `{r}`"));
        }
    }
    let props = schema["properties"].as_object();
    for (k, v) in a {
        let Some(p) = props.and_then(|p| p.get(k)) else {
            return Err(format!("{name}: unknown argument `{k}`"));
        };
        let ok = match &p["type"] {
            Value::String(t) => type_matches(t, v),
            Value::Array(ts) => ts.iter().filter_map(Value::as_str).any(|t| type_matches(t, v)),
            _ => true,
        };
        if !ok {
            return Err(format!("{name}: argument `{k}` must be of type {}", p["type"]));
        }
        if let Some(allowed) = p["enum"].as_array()
            && !allowed.contains(v)
        {
            return Err(format!("{name}: `{k}` must be one of {}", Value::Array(allowed.clone())));
        }
        if (p["oneOf"].is_array() || p["items"]["oneOf"].is_array()) && !schema_matches(p, v) {
            return Err(format!("{name}: argument `{k}` does not match the schema: {}", p["description"]));
        }
    }
    Ok(())
}

// ---------- implementations ----------

pub(crate) type Args = Map<String, Value>;

pub(crate) fn req_str<'a>(a: &'a Args, k: &str) -> Result<&'a str, String> {
    a.get(k).and_then(Value::as_str).ok_or_else(|| format!("missing string argument `{k}`"))
}

pub(crate) fn exec(b: &mut dyn Backend, cmd: &str, params: Value) -> Result<Value, String> {
    b.call("engine.execute", json!({"command": cmd, "params": params}))
}

/// Copies the listed keys (when present and non-null) into a params object.
pub(crate) fn pick(a: &Args, keys: &[&str]) -> Value {
    Value::Object(keys.iter().filter_map(|k| a.get(*k).filter(|v| !v.is_null()).map(|v| (k.to_string(), v.clone()))).collect())
}

fn format_range(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    let range = req_str(a, "range")?;
    let mut steps: Vec<(&str, Value)> = Vec::new();
    let r = json!(range);
    if let Some(v) = a.get("bold").and_then(Value::as_bool) {
        steps.push(("home.bold", json!({"range": r, "on": v})));
    }
    if let Some(v) = a.get("italic").and_then(Value::as_bool) {
        steps.push(("home.italic", json!({"range": r, "on": v})));
    }
    if let Some(v) = a.get("underline").and_then(Value::as_bool) {
        steps.push(("home.underline", json!({"range": r, "on": v})));
    }
    if let Some(v) = a.get("fontName").and_then(Value::as_str) {
        steps.push(("home.fontName", json!({"range": r, "name": v})));
    }
    if let Some(v) = a.get("fontSize").and_then(Value::as_f64) {
        steps.push(("home.fontSize", json!({"range": r, "size": v})));
    }
    if let Some(v) = a.get("fontColor").and_then(Value::as_str) {
        steps.push(("home.fontColor", json!({"range": r, "color": v})));
    }
    if let Some(v) = a.get("fillColor").and_then(Value::as_str) {
        steps.push(("home.fillColor", json!({"range": r, "color": v})));
    }
    if let Some(v) = a.get("numberFormat").and_then(Value::as_str) {
        steps.push(("home.numberFormat", json!({"range": r, "format": v})));
    }
    if let Some(v) = a.get("horizontalAlign").and_then(Value::as_str) {
        let id = match v {
            "left" => "home.alignLeft",
            "center" => "home.alignCenter",
            "right" => "home.alignRight",
            "justify" => "home.justify",
            other => return Err(format!("horizontalAlign `{other}` must be left, center, right or justify")),
        };
        steps.push((id, json!({"range": r})));
    }
    if let Some(v) = a.get("wrapText").and_then(Value::as_bool) {
        steps.push(("home.wrapText", json!({"range": r, "on": v})));
    }
    if let Some(v) = a.get("direction").and_then(Value::as_str) {
        steps.push(("home.textDirection", json!({"range": r, "direction": v})));
    }
    match a.get("borders") {
        None | Some(Value::Null) => {}
        Some(Value::String(preset)) => steps.push(("home.borders", json!({"range": r, "preset": preset}))),
        Some(Value::Object(o)) => {
            let mut p = Value::Object(o.clone());
            p["range"] = r.clone();
            steps.push(("home.borders", p));
        }
        Some(_) => return Err("`borders` must be a preset name or {preset, style?, color?}".into()),
    }
    if steps.is_empty() {
        return Err("nothing to apply: give at least one of bold, italic, underline, fontName, fontSize, fontColor, fillColor, numberFormat, horizontalAlign, wrapText, direction, borders".into());
    }
    let mut applied = Vec::new();
    for (id, p) in steps {
        exec(b, id, p).map_err(|e| format!("{id}: {e} (applied before the failure: {applied:?})"))?;
        applied.push(id);
    }
    Ok(json!({"range": range, "applied": applied}))
}

fn sort_range(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    let keys = a.get("keys").and_then(Value::as_array).ok_or("`keys` must be an array")?;
    if keys.is_empty() {
        return Err("`keys` needs at least one sort level".into());
    }
    let mut norm = Vec::with_capacity(keys.len());
    for k in keys {
        norm.push(match k {
            Value::String(c) => {
                // "B" or "B desc"
                let mut it = c.split_whitespace();
                let col = it.next().unwrap_or_default();
                let order = match it.next().map(str::to_ascii_lowercase).as_deref() {
                    Some("desc" | "descending") => "desc",
                    _ => "asc",
                };
                json!({"column": col, "order": order})
            }
            Value::Object(o) => {
                let mut o = o.clone();
                o.entry("order").or_insert(json!("asc"));
                Value::Object(o)
            }
            _ => return Err("each sort key is a column letter or {column, order}".into()),
        });
    }
    let mut p = pick(a, &["range", "header"]);
    p["keys"] = Value::Array(norm);
    exec(b, "data.sort", p)
}

fn filter(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    if let Some(r) = a.get("range").and_then(Value::as_str) {
        exec(b, "selection.set", json!({"range": r}))?;
    }
    exec(b, "data.filterBy", pick(a, &["column", "values", "custom", "top", "blanks", "clear"]))
}

fn save_workbook(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    if a.get("path").and_then(Value::as_str).is_none() {
        let info = b.call("document.inspect", json!({}))?;
        if info.get("path").is_none_or(Value::is_null) {
            return Err("this workbook has never been saved: pass `path` (e.g. \"/tmp/book.xlsx\")".into());
        }
    }
    exec(b, "file.save", pick(a, &["path"]))
}

fn screenshot(b: &mut dyn Backend, a: &Args) -> Result<Value, String> {
    if !b.has_ui() {
        return Err(NO_UI.into());
    }
    b.call("ui.screenshot", json!({"path": req_str(a, "path")?}))
}

pub(crate) fn dispatch(b: &mut dyn Backend, name: &str, a: &Args) -> Result<Value, String> {
    match name {
        "execute_command" => {
            let cmd = req_str(a, "command")?;
            let p = match a.get("params") {
                None | Some(Value::Null) => json!({}),
                Some(v @ Value::Object(_)) => v.clone(),
                Some(_) => return Err("`params` must be an object".into()),
            };
            exec(b, cmd, p)
        }
        "list_commands" => b.call("engine.commands", pick(a, &["search"])),
        "inspect_workbook" => b.call("document.inspect", json!({})),
        "read_range" => b.call("sheet.read", pick(a, &["range", "sheet", "formulas", "formatted"])),
        "write_range" => exec(b, "range.setValues", pick(a, &["range", "values"])),
        "set_cell" => exec(b, "cell.set", pick(a, &["cell", "input"])),
        "get_cell" => exec(b, "cell.get", pick(a, &["cell"])),
        "evaluate_formula" => {
            let f = req_str(a, "formula")?;
            let formula = if f.starts_with('=') { f.to_string() } else { format!("={f}") };
            let mut p = pick(a, &["cell"]);
            p["formula"] = json!(formula);
            let v = exec(b, "formulas.evaluate", p)?;
            Ok(json!({"formula": formula, "result": v}))
        }
        "select" => exec(b, "selection.set", pick(a, &["range", "active"])),
        "format_range" => format_range(b, a),
        "insert_chart" => {
            let mut p = pick(a, &["range", "type", "subtype", "title", "at", "line", "markers", "line_width", "color"]);
            // WHIQ: a scatter chart is drawn with straight lines unless `line: false` (or subtype lines, no markers).
            let scatter = p.get("type").and_then(Value::as_str).is_some_and(|t| t.eq_ignore_ascii_case("scatter"));
            let subtype = p.get("subtype").and_then(Value::as_str).unwrap_or("");
            if scatter && p.get("line").is_none() && !subtype.eq_ignore_ascii_case("lines") {
                p["line"] = json!(true);
            }
            exec(b, "insert.chart", p)
        }
        "create_table" => exec(b, "insert.table", pick(a, &["range", "style", "header", "name"])),
        "sort_range" => sort_range(b, a),
        "filter" => filter(b, a),
        "open_workbook" => exec(b, "file.open", pick(a, &["path"])),
        "save_workbook" => save_workbook(b, a),
        "new_workbook" => exec(b, "file.new", pick(a, &["sample"])),
        "undo" => exec(b, "edit.undo", pick(a, &["steps"])),
        "redo" => exec(b, "edit.redo", pick(a, &["steps"])),
        "screenshot" => screenshot(b, a),
        "list_functions" => exec(b, "formulas.functions", pick(a, &["search", "category"])),
        other => crate::whiq::dispatch_extra(b, other, a),
    }
}

/// Runs one tool. Failures become `isError` results with a code, reason and next step (WHIQ:
/// the whole contract pipeline, [`crate::whiq::run_tool`], with default settings).
pub fn call_tool(b: &mut dyn Backend, name: &str, args: &Value) -> ToolResult {
    crate::whiq::run_tool(b, &mut crate::whiq::CallState::default(), name, args)
}
