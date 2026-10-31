//! WHIQ 0.7.6: the defects a test investigation found on v0.7.5 (VBA and protection, formulas,
//! charts). Each test drives the MCP tools the way the platform's agents do; the files are built
//! with XlsxWriter (`tests/mcp-contract/fixtures/make_vba_protection.py`) or by the tools.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::Read;
use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::{Headless, Server};

fn server() -> Server {
    Server::new(Box::new(Headless::new()))
}

fn call(s: &mut Server, name: &str, args: Value) -> Value {
    let line = json!({"jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": {"name": name, "arguments": args}}).to_string();
    let reply: Value = serde_json::from_str(&s.handle_line(&line).expect("reply")).expect("JSON");
    assert!(reply.get("error").is_none(), "{reply}");
    reply["result"].clone()
}

/// A call that must succeed: its structured result.
fn ok(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, name, args.clone());
    assert_eq!(r["isError"], false, "{name} {args}: {r}");
    r["structuredContent"].clone()
}

/// A call that must fail: its error object.
fn err(s: &mut Server, name: &str, args: Value) -> Value {
    let r = call(s, name, args.clone());
    assert_eq!(r["isError"], true, "{name} {args} should fail: {r}");
    r["structuredContent"]["error"].clone()
}

fn dir() -> PathBuf {
    let d = std::env::temp_dir().join(format!("gridcraft-mcp-v076-{}", std::process::id()));
    std::fs::create_dir_all(&d).unwrap();
    d
}

/// A copy of a committed fixture in the test folder (the fixture itself is never written).
fn fixture(name: &str, copy: &str) -> PathBuf {
    let src = Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/mcp-contract/fixtures").join(name);
    let to = dir().join(copy);
    std::fs::copy(&src, &to).unwrap();
    to
}

fn open(path: &Path) -> (Server, Value) {
    let mut s = server();
    let v = ok(&mut s, "open_workbook", json!({"path": path.to_str().unwrap()}));
    (s, v)
}

fn save(s: &mut Server, path: &Path) -> Value {
    ok(s, "save_workbook", json!({"path": path.to_str().unwrap(), "overwrite": true}))
}

/// One part of a saved package, as text.
fn part(path: &Path, name: &str) -> String {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut f = z.by_name(name).unwrap_or_else(|_| panic!("{name} is not in {}", path.display()));
    let mut s = String::new();
    f.read_to_string(&mut s).unwrap();
    s
}

fn has_part(path: &Path, name: &str) -> bool {
    zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap().by_name(name).is_ok()
}

fn names(z: &Path) -> Vec<String> {
    let z = zip::ZipArchive::new(std::fs::File::open(z).unwrap()).unwrap();
    z.file_names().map(str::to_string).collect()
}

fn text(v: &Value) -> String {
    v.to_string()
}

fn sheet<'a>(info: &'a Value, name: &str) -> &'a Value {
    info["sheets"].as_array().unwrap().iter().find(|s| s["name"] == name).unwrap_or_else(|| panic!("no sheet {name}: {info}"))
}

// ---------------------------------------------------------------- 1. codeName

#[test]
fn vba_code_names_and_the_other_workbook_and_sheet_properties_survive_a_save() {
    let src = fixture("vba-button.xlsm", "codename.xlsm");
    let (mut s, _) = open(&src);
    ok(&mut s, "set_cell", json!({"cell": "Data!B2", "input": 11}));
    let out = dir().join("codename-saved.xlsm");
    save(&mut s, &out);
    let wb = part(&out, "xl/workbook.xml");
    let pr = wb.split("<workbookPr").nth(1).and_then(|t| t.split('>').next()).unwrap_or_default().to_string();
    assert!(pr.contains("codeName=\"ThisWorkbook\""), "workbookPr: {pr}");
    assert!(pr.contains("defaultThemeVersion=\"124226\""), "the other workbookPr attributes stay: {pr}");
    // Sheet parts are written in order; find each sheet's part by its rows.
    let data = part(&out, "xl/worksheets/sheet1.xml");
    let inputs = part(&out, "xl/worksheets/sheet2.xml");
    assert!(data.contains("<sheetPr codeName=\"Sheet1\""), "{}", &data[..400.min(data.len())]);
    assert!(data.contains("<outlinePr summaryBelow=\"0\"/>"), "outlinePr stays: {}", &data[..400.min(data.len())]);
    assert!(data.contains("fitToPage=\"1\"") && data.contains("<tabColor"));
    assert!(inputs.contains("<sheetPr codeName=\"InputSheet\""), "{}", &inputs[..300.min(inputs.len())]);
    assert!(has_part(&out, "xl/vbaProject.bin"));
    // And the model reads them back: a second save keeps them too.
    let (mut t, _) = open(&out);
    let again = dir().join("codename-again.xlsm");
    save(&mut t, &again);
    assert!(part(&again, "xl/workbook.xml").contains("codeName=\"ThisWorkbook\""));
    assert!(part(&again, "xl/worksheets/sheet2.xml").contains("codeName=\"InputSheet\""));
}

