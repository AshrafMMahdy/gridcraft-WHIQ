//! WHIQ: chart primitives as inline SVG markup, for the HTML preview (`file.previewHtml`).
//!
//! The same [`Prim`] list the PNG and PDF painters draw becomes one self-contained `<svg>`
//! element: no external fonts or images, text left as text (selectable, searchable). Text is
//! placed the way the page painter places it (the same baseline offsets per vertical anchor).

use std::fmt::Write as _;

use crate::{HAlign, Prim, Rgba, VAlign};

/// Escapes text for an XML text node or attribute value.
pub fn xml_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            // Control characters are not allowed in XML 1.0 text.
            c if (c as u32) < 0x20 && c != '\n' && c != '\t' => out.push(' '),
            c => out.push(c),
        }
    }
    out
}

/// A number short and finite for an attribute (`NaN`/infinite become 0).
fn n(v: f32) -> String {
    if !v.is_finite() {
        return "0".into();
    }
    let r = (v * 100.0).round() / 100.0;
    if r == r.trunc() { format!("{}", r as i64) } else { format!("{r}") }
}

fn rgb(c: Rgba) -> String {
    format!("#{:02x}{:02x}{:02x}", c[0], c[1], c[2])
}

/// ` fill="…"` (with an opacity when not opaque) or ` fill="none"`.
fn fill_attr(c: Option<Rgba>) -> String {
    match c {
        Some(c) if c[3] == 255 => format!(" fill=\"{}\"", rgb(c)),
        Some(c) if c[3] > 0 => format!(" fill=\"{}\" fill-opacity=\"{}\"", rgb(c), n(f32::from(c[3]) / 255.0)),
        _ => " fill=\"none\"".into(),
    }
}

fn stroke_attr(s: Option<(Rgba, f32)>) -> String {
    match s {
        Some((c, w)) if c[3] > 0 && w > 0.0 => {
            let mut a = format!(" stroke=\"{}\" stroke-width=\"{}\"", rgb(c), n(w));
            if c[3] < 255 {
                let _ = write!(a, " stroke-opacity=\"{}\"", n(f32::from(c[3]) / 255.0));
            }
            a
        }
        _ => String::new(),
    }
}

fn points(pts: &[[f32; 2]]) -> String {
    let mut s = String::with_capacity(pts.len() * 12);
    for (i, p) in pts.iter().enumerate() {
        if i > 0 {
            s.push(' ');
        }
        let _ = write!(s, "{},{}", n(p[0]), n(p[1]));
    }
    s
}

/// The path of a pie or doughnut sector (angles in radians, 0 = 12 o'clock, clockwise).
fn wedge_path(cx: f32, cy: f32, ro: f32, ri: f32, a0: f32, a1: f32) -> String {
    let at = |a: f32, r: f32| (cx + r * a.sin(), cy - r * a.cos());
    let span = (a1 - a0).abs();
    if span >= std::f32::consts::TAU - 1e-4 {
        // A whole ring: two half arcs (one arc cannot start and end at the same point).
        let (top, bottom) = ((cx, cy - ro), (cx, cy + ro));
        let mut d =
            format!("M{} {}A{r} {r} 0 1 1 {} {}A{r} {r} 0 1 1 {} {}Z", n(top.0), n(top.1), n(bottom.0), n(bottom.1), n(top.0), n(top.1), r = n(ro));
        if ri > 0.0 {
            let _ = write!(d, "M{} {}A{r} {r} 0 1 0 {} {}A{r} {r} 0 1 0 {} {}Z", n(cx), n(cy - ri), n(cx), n(cy + ri), n(cx), n(cy - ri), r = n(ri));
        }
        return d;
    }
    let large = if span > std::f32::consts::PI { 1 } else { 0 };
    let sweep = if a1 >= a0 { 1 } else { 0 };
    let (x0, y0) = at(a0, ro);
    let (x1, y1) = at(a1, ro);
    let mut d = format!("M{} {}A{r} {r} 0 {large} {sweep} {} {}", n(x0), n(y0), n(x1), n(y1), r = n(ro));
    if ri > 0.0 {
        let (x2, y2) = at(a1, ri);
        let (x3, y3) = at(a0, ri);
        let _ = write!(d, "L{} {}A{r} {r} 0 {large} {} {} {}Z", n(x2), n(y2), 1 - sweep, n(x3), n(y3), r = n(ri));
    } else {
        let _ = write!(d, "L{} {}Z", n(cx), n(cy));
    }
    d
}

