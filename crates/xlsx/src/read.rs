//! Reading the workbook part and everything hanging off it.

use std::sync::Arc;

use gridcraft_core::{DateSystem, RangeRef};
use gridcraft_model::{CalcMode, CellPicture, DefinedName, Sheet, Style, StyleId, StyleTable, Visibility, Workbook};

use crate::package::{Package, Rel};
use crate::styles::{StylesIn, read_styles};
use crate::xml::{self, El};
use crate::{IoError, ReadReport};

/// State shared while reading one package.
pub struct Ctx<'a> {
    pub pkg: Package<'a>,
    pub report: ReadReport,
    pub sst: Arc<Vec<Arc<str>>>,
    /// Cell format index → model style.
    pub xf_map: Vec<StyleId>,
    pub dxfs: Vec<Style>,
    pub palette: Vec<u32>,
    pub date_system: DateSystem,
    pub styles: StyleTable,
    /// Threaded-comment person id → display name.
    pub persons: std::collections::HashMap<String, String>,
    pub next_id: u32,
    /// Custom number formats from the styles part, by id.
    pub num_fmts: std::collections::HashMap<u32, String>,
    /// Value metadata blocks, indexed by the worksheet's one-based `vm` attribute.
    pub cell_pictures: Vec<Option<Arc<CellPicture>>>,
    /// x14 conditional-format ids of the current sheet's data bars, with the index of the rule in `cond_formats`.
    pub cf_ext_ids: Vec<(String, usize)>,
    /// WHIQ: slicers read from the sheets, their drawing frames (sheet, name, anchor) and the
    /// tables' ids in the file (id, table name), tied together after the sheets ([`crate::slicers`]).
    pub slicers: Vec<crate::slicers::Pending>,
    pub slicer_frames: Vec<(String, String, gridcraft_model::Anchor)>,
    pub table_file_ids: Vec<(u32, String)>,
    /// `sheetId` → model sheet name (PivotTable slicers name their PivotTables' sheets by id).
    pub sheet_ids: Vec<(u32, String)>,
    /// WHIQ: the charts of the sheet being read, to keep as the file wrote them
    /// ([`crate::passthrough::KeptReader::charts`]).
    pub chart_sources: Vec<crate::passthrough::ChartSource>,
    /// WHIQ: workbook-level XML kept verbatim while the model's view of it is unchanged (the theme
    /// part, the indexed colour palette, `workbookProtection`).
    pub kept_xml: Vec<gridcraft_model::PreservedXml>,
}

impl Ctx<'_> {
    pub fn warn(&mut self, m: impl Into<String>) {
        self.report.warn(m);
    }
    /// A fresh id for a chart, picture, shape or table.
    pub fn next_object_id(&mut self) -> u32 {
        let id = self.next_id;
        self.next_id = self.next_id.saturating_add(1);
        id
    }
    pub fn style(&self, xf: Option<u32>) -> StyleId {
        xf.and_then(|i| self.xf_map.get(i as usize).copied()).unwrap_or_default()
    }
    /// Reads and parses an optional part: problems other than size limits become warnings.
    pub fn optional_xml(&mut self, part: &str) -> Result<Option<El>, IoError> {
        match self.pkg.read_xml(part) {
            Ok(Some(e)) => Ok(Some(e)),
            Ok(None) => {
                self.warn(format!("missing part {part}"));
                Ok(None)
            }
            Err(IoError::TooLarge(m)) => Err(IoError::TooLarge(m)),
            Err(e) => {
                self.warn(format!("skipped {part}: {e}"));
                Ok(None)
            }
        }
    }
}

pub fn find_rel<'r>(rels: &'r [Rel], kind: &str) -> Option<&'r Rel> {
    rels.iter().find(|r| r.kind == kind)
}

pub fn read_xlsx(bytes: &[u8]) -> Result<(Workbook, ReadReport), IoError> {
    read_package(Package::open(bytes)?)
}

/// WHIQ: [`read_xlsx`] straight from the file: the file is never read whole, and each sheet
/// streams from the zip.
pub fn read_xlsx_file(file: std::fs::File) -> Result<(Workbook, ReadReport), IoError> {
    read_package(Package::open_file(file)?)
}

