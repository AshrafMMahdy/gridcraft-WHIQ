#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Write};

use serde_json::{Value, json};

use crate::{Backend, Headless, PROTOCOL_VERSION, Remote, Server, control_addr, tool_definitions};

fn server() -> Server {
    Server::new(Box::new(Headless::new()))
}

fn rpc(s: &mut Server, id: u64, method: &str, params: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": id, "method": method, "params": params}).to_string();
    let reply = s.handle_line(&line).expect("reply");
    let v: Value = serde_json::from_str(&reply).expect("reply is JSON");
    assert_eq!(v["jsonrpc"], "2.0");
    assert_eq!(v["id"], id);
    v
}

fn call(s: &mut Server, name: &str, args: Value) -> Value {
    let v = rpc(s, 99, "tools/call", json!({"name": name, "arguments": args}));
    assert!(v.get("error").is_none(), "{v}");
    v["result"].clone()
}

fn text_of(result: &Value) -> String {
    result["content"].as_array().unwrap().iter().filter(|c| c["type"] == "text").map(|c| c["text"].as_str().unwrap().to_string()).collect()
}

/// Call a tool that must succeed; returns its JSON payload.
fn ok(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, name, args);
    assert_eq!(r["isError"], false, "{name}: {r}");
    serde_json::from_str(&text_of(&r)).unwrap_or(Value::Null)
}

/// Call a tool that must fail; returns its message.
fn fails(s: &mut Server, name: &str, args: Value) -> String {
    let r = call(s, name, args);
    assert_eq!(r["isError"], true, "{name} should fail: {r}");
    text_of(&r)
}

fn err_code(v: &Value) -> i64 {
    v["error"]["code"].as_i64().unwrap_or_else(|| panic!("expected an error: {v}"))
}

