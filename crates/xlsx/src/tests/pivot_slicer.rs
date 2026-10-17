//! WHIQ: PivotTable slicers, read from an Excel-shaped package and written back with every
//! reference (sheet ids, PivotTable names, PivotCache ids) regenerated.

use std::io::Read;

use crate::{read_xlsx, write_xlsx};

const FIXTURE: &[u8] = include_bytes!("../../../../tests/mcp-contract/fixtures/pivot-slicer.xlsx");

fn parts(bytes: &[u8]) -> Vec<(String, String)> {
    let mut z = zip::ZipArchive::new(std::io::Cursor::new(bytes)).unwrap();
    (0..z.len())
        .map(|i| {
            let mut f = z.by_index(i).unwrap();
            let mut s = String::new();
            let _ = f.read_to_string(&mut s);
            (f.name().to_string(), s)
        })
        .collect()
}

fn part<'a>(p: &'a [(String, String)], name: &str) -> &'a str {
    p.iter().find(|(n, _)| n == name).map(|(_, s)| s.as_str()).unwrap_or_else(|| panic!("no {name}"))
}

fn attr<'a>(xml: &'a str, after: &str, name: &str) -> &'a str {
    let at = xml.find(after).unwrap_or_else(|| panic!("no {after}"));
    let rest = &xml[at..];
    let k = rest.find(&format!("{name}=\"")).unwrap_or_else(|| panic!("no {name} after {after}")) + name.len() + 2;
    let end = rest[k..].find('"').unwrap();
    &rest[k..k + end]
}

#[test]
fn excel_pivot_slicers_are_read() {
    let (wb, report) = read_xlsx(FIXTURE).unwrap();
    assert!(report.warnings.iter().all(|w| !w.to_lowercase().contains("slicer")), "{:?}", report.warnings);
    let sh = wb.sheet(1).unwrap();
    assert_eq!(sh.name, "Pivot");
    assert_eq!(sh.pivots.len(), 1);
    assert_eq!(sh.pivots[0].style, "PivotStyleMedium9");
    assert_eq!(sh.slicers.len(), 2, "{:?}", sh.slicers);
    let product = sh.slicers.iter().find(|s| s.column == "Product").unwrap();
    assert_eq!(product.pivots, vec!["Pivot!PivotTable1".to_string()]);
    assert_eq!(product.selected, Some(vec!["Bolts".to_string()]));
    assert!(product.table.is_empty());
    let region = sh.slicers.iter().find(|s| s.column == "Region").unwrap();
    assert_eq!(region.selected, None, "all selected");
    assert_eq!(region.columns, 2);
    assert_eq!(region.anchor.cell.col, 8, "the frame's anchor");
    // The slicer owns the field's filter: no duplicate hidden-items filter on the PivotTable.
    assert!(sh.pivots[0].filters.iter().all(|f| f.selected.is_none()), "{:?}", sh.pivots[0].filters);
    assert!(wb.names.iter().all(|n| !n.name.starts_with("Slicer_")));
}

#[test]
fn pivot_slicers_are_written_with_consistent_references() {
    let (mut wb, _) = read_xlsx(FIXTURE).unwrap();
    // Rename the PivotTable's sheet and move it after a new sheet: tab ids and names must follow.
    let mut extra = gridcraft_model::Sheet::new("Notes");
    extra.slicers.clear();
    wb.sheets.insert(0, std::sync::Arc::new(extra));
    let pivot_sheet = wb.sheet_mut(2).unwrap();
    pivot_sheet.name = "Report".into();
    for s in pivot_sheet.slicers.iter_mut() {
        s.pivots = vec!["Report!PivotTable1".into()];
    }
    let bytes = write_xlsx(&wb).unwrap();
    let p = parts(&bytes);
    let workbook = part(&p, "xl/workbook.xml");
    // The Report sheet's id.
    let report_tag = workbook.find("<sheet name=\"Report\"").unwrap();
    let report_id = attr(&workbook[report_tag..], "<sheet", "sheetId").to_string();
    assert!(workbook.contains("{BBE1A952-AA13-448e-AADC-164F8A28A991}") && workbook.contains("<x14:slicerCaches>"), "{workbook}");
    assert!(workbook.contains("<definedName name=\"Slicer_Product\">#N/A</definedName>"));
    let cache_def = part(&p, "xl/pivotCache/pivotCacheDefinition1.xml");
    assert!(cache_def.contains("refreshOnLoad=\"1\""), "Excel recomputes the report on open");
    let cache_id = attr(cache_def, "x14:pivotCacheDefinition", "pivotCacheId").to_string();
    let caches: Vec<&str> = p.iter().filter(|(n, _)| n.starts_with("xl/slicerCaches/")).map(|(_, s)| s.as_str()).collect();
    assert_eq!(caches.len(), 2);
    for c in &caches {
        assert_eq!(attr(c, "<pivotTable ", "tabId"), report_id, "{c}");
        assert_eq!(attr(c, "<pivotTable ", "name"), "PivotTable1");
        assert_eq!(attr(c, "<tabular", "pivotCacheId"), cache_id);
    }
    let product = caches.iter().find(|c| c.contains("sourceName=\"Product\"")).unwrap();
    assert!(product.contains("<i x=\"0\" s=\"1\"/><i x=\"1\"/>"), "Bolts selected, Nuts not: {product}");
    // The PivotTable hides Nuts on the Product field (the slicer's selection), on no axis.
    let table = part(&p, "xl/pivotTables/pivotTable1.xml");
    assert!(table.contains("<pivotField showAll=\"0\"><items count=\"3\"><item x=\"0\"/><item x=\"1\" h=\"1\"/>"), "{table}");
    assert!(table.contains("PivotStyleMedium9"));
    // The slicer part is listed by the x14 sheet extension and framed in the drawing.
    let sheet = part(&p, "xl/worksheets/sheet3.xml");
    assert!(sheet.contains("{A8765BA9-456A-4dab-B4F3-ACF838C121DE}"), "{sheet}");
    let drawing = p.iter().find(|(n, _)| n.starts_with("xl/drawings/drawing")).map(|(_, s)| s.as_str()).unwrap();
    assert!(drawing.contains("Requires=\"a14\"") && drawing.contains("<sle:slicer") && drawing.contains("name=\"Product\""), "{drawing}");
    // Read back: the same slicers.
    let (back, report) = read_xlsx(&bytes).unwrap();
    assert!(report.warnings.iter().all(|w| !w.to_lowercase().contains("slicer")), "{:?}", report.warnings);
    let sh = back.sheet(2).unwrap();
    assert_eq!(sh.slicers.len(), 2);
    let pr = sh.slicers.iter().find(|s| s.column == "Product").unwrap();
    assert_eq!(pr.pivots, vec!["Report!PivotTable1".to_string()]);
    assert_eq!(pr.selected, Some(vec!["Bolts".to_string()]));
    assert_eq!(sh.pivots[0].style, "PivotStyleMedium9");
}

#[test]
fn slicers_without_their_pivot_are_left_out() {
    let (mut wb, _) = read_xlsx(FIXTURE).unwrap();
    wb.sheet_mut(1).unwrap().pivots.clear();
    let bytes = write_xlsx(&wb).unwrap();
    let p = parts(&bytes);
    assert!(p.iter().all(|(n, _)| !n.starts_with("xl/slicer")), "no dangling slicer parts");
    assert!(!part(&p, "xl/workbook.xml").contains("Slicer_"));
    let (back, _) = read_xlsx(&bytes).unwrap();
    assert!(back.sheet(1).unwrap().slicers.is_empty());
}