fn read_package(pkg: Package<'_>) -> Result<(Workbook, ReadReport), IoError> {
    let mut cx = Ctx {
        pkg,
        report: ReadReport::default(),
        sst: Arc::new(vec![]),
        xf_map: vec![],
        dxfs: vec![],
        palette: crate::tables::INDEXED.to_vec(),
        date_system: DateSystem::D1900,
        styles: StyleTable::default(),
        persons: Default::default(),
        next_id: 1,
        num_fmts: Default::default(),
        cell_pictures: vec![],
        cf_ext_ids: vec![],
        slicers: vec![],
        slicer_frames: vec![],
        table_file_ids: vec![],
        sheet_ids: vec![],
        chart_sources: vec![],
        kept_xml: vec![],
    };
    let root_rels = cx.pkg.rels("")?;
    let wb_part = find_rel(&root_rels, "officeDocument").map(|r| r.target.clone()).unwrap_or_else(|| "xl/workbook.xml".into());
    let wb_xml = match cx.pkg.read_xml(&wb_part)? {
        Some(x) => x,
        None => {
            if cx.pkg.has("xl/workbook.xml") && wb_part != "xl/workbook.xml" {
                cx.pkg.read_xml("xl/workbook.xml")?.ok_or_else(|| IoError::Format("no workbook part".into()))?
            } else if cx.pkg.has("content.xml") {
                return Err(IoError::Format("this is an OpenDocument file, not XLSX".into()));
            } else {
                return Err(IoError::Format("no workbook part".into()));
            }
        }
    };
    if wb_xml.name != "workbook" {
        return Err(IoError::Format(format!("unexpected root element <{}> in workbook part", wb_xml.name)));
    }
    let wb_rels = cx.pkg.rels(&wb_part)?;
    let mut kept = crate::passthrough::KeptReader::new(&mut cx);
    cx.cell_pictures = match crate::richdata::read(&mut cx, &wb_rels) {
        Ok(pictures) => pictures,
        Err(error) => {
            cx.warn(format!("cell-picture data could not be read ({error}); scalar fallbacks were retained"));
            vec![]
        }
    };
    let mut wb = Workbook::new();
    wb.sheets.clear();

    // Styles and theme. Cell checkboxes live in the feature property bags, which styles refer to.
    let mut checkboxes = Vec::new();
    if let Some(r) = find_rel(&wb_rels, "FeaturePropertyBag").cloned()
        && let Some(x) = cx.optional_xml(&r.target)?
    {
        checkboxes = crate::feature_bags::checkbox_complements(&x);
    }
    if let Some(r) = find_rel(&wb_rels, "styles").cloned() {
        if let Some(x) = cx.optional_xml(&r.target)? {
            // WHIQ: the palette as the file wrote it (indexed and recently used colours).
            if let Some(c) = x.child("colors") {
                cx.kept_xml.push(gridcraft_model::PreservedXml { slot: "colors".into(), xml: xml::el_xml(c) });
            }
            let StylesIn { xfs, dxfs, palette, named, num_fmts } = read_styles(&x, &checkboxes);
            cx.num_fmts = num_fmts;
            // Cell format 0 is the Normal style: its font is what every unformatted cell shows.
            if let Some(normal) = xfs.first() {
                cx.styles = StyleTable::with_default(Style { font: normal.font.clone(), ..Style::default() });
            }
            cx.xf_map = xfs.into_iter().enumerate().map(|(i, s)| if i == 0 { StyleId::DEFAULT } else { cx.styles.intern(s) }).collect();
            cx.dxfs = dxfs;
            cx.palette = palette;
            wb.cell_styles = named;
        }
    } else {
        cx.warn("no styles part");
    }
    if let Some(r) = find_rel(&wb_rels, "theme").cloned()
        && let Some(x) = cx.optional_xml(&r.target)?
    {
        wb.theme = crate::theme::read_theme(&x);
        // WHIQ: the theme part as written (effect styles, the per-script font table…), written back
        // verbatim while the model's theme is the one read from it.
        if let Ok(Some(raw)) = cx.pkg.read_limited(&r.target, 8 << 20)
            && let Ok(text) = String::from_utf8(raw)
        {
            cx.kept_xml.push(gridcraft_model::PreservedXml { slot: "theme".into(), xml: text });
        }
    }
    // Shared strings, streamed item by item. WHIQ: a workbook whose sheets stay on disk does not
    // hold them: a sheet read whole loads them when it needs them, a sheet read later takes the
    // few it uses from the part ([`crate::lazy`]).
    let sst_target = find_rel(&wb_rels, "sharedStrings").map(|r| r.target.clone());
    let sheet_els: Vec<El> = wb_xml.child("sheets").map(|s| s.kids("sheet").cloned().collect()).unwrap_or_default();
    let lazy = lazy_plan(&cx, &wb_rels, &sheet_els, sst_target.clone());
    if lazy.is_none() {
        load_sst(&mut cx, sst_target.as_deref())?;
    }
    let mut sst_loaded = lazy.is_none();

    if let Some(r) = find_rel(&wb_rels, "person").cloned()
        && let Ok(Some(x)) = cx.pkg.read_xml(&r.target)
    {
        for p in x.kids("person") {
            if let (Some(id), Some(n)) = (p.attr("id"), p.attr("displayName")) {
                cx.persons.insert(id.to_string(), n.to_string());
            }
        }
    }

    // Workbook properties.
    if let Some(p) = wb_xml.child("workbookPr") {
        if p.flag("date1904", false) {
            cx.date_system = DateSystem::D1904;
        }
        // WHIQ: the VBA code name, and the properties the model does not hold, kept as read.
        wb.code_name = p.attr("codeName").map(str::trim).filter(|c| !c.is_empty()).map(str::to_string);
        let extra = crate::kept::schema_attrs(p, crate::kept::WORKBOOK_PR_ATTRS);
        if !extra.is_empty() {
            cx.kept_xml.push(gridcraft_model::PreservedXml { slot: "workbookPr".into(), xml: extra });
        }
    }
    wb.date_system = cx.date_system;
    if let Some(c) = wb_xml.child("calcPr") {
        wb.calc.mode = match c.attr("calcMode") {
            Some("manual") => CalcMode::Manual,
            Some("autoNoTable") => CalcMode::AutomaticExceptTables,
            _ => CalcMode::Automatic,
        };
        wb.calc.iterative = c.flag("iterate", false);
        if let Some(n) = c.attr_u32("iterateCount") {
            wb.calc.set_max_iterations(n);
        }
        if let Some(d) = c.attr_f64("iterateDelta") {
            wb.calc.set_max_change(d);
        }
        wb.calc.precision_as_displayed = !c.flag("fullPrecision", true);
    }
    if let Some(p) = wb_xml.child("workbookProtection") {
        wb.protected_structure = p.flag("lockStructure", false);
        wb.structure_password = if wb.protected_structure { crate::sheet_read::read_password(p, "workbook") } else { None };
        // WHIQ: written back as it was (an empty element too) while the protection is unchanged.
        cx.kept_xml.push(gridcraft_model::PreservedXml { slot: "workbookProtection".into(), xml: xml::el_xml(p) });
    }
    let active_tab = wb_xml.path(&["bookViews", "workbookView"]).and_then(|v| v.attr_u32("activeTab")).unwrap_or(0) as usize;

    // Sheets. `file_to_model[i]` maps the i-th <sheet> to a model sheet index.
    let mut file_to_model: Vec<Option<usize>> = vec![];
    let mut pivot_caches = crate::pivot::CachesIn::new(&wb_xml, &wb_rels);
    // WHIQ: a workbook whose worksheets would fill a good share of the memory budget keeps the cells
    // of its plain worksheets in the file until they are looked at, and writes the others to disk
    // as they are read once the cells in memory reach half the budget.
    let mut resident = 0u64;
    for (i, se) in sheet_els.iter().enumerate() {
        // WHIQ parity pass: progress by sheet for job_status.
        gridcraft_core::progress::step_in("reading sheets", i as u64, sheet_els.len() as u64);
        let name = se.attr("name").unwrap_or("").to_string();
        let rel = se.attr("id").and_then(|id| wb_rels.iter().find(|r| r.id == id)).cloned();
        let Some(rel) = rel else {
            cx.warn(format!("sheet \"{name}\" has no part; skipped"));
            file_to_model.push(None);
            continue;
        };
        if rel.kind == "chartsheet" {
            // Kept as it is and written back in place (see `passthrough`); nothing to model.
            file_to_model.push(None);
            continue;
        }
        if rel.kind != "worksheet" {
            cx.warn(format!("sheet \"{name}\" is a {} and was skipped", rel.kind));
            file_to_model.push(None);
            continue;
        }
        let name = unique_name(&wb, &name, i);
        let lazy_part = lazy.as_ref().filter(|_| cx.pkg.part_size(&rel.target).is_some_and(|n| n >= LAZY_MIN_PART));
        // A sheet read whole needs the shared strings; one that may stay on disk is scanned for
        // everything but its cells first and needs them only if it turns out to have to be read.
        let read = match lazy_part {
            Some(plan) => crate::sheet_read::read_sheet_lazy(&mut cx, &rel.target, &name, plan, &mut |cx| {
                if !sst_loaded {
                    sst_loaded = true;
                    load_sst(cx, sst_target.as_deref())
                } else {
                    Ok(())
                }
            }),
            None => {
                if !sst_loaded {
                    sst_loaded = true;
                    load_sst(&mut cx, sst_target.as_deref())?;
                }
                crate::sheet_read::read_sheet(&mut cx, &rel.target, &name)
            }
        };
        let mut sheet = match read {
            Ok(s) => s,
            Err(IoError::TooLarge(m)) => return Err(IoError::TooLarge(m)),
            Err(e) => {
                cx.warn(format!("sheet \"{name}\" could not be read: {e}"));
                Sheet::new(name.clone())
            }
        };
        match crate::pivot::read_pivots(&mut cx, &mut pivot_caches, &rel.target, &name) {
            Ok(p) => sheet.pivots = p,
            Err(IoError::TooLarge(m)) => return Err(IoError::TooLarge(m)),
            Err(e) => cx.warn(format!("PivotTables on sheet \"{name}\" could not be read: {e}")),
        }
        // WHIQ: what the sheet part kept itself (`sheetPr` attributes) joins the pass-through.
        let own = sheet.preserved.take();
        sheet.preserved = kept.sheet(&mut cx, &rel.target, &name);
        if let Some(own) = own.filter(|o| !o.is_empty()) {
            let mut p = sheet.preserved.take().map(|a| (*a).clone()).unwrap_or_default();
            p.elements.extend(own.elements.iter().cloned());
            sheet.preserved = Some(Arc::new(p));
        }
        kept.charts(&mut cx, &mut sheet);
        sheet.visibility = match se.attr("state") {
            Some("hidden") => Visibility::Hidden,
            Some("veryHidden") => Visibility::VeryHidden,
            _ => Visibility::Visible,
        };
        if lazy.is_some() {
            resident = resident.saturating_add(sheet.cells.resident_bytes());
            if resident > gridcraft_core::memory::budget() / 2 {
                let bytes = sheet.cells.resident_bytes();
                match sheet.cells.spill() {
                    Ok(true) => resident = resident.saturating_sub(bytes),
                    Ok(false) => {}
                    Err(e) => cx.warn(format!("sheet \"{}\" stays in memory, it could not be written to disk: {e}", sheet.name)),
                }
            }
        }
        file_to_model.push(Some(wb.sheets.len()));
        if let Some(id) = se.attr_u32("sheetId") {
            cx.sheet_ids.push((id, sheet.name.clone()));
        }
        wb.sheets.push(Arc::new(sheet));
    }
    if lazy.is_some() {
        cx.sst = Arc::new(Vec::new());
    }
    if wb.sheets.is_empty() {
        cx.warn("the workbook has no worksheets; added an empty sheet");
        wb.sheets.push(Arc::new(Sheet::new("Sheet1")));
    }
    wb.active_sheet = file_to_model.get(active_tab).copied().flatten().unwrap_or(0);
    // WHIQ: table slicers; their caches' `#N/A` names are the writer's to make.
    let slicer_caches = crate::slicers::resolve(&mut cx, &wb_xml, &wb_rels, &mut wb);

    // Defined names. Chartex parts refer to their data through hidden `_xlchart.*` names.
    let mut chart_names: Vec<(String, String)> = vec![];
    if let Some(dn) = wb_xml.child("definedNames") {
        for d in dn.kids("definedName") {
            let Some(name) = d.attr("name") else { continue };
            let scope = match d.attr_u32("localSheetId") {
                Some(i) => match file_to_model.get(i as usize).copied().flatten() {
                    Some(m) => Some(m),
                    None => continue,
                },
                None => None,
            };
            let text = d.text.trim();
            let lname = name.to_ascii_lowercase();
            if lname == "_xlnm._filterdatabase" || slicer_caches.iter().any(|c| c.eq_ignore_ascii_case(name)) {
                continue;
            }
            if lname.starts_with("_xlchart.") {
                chart_names.push((lname, crate::fmla::from_file(text)));
                continue;
            }
            if lname == "_xlnm.print_area" || lname == "_xlnm.print_titles" {
                if let Some(si) = scope
                    && let Some(sheet) = wb.sheet_mut(si)
                {
                    if lname == "_xlnm.print_area" {
                        sheet.print.print_area = parse_print_area(text);
                    } else {
                        let (rows, cols) = parse_print_titles(text);
                        sheet.print.title_rows = rows;
                        sheet.print.title_cols = cols;
                    }
                }
                continue;
            }
            wb.names.push(DefinedName {
                name: name.to_string(),
                scope,
                formula: crate::fmla::from_file(text),
                comment: d.attr("comment").unwrap_or("").to_string(),
                hidden: d.flag("hidden", false),
            });
        }
    }

    if !chart_names.is_empty() {
        let resolve = |f: &mut String| {
            if let Some((_, r)) = chart_names.iter().find(|(n, _)| f.eq_ignore_ascii_case(n)) {
                *f = r.clone();
            }
        };
        for sheet in wb.sheets.iter_mut().filter(|s| !s.charts.is_empty()) {
            for ch in Arc::make_mut(sheet).charts.iter_mut() {
                for s in ch.series.iter_mut() {
                    resolve(&mut s.values);
                    s.categories.iter_mut().for_each(&resolve);
                    s.name.iter_mut().for_each(&resolve);
                }
            }
        }
    }

    // Document properties.
    if let Some(r) = find_rel(&root_rels, "core-properties").cloned()
        && let Ok(Some(x)) = cx.pkg.read_xml(&r.target)
    {
        let get = |n: &str| x.child(n).map(|e| e.text.clone()).unwrap_or_default();
        wb.props.title = get("title");
        wb.props.subject = get("subject");
        wb.props.author = get("creator");
        wb.props.last_modified_by = get("lastModifiedBy");
        wb.props.keywords = get("keywords");
        wb.props.description = get("description");
        wb.props.created = get("created").trim().to_string();
        wb.props.modified = get("modified").trim().to_string();
    }
    if let Some(r) = find_rel(&root_rels, "extended-properties").cloned()
        && let Ok(Some(x)) = cx.pkg.read_xml(&r.target)
    {
        wb.props.company = x.child("Company").map(|e| e.text.clone()).unwrap_or_default();
    }

    // A VBA project is carried through unchanged: GridCraft never runs macros, but preserving the
    // binary keeps macro-enabled workbooks from being silently stripped on save.
    if let Some(r) = find_rel(&wb_rels, "vbaProject").cloned() {
        match cx.pkg.read(&r.target) {
            Ok(Some(data)) => {
                wb.vba_project = Some(data);
                cx.warn("macros (VBA project) were kept but not executed");
            }
            Ok(None) => cx.warn("macros (VBA project) reference a missing part and were dropped"),
            Err(IoError::TooLarge(m)) => return Err(IoError::TooLarge(m)),
            Err(e) => cx.warn(format!("macros (VBA project) could not be read and were dropped: {e}")),
        }
    }
    wb.preserved = kept.workbook(&mut cx, &wb_part, &wb_xml, &file_to_model);
    // WHIQ: the workbook XML kept verbatim, and each kept chart's fingerprint (the charts as read,
    // names resolved), so a save can tell an untouched chart from an edited one.
    if !cx.kept_xml.is_empty() {
        let mut p = wb.preserved.take().map(|a| (*a).clone()).unwrap_or_default();
        p.elements.append(&mut cx.kept_xml);
        wb.preserved = Some(Arc::new(p));
    }
    crate::passthrough::fingerprint_kept_charts(&mut wb);
    // Calls of LAMBDA names are spelled like the names (the parser upper-cases them).
    wb.apply_name_call_case();
    wb.styles = cx.styles;
    Ok((wb, cx.report))
}

