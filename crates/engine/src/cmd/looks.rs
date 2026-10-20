//! WHIQ: cell looks for `file.build` — font, fill, borders, alignment, wrap and number format,
//! given per range (`styles: [{range, …}]`) or per cell (a cell object in `rows`), applied to
//! the workbook in one edit per sheet (one interned style per distinct look, however many cells
//! it covers), so a formatted workbook is one call instead of a command per range.

use std::collections::HashMap;

use gridcraft_core::RangeRef;
use gridcraft_model::{BorderLine, Color, Fill, HAlign, NumFmt, PatternType, Style, StyleId, Underline, VAlign};
use serde_json::{Map, Value as Json, json};

use super::*;

/// The look keys a range style or a cell object takes (each with its accepted spellings).
pub(crate) const LOOK_KEYS: &[(&str, &[&str])] = &[
    ("bold", &["bold"]),
    ("italic", &["italic"]),
    ("underline", &["underline"]),
    ("strike", &["strike", "strikethrough"]),
    ("font", &["font", "font_name", "fontname"]),
    ("font_size", &["font_size", "fontsize", "size"]),
    ("font_color", &["font_color", "fontcolor", "color", "ink"]),
    ("fill", &["fill", "fill_color", "fillcolor", "background"]),
    ("align", &["align", "h_align", "halign", "horizontal", "horizontal_align", "horizontalalign"]),
    ("v_align", &["v_align", "valign", "vertical", "vertical_align", "verticalalign"]),
    ("wrap", &["wrap", "wrap_text", "wraptext"]),
    ("indent", &["indent"]),
    ("rotation", &["rotation", "text_rotation"]),
    ("number_format", &["number_format", "numberformat", "num_fmt", "format"]),
    ("borders", &["borders", "border"]),
];

/// The canonical look key for `k` in any spelling (`fontColor`, `font-color` → `font_color`).
pub(crate) fn look_key(k: &str) -> Option<&'static str> {
    let k = k.trim().to_ascii_lowercase().replace('-', "_");
    let flat = k.replace('_', "");
    LOOK_KEYS.iter().find(|(_, names)| names.iter().any(|n| *n == k || n.replace('_', "") == flat)).map(|(name, _)| *name)
}

fn accepted() -> String {
    LOOK_KEYS.iter().map(|(k, _)| *k).collect::<Vec<_>>().join(", ")
}

/// One cell edge: `"thin"` or `{style?, color?}`.
#[derive(Clone, Copy, Debug, PartialEq)]
struct Edge(BorderLine);