fn tmp(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!("gridcraft-mcp-test-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    dir.join(name)
}

// ---------- protocol ----------

#[test]
fn initialize_and_version_negotiation() {
    let mut s = server();
    let v =
        rpc(&mut s, 1, "initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "t", "version": "1"}}));
    let r = &v["result"];
    assert_eq!(r["protocolVersion"], PROTOCOL_VERSION);
    assert_eq!(r["serverInfo"]["name"], "gridcraft");
    assert!(r["capabilities"]["tools"].is_object());
    assert!(r["capabilities"]["resources"].is_object());
    assert!(r["instructions"].as_str().unwrap().contains("headless"));
    // Older supported version is echoed back; unknown versions get ours.
    let v = rpc(&mut s, 2, "initialize", json!({"protocolVersion": "2024-11-05"}));
    assert_eq!(v["result"]["protocolVersion"], "2024-11-05");
    let v = rpc(&mut s, 3, "initialize", json!({"protocolVersion": "1999-01-01"}));
    assert_eq!(v["result"]["protocolVersion"], PROTOCOL_VERSION);
    assert!(!s.is_initialized());
    assert_eq!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#), None);
    assert!(s.is_initialized());
    assert_eq!(rpc(&mut s, 4, "ping", json!({}))["result"], json!({}));
    assert_eq!(rpc(&mut s, 5, "prompts/list", json!({}))["result"]["prompts"], json!([]));
}

#[test]
fn tools_list_has_schemas() {
    let mut s = server();
    let v = rpc(&mut s, 1, "tools/list", json!({}));
    let tools = v["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    for want in [
        "execute_command",
        "list_commands",
        "inspect_workbook",
        "read_range",
        "write_range",
        "set_cell",
        "get_cell",
        "evaluate_formula",
        "select",
        "format_range",
        "insert_chart",
        "create_table",
        "sort_range",
        "filter",
        "open_workbook",
        "save_workbook",
        "new_workbook",
        "undo",
        "redo",
        "screenshot",
        "list_functions",
    ] {
        assert!(names.contains(&want), "missing tool {want}");
    }
    for t in tools {
        assert_eq!(t["inputSchema"]["type"], "object", "{t}");
        assert!(t["description"].as_str().unwrap().len() > 20, "{t}");
        let name = t["name"].as_str().unwrap();
        assert!(name.chars().all(|c| c.is_ascii_lowercase() || c == '_'), "{name} is not snake_case");
        for r in t["inputSchema"]["required"].as_array().into_iter().flatten() {
            assert!(t["inputSchema"]["properties"].get(r.as_str().unwrap()).is_some(), "{name}: required {r} not in properties");
        }
    }
    assert_eq!(tools.len(), tool_definitions().len());
}

#[test]
fn malformed_input_never_panics() {
    let mut s = server();
    assert_eq!(s.handle_line("   "), None);
    let parse = |s: &mut Server, l: &str| -> Value { serde_json::from_str(&s.handle_line(l).unwrap()).unwrap() };
    assert_eq!(err_code(&parse(&mut s, "{not json")), -32700);
    assert_eq!(err_code(&parse(&mut s, "\"just a string\"")), -32600);
    assert_eq!(err_code(&parse(&mut s, "[]")), -32600);
    assert_eq!(err_code(&parse(&mut s, r#"{"jsonrpc":"2.0","id":1}"#)), -32600);
    assert_eq!(err_code(&parse(&mut s, r#"{"jsonrpc":"2.0","id":{"x":1},"method":"ping"}"#)), -32600);
    assert_eq!(err_code(&parse(&mut s, r#"{"jsonrpc":"2.0","id":1,"method":"ping","params":[1,2]}"#)), -32602);
    // Unknown method.
    assert_eq!(err_code(&rpc(&mut s, 1, "nope/nothing", json!({}))), -32601);
    // Bad tools/call params.
    assert_eq!(err_code(&rpc(&mut s, 2, "tools/call", json!({}))), -32602);
    // WHIQ parity pass: an unknown tool is a tool result the agent sees (`not_found`), as in every
    // craft app.
    let r = rpc(&mut s, 3, "tools/call", json!({"name": "no_such_tool"}));
    assert_eq!(r["result"]["structuredContent"]["error"]["code"], "not_found", "{r}");
    assert_eq!(err_code(&rpc(&mut s, 4, "tools/call", json!({"name": "set_cell", "arguments": "x"}))), -32602);
    // WHIQ: argument problems are tool results the model sees, with a code and a next step;
    // unknown arguments are warnings on a successful call.
    let invalid = |s: &mut Server, args: Value, name: &str| {
        let r = call(s, name, args);
        assert_eq!(r["isError"], true, "{r}");
        assert!(r["structuredContent"]["error"]["next_step"].as_str().is_some_and(|n| !n.is_empty()), "{r}");
        r["structuredContent"]["error"]["code"].as_str().unwrap_or("").to_string()
    };
    assert_eq!(invalid(&mut s, json!({"cell": "A1"}), "set_cell"), "invalid_argument");
    invalid(&mut s, json!({"cell": 5, "input": "x"}), "set_cell");
    assert_eq!(invalid(&mut s, json!({"range": "A1", "type": "piechart"}), "insert_chart"), "invalid_argument");
    let r = call(&mut s, "get_cell", json!({"cell": "A1", "bogus": 1}));
    assert_eq!(r["isError"], false, "{r}");
    assert!(r["structuredContent"]["warnings"][0].as_str().unwrap().contains("bogus"), "{r}");
    assert_eq!(err_code(&rpc(&mut s, 9, "resources/read", json!({}))), -32602);
    assert_eq!(err_code(&rpc(&mut s, 10, "resources/read", json!({"uri": "gridcraft://nope"}))), -32002);
    // Responses and unknown notifications are ignored.
    assert_eq!(s.handle_line(r#"{"jsonrpc":"2.0","id":7,"result":{}}"#), None);
    assert_eq!(s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/whatever"}"#), None);
    // A batch still gets answers.
    let v = parse(&mut s, r#"[{"jsonrpc":"2.0","id":1,"method":"ping"},{"jsonrpc":"2.0","method":"notifications/initialized"}]"#);
    assert_eq!(v.as_array().unwrap().len(), 1);
    // Tool-level errors are isError results, not protocol errors.
    assert!(fails(&mut s, "set_cell", json!({"cell": "not a cell!!", "input": "1"})).contains("set_cell"));
    assert!(fails(&mut s, "execute_command", json!({"command": "no.such.command"})).contains("unknown command"));
    fails(&mut s, "read_range", json!({"range": "ZZZZZZZZ99999999999"}));
    fails(&mut s, "format_range", json!({"range": "A1"}));
    fails(&mut s, "sort_range", json!({"keys": []}));
    fails(&mut s, "open_workbook", json!({"path": "/definitely/not/here.xlsx"}));
    fails(&mut s, "save_workbook", json!({}));
    assert!(fails(&mut s, "screenshot", json!({"path": "/tmp/x.png"})).contains("no UI in headless mode"));
}

#[test]
fn every_tool_survives_hostile_arguments() {
    let hostile = [json!({}), json!({"range": "A1:XFD1048576"}), json!({"range": "", "cell": "", "path": "", "formula": "", "command": ""})];
    for t in tool_definitions() {
        let name = t["name"].as_str().unwrap();
        for args in &hostile {
            let mut s = server();
            // Either a protocol error or a tool result; never a panic.
            let _ =
                s.handle_line(&json!({"jsonrpc": "2.0", "id": 1, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string());
        }
    }
}

#[test]
fn resources() {
    let mut s = server();
    let v = rpc(&mut s, 1, "resources/list", json!({}));
    let uris: Vec<&str> = v["result"]["resources"].as_array().unwrap().iter().map(|r| r["uri"].as_str().unwrap()).collect();
    assert_eq!(uris, ["gridcraft://workbook", "gridcraft://commands", "gridcraft://functions"]);
    for uri in uris {
        let v = rpc(&mut s, 2, "resources/read", json!({"uri": uri}));
        let text = v["result"]["contents"][0]["text"].as_str().unwrap();
        let parsed: Value = serde_json::from_str(text).unwrap();
        assert!(!parsed.is_null(), "{uri}");
    }
    let v = rpc(&mut s, 3, "resources/read", json!({"uri": "gridcraft://functions"}));
    assert!(v["result"]["contents"][0]["text"].as_str().unwrap().contains("VLOOKUP"));
}

#[test]
fn basic_tools() {
    let mut s = server();
    let cmds = ok(&mut s, "list_commands", json!({"search": "bold"}));
    assert!(cmds.as_array().unwrap().iter().any(|c| c["id"] == "home.bold"));
    let all = ok(&mut s, "list_commands", json!({}));
    assert!(all.as_array().unwrap().len() > 100);
    let f = ok(&mut s, "list_functions", json!({"search": "vlookup"}));
    assert!(f.as_array().unwrap().iter().any(|x| x["name"] == "VLOOKUP"));
    assert_eq!(ok(&mut s, "evaluate_formula", json!({"formula": "SUM(1,2,3)"}))["result"], json!(6.0));
    ok(&mut s, "set_cell", json!({"cell": "A1", "input": "5"}));
    // #697: a number for `input` is entered, not rejected or cleared.
    ok(&mut s, "set_cell", json!({"cell": "B1", "input": 42.5}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B1"}))["value"], json!(42.5));
    ok(&mut s, "execute_command", json!({"command": "cell.set", "params": {"cell": "A2", "input": "=A1*2"}}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "A2"}))["value"], json!(10.0));
    assert_eq!(ok(&mut s, "evaluate_formula", json!({"formula": "=A1+A2", "cell": "C1"}))["result"], json!(15.0));
    ok(&mut s, "undo", json!({}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "A2"}))["value"], Value::Null);
    ok(&mut s, "redo", json!({}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "A2"}))["value"], json!(10.0));
    ok(&mut s, "select", json!({"range": "A1:A2"}));
    assert_eq!(ok(&mut s, "inspect_workbook", json!({}))["selection"], "A1:A2");
    ok(&mut s, "new_workbook", json!({"sample": "sales"}));
    let info = ok(&mut s, "inspect_workbook", json!({}));
    assert!(info["sheets"][0]["cells"].as_u64().unwrap() > 10, "{info}");
}

#[test]
fn sort_and_filter() {
    let mut s = server();
    ok(&mut s, "write_range", json!({"range": "A1", "values": [["Name", "Score"], ["b", 2], ["c", 3], ["a", 1]]}));
    ok(&mut s, "sort_range", json!({"range": "A1:B4", "keys": [{"column": "B", "order": "desc"}], "header": true}));
    let v = ok(&mut s, "read_range", json!({"range": "A2:A4"}));
    assert_eq!(v["values"], json!([["c"], ["b"], ["a"]]));
    ok(&mut s, "sort_range", json!({"range": "A1:B4", "keys": ["A"], "header": true}));
    let v = ok(&mut s, "read_range", json!({"range": "A2:A4"}));
    assert_eq!(v["values"], json!([["a"], ["b"], ["c"]]));
    ok(&mut s, "filter", json!({"range": "A1:B4", "column": "A", "values": ["a", "c"]}));
    let info = ok(&mut s, "inspect_workbook", json!({}));
    assert_eq!(info["sheets"][0]["autofilter"], "A1:B4");
}

#[test]
fn save_and_open_roundtrip() {
    let mut s = server();
    ok(&mut s, "write_range", json!({"range": "B2", "values": [[1, 2], [3, "=SUM(B2:C2)+B3"]]}));
    let path = tmp("roundtrip.xlsx");
    let p = path.to_string_lossy().to_string();
    ok(&mut s, "save_workbook", json!({"path": p}));
    ok(&mut s, "save_workbook", json!({}));
    ok(&mut s, "new_workbook", json!({}));
    ok(&mut s, "open_workbook", json!({"path": p}));
    let v = ok(&mut s, "read_range", json!({"range": "B2:C3", "formulas": true}));
    assert_eq!(v["values"], json!([[1.0, 2.0], [3.0, "=SUM(B2:C2)+B3"]]));
    let v = ok(&mut s, "read_range", json!({"range": "C3"}));
    assert_eq!(v["values"], json!([[6.0]]));
    let _ = std::fs::remove_file(path);
}

/// An agent builds a small monthly budget through MCP tool calls only.
#[test]
fn agent_builds_a_budget() {
    let mut s = server();
    rpc(&mut s, 1, "initialize", json!({"protocolVersion": PROTOCOL_VERSION, "capabilities": {}, "clientInfo": {"name": "agent", "version": "1"}}));
    s.handle_line(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#);

    ok(
        &mut s,
        "write_range",
        json!({"range": "A1", "values": [
            ["Category", "January", "February", "March"],
            ["Rent", 1200, 1200, 1250],
            ["Groceries", 410.5, 389.25, 402],
            ["Utilities", 150, 162, 140],
            ["Transport", 90, 85, 110],
        ]}),
    );
    ok(&mut s, "set_cell", json!({"cell": "A6", "input": "Total"}));
    for col in ["B", "C", "D"] {
        ok(&mut s, "set_cell", json!({"cell": format!("{col}6"), "input": format!("=SUM({col}2:{col}5)")}));
    }
    ok(&mut s, "set_cell", json!({"cell": "E1", "input": "Quarter"}));
    ok(
        &mut s,
        "write_range",
        json!({"range": "E2", "values": [["=SUM(B2:D2)"], ["=SUM(B3:D3)"], ["=SUM(B4:D4)"], ["=SUM(B5:D5)"], ["=SUM(B6:D6)"]]}),
    );

    let fmt =
        ok(&mut s, "format_range", json!({"range": "A1:E1", "bold": true, "fillColor": "#DDEBF7", "horizontalAlign": "center", "borders": "bottom"}));
    assert_eq!(fmt["applied"].as_array().unwrap().len(), 4, "{fmt}");
    ok(&mut s, "format_range", json!({"range": "B2:E6", "numberFormat": "#,##0.00"}));
    ok(&mut s, "format_range", json!({"range": "A6:E6", "bold": true, "borders": {"preset": "top", "style": "double"}}));

    ok(&mut s, "create_table", json!({"range": "A1:E5", "style": "TableStyleLight9"}));
    ok(&mut s, "insert_chart", json!({"range": "A1:D5", "type": "column", "title": "Monthly spend", "at": "G2"}));

    // Read back computed values.
    let totals = ok(&mut s, "read_range", json!({"range": "B6:E6"}));
    let row = totals["values"][0].as_array().unwrap();
    let nums: Vec<f64> = row.iter().map(|v| v.as_f64().unwrap()).collect();
    assert!((nums[0] - 1850.5).abs() < 1e-9, "{nums:?}");
    assert!((nums[1] - 1836.25).abs() < 1e-9, "{nums:?}");
    assert!((nums[2] - 1902.0).abs() < 1e-9, "{nums:?}");
    assert!((nums[3] - 5588.75).abs() < 1e-9, "{nums:?}");
    let shown = ok(&mut s, "read_range", json!({"range": "E6", "formatted": true}));
    assert_eq!(shown["values"][0][0], "5,588.75");
    let formulas = ok(&mut s, "read_range", json!({"range": "B6", "formulas": true}));
    assert_eq!(formulas["values"][0][0], "=SUM(B2:B5)");
    let cell = ok(&mut s, "get_cell", json!({"cell": "A1"}));
    assert_eq!(cell["style"]["font"]["bold"], true, "{cell}");

    let info = ok(&mut s, "inspect_workbook", json!({}));
    let sheet = &info["sheets"][0];
    assert_eq!(sheet["tables"].as_array().unwrap().len(), 1, "{info}");
    assert_eq!(sheet["tables"][0]["range"], "A1:E5");
    assert_eq!(sheet["charts"].as_array().unwrap().len(), 1, "{info}");
    assert_eq!(sheet["charts"][0]["title"], "Monthly spend");

    // Change an input: totals follow.
    ok(&mut s, "set_cell", json!({"cell": "B2", "input": "1300"}));
    let v = ok(&mut s, "read_range", json!({"range": "E6"}));
    assert!((v["values"][0][0].as_f64().unwrap() - 5688.75).abs() < 1e-9);
    // And the whole thing evaluates without touching the sheet.
    let v = ok(&mut s, "evaluate_formula", json!({"formula": "=AVERAGE(B6:D6)"}));
    assert!((v["result"].as_f64().unwrap() - (1950.5 + 1836.25 + 1902.0) / 3.0).abs() < 1e-9);
}

// ---------- serve loop + remote ----------

#[test]
fn serve_loop_over_buffers() {
    let input = concat!(
        r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18"}}"#,
        "\n",
        r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#,
        "\n",
        "garbage\n",
        r#"{"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"evaluate_formula","arguments":{"formula":"=2*21"}}}"#,
        "\n"
    );
    let mut out = Vec::new();
    let mut s = server();
    s.serve(input.as_bytes(), &mut out).unwrap();
    let lines: Vec<Value> = String::from_utf8(out).unwrap().lines().map(|l| serde_json::from_str(l).unwrap()).collect();
    assert_eq!(lines.len(), 3);
    assert_eq!(lines[1]["error"]["code"], -32700);
    assert!(lines[2]["result"]["content"][0]["text"].as_str().unwrap().contains("42"));
}

#[test]
fn cell_set_without_input_is_error() {
    let mut s = server();
    ok(&mut s, "set_cell", json!({"cell": "B2", "input": "keep"}));
    fails(&mut s, "execute_command", json!({"command": "cell.set", "params": {"cell": "B2"}}));
    fails(&mut s, "execute_command", json!({"command": "cell.set", "params": {}}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B2"}))["text"], "keep");
    // An explicit empty string still clears.
    ok(&mut s, "execute_command", json!({"command": "cell.set", "params": {"cell": "B2", "input": ""}}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B2"}))["text"], "");
}

#[test]
fn remote_backend_over_tcp() {
    // A fake app: answers engine.execute by running a headless session; closes after the first
    // request to exercise the reconnect path.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let handle = std::thread::spawn(move || {
        let mut h = Headless::new();
        for (i, stream) in listener.incoming().take(2).enumerate() {
            let stream = stream.unwrap();
            let mut w = stream.try_clone().unwrap();
            let mut r = BufReader::new(stream);
            loop {
                let mut line = String::new();
                if r.read_line(&mut line).unwrap_or(0) == 0 {
                    break;
                }
                let req: Value = serde_json::from_str(&line).unwrap();
                let reply = match h.call(req["method"].as_str().unwrap(), req["params"].clone()) {
                    Ok(v) => json!({"id": req["id"], "ok": true, "result": v}),
                    Err(e) => json!({"id": req["id"], "ok": false, "error": e}),
                };
                w.write_all(format!("{reply}\n").as_bytes()).unwrap();
                if i == 0 {
                    break; // drop the first connection
                }
            }
        }
    });
    let addr = control_addr(&port.to_string());
    assert_eq!(addr, format!("127.0.0.1:{port}"));
    let mut s = Server::new(Box::new(Remote::connect(&addr).unwrap()));
    assert_eq!(ok(&mut s, "evaluate_formula", json!({"formula": "=1+1"}))["result"], json!(2.0));
    // Second call reconnects transparently.
    ok(&mut s, "set_cell", json!({"cell": "A1", "input": "7"}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "A1"}))["value"], json!(7.0));
    let msg = fails(&mut s, "execute_command", json!({"command": "nope"}));
    assert!(msg.contains("unknown command"), "{msg}");
    drop(s);
    handle.join().unwrap();
    assert!(Remote::connect("127.0.0.1:1").is_err());
}

#[test]
fn file_commands_without_a_file_fail() {
    let mut s = server();
    // `file.save` on Book1, which has no file yet, would ask for one too.
    for command in ["file.open", "file.saveAs", "file.save", "data.getData", "data.fromTextCsv"] {
        let msg = fails(&mut s, "execute_command", json!({"command": command}));
        assert!(msg.contains("missing") && msg.contains("never open dialogs"), "{command}: {msg}");
        fails(&mut s, "execute_command", json!({"command": command, "params": {}}));
    }
}

// ---------- cell references ----------

fn cmd_fails(s: &mut Server, command: &str, params: Value) -> String {
    fails(s, "execute_command", json!({"command": command, "params": params}))
}

fn a1_display(s: &mut Server) -> String {
    ok(s, "get_cell", json!({"cell": "A1"}))["text"].as_str().unwrap_or("").to_string()
}

#[test]
fn set_cell_invalid_ref_is_error() {
    let mut s = server();
    for bad in ["ZZZZ0", "B0", "A", "1", "A1:B2", "not a cell", "", "XFE1", "A1048577"] {
        fails(&mut s, "set_cell", json!({"cell": bad, "input": "x"}));
    }
    assert_eq!(a1_display(&mut s), "");
}

#[test]
fn cell_ref_boundaries_still_work() {
    let mut s = server();
    ok(&mut s, "set_cell", json!({"cell": "A1", "input": "first"}));
    ok(&mut s, "set_cell", json!({"cell": "$B$2", "input": "abs"}));
    ok(&mut s, "set_cell", json!({"cell": "XFD1048576", "input": "last"}));
    ok(&mut s, "set_cell", json!({"cell": "xfd1", "input": "lower"}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B2"}))["text"], "abs");
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "XFD1048576"}))["text"], "last");
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "XFD1"}))["text"], "lower");
    assert_eq!(a1_display(&mut s), "first");
    // Ranges are still ranges for the commands that take them.
    ok(&mut s, "read_range", json!({"range": "A1:B2"}));
    ok(&mut s, "read_range", json!({"range": "$A$1:$B$2"}));
    // The active sheet's own name is an accepted prefix; an absent cell still means the selection.
    ok(&mut s, "set_cell", json!({"cell": "Sheet1!C3", "input": "named"}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "C3"}))["text"], "named");
    ok(&mut s, "execute_command", json!({"command": "formulas.tracePrecedents", "params": {}}));
}

#[test]
fn bad_ref_in_command_is_error() {
    let mut s = server();
    ok(&mut s, "set_cell", json!({"cell": "A1", "input": "7"}));
    for (cmd, key) in [("view.freezePanes", "cell"), ("formulas.tracePrecedents", "cell"), ("formulas.traceDependents", "cell"), ("cell.get", "cell")]
    {
        cmd_fails(&mut s, cmd, json!({ key: "B0" }));
        cmd_fails(&mut s, cmd, json!({ key: "nonsense" }));
    }
    assert_eq!(a1_display(&mut s), "7");
}

#[test]
fn other_sheet_prefix_is_error() {
    let mut s = server();
    ok(&mut s, "execute_command", json!({"command": "home.insertSheet", "params": {"name": "Other"}}));
    ok(&mut s, "execute_command", json!({"command": "sheet.activate", "params": {"sheet": 0}}));
    fails(&mut s, "set_cell", json!({"cell": "Nope!B2", "input": "x"}));
    // A command that cannot honour a sheet prefix must not drop it and use the active sheet.
    cmd_fails(&mut s, "view.freezePanes", json!({"cell": "Other!B2"}));
    cmd_fails(&mut s, "formulas.tracePrecedents", json!({"cell": "Other!B2"}));
    ok(&mut s, "set_cell", json!({"cell": "Sheet1!B2", "input": "here"}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B2"}))["text"], "here");
    // cell.set resolves another existing sheet itself and writes there, not to the active sheet.
    ok(&mut s, "set_cell", json!({"cell": "Other!B3", "input": "there"}));
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B3"}))["text"], "");
    let r = ok(&mut s, "read_range", json!({"range": "Other!B3"}));
    assert!(r.to_string().contains("there"), "{r}");
}

#[test]
fn cross_sheet_cell_refs_resolve() {
    let mut s = server();
    ok(&mut s, "execute_command", json!({"command": "home.insertSheet", "params": {"name": "Other"}}));
    ok(&mut s, "execute_command", json!({"command": "home.insertSheet", "params": {"name": "My Sheet"}}));
    ok(&mut s, "execute_command", json!({"command": "sheet.activate", "params": {"sheet": 0}}));
    ok(&mut s, "set_cell", json!({"cell": "Other!B3", "input": "there"}));
    ok(&mut s, "set_cell", json!({"cell": "'My Sheet'!A1", "input": "quoted"}));
    // Reads honour the prefix.
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "Other!B3"}))["text"], "there");
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "'My Sheet'!A1"}))["text"], "quoted");
    assert_eq!(ok(&mut s, "get_cell", json!({"cell": "B3"}))["text"], "");
    // Cell-level commands that resolve their own sheet land on it.
    ok(&mut s, "execute_command", json!({"command": "review.newComment", "params": {"cell": "Other!B3", "text": "note"}}));
    let c = ok(&mut s, "get_cell", json!({"cell": "Other!B3"}));
    assert!(c.to_string().contains("note"), "{c}");
    let c = ok(&mut s, "get_cell", json!({"cell": "B3"}));
    assert!(!c.to_string().contains("note"), "{c}");
    // The active sheet may be a quoted name with spaces.
    ok(&mut s, "execute_command", json!({"command": "sheet.activate", "params": {"sheet": "My Sheet"}}));
    ok(&mut s, "execute_command", json!({"command": "view.freezePanes", "params": {"cell": "'My Sheet'!B2"}}));
    cmd_fails(&mut s, "view.freezePanes", json!({"cell": "Other!B2"}));
    // A prefix naming a different sheet than the `sheet` argument is an error, not resolved in favour of either.
    cmd_fails(&mut s, "cell.get", json!({"cell": "Other!B3", "sheet": "Sheet1"}));
}

