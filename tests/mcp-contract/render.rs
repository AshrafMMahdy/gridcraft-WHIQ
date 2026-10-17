//! gridcraft: Arabic shaping and bidi in PNG and PDF exports, page setup, RTL sheets.

use serde_json::json;

use crate::harness::{Mcp, folder};

fn start(dir: &std::path::Path) -> Mcp {
    let d = dir.to_str().unwrap();
    Mcp::start(&["--automation-read-root", d, "--automation-write-root", d, "--author", "Contract"])
}

/// Dark pixels in the left and right halves of a box of a PNG.
fn ink(png: &[u8], x0: u32, x1: u32, y0: u32, y1: u32) -> (usize, usize) {
    let img = image::load_from_memory(png).unwrap().to_rgba8();
    let mid = (x0 + x1) / 2;
    let (mut l, mut r) = (0, 0);
    for y in y0..y1.min(img.height()) {
        for x in x0..x1.min(img.width()) {
            if img.get_pixel(x, y).0[0] < 110 {
                if x < mid {
                    l += 1;
                } else {
                    r += 1;
                }
            }
        }
    }
    (l, r)
}

fn arabic_sheet(m: &mut Mcp) {
    m.ok(
        "write_range",
        json!({"range": "A1", "values": [
            ["البند", "الكمية", "السعر", "الإجمالي"],
            ["أعمال الخرسانة", 120, 350.5, "=B2*C2"],
            ["Steel works حديد", 45, 1200, "=B3*C3"],
            ["المجموع", null, null, "=SUM(D2:D3)"],
        ]}),
    );
    m.ok("execute_command", json!({"command": "home.columnWidth", "params": {"cols": "A:D", "chars": 20}}));
}

#[test]
fn arabic_png_render() {
    let dir = folder("arabic-png");
    let mut m = start(&dir);
    arabic_sheet(&mut m);
    let v = m.ok("render_range", json!({"range": "A1:D4", "path": "arabic.png", "scale": 1, "gridlines": false}));
    let png = std::fs::read(dir.join("arabic.png")).unwrap();
    assert!(png.starts_with(b"\x89PNG"), "{v}");
    let (w, h) = (v["width"].as_u64().unwrap() as u32, v["height"].as_u64().unwrap() as u32);
    assert!(w > 400 && h >= 80, "{v}");
    // Column A (the first quarter): Arabic text right-aligned; row 1 = 0..20 px.
    let quarter = w / 4;
    let (l, r) = ink(&png, 0, quarter, 0, 20);
    assert!(r > 30 && l == 0, "A1 Arabic right-aligned: {l}/{r}");
    // Numbers right-aligned in B2.
    let (l, r) = ink(&png, quarter, 2 * quarter, 20, 40);
    assert!(r > 10 && l == 0, "B2 number right-aligned: {l}/{r}");
    // Inline picture without a path: an MCP image block.
    let r = m.call("render_range", json!({"range": "A1:B2", "scale": 1}));
    let img = r["content"].as_array().unwrap().iter().find(|c| c["type"] == "image").expect("image content");
    assert_eq!(img["mimeType"], "image/png");
    assert!(img["data"].as_str().unwrap().len() > 100);
    // The whole sheet, fitted.
    let v = m.ok("render_sheet", json!({"path": "sheet.png", "max_pixels": 600, "headings": true}));
    assert!(v["width"].as_u64().unwrap() <= 601, "{v}");
    // A right-to-left sheet: column A on the right.
    m.ok("execute_command", json!({"command": "view.rightToLeft", "params": {"on": true}}));
    let v = m.ok("render_range", json!({"range": "A1:D1", "path": "rtl.png", "scale": 1, "gridlines": false}));
    let png = std::fs::read(dir.join("rtl.png")).unwrap();
    let w = v["width"].as_u64().unwrap() as u32;
    let (al, ar) = ink(&png, 3 * w / 4, w, 0, 20);
    assert!(ar > 30 && al < ar / 4, "A1 is the rightmost column and its Arabic hugs the sheet's start: {al}/{ar}");
}

#[test]
fn arabic_pdf_export_embeds_subset_fonts() {
    let dir = folder("arabic-pdf");
    let mut m = start(&dir);
    arabic_sheet(&mut m);
    let v = m.ok("export_pdf", json!({"path": "report.pdf"}));
    assert_eq!(v["pages"], 1, "{v}");
    let pdf = std::fs::read(dir.join("report.pdf")).unwrap();
    let text = String::from_utf8_lossy(&pdf);
    assert!(text.contains("/Subtype /Type0") && text.contains("/FontFile2") && text.contains("/ToUnicode"));
    assert!(text.contains("+NotoSansArabic-") && text.contains("+NotoSans-Regular"), "subset fonts embedded");
    assert!(!text.contains("/BaseFont /Helvetica"), "every font embedded by default");
    assert!(pdf.len() < 120_000, "subsets, not whole fonts: {} bytes", pdf.len());
    if let Ok(out) = std::process::Command::new("pdftotext").arg(dir.join("report.pdf")).arg("-").output() {
        let t = String::from_utf8_lossy(&out.stdout);
        let flat: String = t.chars().filter(|c| !matches!(*c, '\u{202A}'..='\u{202E}')).collect();
        for word in ["البند", "الإجمالي", "المجموع", "Steel works", "96060"] {
            assert!(flat.contains(word), "pdftotext finds {word}: {t}");
        }
    }
}

#[test]
fn pdf_page_setup() {
    let dir = folder("pagesetup");
    let mut m = start(&dir);
    let mut rows = vec![json!(["Item", "Qty", "Price", "Total", "Notes", "More", "Wide", "Columns", "To", "Fit"])];
    for i in 1..=150 {
        rows.push(json!([format!("Row {i}"), i, 2.5, format!("=B{}*C{}", i + 1, i + 1), "n", "m", "w", "c", "t", "f"]));
    }
    m.ok("write_range", json!({"range": "A1", "values": rows}));
    let v = m.ok(
        "export_pdf",
        json!({"path": "setup.pdf", "paper": "A4", "orientation": "landscape", "fit_width": true, "repeat_header_rows": "1:1", "footer": "&CPage &P of &N"}),
    );
    let pages = v["pages"].as_u64().unwrap();
    assert!(pages >= 2, "{v}");
    let pdf = String::from_utf8_lossy(&std::fs::read(dir.join("setup.pdf")).unwrap()).into_owned();
    assert!(pdf.contains("/MediaBox [0 0 841.89 595.28]"), "A4 landscape");
    if let Ok(out) = std::process::Command::new("pdftotext").arg(dir.join("setup.pdf")).arg("-").output() {
        let t = String::from_utf8_lossy(&out.stdout);
        assert_eq!(t.matches("Item").count() as u64, pages, "the header row repeats on every page: {t}");
        assert!(t.contains(&format!("Page 1 of {pages}")));
    }
    // A part of a long export.
    let v = m.ok("export_pdf", json!({"path": "part.pdf", "pages": "2-2"}));
    assert_eq!(v["pages"], 1, "{v}");
    let e = m.err("export_pdf", json!({"pages": "99"}));
    assert!(e["reason"].as_str().unwrap().contains(&format!("{pages} pages")), "{e}");
    // The page setup stays on the sheet.
    let p = m.ok("execute_command", json!({"command": "file.printPreview", "params": {}}));
    assert_eq!(p["pageList"][0]["orientation"], "landscape");
    assert_eq!(p["pageList"][0]["paper"], "A4");
}