/// A look: every field given is set, every other one is left as the cell has it.
#[derive(Clone, Debug, Default, PartialEq)]
pub(crate) struct Look {
    bold: Option<bool>,
    italic: Option<bool>,
    underline: Option<bool>,
    strike: Option<bool>,
    font: Option<String>,
    font_size: Option<f32>,
    font_color: Option<Color>,
    fill: Option<Fill>,
    h: Option<HAlign>,
    v: Option<VAlign>,
    wrap: Option<bool>,
    indent: Option<u8>,
    rotation: Option<i16>,
    num_fmt: Option<String>,
    /// Per-edge borders set on every cell of the range.
    edges: Vec<(&'static str, Edge)>,
    /// A border preset over the whole range (`all`, `outside`, `bottom`…), run as Home › Borders.
    pub(crate) preset: Option<Json>,
}

fn as_bool(k: &str, v: &Json) -> std::result::Result<bool, String> {
    match v {
        Json::Bool(b) => Ok(*b),
        Json::String(s) if s.eq_ignore_ascii_case("true") => Ok(true),
        Json::String(s) if s.eq_ignore_ascii_case("false") => Ok(false),
        Json::Number(n) => Ok(n.as_f64().is_some_and(|x| x != 0.0)),
        _ => Err(format!("`{k}` is true or false (got {v})")),
    }
}

fn as_num(k: &str, v: &Json) -> std::result::Result<f64, String> {
    v.as_f64()
        .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
        .filter(|x: &f64| x.is_finite())
        .ok_or_else(|| format!("`{k}` is a number (got {v})"))
}

fn color(k: &str, v: &Json) -> std::result::Result<Color, String> {
    super::format::color_param(Some(v)).ok_or_else(|| format!("`{k}` is a colour \"#RRGGBB\" (or \"none\"; got {v})"))
}

fn edge(k: &str, v: &Json) -> std::result::Result<Edge, String> {
    let (style, c) = match v {
        Json::String(s) => (s.as_str(), None),
        Json::Object(o) => (o.get("style").and_then(Json::as_str).unwrap_or("thin"), o.get("color")),
        Json::Bool(true) => ("thin", None),
        Json::Bool(false) | Json::Null => ("none", None),
        _ => {
            return Err(format!(
                "border `{k}` is a style (\"thin\", \"medium\", \"thick\", \"dashed\", \"dotted\", \"double\", \"hair\", \"none\") or {{style, color}}"
            ));
        }
    };
    let color = c.map(|c| color(k, c)).transpose()?.unwrap_or(Color::Auto);
    let style = if style.eq_ignore_ascii_case("none") { gridcraft_model::BorderStyle::None } else { super::format::border_style(style) };
    Ok(Edge(BorderLine { style, color }))
}

impl Look {
    /// Reads a look from an object's look keys; `skip` lists other keys the object may hold
    /// (a cell object's value keys, a style's `range`). Unknown keys are an error naming the
    /// accepted ones.
    pub(crate) fn parse(o: &Map<String, Json>, skip: &[&str]) -> std::result::Result<Option<Look>, String> {
        let mut l = Look::default();
        let mut any = false;
        for (k, v) in o {
            if skip.iter().any(|s| s.eq_ignore_ascii_case(k)) {
                continue;
            }
            let Some(key) = look_key(k) else {
                return Err(format!("unknown style key `{k}` (style keys: {accepted})", accepted = accepted()));
            };
            if v.is_null() {
                continue;
            }
            any = true;
            match key {
                "bold" => l.bold = Some(as_bool(k, v)?),
                "italic" => l.italic = Some(as_bool(k, v)?),
                "underline" => l.underline = Some(as_bool(k, v)?),
                "strike" => l.strike = Some(as_bool(k, v)?),
                "font" => l.font = Some(v.as_str().ok_or_else(|| format!("`{k}` is a font name, e.g. \"Calibri\""))?.to_string()),
                "font_size" => {
                    let n = as_num(k, v)?;
                    if !(1.0..=409.0).contains(&n) {
                        return Err(format!("`{k}` is a size in points from 1 to 409 (got {n})"));
                    }
                    l.font_size = Some(n as f32);
                }
                "font_color" => l.font_color = Some(color(k, v)?),
                "fill" => {
                    let (c, pattern) = match v {
                        Json::Object(f) => (f.get("color").or(f.get("fg")).unwrap_or(&Json::Null), f.get("pattern").and_then(Json::as_str)),
                        other => (other, None),
                    };
                    let c = color(k, c)?;
                    l.fill = Some(match (c, pattern) {
                        (Color::Auto, _) => Fill::default(),
                        (c, Some(p)) if !p.eq_ignore_ascii_case("solid") => {
                            let pattern = serde_json::from_value::<PatternType>(json!(capitalise(p)))
                                .map_err(|_| format!("fill pattern `{p}` is not known (solid, gray50, gray25, lightGrid…)"))?;
                            Fill { pattern, fg: c, bg: Color::Auto }
                        }
                        (c, _) => Fill::solid(c),
                    });
                }
                "align" => {
                    let s = v.as_str().unwrap_or("").to_ascii_lowercase().replace(['-', '_', ' '], "");
                    l.h = Some(match s.as_str() {
                        "left" => HAlign::Left,
                        "center" | "centre" => HAlign::Center,
                        "right" => HAlign::Right,
                        "justify" => HAlign::Justify,
                        "general" => HAlign::General,
                        "fill" => HAlign::Fill,
                        "centeracross" | "centeracrossselection" => HAlign::CenterAcross,
                        "distributed" => HAlign::Distributed,
                        _ => return Err(format!("`{k}` is left, center, right, justify, general, fill, center_across or distributed (got {v})")),
                    });
                }
                "v_align" => {
                    let s = v.as_str().unwrap_or("").to_ascii_lowercase();
                    l.v = Some(match s.as_str() {
                        "top" => VAlign::Top,
                        "middle" | "center" | "centre" => VAlign::Center,
                        "bottom" => VAlign::Bottom,
                        "justify" => VAlign::Justify,
                        "distributed" => VAlign::Distributed,
                        _ => return Err(format!("`{k}` is top, middle, bottom, justify or distributed (got {v})")),
                    });
                }
                "wrap" => l.wrap = Some(as_bool(k, v)?),
                "indent" => l.indent = Some(as_num(k, v)?.clamp(0.0, 250.0) as u8),
                "rotation" => {
                    let n = as_num(k, v)?;
                    if !((-90.0..=90.0).contains(&n) || n == 255.0) {
                        return Err(format!("`{k}` is degrees from -90 to 90, or 255 for stacked text (got {n})"));
                    }
                    l.rotation = Some(n as i16);
                }
                "number_format" => l.num_fmt = Some(v.as_str().ok_or_else(|| format!("`{k}` is a format code, e.g. \"#,##0.00\""))?.to_string()),
                "borders" => match v {
                    Json::String(_) => l.preset = Some(json!({"preset": v})),
                    Json::Object(b) if b.contains_key("preset") => l.preset = Some(v.clone()),
                    Json::Object(b) => {
                        for (side, e) in b {
                            let side = match side.to_ascii_lowercase().as_str() {
                                "top" => "top",
                                "bottom" => "bottom",
                                "left" => "left",
                                "right" => "right",
                                "diagonal_down" | "diagdown" | "diag_down" => "diag_down",
                                "diagonal_up" | "diagup" | "diag_up" => "diag_up",
                                other => return Err(format!("border side `{other}` is top, bottom, left, right, diagonal_down or diagonal_up")),
                            };
                            l.edges.push((side, edge(side, e)?));
                        }
                    }
                    _ => {
                        return Err(format!(
                            "`{k}` is a preset (\"all\", \"outside\", \"bottom\", \"thickOutside\", \"none\"…), {{preset, style?, color?}} or per side {{top?, bottom?, left?, right?: \"thin\" | {{style, color}}}}"
                        ));
                    }
                },
                _ => {}
            }
        }
        Ok(any.then_some(l))
    }