#[test]
fn text_to_columns_destination_follows_its_sheet() {
    let mut s = server();
    ok(&mut s, "execute_command", json!({"command": "home.insertSheet", "params": {"name": "Other"}}));
    ok(&mut s, "execute_command", json!({"command": "sheet.activate", "params": {"sheet": 0}}));
    ok(&mut s, "set_cell", json!({"cell": "Other!A1", "input": "a,b"}));
    ok(
        &mut s,
        "execute_command",
        json!({"command": "data.textToColumns", "params": {"range": "Other!A1", "delimiters": [","], "destination": "Other!D1"}}),
    );
    let r = ok(&mut s, "read_range", json!({"range": "Other!D1:E1"}));
    assert!(r.to_string().contains('a') && r.to_string().contains('b'), "{r}");
    // An unparsable destination and a prefix that contradicts the command's sheet are errors, not A1.
    cmd_fails(&mut s, "data.textToColumns", json!({"range": "Other!A1", "delimiters": [","], "destination": "D0"}));
    cmd_fails(&mut s, "data.textToColumns", json!({"range": "Other!A1", "delimiters": [","], "destination": "Sheet1!D1"}));
}

#[test]
fn invalid_borders_are_invalid_params() {
    let mut s = server();
    let bad = |s: &mut Server, id: u64, borders: Value| {
        let v = rpc(s, id, "tools/call", json!({"name": "format_range", "arguments": {"range": "A1", "borders": borders}}));
        assert_eq!(v["result"]["structuredContent"]["error"]["code"], "invalid_argument", "{v}");
    };
    bad(&mut s, 1, json!(123));
    bad(&mut s, 2, json!("bogus"));
    bad(&mut s, 3, json!({"preset": "bogus"}));
    bad(&mut s, 4, json!({"style": "thick"}));
    ok(&mut s, "format_range", json!({"range": "A1", "borders": "all"}));
    ok(&mut s, "format_range", json!({"range": "B1", "borders": {"preset": "all", "style": "thick", "color": "#123456"}}));
}

