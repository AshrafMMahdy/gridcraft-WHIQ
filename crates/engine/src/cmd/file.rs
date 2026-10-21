//! File menu: new, open, save, close, export, properties, options.

use serde_json::{Value as Json, json};

use super::*;
use crate::DocState;

pub fn specs() -> Vec<CommandSpec> {
    vec![
        cmd!(noundo "file.new", "New Workbook", ["File"], Some("Cmd+N"), "{sample?: \"budget\"|\"sales\"|\"grades\"}", always, new_workbook),
        cmd!(noundo "file.open", "Open…", ["File"], Some("Cmd+O"), "{path} | {name, base64}, plus {password} for a password-protected workbook (xlsx, xlsm, xls import, xlsb and ods data import, csv, tsv, txt, json)", always, open),
        cmd!(noundo "file.save", "Save", ["File"], Some("Cmd+S"), "{path?} (xlsx by default; .csv/.tsv/.json/.html/.pdf by extension)", has_doc, save),
        cmd!(noundo "file.saveAs", "Save As…", ["File"], Some("Cmd+Shift+S"), "{path} (.xlsx, .csv, .tsv, .json, .html, or .pdf: a PDF of the active sheet)", has_doc, save_as),
        cmd!(query "file.saveBytes", "Encode Workbook", [], None, "{format?: xlsx|csv|tsv|json|html} → {base64}", has_doc, save_bytes),
        cmd!(noundo "file.close", "Close", ["File"], Some("Cmd+W"), "{force?: bool}", has_doc, close),
        cmd!(noundo "file.exportCsv", "Export as CSV", ["File", "Export"], None, "{path, sheet?, range?} (range: only those cells, e.g. \"A1:L500000\"; streamed, no size limit)", has_doc, |s, p| export(s, p, "csv")),
        cmd!(noundo "file.exportHtml", "Save as Web Page", ["File", "Export"], None, "{path}", has_doc, |s, p| export(s, p, "html")),
        cmd!(
            "file.properties",
            "Properties",
            ["File"],
            None,
            "{title?, subject?, author?, company?, keywords?, description?, lastModifiedBy?}",
            has_doc,
            properties
        ),
        cmd!(
            query "file.options",
            "Options",
            ["File"],
            None,
            "{} → {calculation: {mode, iterative, maxIterations, maxChange}} for the active workbook (the interface opens File › Options; change settings with their own commands, e.g. formulas.calculationOptions, view.theme, app.language.set)",
            always,
            options
        ),
        cmd!(noundo "window.activate", "Switch Window", ["Window"], None, "{index}", has_doc, activate_window),
    ]
}

fn new_workbook(s: &mut Session, p: &Json) -> Result<Json> {
    let i = match str_param(p, "sample") {
        Some(name) => {
            let wb = crate::sample::build(name).ok_or_else(|| bad("file.new", format!("unknown sample `{name}`")))?;
            let title = crate::sample::title(name);
            s.add_document(DocState::new(wb, None, title))
        }
        None => s.new_workbook(),
    };
    Ok(json!({"index": i, "title": s.doc()?.display_title()}))
}

fn open(s: &mut Session, p: &Json) -> Result<Json> {
    // WHIQ: an .xlsx package is read straight from its file (never whole into memory; its
    // sheets stream from the zip). Everything else is read as bytes.
    #[cfg(not(target_arch = "wasm32"))]
    if str_param(p, "base64").is_none()
        && let Some(path) = str_param(p, "path")
        && !s.documents().iter().any(|d| d.path.as_deref() == Some(path))
        && let Some((wb, warnings)) = crate::io::open_xlsx_file(path)?
    {
        return adopt_opened(s, p, path.to_string(), wb, warnings, false, false, gridcraft_xlsx::Format::Xlsx);
    }
    let (name, bytes) = if let Some(b) = str_param(p, "base64") {
        (str_param(p, "name").unwrap_or("Book.xlsx").to_string(), crate::io::base64_decode(b).ok_or_else(|| bad("file.open", "invalid base64"))?)
    } else if let Some(path) = str_param(p, "path") {
        // Already open? Switch to it.
        if let Some(i) = s.documents().iter().position(|d| d.path.as_deref() == Some(path)) {
            s.set_active(i);
            return Ok(json!({"index": i, "alreadyOpen": true}));
        }
        (path.to_string(), crate::io::read_file(path)?)
    } else {
        s.ui_requests.push(crate::UiRequest::Dialog("open".into(), json!({})));
        return ok();
    };
    let sniffed = gridcraft_xlsx::sniff(&bytes);
    let encrypted = sniffed == gridcraft_xlsx::Format::Encrypted;
    let (wb, warnings) = crate::io::open_bytes_with_password(&name, &bytes, str_param(p, "password"))?;
    drop(bytes);
    let imported =
        matches!(crate::io::FileKind::from_path(&name), Some(crate::io::FileKind::Ods | crate::io::FileKind::Xlsb | crate::io::FileKind::Xls))
            || matches!(sniffed, gridcraft_xlsx::Format::Ods | gridcraft_xlsx::Format::Xlsb | gridcraft_xlsx::Format::Xls);
    adopt_opened(s, p, name, wb, warnings, imported, encrypted, sniffed)
}