#[test]
fn a_copied_sheet_gets_a_code_name_of_its_own() {
    let src = fixture("vba-button.xlsm", "codename-copy.xlsm");
    let (mut s, _) = open(&src);
    ok(&mut s, "execute_command", json!({"command": "sheet.move", "params": {"sheet": "Inputs", "to": 2, "copy": true}}));
    let out = dir().join("codename-copy-saved.xlsm");
    save(&mut s, &out);
    let all: Vec<String> = names(&out).into_iter().filter(|n| n.starts_with("xl/worksheets/sheet")).collect();
    let mut code_names: Vec<String> =
        all.iter().filter_map(|n| part(&out, n).split("codeName=\"").nth(1).and_then(|t| t.split('"').next()).map(str::to_string)).collect();
    code_names.sort();
    let before = code_names.len();
    code_names.dedup();
    assert_eq!(before, 3, "every sheet has a code name: {code_names:?}");
    assert_eq!(code_names.len(), 3, "no two sheets share one: {code_names:?}");
}

// ---------------------------------------------------------------- 2. structure lock

fn locked_book() -> Server {
    let mut s = server();
    ok(&mut s, "build_workbook", json!({"sheets": [{"name": "A", "rows": [[1]]}, {"name": "B", "rows": [[2]]}, {"name": "Hid", "rows": [[3]]}]}));
    ok(&mut s, "execute_command", json!({"command": "sheet.hide", "params": {"sheet": "Hid"}}));
    ok(&mut s, "execute_command", json!({"command": "review.protectWorkbook", "params": {"on": true}}));
    s
}

#[test]
fn a_structure_lock_refuses_every_sheet_change_excel_refuses() {
    let mut s = locked_book();
    let before = ok(&mut s, "inspect_workbook", json!({}));
    let tries = [
        ("rename_sheet", json!({"sheet": "A", "name": "Renamed"})),
        ("add_sheet", json!({"name": "New"})),
        ("delete_sheet", json!({"sheet": "B"})),
        ("execute_command", json!({"command": "sheet.move", "params": {"sheet": "A", "to": 1}})),
        ("execute_command", json!({"command": "sheet.move", "params": {"sheet": "A", "to": 2, "copy": true}})),
        ("execute_command", json!({"command": "sheet.hide", "params": {"sheet": "B"}})),
        ("execute_command", json!({"command": "sheet.unhide", "params": {"sheet": "Hid"}})),
        ("execute_command", json!({"command": "sheet.tabColor", "params": {"sheet": "A", "color": "#FF0000"}})),
    ];
    for (tool, args) in tries {
        let e = err(&mut s, tool, args.clone());
        assert_eq!(e["code"], "protected", "{tool} {args}: {e}");
    }
    let after = ok(&mut s, "inspect_workbook", json!({}));
    let shape = |v: &Value| {
        v["sheets"].as_array().unwrap().iter().map(|x| (x["name"].clone(), x["visibility"].clone(), x["tabColor"].clone())).collect::<Vec<_>>()
    };
    assert_eq!(shape(&before), shape(&after));
    // Unprotected, the same calls go through.
    ok(&mut s, "execute_command", json!({"command": "review.protectWorkbook", "params": {"on": false}}));
    ok(&mut s, "rename_sheet", json!({"sheet": "A", "name": "Renamed"}));
    ok(&mut s, "execute_command", json!({"command": "sheet.tabColor", "params": {"sheet": "B", "color": "#FF0000"}}));
}