#[test]
fn invalid_sort_keys_are_invalid_params() {
    let mut s = server();
    ok(&mut s, "write_range", json!({"range": "A1", "values": [["b"], ["c"], ["a"]]}));
    for (id, keys) in [(1, json!([123])), (2, json!([{"order": "desc"}])), (3, json!([{"column": "A", "order": "sideways"}]))] {
        let v = rpc(&mut s, id, "tools/call", json!({"name": "sort_range", "arguments": {"range": "A1:A3", "keys": keys}}));
        assert_eq!(v["result"]["structuredContent"]["error"]["code"], "invalid_argument", "{v}");
    }
    ok(&mut s, "sort_range", json!({"range": "A1:A3", "keys": ["A", {"column": "A", "order": "desc"}]}));
}

// ---------- WHIQ ----------

#[test]
fn annotations_say_what_each_tool_does() {
    let tools = tool_definitions();
    let hint = |name: &str, h: &str| {
        let t = tools.iter().find(|t| t["name"] == name).unwrap_or_else(|| panic!("{name}"));
        t["annotations"][h].as_bool().unwrap_or_else(|| panic!("{name}: {h}"))
    };
    for t in &tools {
        let name = t["name"].as_str().unwrap();
        let a = &t["annotations"];
        for h in ["readOnlyHint", "destructiveHint", "idempotentHint", "openWorldHint"] {
            assert!(a[h].is_boolean(), "{name}: {h}");
        }
        assert!(!(hint(name, "readOnlyHint") && hint(name, "destructiveHint")), "{name}: a read can't destroy");
    }
    for read in ["read_range", "get_cell", "inspect_workbook", "list_commands", "list_functions", "evaluate_formula"] {
        assert!(hint(read, "readOnlyHint"), "{read}");
    }
    for writes in ["write_range", "set_cell", "save_workbook", "execute_command", "format_range", "open_workbook", "screenshot"] {
        assert!(!hint(writes, "readOnlyHint"), "{writes} is not read-only");
    }
    for destructive in ["save_workbook", "write_range", "set_cell", "execute_command"] {
        assert!(hint(destructive, "destructiveHint"), "{destructive}");
    }
    assert!(!hint("insert_chart", "destructiveHint"));
}