    /// The look with its per-cell parts only (a preset is run over the range separately).
    fn applies_per_cell(&self) -> bool {
        Look { preset: None, ..self.clone() } != Look::default()
    }

    fn apply(&self, st: &mut Style) {
        if let Some(b) = self.bold {
            st.font.bold = b;
        }
        if let Some(b) = self.italic {
            st.font.italic = b;
        }
        if let Some(b) = self.underline {
            st.font.underline = if b { Underline::Single } else { Underline::None };
        }
        if let Some(b) = self.strike {
            st.font.strike = b;
        }
        if let Some(f) = &self.font {
            st.font.name = f.clone();
        }
        if let Some(n) = self.font_size {
            st.font.size = n;
        }
        if let Some(c) = self.font_color {
            st.font.color = c;
        }
        if let Some(f) = self.fill {
            st.fill = f;
        }
        if let Some(h) = self.h {
            st.align.h = h;
        }
        if let Some(v) = self.v {
            st.align.v = v;
        }
        if let Some(w) = self.wrap {
            st.align.wrap = w;
        }
        if let Some(i) = self.indent {
            st.align.indent = i;
        }
        if let Some(r) = self.rotation {
            st.align.rotation = r;
        }
        if let Some(f) = &self.num_fmt {
            st.num_fmt = NumFmt::new(f);
        }
        for (side, Edge(e)) in &self.edges {
            let slot = match *side {
                "top" => &mut st.borders.top,
                "bottom" => &mut st.borders.bottom,
                "left" => &mut st.borders.left,
                "right" => &mut st.borders.right,
                "diag_down" => &mut st.borders.diag_down,
                _ => &mut st.borders.diag_up,
            };
            *slot = *e;
        }
    }