// ---------------------------------------------------------------- 3. VML-only form controls

#[test]
fn an_old_style_form_button_with_no_controls_part_is_kept_and_named() {
    let src = fixture("vba-button.xlsm", "button.xlsm");
    let (mut s, opened) = open(&src);
    assert!(text(&opened).to_lowercase().contains("form control"), "the open says the button is kept: {opened}");
    ok(&mut s, "set_cell", json!({"cell": "Data!B3", "input": 21}));
    let out = dir().join("button-saved.xlsm");
    save(&mut s, &out);
    let data = part(&out, "xl/worksheets/sheet1.xml");
    assert!(data.contains("<legacyDrawing"), "the sheet keeps its VML drawing");
    let vml: Vec<String> = names(&out).into_iter().filter(|n| n.ends_with(".vml")).collect();
    assert_eq!(vml.len(), 1, "{vml:?}");
    let v = part(&out, &vml[0]);
    assert!(v.contains("<x:FmlaMacro>[0]!RunReport</x:FmlaMacro>") && v.contains("ObjectType=\"Button\""), "{v}");
    // With a note on the same sheet both are in the one drawing.
    ok(&mut s, "add_comment", json!({"cell": "Data!A2", "text": "check", "note": true}));
    let out2 = dir().join("button-note.xlsm");
    save(&mut s, &out2);
    let vml: Vec<String> = names(&out2).into_iter().filter(|n| n.ends_with(".vml")).collect();
    let v = part(&out2, &vml[0]);
    assert!(v.contains("[0]!RunReport") && v.contains("ObjectType=\"Note\""), "{v}");
}

// ---------------------------------------------------------------- 4. protection permissions

#[test]
fn sheet_protection_permissions_round_trip() {
    let src = fixture("protected-perms.xlsx", "perms.xlsx");
    let (mut s, _) = open(&src);
    ok(&mut s, "set_cell", json!({"cell": "Locked!B2", "input": 5}));
    let out = dir().join("perms-saved.xlsx");
    save(&mut s, &out);
    let x = part(&out, "xl/worksheets/sheet1.xml");
    let p = x.split("<sheetProtection").nth(1).and_then(|t| t.split("/>").next()).unwrap_or_default().to_string();
    assert!(!p.contains("objects=\"1\"") && !p.contains("scenarios=\"1\""), "objects and scenarios stay editable: {p}");
    assert!(p.contains("insertHyperlinks=\"0\"") && p.contains("pivotTables=\"0\"") && p.contains("formatCells=\"0\""), "{p}");
    // A sheet protected with the defaults protects objects and scenarios, as Excel does.
    ok(&mut s, "execute_command", json!({"command": "review.protectSheet", "params": {"sheet": "Open"}}));
    save(&mut s, &out);
    let x = part(&out, "xl/worksheets/sheet2.xml");
    assert!(x.contains("objects=\"1\"") && x.contains("scenarios=\"1\"") && !x.contains("insertHyperlinks=\"0\""), "{x}");
}

// ---------------------------------------------------------------- 5. protection refusals