#[test]
fn automation_roots_confine_the_headless_server() {
    let base = std::env::temp_dir().join(format!("gridcraft-mcp-roots-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&base);
    std::fs::create_dir_all(base.join("chat")).unwrap();
    std::fs::create_dir_all(base.join("outside")).unwrap();
    let roots = gridcraft_engine::file_access::AutomationRoots::new(Some(&base.join("chat")), Some(&base.join("chat"))).unwrap();
    let mut s = Server::new(Box::new(Headless::new().with_automation_roots(roots)));
    ok(&mut s, "set_cell", json!({"cell": "A1", "input": "kept"}));
    let out = base.join("outside/leak.xlsx");
    let e = fails(&mut s, "save_workbook", json!({"path": out.to_str().unwrap()}));
    assert!(e.contains("outside the write root"), "{e}");
    assert!(!out.exists());
    // A relative path lands inside the write root.
    ok(&mut s, "save_workbook", json!({"path": "inside.xlsx"}));
    assert!(base.join("chat/inside.xlsx").exists());
    let e = fails(&mut s, "open_workbook", json!({"path": base.join("outside/x.xlsx").to_str().unwrap()}));
    assert!(e.contains("outside the read root"), "{e}");
    let _ = std::fs::remove_dir_all(&base);
}

// ---------- WHIQ: preview, Markdown reads, bulk writes and batches ----------

/// A tool's structuredContent (successful).
fn structured(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, name, args);
    assert_eq!(r["isError"], false, "{name}: {r}");
    r["structuredContent"].clone()
}

#[test]
fn render_preview_draws_a_window_and_says_how_to_page() {
    let mut s = server();
    let rows: Vec<Value> = (1..=5000).map(|i| json!([format!("item {i}"), i, format!("=B{i}*2")])).collect();
    structured(&mut s, "write_range", json!({"range": "A1", "values": rows}));
    let r = call(&mut s, "render_preview", json!({}));
    assert_eq!(r["isError"], false, "{r}");
    let v = &r["structuredContent"];
    let html = v["html"].as_str().unwrap();
    assert!(html.starts_with("<!DOCTYPE html>") && html.contains("class=\"tab on\""));
    assert!(html.contains("data-f=\"=B1*2\""));
    // The HTML is its own content block; the first is the JSON without it.
    let blocks = r["content"].as_array().unwrap();
    assert_eq!(blocks.len(), 2);
    assert_eq!(blocks[1]["text"].as_str().unwrap(), html);
    assert!(!blocks[0]["text"].as_str().unwrap().contains("<!DOCTYPE"));
    assert_eq!(v["window"]["rows"], 5000);
    assert_eq!(v["window"]["more_rows"], true);
    assert_eq!(v["sheets"][0]["used_range"], "A1:C5000");
    assert!(v["next_step"].as_str().unwrap().contains("row_start"));
    // Far past the old 2,000-row cap, and big HTML is never trimmed by --max-result-bytes.
    let v = structured(&mut s, "render_preview", json!({"sheet": 0, "row_start": 3001, "row_count": 2000, "format": "rows"}));
    let html = v["html"].as_str().unwrap();
    assert!(html.len() > crate::contract::MAX_RESULT_BYTES, "{}", html.len());
    assert!(html.contains("data-r=\"3001\"") && html.contains("data-r=\"5000\"") && html.contains("item 5000"));
    assert_eq!(v["window"]["more_rows"], false);
}

#[test]
fn render_preview_and_export_markdown_read_a_file_and_leave_the_active_workbook() {
    let mut s = server();
    let path = tmp("preview-source.xlsx");
    let _ = std::fs::remove_file(&path);
    let source = structured(&mut s, "new_workbook", json!({}))["document"].clone();
    structured(&mut s, "write_range", json!({"range": "A1", "values": [["Region", "Units"], ["North", 1200.5], ["South", 800]]}));
    structured(&mut s, "format_range", json!({"range": "B2:B3", "numberFormat": "#,##0.00"}));
    structured(&mut s, "save_workbook", json!({"path": path.to_str().unwrap(), "overwrite": true}));
    let mine = structured(&mut s, "new_workbook", json!({}))["document"].clone();
    structured(&mut s, "set_cell", json!({"cell": "A1", "input": "mine"}));
    // The file is open here (it was saved from this server): that workbook is drawn.
    let v = structured(&mut s, "render_preview", json!({"path": path.to_str().unwrap()}));
    assert_eq!(v["opened"], false, "{v}");
    assert_eq!(v["document"], source);
    structured(&mut s, "close_workbook", json!({"document": source}));
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "A1"}))["value"], "mine");
    // Closed: the file is opened for the preview and kept open, the agent's workbook stays active.
    let v = structured(&mut s, "render_preview", json!({"path": path.to_str().unwrap()}));
    assert_eq!(v["opened"], true, "{v}");
    let handle = v["document"].as_str().unwrap().to_string();
    assert_ne!(Value::String(handle.clone()), mine);
    assert!(v["html"].as_str().unwrap().contains(">1,200.50<"));
    // The active workbook is still the agent's.
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "A1"}))["value"], "mine");
    // The next window reuses the open file.
    let v = structured(&mut s, "export_markdown", json!({"path": path.to_str().unwrap()}));
    assert_eq!(v["opened"], false, "{v}");
    assert_eq!(v["document"], handle);
    let md = v["markdown"].as_str().unwrap();
    assert!(md.contains("| 2 | North | 1,200.50 |"), "{md}");
    assert_eq!(v["sheets"][0]["used_range"], "A1:B3");
    // And by handle, paged.
    let v = structured(&mut s, "export_markdown", json!({"document": handle, "sheet": "Sheet1", "row_start": 2, "row_count": 1}));
    assert!(v["markdown"].as_str().unwrap().contains("| 2 | North |") && !v["markdown"].as_str().unwrap().contains("| 3 |"));
    assert_eq!(v["next"]["row_start"], 3);
    let _ = std::fs::remove_file(&path);
}