/// The shared end of `file.open`: the read workbook becomes a document.
#[allow(clippy::too_many_arguments)]
fn adopt_opened(
    s: &mut Session,
    p: &Json,
    name: String,
    mut wb: gridcraft_model::Workbook,
    warnings: Vec<String>,
    imported: bool,
    encrypted: bool,
    sniffed: gridcraft_xlsx::Format,
) -> Result<Json> {
    wb.source_dir = str_param(p, "path").and_then(crate::io::source_dir);
    let title = if imported {
        format!("{} (imported).xlsx", std::path::Path::new(&name).file_stem().and_then(|n| n.to_str()).unwrap_or("Book"))
    } else {
        std::path::Path::new(&name).file_name().and_then(|n| n.to_str()).unwrap_or("Book").to_string()
    };
    // An import has no save target: ordinary Save must ask for a new destination. Neither does a
    // workbook decrypted with its password, so Save never replaces the encrypted original with
    // an unprotected copy.
    let path = str_param(p, "path").filter(|_| !imported && !encrypted).map(str::to_string);
    // Replace an untouched blank Book1 like Excel does.
    if s.documents().len() == 1
        && s.active().is_some_and(|d| d.path.is_none() && !d.is_dirty() && d.undo.is_empty() && d.wb.sheets.iter().all(|sh| sh.cells.is_empty()))
    {
        s.close_document(0);
    }
    let mut doc = DocState::new(wb, path, title);
    if crate::io::FileKind::from_path(&name).is_none() && sniffed == gridcraft_xlsx::Format::Xlsx {
        doc.content_kind = Some(crate::io::FileKind::Xlsx);
    }
    let i = s.add_document(doc);
    // The decrypted-workbook notice is for the info bar, not a message box.
    let shown: Vec<&str> = warnings.iter().map(String::as_str).filter(|w| *w != crate::io::DECRYPTED_NOTICE).collect();
    if !shown.is_empty() {
        let mut message = shown.iter().take(5).copied().collect::<Vec<_>>().join("\n\n");
        if shown.len() > 5 {
            message.push_str(&format!("\n\n{} additional import warnings.", shown.len() - 5));
        }
        s.ui_requests.push(crate::UiRequest::Message(message));
    }
    let d = s.doc()?;
    Ok(
        json!({"index": i, "title": d.display_title(), "sheets": d.wb.sheets.iter().map(|s| s.name.clone()).collect::<Vec<_>>(), "warnings": warnings}),
    )
}

fn save(s: &mut Session, p: &Json) -> Result<Json> {
    let path = match str_param(p, "path").map(str::to_string).or_else(|| s.active().and_then(|d| d.path.clone())) {
        Some(p) => p,
        None => {
            s.ui_requests.push(crate::UiRequest::Dialog("saveAs".into(), json!({})));
            return ok();
        }
    };
    // A PDF is a print of the sheet (Page Setup applies), not a copy of the workbook: the
    // document keeps its file and its unsaved changes.
    if std::path::Path::new(&path).extension().and_then(|e| e.to_str()).is_some_and(|e| e.eq_ignore_ascii_case("pdf")) {
        return super::print::export_pdf(s, &json!({"path": path}));
    }
    let d = s.doc_mut()?;
    // The document's own file keeps the format its content had when opened; any other path
    // goes by its extension. Relative paths to linked workbooks are rewritten for the new folder.
    let kind = crate::io::FileKind::from_path(&path).or(d.content_kind.filter(|_| d.path.as_deref() == Some(path.as_str())));
    let rebased = crate::io::source_dir(&path).and_then(|f| gridcraft_xlsx::rebase_links(&d.wb, &f));
    let wb = rebased.as_ref().unwrap_or(&d.wb);
    // WHIQ: an .xlsx streams into the file.
    let bytes = crate::io::save_to_path(wb, &path, kind)?;
    // Only a full-fidelity format clears the dirty flag and becomes the document's file.
    if matches!(kind, Some(crate::io::FileKind::Xlsx | crate::io::FileKind::Json)) {
        d.saved = d.wb.clone();
        d.path = Some(path.clone());
    }
    Ok(json!({"path": path, "bytes": bytes}))
}