#[test]
fn protection_refusals_say_protected_and_what_can_still_be_edited() {
    let src = fixture("protected-perms.xlsx", "refuse.xlsx");
    let (mut s, _) = open(&src);
    let e = err(&mut s, "set_cell", json!({"cell": "Locked!A2", "input": "x"}));
    assert_eq!(e["code"], "protected", "{e}");
    let next = e["next_step"].as_str().unwrap();
    assert!(next.contains("Locked") && next.contains("B2:B4") && next.contains("ask the owner to unprotect"), "{next}");
    let e = err(&mut s, "write_range", json!({"range": "Locked!A3", "values": [["y"]]}));
    assert_eq!(e["code"], "protected", "{e}");
    // An unlocked cell is written.
    ok(&mut s, "set_cell", json!({"cell": "Locked!B3", "input": 7}));
    // Allow Edit Ranges are named too.
    ok(&mut s, "execute_command", json!({"command": "review.unprotectSheet", "params": {"sheet": "Locked"}}));
    ok(
        &mut s,
        "execute_command",
        json!({"command": "review.allowEditRanges", "params": {"sheet": "Locked", "action": "add", "title": "Items", "range": "A2:A4"}}),
    );
    ok(&mut s, "execute_command", json!({"command": "review.protectSheet", "params": {"sheet": "Locked"}}));
    let e = err(&mut s, "set_cell", json!({"cell": "Locked!A1", "input": "x"}));
    assert!(e["next_step"].as_str().unwrap().contains("Items"), "{e}");
    // The workbook's structure.
    ok(&mut s, "execute_command", json!({"command": "review.protectWorkbook", "params": {"on": true}}));
    let e = err(&mut s, "rename_sheet", json!({"sheet": "Open", "name": "Other"}));
    assert_eq!(e["code"], "protected", "{e}");
    assert!(e["next_step"].as_str().unwrap().contains("ask the owner to unprotect"), "{e}");
    // The readers say so.
    let info = ok(&mut s, "inspect_workbook", json!({}));
    assert_eq!(info["structureProtected"], true, "{info}");
    let locked = sheet(&info, "Locked");
    assert_eq!(locked["protected"], true);
    assert!(text(&locked["protection"]).contains("B2:B4") && text(&locked["protection"]).contains("Items"), "{locked}");
    let md = ok(&mut s, "export_markdown", json!({}));
    let md = md["markdown"].as_str().unwrap();
    assert!(md.contains("structure is protected") && md.contains("protected sheet"), "{md}");
    let pv = ok(&mut s, "render_preview", json!({"sheet": "Locked"}));
    assert_eq!(pv["structure_protected"], true);
    assert_eq!(pv["sheets"][0]["protected"], true, "{}", pv["sheets"]);
    assert!(pv["html"].as_str().unwrap().contains("protected"));
}

// ---------------------------------------------------------------- 6. macros dropped on .xlsx

#[test]
fn saving_a_macro_workbook_as_xlsx_warns_that_the_macros_are_dropped() {
    let src = fixture("vba-button.xlsm", "macros.xlsm");
    let (mut s, _) = open(&src);
    let v = save(&mut s, &dir().join("macros.xlsx"));
    assert!(text(&v["warnings"]).contains("macro"), "{v}");
    let v = save(&mut s, &dir().join("macros-again.xlsm"));
    assert!(!text(&v).contains("macros were not saved"), "{v}");
}

// ---------------------------------------------------------------- 7–9. formulas

#[test]
fn a_name_cannot_start_with_a_dot() {
    let mut s = server();
    ok(&mut s, "new_workbook", json!({}));
    for bad in ["=.F35", "=SUM(.A1)", "=?X"] {
        let e = err(&mut s, "set_cell", json!({"cell": "B2", "input": bad}));
        assert_eq!(e["code"], "invalid_argument", "{bad}: {e}");
        assert!(e["reason"].as_str().unwrap().contains("B2"), "the cell is named: {e}");
    }
    // Decimals without a leading zero are numbers, not names.
    assert_eq!(ok(&mut s, "set_cell", json!({"cell": "B3", "input": "=.5+1"}))["value"], "1.5");
}

#[test]
fn only_missing_parentheses_are_added_and_only_at_the_end() {
    let mut s = server();
    ok(&mut s, "new_workbook", json!({}));
    let v = ok(&mut s, "set_cell", json!({"cell": "A1", "input": "=SUM((1+2"}));
    assert_eq!(v["corrected_to"], "=SUM((1+2))", "{v}");
    assert_eq!(v["value"], "3");
    let v = ok(&mut s, "set_cell", json!({"cell": "A2", "input": "=1+2"}));
    assert!(v.get("corrected_to").is_none(), "{v}");
    // The real error is reported where it is, not after a run of added `)`.
    let e = err(&mut s, "set_cell", json!({"cell": "A3", "input": "=IF(A1>0,\"yes\" \"no\""}));
    let r = e["reason"].as_str().unwrap();
    assert_eq!(e["code"], "invalid_argument", "{e}");
    assert!(r.contains("character 16") && !r.contains("))"), "{r}");
    let e = err(&mut s, "set_cell", json!({"cell": "A4", "input": "=SUM(1,2))"}));
    assert!(e["reason"].as_str().unwrap().contains("character 10"), "{e}");
}