#[test]
fn one_call_writes_a_hundred_thousand_cells() {
    let mut s = server();
    let rows: Vec<Value> = (0..1000).map(|r| Value::Array((0..100).map(|c| json!(r * 100 + c)).collect())).collect();
    let v = structured(&mut s, "write_range", json!({"range": "A1", "values": rows.clone()}));
    assert_eq!(v["cells"], 100_000);
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "CV1000"}))["value"], 99_999.0);
    // The same block as one operation of a batch.
    let v = structured(&mut s, "batch", json!({"ops": [{"op": "write", "range": "Sheet1!A2001", "values": rows}]}));
    assert_eq!(v["ok"], 1);
    assert_eq!(v["results"][0]["result"]["cells"], 100_000);
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "CV3000"}))["value"], 99_999.0);
    // Many ranges formatted in one call.
    structured(&mut s, "format_range", json!({"range": "A1:B2,D5:E6,CV1000", "bold": true, "fillColor": "#FFEB9C"}));
    for c in ["A1", "B2", "E6", "CV1000"] {
        assert_eq!(structured(&mut s, "get_cell", json!({"cell": c}))["style"]["font"]["bold"], true, "{c}");
    }
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "C3"}))["style"]["font"]["bold"], false);
}

#[test]
fn a_fifty_operation_batch_runs_in_one_call_as_one_undo_step() {
    let mut s = server();
    let mut ops = vec![json!({"op": "write", "range": "A1", "values": [["Month", "Sales", "Cost"]]})];
    for i in 0..12 {
        ops.push(json!({"tool": "write_range", "args": {"range": format!("A{}", i + 2), "values": [[format!("M{}", i + 1), (i + 1) * 100, (i + 1) * 60]]}}));
    }
    for i in 0..12 {
        ops.push(json!({"op": "format", "range": format!("B{}:C{}", i + 2, i + 2), "numberFormat": "#,##0", "fillColor": if i % 2 == 0 { "#F2F2F2" } else { "#FFFFFF" }}));
    }
    ops.push(json!({"op": "format", "range": "A1:C1", "bold": true, "fillColor": "#DDEBF7", "borders": "all"}));
    ops.push(json!({"op": "merge", "range": "E1:H1", "center": true}));
    ops.push(json!({"op": "set", "cell": "E1", "input": "Summary"}));
    ops.push(json!({"op": "set", "cell": "E2", "input": "=SUM(B2:B13)"}));
    ops.push(json!({"op": "chart", "range": "A1:B13", "type": "column", "title": "Sales", "at": "E4"}));
    ops.push(json!({"op": "conditional_format", "range": "B2:B13", "rule": {"type": "cellIs", "operator": "greaterThan", "value": "600", "preset": "greenFill"}}));
    ops.push(json!({"command": "home.italic", "params": {"range": "A2:A13", "on": true}}));
    while ops.len() < 50 {
        let n = ops.len();
        ops.push(json!({"op": "set", "cell": format!("J{n}"), "input": n}));
    }
    assert_eq!(ops.len(), 50);
    let v = structured(&mut s, "batch", json!({"ops": ops}));
    assert_eq!(v["ran"], 50, "{v}");
    assert_eq!(v["ok"], 50, "{v}");
    assert_eq!(v["failed"], 0);
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "E2"}))["value"], 7800.0);
    let wb = structured(&mut s, "inspect_workbook", json!({}));
    let sheet = &wb["sheets"][0];
    assert_eq!(sheet["charts"].as_array().map_or_else(|| sheet["charts"].as_u64().unwrap_or(0) as usize, Vec::len), 1, "{sheet}");
    // One undo takes the whole batch back.
    structured(&mut s, "undo", json!({}));
    assert!(structured(&mut s, "get_cell", json!({"cell": "A1"}))["value"].is_null());
    structured(&mut s, "redo", json!({}));
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "A1"}))["value"], "Month");
}

#[test]
fn a_failing_batch_is_rolled_back_unless_keep_going() {
    let mut s = server();
    structured(&mut s, "set_cell", json!({"cell": "A1", "input": "before"}));
    let ops = json!([
        {"op": "set", "cell": "A1", "input": "changed"},
        {"op": "write", "range": "B1", "values": [[1, 2, 3]]},
        {"op": "set", "cell": "not a cell!!", "input": "x"},
        {"op": "set", "cell": "C5", "input": "after"},
    ]);
    let r = call(&mut s, "batch", json!({"ops": ops}));
    assert_eq!(r["isError"], true, "{r}");
    let e = &r["structuredContent"]["error"];
    assert_eq!(e["failed_at"], 2, "{e}");
    assert_eq!(e["rolled_back"], true, "{e}");
    assert_eq!(e["results"].as_array().unwrap().len(), 3);
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "A1"}))["value"], "before");
    assert!(structured(&mut s, "get_cell", json!({"cell": "B1"}))["value"].is_null());
    // keepGoing: the failure is reported and the rest is kept.
    let v = structured(&mut s, "batch", json!({"ops": ops, "keepGoing": true}));
    assert_eq!(v["failed"], 1);
    assert_eq!(v["ok"], 3);
    assert_eq!(v["results"][2]["ok"], false);
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "C5"}))["value"], "after");
    // Tools that open or close documents are not batched; unknown ops name the closest.
    let r = call(&mut s, "batch", json!({"ops": [{"tool": "open_workbook", "path": "x.xlsx"}]}));
    assert_eq!(r["structuredContent"]["error"]["code"], "unsupported", "{r}");
    let r = call(&mut s, "batch", json!({"ops": [{"tool": "wrte_range", "range": "A1", "values": [[1]]}]}));
    assert_eq!(r["isError"], true, "{r}");
    let r = call(&mut s, "batch", json!({}));
    assert_eq!(r["structuredContent"]["error"]["code"], "invalid_argument", "{r}");
}

// ---------- WHIQ: working a workbook fully, like a person in Excel (coverage table) ----------
//
// Each test opens a real .xlsx written to disk (built, saved, then read back by a fresh server),
// works it through the tools, saves it and reads the saved file back again.

/// A saved workbook with values, formulas, formats, a merge, a table, a chart, a name, a comment,
/// a note, a validation, a conditional format and a hidden sheet.
fn coverage_fixture(name: &str) -> std::path::PathBuf {
    let path = tmp(name);
    let _ = std::fs::remove_file(&path);
    let mut s = server();
    structured(
        &mut s,
        "build_workbook",
        json!({"sheets": [
            {"name": "Sales", "header": ["Region", "Units", "Price", "Total"],
             "rows": [["North", 10, 2.5, "=B2*C2"], ["South", 20, 3, "=B3*C3"], ["East", 30, 1.5, "=B4*C4"], ["West", 40, 2, "=B5*C5"]],
             "columns": [{"width": 16}, {"format": "#,##0"}, {"format": "0.00"}, {"format": "#,##0.00"}],
             "table": "TableStyleMedium2",
             "charts": [{"type": "column", "title": "Units", "series": [{"name": "Units", "values": "B2:B5", "categories": "A2:A5"}], "at": "F2"}]},
            {"name": "Notes", "rows": [["Title spanning", null, null], ["North has the lowest units"]], "merges": ["A1:C1"]},
            {"name": "Secret", "rows": [["hidden value", 42]]}
        ]}),
    );
    structured(&mut s, "execute_command", json!({"command": "sheet.hide", "params": {"sheet": "Secret"}}));
    structured(&mut s, "define_name", json!({"name": "Units", "refers_to": "=Sales!$B$2:$B$5"}));
    structured(&mut s, "set_cell", json!({"cell": "Sales!F1", "input": "=SUM(Units)"}));
    structured(&mut s, "add_comment", json!({"cell": "Sales!A2", "text": "Check North", "caller": "Ann"}));
    structured(&mut s, "add_comment", json!({"cell": "Notes!A2", "text": "a note", "note": true}));
    structured(
        &mut s,
        "data_validation",
        json!({"range": "Sales!B2:B5", "type": "whole", "operator": "between", "formula1": "0", "formula2": "1000"}),
    );
    structured(
        &mut s,
        "conditional_format",
        json!({"range": "Sales!B2:B5", "rule": {"type": "cellIs", "operator": "greaterThan", "value": "15", "preset": "greenFill"}}),
    );
    structured(&mut s, "format_range", json!({"range": "Sales!A1:D1", "bold": true, "fillColor": "#DDEBF7"}));
    structured(&mut s, "save_workbook", json!({"path": path.to_str().unwrap(), "overwrite": true}));
    path
}