/// The shared strings part read into `cx.sst` (a missing or damaged one is a warning).
fn load_sst(cx: &mut Ctx<'_>, target: Option<&str>) -> Result<(), IoError> {
    let Some(target) = target else { return Ok(()) };
    match cx.pkg.read(target)? {
        Some(b) => {
            let mut sst = Vec::new();
            let res = xml::parse_streaming(&b, &["si"], &mut |si| {
                sst.push(Arc::from(xml::rich_text(&si)));
                Ok(())
            });
            if let Err(e) = res {
                cx.warn(format!("shared strings are damaged ({e}); kept {} strings", sst.len()));
            }
            cx.sst = Arc::new(sst);
        }
        None => cx.warn(format!("missing part {target}")),
    }
    Ok(())
}

/// A worksheet part at least this big (inflated) is a candidate for staying in the file.
const LAZY_MIN_PART: u64 = 256 * 1024;

/// The plan for leaving cells in the file, when the package is a file and its worksheets, inflated,
/// would take more than a quarter of the memory budget.
fn lazy_plan(cx: &Ctx<'_>, wb_rels: &[Rel], sheet_els: &[El], sst_part: Option<String>) -> Option<crate::lazy::LazyPlan> {
    cx.pkg.file()?;
    let total: u64 = sheet_els
        .iter()
        .filter_map(|se| se.attr("id").and_then(|id| wb_rels.iter().find(|r| r.id == id)))
        .filter(|r| r.kind == "worksheet")
        .filter_map(|r| cx.pkg.part_size(&r.target))
        .fold(0, u64::saturating_add);
    (total > gridcraft_core::memory::budget() / 4).then(|| crate::lazy::LazyPlan::new(cx, sst_part)).flatten()
}

