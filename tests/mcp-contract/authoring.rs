//! gridcraft: build_workbook (sheets, formats, frozen header, autofilter, table, native charts),
//! formulas with recalculated cached values, recalculate, and in-place edits that keep charts,
//! PivotTables, slicers and parts GridCraft does not model.

use std::io::{Read, Write};
use std::path::Path;

use serde_json::json;

use crate::harness::{Mcp, binary, cells_of, folder, zip_entries, zip_text};

fn start(dir: &Path, author: &str) -> Mcp {
    let d = dir.to_str().unwrap();
    Mcp::start(&["--automation-read-root", d, "--automation-write-root", d, "--author", author])
}

#[test]
fn build_workbook_with_native_charts() {
    let dir = folder("build");
    let mut m = start(&dir, "Report Bot");
    let v = m.ok(
        "build_workbook",
        json!({
            "path": "q3.xlsx",
            "title": "Q3",
            "sheets": [
                {
                    "name": "Sales",
                    "header": ["Region", "Units", "Price", "Revenue", "Share"],
                    "rows": [
                        ["North", 120, 2.5, "=B2*C2", "=D2/$D$6"],
                        ["South", 80, 3.1, "=B3*C3", "=D3/$D$6"],
                        ["East", 150, 2.2, "=B4*C4", "=D4/$D$6"],
                        ["West", 95, 2.9, "=B5*C5", "=D5/$D$6"],
                        ["Total", "=SUM(B2:B5)", null, "=SUM(D2:D5)", "=SUM(E2:E5)"]
                    ],
                    "columns": [{"width": 16}, {"format": "#,##0"}, {"format": "0.00"}, {"format": "#,##0.00"}, {"format": "0.0%"}],
                    "freeze_header": true,
                    "table": {"name": "SalesTable", "style": "TableStyleMedium2"},
                    "charts": [
                        {"type": "bar", "title": "Units by region", "x_title": "Region", "y_title": "Units", "series": [{"name": "Units", "values": "B2:B5", "categories": "A2:A5"}], "at": "H2"},
                        {"type": "line", "title": "Revenue", "data": "A1:A5", "series": [{"name": "Revenue", "values": "D2:D5", "categories": "A2:A5"}], "at": "H20"},
                        {"type": "pie", "title": "Share", "data": "A1:B5", "at": "P2"},
                        {"type": "scatter", "title": "Price vs units", "series": [{"name": "Price", "values": "C2:C5", "categories": "B2:B5"}], "at": "P20"},
                        {"type": "combo", "title": "Units and revenue", "series": [{"name": "Units", "values": "B2:B5", "categories": "A2:A5"}, {"name": "Revenue", "values": "D2:D5", "secondary": true}], "at": "H38"}
                    ]
                },
                {"name": "Log", "header": ["When", "What"], "rows": [["2026-10-01", "start"], ["2026-10-02", "end"]], "autofilter": true, "column_widths": {"A": 14, "B": 30}, "number_formats": {"A": "yyyy-mm-dd"}}
            ]
        }),
    );
    assert_eq!(v["charts"], 5, "{v}");
    let path = dir.join("q3.xlsx");
    let parts = zip_entries(&path);
    for c in 1..=5 {
        assert!(parts.contains(&format!("xl/charts/chart{c}.xml")), "{parts:?}");
    }
    let all_charts: String = (1..=5).map(|c| zip_text(&path, &format!("xl/charts/chart{c}.xml"))).collect();
    assert!(all_charts.contains("<c:barDir val=\"bar\"/>"), "horizontal bars");
    for kind in ["<c:barChart>", "<c:lineChart>", "<c:pieChart>", "<c:scatterChart>"] {
        assert!(all_charts.contains(kind), "{kind}");
    }
    let combo = (1..=5).map(|c| zip_text(&path, &format!("xl/charts/chart{c}.xml"))).find(|x| x.contains("Units and revenue")).unwrap();
    assert!(combo.contains("<c:barChart>") && combo.contains("<c:lineChart>"), "combo = columns + line");
    assert_eq!(combo.matches("<c:valAx>").count(), 2, "secondary axis");
    assert!(all_charts.contains("Units by region") && all_charts.contains("Region"), "titles and axis titles");
    let sheet1 = zip_text(&path, "xl/worksheets/sheet1.xml");
    assert!(sheet1.contains("state=\"frozen\"") && sheet1.contains("ySplit=\"1\""), "frozen header");
    assert!(sheet1.contains("<f>B2*C2</f><v>300</v>"), "formulas keep cached values: {sheet1}");
    assert!(sheet1.contains("<f>SUM(D2:D5)</f><v>1153.5</v>"));
    assert!(parts.iter().any(|p| p.starts_with("xl/tables/")), "a table");
    let sheet2 = zip_text(&path, "xl/worksheets/sheet2.xml");
    assert!(sheet2.contains("<autoFilter ref=\"A1:B3\""), "{sheet2}");
    let styles = zip_text(&path, "xl/styles.xml");
    for f in ["0.0%", "yyyy-mm-dd"] {
        assert!(styles.contains(f), "number format {f}");
    }
    assert!(styles.contains("numFmtId=\"4\"") && styles.contains("numFmtId=\"3\""), "built-in #,##0.00 and #,##0");
    let core = zip_text(&path, "docProps/core.xml");
    assert!(core.contains("<dc:creator>Report Bot</dc:creator>") && core.contains("<dc:title>Q3</dc:title>"), "{core}");
    // The cached values are right: reopened without recalculating, and the check oracle agrees.
    assert_eq!(cells_of(&path, "Sales!D6"), json!([[1153.5]]));
    let out = std::process::Command::new(binary()).args(["check", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    // A bad spec says what to fix, and leaves the open workbook alone.
    let e = m.err("build_workbook", json!({"sheets": [{"name": "X", "rows": [[1]], "charts": [{"type": "radial"}]}]}));
    assert_eq!(e["code"], "invalid_argument", "{e}");
    assert!(e["reason"].as_str().unwrap().contains("chart 1"), "{e}");
    assert_eq!(m.ok("inspect_workbook", json!({}))["sheets"][0]["name"], "Sales");
}

/// WHIQ: a scatter chart (the platform's Line-of-Balance: one production line per trade) is a
/// solid straight line per series, with markers unless `markers: false`; `line: false` keeps the
/// markers-only scatter. Both build_workbook and insert_chart.
#[test]
fn scatter_charts_are_solid_lines_per_series() {
    let dir = folder("scatter-lines");
    let mut m = start(&dir, "LOB");
    m.ok(
        "build_workbook",
        json!({
            "path": "lob.xlsx",
            "sheets": [{
                "name": "LOB",
                "header": ["Day", "Excavation", "Day", "Concrete"],
                "rows": [[0, 1, 5, 1], [10, 20, 15, 20]],
                "charts": [
                    {"type": "scatter", "title": "Line of balance", "markers": false, "series": [
                        {"name": "Excavation", "categories": "A2:A3", "values": "B2:B3", "color": "#2563EB"},
                        {"name": "Concrete", "categories": "C2:C3", "values": "D2:D3", "line_width": 3}
                    ]},
                    {"type": "scatter", "line": false, "series": [{"categories": "A2:A3", "values": "B2:B3"}]}
                ]
            }]
        }),
    );
    let path = dir.join("lob.xlsx");
    let lob = zip_text(&path, "xl/charts/chart1.xml");
    assert!(lob.contains("<c:scatterStyle val=\"lineMarker\"/>"), "{lob}");
    assert!(lob.contains("<a:ln w=\"28575\" cap=\"rnd\"><a:solidFill><a:srgbClr val=\"2563EB\"/></a:solidFill>"), "{lob}");
    assert!(lob.contains("<a:ln w=\"38100\" cap=\"rnd\"><a:solidFill><a:srgbClr"), "{lob}");
    assert_eq!(lob.matches("<c:marker><c:symbol val=\"none\"/></c:marker>").count(), 2, "{lob}");
    assert!(!lob.contains("noFill"), "{lob}");
    let pts = zip_text(&path, "xl/charts/chart2.xml");
    assert!(pts.contains("<a:noFill/>") && pts.contains("<c:symbol val=\"circle\"/>"), "markers only: {pts}");
    // insert_chart: scatter = lines and markers by default; the options reach it.
    m.ok("insert_chart", json!({"range": "A1:B3", "type": "scatter", "title": "Default", "at": "H2"}));
    m.ok("insert_chart", json!({"range": "A1:B3", "type": "scatter", "markers": false, "line_width": 1.5, "color": "#E11D48", "at": "H20"}));
    m.ok("save_workbook", json!({"path": "lob.xlsx"}));
    let all: Vec<String> = (1..=4).map(|c| zip_text(&path, &format!("xl/charts/chart{c}.xml"))).collect();
    let default = all.iter().find(|x| x.contains("Default")).unwrap();
    assert!(default.contains("cap=\"rnd\"><a:solidFill>") && default.contains("<c:symbol val=\"circle\"/>"), "{default}");
    assert!(
        all.iter()
            .any(|x| x.contains("<a:ln w=\"19050\" cap=\"rnd\"><a:solidFill><a:srgbClr val=\"E11D48\"/>") && x.contains("<c:symbol val=\"none\"/>")),
        "{all:?}"
    );
}

/// WHIQ: the platform's schedule workbook in one call — section titles and table heads styled,
/// Gantt bars as real fills, row heights in points, ID / Activity frozen, WBS codes kept as text
/// and ISO dates as dates — instead of build, format_range / execute_command per sheet, save.
#[test]
fn build_workbook_formats_in_one_call() {
    let dir = folder("build-styled");
    let mut m = start(&dir, "Schedule");
    let v = m.ok(
        "build_workbook",
        json!({
            "path": "schedule.xlsx",
            "literal_strings": true,
            "sheets": [{
                "name": "Gantt (weekly)",
                "rows": [
                    ["High-Level Timeline · 1 column = 1 week"],
                    ["ID", "Activity", "Start", "W1", "W2"],
                    ["2.10", "Excavation", "2026-04-03", {"value": "", "fill": "#3B82F6"}, null],
                    ["2.20", "=not a formula", "2026-04-10", null, {"value": "", "fill": "#E11D48"}]
                ],
                "merges": ["A1:E1"],
                "styles": [
                    {"range": "A1:E1", "bold": true, "font_size": 12, "fill": "#EAF1FB", "font_color": "#1D4ED8"},
                    {"range": "A2:E2", "bold": true, "fill": "#1D4ED8", "font_color": "#FFFFFF", "wrap": true, "borders": "all"}
                ],
                "row_heights": [{"row": 1, "height": 20}, {"row": 2, "height": 22}],
                "column_widths": [6, 38, 12, 3.2, 3.2],
                "freeze": "D3"
            }]
        }),
    );
    assert!(v["document"].as_str().is_some_and(|d| d.starts_with("doc-")), "{v}");
    let path = dir.join("schedule.xlsx");
    let sheet = zip_text(&path, "xl/worksheets/sheet1.xml");
    assert!(sheet.contains("ht=\"20\"") && sheet.contains("ht=\"22\""), "row heights in points: {sheet}");
    assert!(sheet.contains("xSplit=\"3\"") && sheet.contains("ySplit=\"2\""), "frozen at D3: {sheet}");
    let styles = zip_text(&path, "xl/styles.xml");
    for rgb in ["3B82F6", "E11D48", "1D4ED8", "EAF1FB", "FFFFFF"] {
        assert!(styles.contains(rgb), "{rgb}");
    }
    assert!(styles.contains("yyyy-mm-dd"), "dates formatted by the engine");
    assert_eq!(cells_of(&path, "A3:C4"), json!([["2.10", "Excavation", 46115.0], ["2.20", "=not a formula", 46122.0]]), "text kept, dates as dates");
    // The WBS codes are text cells, the dates numbers.
    let r = m.ok("read_range", json!({"range": "A3:C3", "formulas": true}));
    assert!(r.to_string().contains("2.10"), "{r}");
    assert_eq!(m.ok("evaluate_formula", json!({"formula": "=ISTEXT(A3)+ISNUMBER(C3)*2"}))["result"], json!(3.0), "text and a date");
    let out = std::process::Command::new(binary()).args(["check", path.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

#[test]
fn recalculate_reports_formulas_and_errors() {
    let dir = folder("recalc");
    let mut m = start(&dir, "Calc");
    m.ok("write_range", json!({"range": "A1", "values": [[2, 3, "=A1*B1", "=A1/0", "=NOPE(1)"]]}));
    let v = m.ok("recalculate", json!({}));
    assert_eq!(v["formulas"], 3, "{v}");
    assert_eq!(v["errorCount"], 2);
    let errs: Vec<String> = v["errors"].as_array().unwrap().iter().map(|e| e["error"].as_str().unwrap().to_string()).collect();
    assert!(errs.contains(&"#DIV/0!".to_string()) && errs.contains(&"#NAME?".to_string()), "{errs:?}");
    // The CLI has it too.
    m.ok("save_workbook", json!({"path": "calc.xlsx"}));
    let out = std::process::Command::new(binary()).args(["recalculate", dir.join("calc.xlsx").to_str().unwrap(), "--json"]).output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    let r: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(r["errorCount"], 2);
}

/// Adds parts GridCraft does not model to an .xlsx: custom document properties (package root)
/// and a custom XML data part (workbook).
fn add_foreign_parts(path: &Path) {
    let mut z = zip::ZipArchive::new(std::fs::File::open(path).unwrap()).unwrap();
    let mut entries: Vec<(String, Vec<u8>)> = Vec::new();
    for i in 0..z.len() {
        let mut f = z.by_index(i).unwrap();
        let mut b = Vec::new();
        f.read_to_end(&mut b).unwrap();
        entries.push((f.name().to_string(), b));
    }
    for (name, data) in &mut entries {
        let text = String::from_utf8_lossy(data).into_owned();
        let edited = match name.as_str() {
            "[Content_Types].xml" => text.replace(
                "</Types>",
                "<Override PartName=\"/docProps/custom.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.custom-properties+xml\"/><Override PartName=\"/customXml/itemProps1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.customXmlProperties+xml\"/></Types>",
            ),
            "_rels/.rels" => text.replace(
                "</Relationships>",
                "<Relationship Id=\"rIdWhiq\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/custom-properties\" Target=\"docProps/custom.xml\"/></Relationships>",
            ),
            "xl/_rels/workbook.xml.rels" => text.replace(
                "</Relationships>",
                "<Relationship Id=\"rIdWhiqXml\" Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXml\" Target=\"../customXml/item1.xml\"/></Relationships>",
            ),
            _ => continue,
        };
        *data = edited.into_bytes();
    }
    entries.push((
        "docProps/custom.xml".into(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<Properties xmlns="http://schemas.openxmlformats.org/officeDocument/2006/custom-properties" xmlns:vt="http://schemas.openxmlformats.org/officeDocument/2006/docPropsVTypes"><property fmtid="{D5CDD505-2E9C-101B-9397-08002B2CF9AE}" pid="2" name="ProjectCode"><vt:lpwstr>CRC-17A</vt:lpwstr></property></Properties>"#
            .to_vec(),
    ));
    entries.push(("customXml/item1.xml".into(), b"<whiq:data xmlns:whiq=\"urn:whiq:test\"><keep>me</keep></whiq:data>".to_vec()));
    entries.push((
        "customXml/itemProps1.xml".into(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="no"?><ds:datastoreItem ds:itemID="{11111111-2222-3333-4444-555555555555}" xmlns:ds="http://schemas.openxmlformats.org/officeDocument/2006/customXml"><ds:schemaRefs/></ds:datastoreItem>"#
            .to_vec(),
    ));
    entries.push((
        "customXml/_rels/item1.xml.rels".into(),
        br#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?><Relationships xmlns="http://schemas.openxmlformats.org/package/2006/relationships"><Relationship Id="rId1" Type="http://schemas.openxmlformats.org/officeDocument/2006/relationships/customXmlProps" Target="itemProps1.xml"/></Relationships>"#
            .to_vec(),
    ));
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in entries {
        w.start_file(name, opts).unwrap();
        w.write_all(&data).unwrap();
    }
    w.finish().unwrap();
}

#[test]
fn in_place_edits_keep_charts_pivots_slicers_and_unmodelled_parts() {
    let dir = folder("inplace");
    let base = dir.join("customer.xlsx");
    // A customer workbook: a table with a chart, a PivotTable and a slicer.
    {
        let mut s = gridcraft_engine::Session::new();
        s.new_workbook();
        let ex =
            |s: &mut gridcraft_engine::Session, id: &str, p: serde_json::Value| s.execute_headless(id, p).unwrap_or_else(|e| panic!("{id}: {e}"));
        ex(&mut s, "sheet.rename", json!({"name": "Data"}));
        ex(
            &mut s,
            "range.setValues",
            json!({"range": "A1", "values": [["Region", "Product", "Units"], ["North", "Bolts", 10], ["South", "Nuts", 20], ["North", "Nuts", 5], ["East", "Bolts", 7]]}),
        );
        ex(&mut s, "insert.table", json!({"range": "A1:C5", "name": "Sales"}));
        ex(&mut s, "insert.chart", json!({"range": "A1:C5", "type": "column", "title": "Units", "at": "F2"}));
        ex(&mut s, "insert.slicer", json!({"table": "Sales", "column": "Region", "at": "F20"}));
        ex(&mut s, "insert.pivotTable", json!({"source": "Data!$A$1:$C$5", "destination": "new", "rows": ["Region"], "values": ["Units"]}));
        ex(&mut s, "file.save", json!({"path": base.to_str().unwrap()}));
    }
    add_foreign_parts(&base);
    let before = zip_entries(&base);
    let custom_props = zip_text(&base, "docProps/custom.xml");
    let custom_xml = zip_text(&base, "customXml/item1.xml");
    for want in ["xl/charts/chart1.xml", "xl/slicers/slicer1.xml", "xl/slicerCaches/slicerCache1.xml"] {
        assert!(before.contains(&want.to_string()), "fixture has {want}: {before:?}");
    }
    assert!(before.iter().any(|p| p.starts_with("xl/pivotTables/")) && before.iter().any(|p| p.starts_with("xl/pivotCache/")), "{before:?}");

    let mut m = start(&dir, "Editor");
    m.ok("open_workbook", json!({"path": "customer.xlsx"}));
    m.ok("set_cell", json!({"cell": "Data!C2", "input": "11"}));
    m.ok("write_range", json!({"range": "Data!E1", "values": [["Note"], ["checked"]]}));
    m.ok("insert_rows", json!({"rows": "7", "sheet": "Data"}));
    m.ok("insert_columns", json!({"cols": "H", "sheet": "Data"}));
    m.ok("delete_rows", json!({"rows": "30", "sheet": "Data"}));
    m.ok("delete_columns", json!({"cols": "Z", "sheet": "Data"}));
    m.ok(
        "conditional_format",
        json!({"range": "Data!C2:C5", "rule": {"type": "cellIs", "operator": "greaterThan", "value": "8", "preset": "lightRedFill"}}),
    );
    m.ok("data_validation", json!({"range": "Data!B2:B5", "type": "list", "formula1": "Bolts,Nuts"}));
    m.ok("add_sheet", json!({"name": "Summary"}));
    m.ok("rename_sheet", json!({"sheet": "Summary", "name": "Résumé ملخص"}));
    m.ok("write_range", json!({"range": "'Résumé ملخص'!A1", "values": [["Total", "=SUM(Data!C2:C5)"]]}));
    let v = m.ok("save_workbook", json!({}));
    assert!(v["previous_files_renamed"][0]["previous"].as_str().unwrap().ends_with("OLD_customer.xlsx"), "the original is kept: {v}");

    let after = zip_entries(&base);
    for p in before.iter().filter(|p| !p.starts_with("xl/worksheets/") && !p.ends_with(".rels") && *p != "[Content_Types].xml") {
        let kept = after.contains(p) || (p.starts_with("xl/") && after.iter().any(|a| a.rsplit('/').next() == p.rsplit('/').next()));
        assert!(kept, "{p} survives the edit: {after:?}");
    }
    assert_eq!(zip_text(&base, "docProps/custom.xml"), custom_props, "custom document properties unchanged");
    let kept_xml = after.iter().find(|p| p.starts_with("customXml/item") && !p.contains("Props") && p.ends_with(".xml")).expect("custom XML kept");
    assert_eq!(zip_text(&base, kept_xml), custom_xml);
    assert!(after.iter().any(|p| p.starts_with("xl/pivotTables/")) && after.iter().any(|p| p.starts_with("xl/slicers/")), "{after:?}");
    // The Data sheet's part (the PivotTable sheet was inserted before it).
    let data = after
        .iter()
        .filter(|p| p.starts_with("xl/worksheets/sheet") && p.ends_with(".xml"))
        .map(|p| zip_text(&base, p))
        .find(|x| x.contains("<tablePart"))
        .expect("the Data sheet");
    assert!(data.contains("<conditionalFormatting") && data.contains("<dataValidation"), "{data}");
    assert!(data.contains("<tablePart") && data.contains("<drawing"), "table and chart still attached");
    assert_eq!(cells_of(&base, "Data!C2:C2"), json!([[11.0]]));
    assert_eq!(cells_of(&base, "Data!E2"), json!([["checked"]]));
    assert_eq!(cells_of(&base, "'Résumé ملخص'!B1"), json!([[43.0]]));
    let core = zip_text(&base, "docProps/core.xml");
    assert!(core.contains("<cp:lastModifiedBy>Editor</cp:lastModifiedBy>"), "{core}");
    // GridCraft's own oracle: cached values agree with a recalculation, the round trip is clean.
    let out = std::process::Command::new(binary()).args(["check", base.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

/// An Excel-shaped workbook with a PivotTable and two PivotTable slicers survives open → edit →
/// save → reopen, with the slicer references rewritten, and `check` passes.
#[test]
fn pivot_and_slicers_survive_open_edit_save_reopen() {
    let dir = folder("pivotslicer");
    let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/mcp-contract/fixtures/pivot-slicer.xlsx");
    let book = dir.join("pivot.xlsx");
    std::fs::copy(&src, &book).unwrap();
    let mut m = start(&dir, "Analyst");
    m.ok("open_workbook", json!({"path": "pivot.xlsx"}));
    // Edit the source, the slicer and the sheet the PivotTable is on.
    m.ok("set_cell", json!({"cell": "Data!C2", "input": "100"}));
    m.ok("execute_command", json!({"command": "sheet.activate", "params": {"sheet": "Pivot"}}));
    let list = m.ok("execute_command", json!({"command": "slicer.list", "params": {}}));
    let product =
        list["result"]["slicers"].as_array().or(list["slicers"].as_array()).unwrap().iter().find(|x| x["field"] == "Product").cloned().unwrap();
    assert_eq!(product["selected"], json!(["Bolts"]), "{list}");
    assert_eq!(product["pivots"], json!(["Pivot!PivotTable1"]));
    m.ok("execute_command", json!({"command": "pivot.refresh", "params": {"replace": true}}));
    m.ok("rename_sheet", json!({"sheet": "Pivot", "name": "Report"}));
    m.ok("insert_rows", json!({"rows": "20", "sheet": "Report"}));
    m.ok("save_workbook", json!({}));
    let parts = zip_entries(&book);
    for want in
        ["xl/pivotTables/pivotTable1.xml", "xl/pivotCache/pivotCacheDefinition1.xml", "xl/pivotCache/pivotCacheRecords1.xml", "docProps/custom.xml"]
    {
        assert!(parts.contains(&want.to_string()), "{want}: {parts:?}");
    }
    assert_eq!(parts.iter().filter(|p| p.starts_with("xl/slicerCaches/")).count(), 2, "{parts:?}");
    assert!(zip_text(&book, "docProps/custom.xml").contains("CRC-17A"));
    let cache = zip_text(&book, "xl/pivotCache/pivotCacheDefinition1.xml");
    assert!(cache.contains("refreshOnLoad=\"1\""));
    let table = zip_text(&book, "xl/pivotTables/pivotTable1.xml");
    assert!(table.contains("PivotStyleMedium9"), "the PivotTable style survives");
    let wbx = zip_text(&book, "xl/workbook.xml");
    let report_id = wbx.split("<sheet name=\"Report\" sheetId=\"").nth(1).and_then(|r| r.split('"').next()).unwrap().to_string();
    for p in parts.iter().filter(|p| p.starts_with("xl/slicerCaches/")) {
        let x = zip_text(&book, p);
        assert!(x.contains(&format!("<pivotTable tabId=\"{report_id}\" name=\"PivotTable1\"/>")), "{x}");
    }
    // The report reflects the edit (Bolts only, North 100 + 2): 100+2+7+3+4 = 116.
    assert_eq!(cells_of(&book, "Report!B8"), json!([[116.0]]), "grand total");
    // Reopen in a fresh process: the slicers are still connected, then toggle one and save again.
    drop(m);
    let mut m = start(&dir, "Analyst");
    m.ok("open_workbook", json!({"path": "pivot.xlsx"}));
    m.ok("execute_command", json!({"command": "sheet.activate", "params": {"sheet": "Report"}}));
    let list = m.ok("execute_command", json!({"command": "slicer.list", "params": {}}));
    let slicers = list["result"]["slicers"].as_array().or(list["slicers"].as_array()).cloned().unwrap();
    assert!(slicers.iter().all(|x| x["pivots"] == json!(["Report!PivotTable1"])), "{list}");
    m.ok("execute_command", json!({"command": "slicer.selectAll", "params": {"slicer": "Product"}}));
    m.ok("save_workbook", json!({"overwrite": true}));
    assert_eq!(cells_of(&book, "Report!B8"), json!([[149.0]]), "every product once the Product slicer is cleared");
    let out = std::process::Command::new(binary()).args(["check", book.to_str().unwrap()]).output().unwrap();
    assert!(out.status.success(), "check: {}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
}

// ---------------------------------------------------------------- a customer's file, edited in place

/// The theme of the customer fixture: original content (GridCraft contributors), laid out as theme
/// parts are, with what a regenerated theme used to lose: effect styles (a shadow, a 3-D scene),
/// gradient fills and a per-script font table (Arabic, Hebrew, Japanese…).
const CUSTOMER_THEME: &str = r#"<?xml version="1.0" encoding="UTF-8" standalone="yes"?>
<a:theme xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main" name="Harbour"><a:themeElements><a:clrScheme name="Harbour"><a:dk1><a:sysClr val="windowText" lastClr="000000"/></a:dk1><a:lt1><a:sysClr val="window" lastClr="FFFFFF"/></a:lt1><a:dk2><a:srgbClr val="1B2A3A"/></a:dk2><a:lt2><a:srgbClr val="E9EEF2"/></a:lt2><a:accent1><a:srgbClr val="2A6F97"/></a:accent1><a:accent2><a:srgbClr val="C8553D"/></a:accent2><a:accent3><a:srgbClr val="7A9E7E"/></a:accent3><a:accent4><a:srgbClr val="E8B04B"/></a:accent4><a:accent5><a:srgbClr val="6C5B7B"/></a:accent5><a:accent6><a:srgbClr val="3E8E8E"/></a:accent6><a:hlink><a:srgbClr val="1F5FA8"/></a:hlink><a:folHlink><a:srgbClr val="7B4FA0"/></a:folHlink></a:clrScheme><a:fontScheme name="Harbour"><a:majorFont><a:latin typeface="Carlito"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Arab" typeface="Amiri"/><a:font script="Hebr" typeface="Noto Sans Hebrew"/><a:font script="Jpan" typeface="Noto Sans JP"/><a:font script="Hans" typeface="Noto Sans SC"/></a:majorFont><a:minorFont><a:latin typeface="Carlito"/><a:ea typeface=""/><a:cs typeface=""/><a:font script="Arab" typeface="Noto Naskh Arabic"/><a:font script="Hebr" typeface="Noto Sans Hebrew"/><a:font script="Jpan" typeface="Noto Sans JP"/><a:font script="Hans" typeface="Noto Sans SC"/></a:minorFont></a:fontScheme><a:fmtScheme name="Harbour"><a:fillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:tint val="60000"/><a:satMod val="110000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:shade val="80000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="5400000" scaled="0"/></a:gradFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:tint val="90000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:shade val="70000"/></a:schemeClr></a:gs></a:gsLst><a:path path="circle"><a:fillToRect l="50000" t="50000" r="50000" b="50000"/></a:path></a:gradFill></a:fillStyleLst><a:lnStyleLst><a:ln w="6350"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="12700"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln><a:ln w="19050"><a:solidFill><a:schemeClr val="phClr"/></a:solidFill></a:ln></a:lnStyleLst><a:effectStyleLst><a:effectStyle><a:effectLst/></a:effectStyle><a:effectStyle><a:effectLst><a:outerShdw blurRad="38100" dist="19050" dir="5400000" rotWithShape="0"><a:srgbClr val="000000"><a:alpha val="40000"/></a:srgbClr></a:outerShdw></a:effectLst></a:effectStyle><a:effectStyle><a:effectLst><a:outerShdw blurRad="50800" dist="25400" dir="5400000" rotWithShape="0"><a:srgbClr val="000000"><a:alpha val="35000"/></a:srgbClr></a:outerShdw></a:effectLst><a:scene3d><a:camera prst="orthographicFront"><a:rot lat="0" lon="0" rev="0"/></a:camera><a:lightRig rig="threePt" dir="t"/></a:scene3d><a:sp3d><a:bevelT w="63500" h="25400"/></a:sp3d></a:effectStyle></a:effectStyleLst><a:bgFillStyleLst><a:solidFill><a:schemeClr val="phClr"/></a:solidFill><a:solidFill><a:schemeClr val="phClr"><a:tint val="95000"/></a:schemeClr></a:solidFill><a:gradFill rotWithShape="1"><a:gsLst><a:gs pos="0"><a:schemeClr val="phClr"><a:tint val="93000"/></a:schemeClr></a:gs><a:gs pos="100000"><a:schemeClr val="phClr"><a:shade val="94000"/></a:schemeClr></a:gs></a:gsLst><a:lin ang="16200000" scaled="1"/></a:gradFill></a:bgFillStyleLst></a:fmtScheme></a:themeElements><a:objectDefaults/><a:extraClrSchemeLst/></a:theme>"#;

/// The chart of the customer fixture as openpyxl writes one: the chart namespace as the default,
/// a line style on each series (`spPr` / `a:ln` / `a:prstDash`), and a stale value cache on the
/// first series (a save refills it from the cells).
const CUSTOMER_CHART: &str = r#"<chartSpace xmlns="http://schemas.openxmlformats.org/drawingml/2006/chart"><chart><title><tx><rich><a:bodyPr xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"/><a:p xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:pPr><a:defRPr/></a:pPr><a:r><a:t>Budget against forecast</a:t></a:r></a:p></rich></tx></title><plotArea><barChart><barDir val="col"/><grouping val="clustered"/><ser><idx val="0"/><order val="0"/><tx><strRef><f>Cost!B1</f></strRef></tx><spPr><a:ln xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:prstDash val="solid"/></a:ln></spPr><cat><numRef><f>Cost!$A$2:$A$4</f></numRef></cat><val><numRef><f>Cost!$B$2:$B$4</f><numCache><formatCode>General</formatCode><ptCount val="3"/><pt idx="0"><v>10</v></pt><pt idx="1"><v>20</v></pt><pt idx="2"><v>30</v></pt></numCache></numRef></val></ser><ser><idx val="1"/><order val="1"/><tx><strRef><f>Cost!C1</f></strRef></tx><spPr><a:ln xmlns:a="http://schemas.openxmlformats.org/drawingml/2006/main"><a:prstDash val="dash"/></a:ln></spPr><cat><numRef><f>Cost!$A$2:$A$4</f></numRef></cat><val><numRef><f>Cost!$C$2:$C$4</f></numRef></val></ser><gapWidth val="150"/><axId val="10"/><axId val="100"/></barChart><catAx><axId val="10"/><scaling><orientation val="minMax"/></scaling><axPos val="l"/><majorTickMark val="none"/><minorTickMark val="none"/><crossAx val="100"/><lblOffset val="100"/></catAx><valAx><axId val="100"/><scaling><orientation val="minMax"/></scaling><axPos val="l"/><majorGridlines/><majorTickMark val="none"/><minorTickMark val="none"/><crossAx val="10"/></valAx></plotArea><legend><legendPos val="r"/></legend><plotVisOnly val="1"/><dispBlanksAs val="gap"/></chart></chartSpace>"#;

/// A customer's workbook laid out as openpyxl writes one (default namespaces, inline strings, a
/// one-cell chart anchor, the indexed colour palette, an empty `workbookProtection`, created and
/// modified dates), with [`CUSTOMER_THEME`] and [`CUSTOMER_CHART`].
fn openpyxl_customer(path: &Path) {
    let rel = |id: &str, ty: &str, target: &str| {
        format!("<Relationship Type=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships/{ty}\" Target=\"{target}\" Id=\"{id}\"/>")
    };
    let rels = |body: String| format!("<Relationships xmlns=\"http://schemas.openxmlformats.org/package/2006/relationships\">{body}</Relationships>");
    let palette: String = (0..64).map(|i| format!("<rgbColor rgb=\"00{:06X}\"/>", (i * 0x040404) & 0xFFFFFF)).collect();
    let row = |r: u32, a: &str, b: u32, c: u32| {
        format!("<row r=\"{r}\"><c r=\"A{r}\" t=\"inlineStr\"><is><t>{a}</t></is></c><c r=\"B{r}\"><v>{b}</v></c><c r=\"C{r}\"><v>{c}</v></c></row>")
    };
    let parts: Vec<(&str, String)> = vec![
        ("[Content_Types].xml", "<Types xmlns=\"http://schemas.openxmlformats.org/package/2006/content-types\"><Default Extension=\"rels\" ContentType=\"application/vnd.openxmlformats-package.relationships+xml\"/><Default Extension=\"xml\" ContentType=\"application/xml\"/><Override PartName=\"/xl/styles.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.styles+xml\"/><Override PartName=\"/xl/theme/theme1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.theme+xml\"/><Override PartName=\"/docProps/core.xml\" ContentType=\"application/vnd.openxmlformats-package.core-properties+xml\"/><Override PartName=\"/docProps/app.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.extended-properties+xml\"/><Override PartName=\"/xl/worksheets/sheet1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.worksheet+xml\"/><Override PartName=\"/xl/drawings/drawing1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawing+xml\"/><Override PartName=\"/xl/charts/chart1.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.drawingml.chart+xml\"/><Override PartName=\"/xl/workbook.xml\" ContentType=\"application/vnd.openxmlformats-officedocument.spreadsheetml.sheet.main+xml\"/></Types>".into()),
        ("_rels/.rels", rels(format!("{}<Relationship Type=\"http://schemas.openxmlformats.org/package/2006/relationships/metadata/core-properties\" Target=\"docProps/core.xml\" Id=\"rId2\"/>{}", rel("rId1", "officeDocument", "xl/workbook.xml"), rel("rId3", "extended-properties", "docProps/app.xml")))),
        ("docProps/app.xml", "<Properties xmlns=\"http://schemas.openxmlformats.org/officeDocument/2006/extended-properties\"><Application>Microsoft Excel Compatible / Openpyxl 3.1.5</Application><AppVersion>3.1</AppVersion></Properties>".into()),
        ("docProps/core.xml", "<cp:coreProperties xmlns:cp=\"http://schemas.openxmlformats.org/package/2006/metadata/core-properties\"><dc:creator xmlns:dc=\"http://purl.org/dc/elements/1.1/\">openpyxl</dc:creator><dcterms:created xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"dcterms:W3CDTF\">2026-10-31T02:51:54Z</dcterms:created><dcterms:modified xmlns:dcterms=\"http://purl.org/dc/terms/\" xmlns:xsi=\"http://www.w3.org/2001/XMLSchema-instance\" xsi:type=\"dcterms:W3CDTF\">2026-10-31T02:51:54Z</dcterms:modified></cp:coreProperties>".into()),
        ("xl/workbook.xml", "<workbook xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><workbookPr/><workbookProtection/><bookViews><workbookView visibility=\"visible\" minimized=\"0\" showHorizontalScroll=\"1\" showVerticalScroll=\"1\" showSheetTabs=\"1\" tabRatio=\"600\" firstSheet=\"0\" activeTab=\"0\" autoFilterDateGrouping=\"1\"/></bookViews><sheets><sheet xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" name=\"Cost\" sheetId=\"1\" state=\"visible\" r:id=\"rId1\"/></sheets><definedNames/><calcPr calcId=\"124519\" fullCalcOnLoad=\"1\"/></workbook>".into()),
        ("xl/_rels/workbook.xml.rels", rels(format!("{}{}{}", rel("rId1", "worksheet", "/xl/worksheets/sheet1.xml"), rel("rId2", "styles", "styles.xml"), rel("rId3", "theme", "theme/theme1.xml")))),
        ("xl/styles.xml", format!("<styleSheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><fonts count=\"1\"><font><name val=\"Calibri\"/><family val=\"2\"/><color theme=\"1\"/><sz val=\"11\"/><scheme val=\"minor\"/></font></fonts><fills count=\"2\"><fill><patternFill/></fill><fill><patternFill patternType=\"gray125\"/></fill></fills><borders count=\"1\"><border><left/><right/><top/><bottom/><diagonal/></border></borders><cellStyleXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\"/></cellStyleXfs><cellXfs count=\"1\"><xf numFmtId=\"0\" fontId=\"0\" fillId=\"0\" borderId=\"0\" pivotButton=\"0\" quotePrefix=\"0\" xfId=\"0\"/></cellXfs><cellStyles count=\"1\"><cellStyle name=\"Normal\" xfId=\"0\" builtinId=\"0\" hidden=\"0\"/></cellStyles><tableStyles count=\"0\" defaultTableStyle=\"TableStyleMedium9\" defaultPivotStyle=\"PivotStyleLight16\"/><colors><indexedColors>{palette}</indexedColors></colors></styleSheet>")),
        ("xl/theme/theme1.xml", CUSTOMER_THEME.into()),
        ("xl/worksheets/sheet1.xml", format!("<worksheet xmlns=\"http://schemas.openxmlformats.org/spreadsheetml/2006/main\"><sheetPr><outlinePr summaryBelow=\"1\" summaryRight=\"1\"/><pageSetUpPr/></sheetPr><dimension ref=\"A1:C4\"/><sheetViews><sheetView workbookViewId=\"0\"><selection activeCell=\"A1\" sqref=\"A1\"/></sheetView></sheetViews><sheetFormatPr baseColWidth=\"8\" defaultRowHeight=\"15\"/><sheetData><row r=\"1\"><c r=\"A1\" t=\"inlineStr\"><is><t>Package</t></is></c><c r=\"B1\" t=\"inlineStr\"><is><t>Budget</t></is></c><c r=\"C1\" t=\"inlineStr\"><is><t>Forecast</t></is></c></row>{}{}{}</sheetData><pageMargins left=\"0.75\" right=\"0.75\" top=\"1\" bottom=\"1\" header=\"0.5\" footer=\"0.5\"/><drawing xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"rId1\"/></worksheet>", row(2, "Frame", 10, 12), row(3, "Facade", 20, 18), row(4, "Lifts", 30, 31))),
        ("xl/worksheets/_rels/sheet1.xml.rels", rels(rel("rId1", "drawing", "/xl/drawings/drawing1.xml"))),
        ("xl/drawings/drawing1.xml", "<wsDr xmlns=\"http://schemas.openxmlformats.org/drawingml/2006/spreadsheetDrawing\"><oneCellAnchor><from><col>5</col><colOff>0</colOff><row>1</row><rowOff>0</rowOff></from><ext cx=\"6480000\" cy=\"3240000\"/><graphicFrame><nvGraphicFramePr><cNvPr id=\"1\" name=\"Chart 1\"/><cNvGraphicFramePr/></nvGraphicFramePr><xfrm/><a:graphic xmlns:a=\"http://schemas.openxmlformats.org/drawingml/2006/main\"><a:graphicData uri=\"http://schemas.openxmlformats.org/drawingml/2006/chart\"><c:chart xmlns:c=\"http://schemas.openxmlformats.org/drawingml/2006/chart\" xmlns:r=\"http://schemas.openxmlformats.org/officeDocument/2006/relationships\" r:id=\"rId1\"/></a:graphicData></a:graphic></graphicFrame><clientData/></oneCellAnchor></wsDr>".into()),
        ("xl/drawings/_rels/drawing1.xml.rels", rels(rel("rId1", "chart", "/xl/charts/chart1.xml"))),
        ("xl/charts/chart1.xml", CUSTOMER_CHART.into()),
    ];
    let mut w = zip::ZipWriter::new(std::fs::File::create(path).unwrap());
    let opts = zip::write::SimpleFileOptions::default().compression_method(zip::CompressionMethod::Deflated);
    for (name, data) in parts {
        w.start_file(name, opts).unwrap();
        w.write_all(data.as_bytes()).unwrap();
    }
    w.finish().unwrap();
}

/// The chart part a saved workbook's first drawing names.
fn chart_part_of(path: &Path) -> String {
    let rels = zip_text(path, "xl/drawings/_rels/drawing1.xml.rels");
    let target = rels.split("Target=\"").nth(1).and_then(|t| t.split('"').next()).unwrap().to_string();
    zip_text(path, &format!("xl/charts/{}", target.rsplit('/').next().unwrap()))
}

#[test]
fn a_customer_file_edited_in_place_keeps_what_the_edit_did_not_touch() {
    let dir = folder("customer-openpyxl");
    let original = dir.join("customer.xlsx");
    openpyxl_customer(&original);
    let mut m = start(&dir, "Editor");
    // What the engine itself says a save would change: none of these parts any more.
    let rc = m.ok("roundtrip_check", json!({"path": "customer.xlsx"}));
    let problems: Vec<String> = rc["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
    for part in ["xl/theme/", "xl/charts/", "xl/drawings/", "workbookProtection", "docProps/core.xml", "colors", "indexedColors", "rgbColor"] {
        assert!(!problems.iter().any(|p| p.contains(part)), "roundtrip_check still reports {part}: {problems:#?}");
    }
    // Edit one cell and save.
    m.ok("open_workbook", json!({"path": "customer.xlsx"}));
    m.ok("set_cell", json!({"cell": "Cost!B2", "input": "99"}));
    m.ok("save_workbook", json!({"path": "edited.xlsx"}));
    let edited = dir.join("edited.xlsx");
    // The theme, verbatim: effect styles and the per-script font table.
    let theme = zip_text(&edited, "xl/theme/theme1.xml");
    assert_eq!(theme, CUSTOMER_THEME);
    assert!(theme.contains("<a:font script=\"Arab\" typeface=\"Noto Naskh Arabic\"/>") && theme.contains("<a:effectStyleLst>"));
    // The chart as the file had it (line styles, namespaces), its stale cache refilled from the
    // edited cell and nothing else changed.
    let chart = chart_part_of(&edited);
    assert_eq!(chart, CUSTOMER_CHART.replace("<pt idx=\"0\"><v>10</v></pt>", "<pt idx=\"0\"><v>99</v></pt>"));
    assert!(chart.contains("<a:prstDash val=\"dash\"/>"));
    // The anchor as it was: one cell and an extent, the chart named by its new relationship.
    let drawing = zip_text(&edited, "xl/drawings/drawing1.xml");
    assert!(drawing.contains("<oneCellAnchor") && drawing.contains("<ext cx=\"6480000\" cy=\"3240000\"/>"), "{drawing}");
    assert!(!drawing.contains("twoCellAnchor"), "{drawing}");
    // The palette, the empty protection element, the creation date kept and the save time set.
    let styles = zip_text(&edited, "xl/styles.xml");
    assert_eq!(styles.matches("<rgbColor ").count(), 64, "{styles}");
    assert!(styles.contains("<colors><indexedColors>"));
    assert!(zip_text(&edited, "xl/workbook.xml").contains("<workbookProtection/>"));
    let core = zip_text(&edited, "docProps/core.xml");
    assert!(core.contains(">2026-10-31T02:51:54Z</dcterms:created>"), "{core}");
    let modified = core.split("<dcterms:modified").nth(1).and_then(|t| t.split('>').nth(1)).and_then(|t| t.split('<').next()).unwrap_or("");
    assert!(modified.len() == 20 && modified.ends_with('Z') && modified != "2026-10-31T02:51:54Z", "modified is the save time: {core}");
    // The edit is there, the workbook reads back, and the engine's own diff of the two files finds
    // nothing lost in those parts.
    assert_eq!(cells_of(&edited, "B2:B4"), json!([[99.0], [20.0], [30.0]]));
    let rc = m.ok("roundtrip_check", json!({"path": "edited.xlsx"}));
    let problems: Vec<String> = rc["problems"].as_array().unwrap().iter().map(|p| p.as_str().unwrap().to_string()).collect();
    assert!(!problems.iter().any(|p| p.contains("theme") || p.contains("charts/") || p.contains("drawing")), "{problems:#?}");

    // Moving the chart's data (a row inserted above it) changes the chart and its anchor: both are
    // written from the model then, references and position following the edit.
    m.ok("insert_rows", json!({"rows": "1", "sheet": "Cost"}));
    m.ok("save_workbook", json!({"path": "moved.xlsx"}));
    let moved = dir.join("moved.xlsx");
    let chart = chart_part_of(&moved);
    assert!(chart.contains("$B$3:$B$5"), "{chart}");
    let drawing = zip_text(&moved, "xl/drawings/drawing1.xml");
    assert!(drawing.contains("twoCellAnchor") && drawing.contains("<xdr:row>2</xdr:row>"), "{drawing}");
    // The theme and palette still stand (the edit did not touch them).
    assert_eq!(zip_text(&moved, "xl/theme/theme1.xml"), CUSTOMER_THEME);
    m.close();
}