#[test]
fn formula_writes_say_what_happened() {
    let mut s = server();
    ok(&mut s, "new_workbook", json!({}));
    let v = ok(&mut s, "set_cell", json!({"cell": "C1", "input": "=VLOKUP(A1,A1:B3,2,FALSE)"}));
    assert!(text(&v["warnings"]).contains("VLOOKUP"), "{v}");
    let v = ok(&mut s, "write_range", json!({"range": "A5", "values": [["=SUM(1,2", "=SUMM(1)"]]}));
    assert!(text(&v).contains("=SUM(1,2)") && text(&v["warnings"]).contains("SUM"), "{v}");
    let e = err(&mut s, "write_range", json!({"range": "A7", "values": [[1, 2], [3, "=1+*2"]]}));
    assert_eq!(e["code"], "invalid_argument");
    assert!(e["reason"].as_str().unwrap().contains("B8"), "{e}");
    let e = err(&mut s, "build_workbook", json!({"sheets": [{"name": "S", "header": ["a", "b"], "rows": [[1, 2], [3, "=1+*2"]]}]}));
    let r = e["reason"].as_str().unwrap();
    assert!(r.contains("row 2") && r.contains("B3"), "{r}");
    assert_eq!(e["code"], "invalid_argument");
    // Every syntax refusal is invalid_argument, and evaluate_formula agrees with set_cell.
    for bad in ["=\"abc", "=1+", "=SUM(1,2))", "=.F35", "=#NOPE!"] {
        let a = err(&mut s, "set_cell", json!({"cell": "D1", "input": bad}));
        let b = err(&mut s, "evaluate_formula", json!({"formula": bad}));
        assert_eq!(a["code"], "invalid_argument", "{bad}: {a}");
        assert_eq!(b["code"], "invalid_argument", "{bad}: {b}");
    }
    let v = ok(&mut s, "evaluate_formula", json!({"formula": "=SUM(1,2"}));
    assert_eq!(v["corrected_to"], "=SUM(1,2)", "{v}");
    assert_eq!(v["result"], 3.0, "{v}");
    let v = ok(&mut s, "evaluate_formula", json!({"formula": "=VLOKUP(1,A1:B2,2)"}));
    assert!(text(&v["warnings"]).contains("VLOOKUP"), "{v}");
    let v = ok(&mut s, "build_workbook", json!({"sheets": [{"name": "S", "rows": [[1, "=SUMM(A1)"]]}]}));
    assert!(text(&v["warnings"]).contains("SUMM"), "{v}");
}

#[test]
fn an_unknown_function_is_saved_as_a_plain_formula() {
    let mut s = server();
    ok(&mut s, "new_workbook", json!({}));
    ok(&mut s, "write_range", json!({"range": "A1", "values": [[1, 2], [3, 4]]}));
    ok(&mut s, "set_cell", json!({"cell": "C1", "input": "=VLOKUP(A1,A1:B2,2,FALSE)"}));
    ok(&mut s, "set_cell", json!({"cell": "C2", "input": "=SQRT(A1:A2)"}));
    let out = dir().join("unknown-fn.xlsx");
    save(&mut s, &out);
    let x = part(&out, "xl/worksheets/sheet1.xml");
    let c1 = x.split("<c r=\"C1\"").nth(1).and_then(|t| t.split("</c>").next()).unwrap_or_default().to_string();
    assert!(!c1.contains("cm=\"1\"") && !c1.contains("t=\"array\""), "C1: {c1}");
    // A known function that intersects a range is still a dynamic-array formula, as Excel saves it.
    let c2 = x.split("<c r=\"C2\"").nth(1).and_then(|t| t.split("</c>").next()).unwrap_or_default().to_string();
    assert!(c2.contains("cm=\"1\""), "C2: {c2}");
}