/// The primitives of a `w`×`h` drawing as one `<svg>` element (`viewBox` in the drawing's own
/// units, drawn at its natural size; `class` names it for the page's styles).
pub fn to_svg(prims: &[Prim], w: f32, h: f32, class: &str) -> String {
    let (w, h) = (if w.is_finite() { w.max(1.0) } else { 1.0 }, if h.is_finite() { h.max(1.0) } else { 1.0 });
    let mut s = String::with_capacity(prims.len() * 80 + 200);
    let _ = write!(
        s,
        "<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"{}\" viewBox=\"0 0 {} {}\" width=\"{}\" height=\"{}\" font-family=\"Carlito, Calibri, 'Segoe UI', Arial, sans-serif\">",
        xml_escape(class),
        n(w),
        n(h),
        n(w),
        n(h)
    );
    for p in prims {
        match p {
            Prim::Rect { x, y, w, h, fill, stroke, radius } => {
                let _ = write!(s, "<rect x=\"{}\" y=\"{}\" width=\"{}\" height=\"{}\"", n(*x), n(*y), n(w.max(0.0)), n(h.max(0.0)));
                if *radius > 0.0 {
                    let _ = write!(s, " rx=\"{}\"", n(*radius));
                }
                let _ = write!(s, "{}{}/>", fill_attr(*fill), stroke_attr(*stroke));
            }
            Prim::Line { pts, color, width, dash } => {
                if pts.len() < 2 {
                    continue;
                }
                let _ = write!(s, "<polyline points=\"{}\" fill=\"none\"{}", points(pts), stroke_attr(Some((*color, width.max(0.1)))));
                if *dash {
                    s.push_str(" stroke-dasharray=\"3 2\"");
                }
                s.push_str(" stroke-linejoin=\"round\"/>");
            }
            Prim::Polygon { pts, fill, stroke } => {
                if pts.len() < 3 {
                    continue;
                }
                let _ = write!(s, "<polygon points=\"{}\"{}{}/>", points(pts), fill_attr(Some(*fill)), stroke_attr(*stroke));
            }
            Prim::Wedge { cx, cy, r_outer, r_inner, a0, a1, fill, stroke } => {
                if !(r_outer.is_finite() && *r_outer > 0.0) {
                    continue;
                }
                let d = wedge_path(*cx, *cy, *r_outer, r_inner.max(0.0), *a0, *a1);
                let _ = write!(s, "<path d=\"{d}\" fill-rule=\"evenodd\"{}{}/>", fill_attr(Some(*fill)), stroke_attr(*stroke));
            }
            Prim::Circle { cx, cy, r, fill, stroke } => {
                let _ =
                    write!(s, "<circle cx=\"{}\" cy=\"{}\" r=\"{}\"{}{}/>", n(*cx), n(*cy), n(r.max(0.0)), fill_attr(*fill), stroke_attr(*stroke));
            }
            Prim::Text { x, y, text, size, color, bold, align, valign, rotation } => {
                if text.is_empty() {
                    continue;
                }
                let anchor = match align {
                    HAlign::Left => "start",
                    HAlign::Center => "middle",
                    HAlign::Right => "end",
                };
                // The page painter's baseline offsets for each vertical anchor.
                let dy = match valign {
                    VAlign::Top => size * 0.78,
                    VAlign::Middle => size * 0.35,
                    VAlign::Bottom => -size * 0.21,
                };
                let _ = write!(s, "<text x=\"{}\" y=\"{}\" dy=\"{}\" font-size=\"{}\" text-anchor=\"{anchor}\"", n(*x), n(*y), n(dy), n(*size));
                if *bold {
                    s.push_str(" font-weight=\"bold\"");
                }
                let _ = write!(s, "{}", fill_attr(Some(*color)));
                if rotation.is_finite() && rotation.abs() > 1e-4 {
                    let _ = write!(s, " transform=\"rotate({} {} {})\"", n(rotation.to_degrees()), n(*x), n(*y));
                }
                let _ = write!(s, ">{}</text>", xml_escape(text));
            }
        }
    }
    s.push_str("</svg>");
    s
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_primitive_becomes_svg() {
        let prims = vec![
            Prim::Rect { x: 0.0, y: 0.0, w: 10.0, h: 5.0, fill: Some([255, 0, 0, 255]), stroke: Some(([0, 0, 0, 255], 1.0)), radius: 2.0 },
            Prim::Line { pts: vec![[0.0, 0.0], [5.0, 5.0]], color: [0, 0, 255, 128], width: 2.0, dash: true },
            Prim::Polygon { pts: vec![[0.0, 0.0], [5.0, 0.0], [5.0, 5.0]], fill: [0, 255, 0, 255], stroke: None },
            Prim::Wedge { cx: 50.0, cy: 50.0, r_outer: 40.0, r_inner: 20.0, a0: 0.0, a1: 1.0, fill: [1, 2, 3, 255], stroke: None },
            Prim::Wedge { cx: 50.0, cy: 50.0, r_outer: 40.0, r_inner: 0.0, a0: 0.0, a1: std::f32::consts::TAU, fill: [1, 2, 3, 255], stroke: None },
            Prim::Circle { cx: 1.0, cy: 1.0, r: 3.0, fill: None, stroke: Some(([9, 9, 9, 255], 1.0)) },
            Prim::Text {
                x: 5.0,
                y: 5.0,
                text: "Sales <&> \"Q1\"".into(),
                size: 10.0,
                color: [0, 0, 0, 255],
                bold: true,
                align: HAlign::Center,
                valign: VAlign::Middle,
                rotation: -std::f32::consts::FRAC_PI_2,
            },
        ];
        let s = to_svg(&prims, 100.0, 60.0, "chart-svg");
        assert!(s.starts_with("<svg xmlns=\"http://www.w3.org/2000/svg\" class=\"chart-svg\" viewBox=\"0 0 100 60\""), "{s}");
        for el in ["<rect ", "rx=\"2\"", "<polyline ", "stroke-dasharray", "stroke-opacity", "<polygon ", "<path d=\"M", "<circle ", "<text "] {
            assert!(s.contains(el), "{el} missing in {s}");
        }
        assert!(s.contains("Sales &lt;&amp;&gt; &quot;Q1&quot;"), "{s}");
        assert!(s.contains("rotate(-90 5 5)"), "{s}");
        assert!(s.ends_with("</svg>"));
        // Hostile numbers never reach the markup.
        let bad = to_svg(&[Prim::Circle { cx: f32::NAN, cy: f32::INFINITY, r: 1.0, fill: Some([0, 0, 0, 255]), stroke: None }], f32::NAN, 0.0, "x");
        assert!(!bad.contains("NaN") && !bad.contains("inf"), "{bad}");
    }
}