    /// A number format to set, used for cells written as dates.
    pub(crate) fn number_format(f: &str) -> Look {
        Look { num_fmt: Some(f.to_string()), ..Look::default() }
    }
}

fn capitalise(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

/// `"A1:C3"`, `"B2"`, `"A1:C3,E5"` → ranges (no whole rows or columns: looks go on cells).
pub(crate) fn ranges(text: &str) -> std::result::Result<Vec<RangeRef>, String> {
    let mut out = Vec::new();
    for part in text.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        let r = RangeRef::parse(part.trim_matches('$')).ok_or_else(|| format!("`{part}` is not a cell or range (e.g. \"B2\" or \"A1:D1\")"))?;
        if r.is_full_cols() || r.is_full_rows() {
            return Err(format!("`{part}` is a whole row or column: give the cells, e.g. \"A1:H1\""));
        }
        out.push(r);
    }
    if out.is_empty() {
        return Err("a style needs a `range`, e.g. \"A1:D1\"".into());
    }
    Ok(out)
}

/// Applies `looks` (cell looks over ranges, in order: a later look wins where they overlap) to
/// the active sheet in one edit, then the border presets as Home › Borders. At most `max_cells`
/// cells in all.
pub(crate) fn apply(s: &mut Session, looks: &[(Vec<RangeRef>, Look)], max_cells: u64, step: &dyn Fn(&str) -> String) -> Result<u64> {
    let total = looks.iter().flat_map(|(rs, _)| rs.iter()).fold(0u64, |n, r| n.saturating_add(r.count()));
    if total > max_cells {
        return Err(bad("file.build", step(&format!("the styles cover {total} cells, more than {max_cells}: style fewer, larger ranges"))));
    }
    let per_cell: Vec<&(Vec<RangeRef>, Look)> = looks.iter().filter(|(_, l)| l.applies_per_cell()).collect();
    if !per_cell.is_empty() {
        edit(s, |cx| {
            let si = cx.sheet_index();
            let mut cache: HashMap<(StyleId, usize), StyleId> = HashMap::new();
            for (k, (rs, look)) in per_cell.iter().enumerate() {
                gridcraft_core::cancel::checkpoint();
                for r in rs {
                    for at in r.iter() {
                        let old = cx.wb.sheet(si).map(|sh| sh.style_id(at)).unwrap_or_default();
                        let new = match cache.get(&(old, k)) {
                            Some(id) => *id,
                            None => {
                                let mut st = cx.wb.styles.get(old).clone();
                                look.apply(&mut st);
                                let id = cx.wb.styles.intern(st);
                                cache.insert((old, k), id);
                                id
                            }
                        };
                        if new != old {
                            cx.sheet_mut(si)?.set_style(at, new);
                        }
                    }
                }
            }
            Ok(())
        })?;
    }
    for (rs, look) in looks {
        let Some(preset) = &look.preset else { continue };
        for r in rs {
            let mut p = preset.clone();
            p["range"] = json!(r.a1());
            s.execute("home.borders", p).map_err(|e| bad("file.build", step(&format!("borders on {}: {e}", r.a1()))))?;
        }
    }
    Ok(total)
}