// ---------------------------------------------------------------- 10–12. charts

fn combo_book(s: &mut Server) {
    ok(s, "new_workbook", json!({}));
    ok(s, "write_range", json!({"range": "A1", "values": [["Month", "Sales", "Margin"], ["Jan", 100, 0.2], ["Feb", 150, 0.25], ["Mar", 120, 0.3]]}));
}

#[test]
fn inspect_and_markdown_describe_each_chart() {
    let mut s = server();
    ok(
        &mut s,
        "build_workbook",
        json!({"sheets": [{"name": "Data", "header": ["Month", "Sales", "Margin"],
        "rows": [["Jan", 100, 0.2], ["Feb", 150, 0.25], ["Mar", 120, 0.3]],
        "charts": [{"type": "combo", "title": "Sales and margin", "at": "E2", "y_title": "Sales", "y2_title": "Margin",
                    "series": [{"name": "Sales", "values": "B2:B4", "categories": "A2:A4"},
                               {"name": "Margin", "values": "C2:C4", "categories": "A2:A4", "type": "line", "secondary": true}]}]}]}),
    );
    let info = ok(&mut s, "inspect_workbook", json!({}));
    let ch = &sheet(&info, "Data")["charts"][0];
    assert_eq!(ch["title"], "Sales and margin", "{ch}");
    assert_eq!(ch["anchor"]["cell"], "E2", "{ch}");
    assert_eq!(ch["type"], "combo");
    assert_eq!(ch["y2_title"], "Margin");
    let series = ch["series"].as_array().expect("series listed");
    assert_eq!(series.len(), 2);
    assert_eq!(series[0]["kind"], "column");
    assert_eq!(series[0]["axis"], "primary");
    assert_eq!(series[1]["kind"], "line");
    assert_eq!(series[1]["axis"], "secondary");
    assert!(series[1]["values"].as_str().unwrap().contains("$C$2:$C$4"), "{ch}");
    let md = ok(&mut s, "export_markdown", json!({}));
    let md = md["markdown"].as_str().unwrap();
    assert!(md.contains("Sales and margin") && md.contains("E2") && md.contains("secondary") && md.contains("$C$2:$C$4"), "{md}");
}

#[test]
fn insert_chart_makes_a_dual_axis_combo_and_a_series_can_be_moved_to_the_secondary_axis() {
    let mut s = server();
    combo_book(&mut s);
    let v = ok(
        &mut s,
        "insert_chart",
        json!({"range": "A1:C4", "type": "combo", "title": "Dual", "y_title": "Sales", "y2_title": "Margin",
               "series": [{"type": "column"}, {"type": "line", "secondary": true}]}),
    );
    let id = v["chart"].as_u64().unwrap();
    let info = ok(&mut s, "inspect_workbook", json!({}));
    let ch = &sheet(&info, "Sheet1")["charts"][0];
    assert_eq!(ch["series"][1]["axis"], "secondary", "{ch}");
    assert_eq!(ch["series"][0]["axis"], "primary", "{ch}");
    // An existing chart: one series moved to the secondary axis as a line.
    let v = ok(&mut s, "insert_chart", json!({"range": "A1:C4", "type": "column", "at": "J2"}));
    let second = v["chart"].as_u64().unwrap();
    ok(&mut s, "execute_command", json!({"command": "chart.setSeries", "params": {"chart": second, "series": 2, "secondary": true, "type": "line"}}));
    let info = ok(&mut s, "inspect_workbook", json!({}));
    let ch = sheet(&info, "Sheet1")["charts"].as_array().unwrap().iter().find(|c| c["id"] == second).unwrap().clone();
    assert_eq!(ch["type"], "combo", "{ch}");
    assert_eq!(ch["series"][1]["axis"], "secondary", "{ch}");
    assert_eq!(ch["series"][1]["kind"], "line", "{ch}");
    // A file holds both axes and the secondary axis' title.
    let out = dir().join("dual.xlsx");
    save(&mut s, &out);
    let charts: Vec<String> = names(&out).into_iter().filter(|n| n.starts_with("xl/charts/chart")).collect();
    assert_eq!(charts.len(), 2);
    let all: String = charts.iter().map(|c| part(&out, c)).collect();
    assert!(all.matches("<c:valAx>").count() >= 4, "two value axes per chart");
    assert!(all.contains("Margin"), "the secondary axis title");
    let (mut t, _) = open(&out);
    let info = ok(&mut t, "inspect_workbook", json!({}));
    let first = sheet(&info, "Sheet1")["charts"].as_array().unwrap().iter().find(|c| c["title"] == "Dual").unwrap().clone();
    assert_eq!(first["y2_title"], "Margin", "{first}");
    assert_eq!(first["y_title"], "Sales", "{first}");
    assert_eq!(first["series"][1]["axis"], "secondary");
    let _ = id;
}