pub(crate) fn unique_name(wb: &Workbook, name: &str, i: usize) -> String {
    let mut base: String = name.chars().filter(|c| !matches!(c, ':' | '\\' | '/' | '?' | '*' | '[' | ']')).take(31).collect();
    if base.trim().is_empty() {
        base = format!("Sheet{}", i + 1);
    }
    if wb.sheet_index(&base).is_none() {
        return base;
    }
    (2..).map(|n| format!("{} ({n})", base.chars().take(25).collect::<String>())).find(|n| wb.sheet_index(n).is_none()).unwrap_or(base)
}

/// Strips a sheet qualifier (`'My Sheet'!$A$1` → `$A$1`).
fn strip_sheet(s: &str) -> &str {
    match s.rfind('!') {
        Some(i) => s.get(i + 1..).unwrap_or(""),
        None => s,
    }
}

pub fn parse_print_area(text: &str) -> Option<RangeRef> {
    let first = text.split(',').next()?;
    let r = strip_sheet(first.trim()).replace('$', "");
    RangeRef::parse(&r)
}

pub fn parse_print_titles(text: &str) -> (Option<(u32, u32)>, Option<(u32, u32)>) {
    let mut rows = None;
    let mut cols = None;
    for part in text.split(',') {
        let p = strip_sheet(part.trim()).replace('$', "");
        let Some((a, b)) = p.split_once(':') else { continue };
        if let (Ok(r0), Ok(r1)) = (a.parse::<u32>(), b.parse::<u32>()) {
            if r0 >= 1 && r1 >= r0 && r1 <= gridcraft_core::MAX_ROWS {
                rows = Some((r0 - 1, r1 - 1));
            }
        } else if let (Some(c0), Some(c1)) = (gridcraft_core::letters_to_col(a), gridcraft_core::letters_to_col(b))
            && c1 >= c0
        {
            cols = Some((c0, c1));
        }
    }
    (rows, cols)
}