fn save_as(s: &mut Session, p: &Json) -> Result<Json> {
    if str_param(p, "path").is_none() {
        s.ui_requests.push(crate::UiRequest::Dialog("saveAs".into(), json!({})));
        return ok();
    }
    save(s, p)
}

fn save_bytes(s: &mut Session, p: &Json) -> Result<Json> {
    let fmt = str_param(p, "format").unwrap_or("xlsx");
    let d = s.doc()?;
    let bytes = crate::io::save_bytes(&d.wb, &format!("x.{fmt}"))?;
    let name = format!("{}.{fmt}", std::path::Path::new(&d.display_title()).file_stem().and_then(|x| x.to_str()).unwrap_or("Book"));
    Ok(json!({"name": name, "base64": crate::io::base64_encode(&bytes), "bytes": bytes.len()}))
}

fn close(s: &mut Session, p: &Json) -> Result<Json> {
    let force = bool_param(p, "force").unwrap_or(false);
    let d = s.doc()?;
    if d.is_dirty() && !force {
        s.ui_requests.push(crate::UiRequest::Dialog("saveChanges".into(), json!({"title": d.display_title()})));
        return Ok(json!({"closed": false, "dirty": true}));
    }
    let i = s.active_index();
    s.close_document(i);
    Ok(json!({"closed": true}))
}

fn export(s: &mut Session, p: &Json, ext: &str) -> Result<Json> {
    let path = str_param(p, "path").ok_or_else(|| bad("file.export", "missing `path`"))?.to_string();
    let d = s.doc()?;
    let mut wb = (*d.wb).clone();
    if p.get("sheet").is_some() {
        wb.active_sheet = target_sheet(s, p)?;
    }
    let target = if path.ends_with(&format!(".{ext}")) { path.clone() } else { format!("{path}.{ext}") };
    // WHIQ: `range` exports only those cells (the bulk read: read_range with `path`), streamed.
    #[cfg(not(target_arch = "wasm32"))]
    if ext == "csv"
        && let Some(text) = str_param(p, "range")
    {
        let r = gridcraft_core::RangeRef::parse(text.rsplit('!').next().unwrap_or(text))
            .ok_or_else(|| bad("file.exportCsv", format!("`{text}` is not a range (e.g. A1:L500000)")))?;
        let sh = wb.sheet(wb.active_sheet).ok_or_else(|| bad("file.exportCsv", "no such sheet"))?;
        let (rows, bytes) = crate::io::export_csv_range(&wb, sh, Some(r), wb.csv_delimiter.unwrap_or(b','), &target)?;
        return Ok(json!({"path": target, "bytes": bytes, "range": r.a1(), "rows": rows, "columns": r.width()}));
    }
    let bytes = crate::io::save_to_path(&wb, &target, None)?;
    Ok(json!({"path": target, "bytes": bytes}))
}

fn properties(s: &mut Session, p: &Json) -> Result<Json> {
    edit(s, |cx| {
        let pr = &mut cx.wb.props;
        for (k, slot) in [
            ("title", &mut pr.title),
            ("subject", &mut pr.subject),
            ("author", &mut pr.author),
            ("company", &mut pr.company),
            ("keywords", &mut pr.keywords),
            ("description", &mut pr.description),
            ("lastModifiedBy", &mut pr.last_modified_by),
        ] {
            if let Some(v) = str_param(p, k) {
                *slot = v.to_string();
            }
        }
        Ok(serde_json::to_value(&cx.wb.props).unwrap_or_default())
    })
}

/// File › Options: reports the workbook-level settings and asks the interface to show its Options
/// page. Nothing changes here; every setting has its own command.
fn options(s: &mut Session, _: &Json) -> Result<Json> {
    s.ui_requests.push(crate::UiRequest::Dialog("options".into(), json!({})));
    let calculation = s.active().map(|d| {
        let c = &d.wb.calc;
        let mode = match c.mode {
            gridcraft_model::CalcMode::Automatic => "automatic",
            gridcraft_model::CalcMode::AutomaticExceptTables => "automaticExceptTables",
            gridcraft_model::CalcMode::Manual => "manual",
        };
        json!({"mode": mode, "iterative": c.iterative, "maxIterations": c.max_iterations, "maxChange": c.max_change})
    });
    Ok(json!({"calculation": calculation}))
}

fn activate_window(s: &mut Session, p: &Json) -> Result<Json> {
    let i = u32_param(p, "index").ok_or_else(|| bad("window.activate", "missing `index`"))? as usize;
    if i >= s.documents().len() {
        return Err(bad("window.activate", "no such window"));
    }
    s.set_active(i);
    ok()
}