// ---------------------------------------------------------------- G2. corpus run: controls, pivots

#[test]
fn excel_2010_form_controls_keep_their_drawing_twin() {
    let src = fixture("form-controls-2010.xlsx", "controls.xlsx");
    let (mut s, _) = open(&src);
    ok(&mut s, "set_cell", json!({"cell": "B2", "input": 21}));
    let out = dir().join("controls-saved.xlsx");
    save(&mut s, &out);
    let sheet = part(&out, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains("<controls>") && sheet.contains("<drawing ") && sheet.contains("<legacyDrawing"), "{sheet}");
    let drawings: Vec<String> = names(&out).into_iter().filter(|n| n.starts_with("xl/drawings/drawing") && n.ends_with(".xml")).collect();
    assert_eq!(drawings.len(), 1, "{:?}", names(&out));
    let d = part(&out, &drawings[0]);
    for sid in ["_x0000_s1025", "_x0000_s1026"] {
        assert!(d.contains(&format!("<a14:compatExt spid=\"{sid}\"/>")), "{sid} twin: {d}");
    }
    assert!(d.contains("id=\"1025\"") && d.contains("Requires=\"a14\""), "{d}");
    // Saved twice, still one twin per control.
    let (mut t, _) = open(&out);
    let again = dir().join("controls-again.xlsx");
    save(&mut t, &again);
    let d: String =
        names(&again).into_iter().filter(|n| n.starts_with("xl/drawings/drawing") && n.ends_with(".xml")).map(|n| part(&again, &n)).collect();
    assert_eq!(d.matches("a14:compatExt").count(), 2, "{d}");
}

#[test]
fn pivot_tables_that_share_a_cache_keep_sharing_it() {
    let src = fixture("pivot-shared-cache.xlsx", "shared-cache.xlsx");
    let (mut s, _) = open(&src);
    ok(&mut s, "set_cell", json!({"cell": "Data!C2", "input": 11}));
    let out = dir().join("shared-cache-saved.xlsx");
    save(&mut s, &out);
    let caches: Vec<String> =
        names(&out).into_iter().filter(|n| n.starts_with("xl/pivotCache/pivotCacheDefinition") && n.ends_with(".xml")).collect();
    let tables: Vec<String> = names(&out).into_iter().filter(|n| n.starts_with("xl/pivotTables/pivotTable") && n.ends_with(".xml")).collect();
    assert_eq!(tables.len(), 2, "{:?}", names(&out));
    assert_eq!(caches.len(), 1, "one cache for both tables, as the file had it: {caches:?}");
    assert_eq!(part(&out, "xl/workbook.xml").matches("<pivotCache ").count(), 1);
    // Both tables name it, and a second save keeps it so.
    let (mut t, _) = open(&out);
    let again = dir().join("shared-cache-again.xlsx");
    save(&mut t, &again);
    assert_eq!(names(&again).iter().filter(|n| n.starts_with("xl/pivotCache/pivotCacheDefinition") && n.ends_with(".xml")).count(), 1);
    let info = ok(&mut t, "inspect_workbook", json!({}));
    assert!(text(&info).contains("Pivot2"));
}
