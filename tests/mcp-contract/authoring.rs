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