/// A fresh server with the fixture opened from disk.
fn open_fixture(name: &str) -> (Server, std::path::PathBuf) {
    let path = coverage_fixture(name);
    let mut s = server();
    structured(&mut s, "open_workbook", json!({"path": path.to_str().unwrap()}));
    (s, path)
}

/// Saves and opens the file again in a fresh server (what is checked is what the file holds).
fn save_and_reopen(s: &mut Server, path: &std::path::Path) -> Server {
    structured(s, "save_workbook", json!({"path": path.to_str().unwrap(), "overwrite": true}));
    let mut t = server();
    structured(&mut t, "open_workbook", json!({"path": path.to_str().unwrap()}));
    t
}

fn sheet_info(s: &mut Server, name: &str) -> Value {
    let wb = structured(s, "inspect_workbook", json!({}));
    wb["sheets"].as_array().unwrap().iter().find(|x| x["name"] == name).cloned().unwrap_or(Value::Null)
}

#[test]
fn coverage_read_values_formulas_formats_comments_charts_tables_names_merges_hidden_sheets_picture() {
    let (mut s, path) = open_fixture("cov-read.xlsx");
    // Values and formulas.
    let v = structured(&mut s, "read_range", json!({"range": "Sales!A1:D5"}));
    assert_eq!(v["values"][4][3], 80.0, "{v}");
    let v = structured(&mut s, "read_range", json!({"range": "Sales!D2:D3", "formulas": true}));
    assert_eq!(v["values"][0][0], "=B2*C2");
    // Formats: the displayed text and the cell's style.
    let v = structured(&mut s, "read_range", json!({"range": "Sales!C2", "formatted": true}));
    assert_eq!(v["values"][0][0], "2.50");
    let c = structured(&mut s, "get_cell", json!({"cell": "Sales!A1"}));
    assert_eq!(c["style"]["font"]["bold"], true, "{c}");
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "Sales!D2"}))["style"]["num_fmt"], "#,##0.00");
    // Comments and notes.
    let v = structured(&mut s, "list_comments", json!({}));
    assert_eq!(v["count"], 2, "{v}");
    assert!(v["comments"].as_array().unwrap().iter().any(|c| c["cell"] == "A2" && c["text"] == "Check North" && c["kind"] == "comment"));
    assert!(v["comments"].as_array().unwrap().iter().any(|c| c["sheet"] == "Notes" && c["kind"] == "note"));
    assert!(structured(&mut s, "get_cell", json!({"cell": "Sales!A2"}))["comment"].to_string().contains("Check North"));
    // Charts, tables, names, merges, hidden sheets.
    let sales = sheet_info(&mut s, "Sales");
    assert_eq!(sales["charts"][0]["title"], "Units", "{sales}");
    assert_eq!(sales["tables"].as_array().unwrap().len(), 1);
    assert_eq!(sales["validations"], 1);
    assert_eq!(sales["conditionalFormats"], 1);
    assert_eq!(sheet_info(&mut s, "Notes")["merges"][0], "A1:C1");
    assert_eq!(sheet_info(&mut s, "Secret")["visibility"], "Hidden");
    let wb = structured(&mut s, "inspect_workbook", json!({}));
    assert!(wb["names"].as_array().unwrap().iter().any(|n| n["name"] == "Units" && n["refersTo"] == "=Sales!$B$2:$B$5"), "{wb}");
    assert_eq!(structured(&mut s, "get_cell", json!({"cell": "Sales!F1"}))["value"], 100.0);
    // A hidden sheet reads like any other.
    assert_eq!(structured(&mut s, "read_range", json!({"range": "Secret!B1"}))["values"][0][0], 42.0);
    // As Markdown, and as a picture for a visual check.
    let md = structured(&mut s, "export_markdown", json!({"sheet": "Sales"}));
    assert!(md["markdown"].as_str().unwrap().contains("| 5 | West | 40 | 2.00 | 80.00 |"), "{md}");
    let r = call(&mut s, "render_range", json!({"range": "Sales!A1:D5"}));
    assert_eq!(r["isError"], false, "{r}");
    assert!(r["content"].as_array().unwrap().iter().any(|c| c["type"] == "image" && c["mimeType"] == "image/png"));
    let p = structured(&mut s, "render_preview", json!({"sheet": "Sales"}));
    assert!(p["html"].as_str().unwrap().contains("class=\"chart obj\""));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn coverage_create_sheets_rows_columns_cells_tables_charts_validations_conditional_formats_comments_names() {
    let (mut s, path) = open_fixture("cov-create.xlsx");
    structured(&mut s, "add_sheet", json!({"name": "Plan"}));
    structured(&mut s, "write_range", json!({"range": "Plan!A1", "values": [["Task", "Days"], ["Dig", 3], ["Pour", 2], ["Cure", 7]]}));
    structured(&mut s, "set_cell", json!({"cell": "Plan!B5", "input": "=SUM(B2:B4)"}));
    structured(&mut s, "insert_rows", json!({"rows": "2", "sheet": "Plan"}));
    structured(&mut s, "insert_columns", json!({"cols": "A", "sheet": "Plan"}));
    structured(&mut s, "create_table", json!({"range": "Plan!B1:C5", "name": "PlanTable"}));
    structured(&mut s, "insert_chart", json!({"range": "Plan!B1:C5", "type": "bar", "title": "Days"}));
    structured(&mut s, "data_validation", json!({"range": "Plan!C3:C5", "type": "list", "formula1": "1,2,3,7"}));
    structured(&mut s, "conditional_format", json!({"range": "Plan!C3:C5", "rule": {"type": "dataBar"}}));
    structured(&mut s, "add_comment", json!({"cell": "Plan!B3", "text": "first task"}));
    structured(&mut s, "define_name", json!({"name": "Days", "refers_to": "=Plan!$C$3:$C$5"}));
    let mut t = save_and_reopen(&mut s, &path);
    let plan = sheet_info(&mut t, "Plan");
    assert_eq!(plan["tables"][0]["name"], "PlanTable", "{plan}");
    assert_eq!(plan["charts"][0]["title"], "Days");
    assert_eq!(plan["validations"], 1);
    assert_eq!(plan["conditionalFormats"], 1);
    assert_eq!(plan["comments"], 1);
    // The inserted row and column moved the cells and the formula with them.
    assert_eq!(structured(&mut t, "get_cell", json!({"cell": "Plan!B3"}))["value"], "Dig");
    let total = structured(&mut t, "get_cell", json!({"cell": "Plan!C6"}));
    assert_eq!(total["formula"], "=SUM(C3:C5)", "{total}");
    assert_eq!(total["value"], 12.0);
    let wb = structured(&mut t, "inspect_workbook", json!({}));
    assert!(wb["names"].as_array().unwrap().iter().any(|n| n["name"] == "Days"));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn coverage_edit_everything_in_one_batch() {
    let (mut s, path) = open_fixture("cov-edit.xlsx");
    let sales = sheet_info(&mut s, "Sales");
    let chart = sales["charts"][0]["id"].clone();
    let table = sales["tables"][0]["name"].as_str().unwrap().to_string();
    let v = structured(
        &mut s,
        "batch",
        json!({"ops": [
            {"op": "set", "cell": "Sales!B2", "input": 15},
            {"op": "write", "range": "Sales!A6", "values": [["Central", 50, 4, "=B6*C6"]]},
            {"op": "format", "range": "Sales!B2:B6", "italic": true, "fontColor": "#C00000", "numberFormat": "0"},
            {"tool": "rename_sheet", "sheet": "Notes", "name": "Remarks"},
            {"command": "sheet.activate", "params": {"sheet": "Sales"}},
            {"command": "chart.set", "params": {"chart": chart, "type": "line", "title": "Units (line)"}},
            {"command": "table.style", "params": {"table": table, "style": "TableStyleLight9"}},
            {"command": "table.resize", "params": {"table": table, "range": "A1:D6"}},
            {"op": "validation", "range": "Sales!C2:C6", "type": "decimal", "operator": "greaterThan", "formula1": "0"},
            {"op": "conditional_format", "range": "Sales!D2:D6", "rule": {"type": "colorScale"}},
            {"op": "merge", "range": "Remarks!A4:C4"},
            {"op": "define_name", "name": "Units", "refers_to": "=Sales!$B$2:$B$6", "old_name": "Units"},
            {"op": "execute_command", "command": "review.replyComment", "params": {"cell": "A2", "text": "done"}},
        ]}),
    );
    assert_eq!(v["ok"], 13, "{v}");
    let mut t = save_and_reopen(&mut s, &path);
    let sales = sheet_info(&mut t, "Sales");
    assert_eq!(sales["charts"][0]["title"], "Units (line)", "{sales}");
    assert!(sales["charts"][0]["kind"].as_str().unwrap().starts_with("Line"), "{sales}");
    assert_eq!(sales["tables"][0]["style"], "TableStyleLight9");
    assert_eq!(sales["tables"][0]["range"], "A1:D6");
    assert_eq!(sales["validations"], 2);
    assert_eq!(sales["conditionalFormats"], 2);
    assert!(sheet_info(&mut t, "Remarks")["merges"].as_array().unwrap().iter().any(|m| m == "A4:C4"));
    assert_eq!(structured(&mut t, "get_cell", json!({"cell": "Sales!D6"}))["value"], 200.0);
    assert_eq!(structured(&mut t, "get_cell", json!({"cell": "Sales!F1"}))["value"], 155.0, "the edited name");
    let c = structured(&mut t, "list_comments", json!({"sheet": "Sales"}));
    assert_eq!(c["comments"][0]["replies"][0]["text"], "done", "{c}");
    let _ = std::fs::remove_file(&path);
}

#[test]
fn coverage_delete_cells_rows_columns_sheets_charts_tables_comments_names() {
    let (mut s, path) = open_fixture("cov-delete.xlsx");
    let table = sheet_info(&mut s, "Sales")["tables"][0]["name"].as_str().unwrap().to_string();
    structured(&mut s, "write_range", json!({"range": "Sales!C2", "values": [[null]]}));
    structured(&mut s, "execute_command", json!({"command": "edit.clearContents", "params": {"range": "Sales!C3"}}));
    structured(&mut s, "delete_rows", json!({"rows": "5", "sheet": "Sales"}));
    structured(&mut s, "delete_columns", json!({"cols": "E", "sheet": "Sales"}));
    structured(&mut s, "delete_chart", json!({"sheet": "Sales"}));
    structured(&mut s, "delete_table", json!({"table": table}));
    structured(&mut s, "delete_comment", json!({"cell": "Sales!A2"}));
    structured(&mut s, "delete_name", json!({"name": "Units"}));
    structured(&mut s, "delete_sheet", json!({"sheet": "Notes"}));
    let mut t = save_and_reopen(&mut s, &path);
    let wb = structured(&mut t, "inspect_workbook", json!({}));
    let names: Vec<&str> = wb["sheets"].as_array().unwrap().iter().filter_map(|x| x["name"].as_str()).collect();
    assert_eq!(names, ["Sales", "Secret"], "{wb}");
    assert!(wb["names"].as_array().unwrap().iter().all(|n| n["name"] != "Units"), "{wb}");
    let sales = sheet_info(&mut t, "Sales");
    assert_eq!(sales["charts"].as_array().unwrap().len(), 0, "{sales}");
    assert_eq!(sales["tables"].as_array().unwrap().len(), 0);
    assert_eq!(sales["comments"], 0);
    assert!(structured(&mut t, "get_cell", json!({"cell": "Sales!C2"}))["value"].is_null());
    assert!(structured(&mut t, "get_cell", json!({"cell": "Sales!C3"}))["value"].is_null());
    // Row 5 (West) is gone; the table's cells stay as plain cells.
    assert_eq!(structured(&mut t, "read_range", json!({"range": "Sales!A2:A5"}))["values"], json!([["North"], ["South"], ["East"], [null]]));
    let _ = std::fs::remove_file(&path);
}

#[test]
fn coverage_search_across_sheets_and_replace() {
    let (mut s, path) = open_fixture("cov-search.xlsx");
    let v = structured(&mut s, "find", json!({"what": "north"}));
    let hits: Vec<(String, String)> = v["results"]
        .as_array()
        .unwrap()
        .iter()
        .map(|h| (h["sheet"].as_str().unwrap().to_string(), h["cell"].as_str().unwrap().to_string()))
        .collect();
    assert!(hits.contains(&("Sales".into(), "A2".into())) && hits.contains(&("Notes".into(), "A2".into())), "{v}");
    // A value as shown, a formula's text, one sheet, a hidden sheet, and nothing found.
    assert_eq!(structured(&mut s, "find", json!({"what": "80.00", "whole_cell": true}))["results"][0]["cell"], "D5");
    assert_eq!(structured(&mut s, "find", json!({"what": "B3*C3", "look_in": "formulas"}))["results"][0]["cell"], "D3");
    assert_eq!(structured(&mut s, "find", json!({"what": "north", "sheet": "Notes"}))["count"], 1);
    assert_eq!(structured(&mut s, "find", json!({"what": 42, "whole_cell": true}))["results"][0]["sheet"], "Secret");
    assert_eq!(structured(&mut s, "find", json!({"what": "no such text"}))["count"], 0);
    // Replace across sheets: the answer names the cells changed.
    let v = structured(&mut s, "find_replace", json!({"what": "North", "with": "Nord", "match_case": true}));
    assert_eq!(v["replaced"], 2, "{v}");
    assert_eq!(v["cells"].as_array().unwrap().len(), 2);
    let mut t = save_and_reopen(&mut s, &path);
    assert_eq!(structured(&mut t, "get_cell", json!({"cell": "Sales!A2"}))["value"], "Nord");
    assert_eq!(structured(&mut t, "get_cell", json!({"cell": "Notes!A2"}))["value"], "Nord has the lowest units");
    assert_eq!(structured(&mut t, "find", json!({"what": "North"}))["count"], 0);
    let _ = std::fs::remove_file(&path);
}
