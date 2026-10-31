//! WHIQ: a raster canvas for headless PNG renders (cells, charts, pictures).
//!
//! The drawing calls mirror `gridcraft-pdf`'s `Page` (top-left origin, y down, units are pixels),
//! so the engine draws a sheet onto either with the same code. Text is laid out by
//! `gridcraft-text` (bidi, HarfRust shaping, the bundled fonts with the Arabic fallback) and
//! filled from the glyph outlines, anti-aliased. Clips are rectangles, as the sheet painter uses.
#![deny(clippy::unwrap_used, clippy::expect_used, clippy::panic, clippy::unimplemented, clippy::todo, clippy::unreachable)]
#![forbid(unsafe_code)]

use tiny_skia::{
    Color, FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, PathBuilder, Pixmap, PixmapPaint, Rect, Stroke, StrokeDash, Transform,
};

/// RGB colour, 0–255 per channel.
pub type Rgb = [u8; 3];

/// Largest canvas side and area accepted (hostile ranges and scales are refused, not allocated).
pub const MAX_SIDE: u32 = 16_384;
pub const MAX_PIXELS: u64 = 100_000_000;

/// A pixel canvas with a white (or given) background.
pub struct Canvas {
    pixmap: Pixmap,
    clip: Option<(f32, f32, f32, f32)>,
    stack: Vec<Option<(f32, f32, f32, f32)>>,
    /// WHIQ: one canvas-sized clip mask, reused: only the pixels of the clip it holds
    /// (`mask_rect`, x0, y0, x1, y1) are set, so moving it to another clip costs those two
    /// rectangles, not the canvas.
    mask: Option<Mask>,
    mask_rect: Option<(u32, u32, u32, u32)>,
}

fn paint(c: Rgb) -> Paint<'static> {
    let mut p = Paint::default();
    p.set_color(Color::from_rgba8(c[0], c[1], c[2], 255));
    p.anti_alias = true;
    p
}

fn finite(v: &[f32]) -> bool {
    v.iter().all(|x| x.is_finite())
}

/// Collects a glyph outline as a path, placed with a transform.
struct GlyphPen {
    pb: PathBuilder,
    m: [f32; 6],
}

impl GlyphPen {
    fn p(&self, x: f32, y: f32) -> (f32, f32) {
        let [a, b, c, d, e, f] = self.m;
        (a * x + c * y + e, b * x + d * y + f)
    }
}

impl gridcraft_text::Pen for GlyphPen {
    fn move_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.move_to(x, y);
    }
    fn line_to(&mut self, x: f32, y: f32) {
        let (x, y) = self.p(x, y);
        self.pb.line_to(x, y);
    }
    fn quad_to(&mut self, cx: f32, cy: f32, x: f32, y: f32) {
        let (cx, cy) = self.p(cx, cy);
        let (x, y) = self.p(x, y);
        self.pb.quad_to(cx, cy, x, y);
    }
    fn curve_to(&mut self, c1x: f32, c1y: f32, c2x: f32, c2y: f32, x: f32, y: f32) {
        let (c1x, c1y) = self.p(c1x, c1y);
        let (c2x, c2y) = self.p(c2x, c2y);
        let (x, y) = self.p(x, y);
        self.pb.cubic_to(c1x, c1y, c2x, c2y, x, y);
    }
    fn close(&mut self) {
        self.pb.close();
    }
}

impl Canvas {
    /// A `width`×`height` canvas filled with `background`; `None` when the size is 0 or over the limits.
    pub fn new(width: u32, height: u32, background: Rgb) -> Option<Canvas> {
        if width == 0 || height == 0 || width > MAX_SIDE || height > MAX_SIDE || u64::from(width) * u64::from(height) > MAX_PIXELS {
            return None;
        }
        let mut pixmap = Pixmap::new(width, height)?;
        pixmap.fill(Color::from_rgba8(background[0], background[1], background[2], 255));
        Some(Canvas { pixmap, clip: None, stack: Vec::new(), mask: None, mask_rect: None })
    }

    pub fn width(&self) -> u32 {
        self.pixmap.width()
    }
    pub fn height(&self) -> u32 {
        self.pixmap.height()
    }

    /// The colour at a pixel (for tests).
    pub fn pixel(&self, x: u32, y: u32) -> Option<Rgb> {
        let p = self.pixmap.pixel(x, y)?.demultiply();
        Some([p.red(), p.green(), p.blue()])
    }

    /// The clip in whole pixels (x0, y0, x1, y1), as a non-antialiased fill of it covers them
    /// (a pixel whose centre is inside).
    fn clip_pixels(&self) -> Option<(u32, u32, u32, u32)> {
        let (x, y, w, h) = self.clip?;
        let (pw, ph) = (self.pixmap.width() as f32, self.pixmap.height() as f32);
        let px = |v: f32, max: f32| (v - 0.5).ceil().clamp(0.0, max) as u32;
        Some((px(x, pw), px(y, ph), px(x + w.max(0.0), pw), px(y + h.max(0.0), ph)))
    }

    /// Makes the mask hold the current clip (allocated once, then only the rectangles change).
    fn ensure_mask(&mut self) {
        let Some(want) = self.clip_pixels() else { return };
        if self.mask.is_none() {
            self.mask = Mask::new(self.pixmap.width(), self.pixmap.height());
            self.mask_rect = None;
        }
        if self.mask_rect == Some(want) {
            return;
        }
        let w = self.pixmap.width() as usize;
        let Some(m) = self.mask.as_mut() else { return };
        let data = m.data_mut();
        let mut set = |(x0, y0, x1, y1): (u32, u32, u32, u32), v: u8| {
            for row in y0..y1 {
                let start = row as usize * w;
                if let Some(line) = data.get_mut(start + x0 as usize..start + x1 as usize) {
                    line.fill(v);
                }
            }
        };
        if let Some(old) = self.mask_rect {
            set(old, 0);
        }
        set(want, 255);
        self.mask_rect = Some(want);
    }

    /// Whether a drawing whose ink lies inside `bounds` (x0, y0, x1, y1) is wholly inside the
    /// clip: then it needs no mask.
    fn inside_clip(&self, (x0, y0, x1, y1): (f32, f32, f32, f32)) -> bool {
        match self.clip_pixels() {
            None => true,
            Some((cx0, cy0, cx1, cy1)) => x0 >= cx0 as f32 && y0 >= cy0 as f32 && x1 <= cx1 as f32 && y1 <= cy1 as f32,
        }
    }

    /// The mask a drawing with ink inside `bounds` needs (`None`: no clip, or wholly inside it).
    fn mask_for(&mut self, bounds: (f32, f32, f32, f32)) -> bool {
        if self.inside_clip(bounds) {
            return false;
        }
        self.ensure_mask();
        self.mask.is_some()
    }

    /// Everything outside the clip is hidden: with an empty clip nothing at all.
    fn hidden(&self) -> bool {
        self.clip.is_some_and(|(_, _, w, h)| w <= 0.0 || h <= 0.0)
    }

    pub fn save(&mut self) {
        self.stack.push(self.clip);
    }

    pub fn restore(&mut self) {
        if let Some(c) = self.stack.pop() {
            self.clip = c;
        }
    }

    /// Intersects the clip with a rectangle (until the next [`Canvas::restore`]).
    pub fn clip_rect(&mut self, x: f32, y: f32, w: f32, h: f32) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let (x0, y0, x1, y1) = (x, y, x + w.max(0.0), y + h.max(0.0));
        let next = match self.clip {
            None => (x0, y0, x1 - x0, y1 - y0),
            Some((cx, cy, cw, ch)) => {
                let (nx0, ny0) = (x0.max(cx), y0.max(cy));
                let (nx1, ny1) = (x1.min(cx + cw), y1.min(cy + ch));
                (nx0, ny0, (nx1 - nx0).max(0.0), (ny1 - ny0).max(0.0))
            }
        };
        self.clip = Some(next);
    }

    pub fn fill_rect(&mut self, x: f32, y: f32, w: f32, h: f32, color: Rgb) {
        if self.hidden() || !finite(&[x, y, w, h]) || w <= 0.0 || h <= 0.0 {
            return;
        }
        let Some(r) = Rect::from_xywh(x, y, w, h) else { return };
        let p = paint(color);
        let masked = self.mask_for((x, y, x + w, y + h));
        self.pixmap.fill_rect(r, &p, Transform::identity(), if masked { self.mask.as_ref() } else { None });
    }

    fn stroke(&mut self, pb: PathBuilder, width: f32, color: Rgb, dash: &[f32], round: bool) {
        if self.hidden() {
            return;
        }
        let Some(path) = pb.finish() else { return };
        let mut s = Stroke { width: if width.is_finite() { width.max(0.0) } else { 0.0 }, ..Stroke::default() };
        if round {
            s.line_join = LineJoin::Round;
            s.line_cap = LineCap::Butt;
        }
        let d: Vec<f32> = dash.iter().take(8).copied().filter(|v| v.is_finite() && *v > 0.0).collect();
        if d.len() >= 2 && d.len().is_multiple_of(2) {
            s.dash = StrokeDash::new(d, 0.0);
        }
        let p = paint(color);
        let b = path.bounds();
        let pad = s.width / 2.0 + 2.0;
        let masked = self.mask_for((b.left() - pad, b.top() - pad, b.right() + pad, b.bottom() + pad));
        self.pixmap.stroke_path(&path, &p, &s, Transform::identity(), if masked { self.mask.as_ref() } else { None });
    }

    pub fn rect_stroke(&mut self, x: f32, y: f32, w: f32, h: f32, width: f32, color: Rgb) {
        if !finite(&[x, y, w, h]) {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(x, y);
        pb.line_to(x + w, y);
        pb.line_to(x + w, y + h);
        pb.line_to(x, y + h);
        pb.close();
        self.stroke(pb, width, color, &[], false);
    }

    pub fn stroke_line(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgb) {
        self.stroke_line_dashed(x0, y0, x1, y1, width, color, &[]);
    }

    pub fn stroke_line_dashed(&mut self, x0: f32, y0: f32, x1: f32, y1: f32, width: f32, color: Rgb, dash: &[f32]) {
        if !finite(&[x0, y0, x1, y1]) {
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(x0, y0);
        pb.line_to(x1, y1);
        self.stroke(pb, width, color, dash, false);
    }

    pub fn polyline(&mut self, pts: &[[f32; 2]], width: f32, color: Rgb, dash: &[f32]) {
        if pts.len() < 2 || !pts.iter().all(|p| finite(p)) {
            return;
        }
        let mut pb = PathBuilder::new();
        for (i, p) in pts.iter().enumerate().take(200_000) {
            if i == 0 {
                pb.move_to(p[0], p[1]);
            } else {
                pb.line_to(p[0], p[1]);
            }
        }
        self.stroke(pb, width, color, dash, true);
    }

    pub fn polygon(&mut self, pts: &[[f32; 2]], fill: Option<Rgb>, stroke: Option<(Rgb, f32)>) {
        if pts.len() < 2 || !pts.iter().all(|p| finite(p)) || self.hidden() {
            return;
        }
        let build = || {
            let mut pb = PathBuilder::new();
            for (i, p) in pts.iter().enumerate().take(200_000) {
                if i == 0 {
                    pb.move_to(p[0], p[1]);
                } else {
                    pb.line_to(p[0], p[1]);
                }
            }
            pb.close();
            pb
        };
        if let Some(f) = fill
            && let Some(path) = build().finish()
        {
            let p = paint(f);
            let b = path.bounds();
            let masked = self.mask_for((b.left() - 1.0, b.top() - 1.0, b.right() + 1.0, b.bottom() + 1.0));
            self.pixmap.fill_path(&path, &p, FillRule::Winding, Transform::identity(), if masked { self.mask.as_ref() } else { None });
        }
        if let Some((c, w)) = stroke {
            self.stroke(build(), w, c, &[], true);
        }
    }

    pub fn circle(&mut self, cx: f32, cy: f32, r: f32, fill: Option<Rgb>, stroke: Option<(Rgb, f32)>) {
        if !finite(&[cx, cy, r]) || r <= 0.0 || self.hidden() {
            return;
        }
        if let Some(f) = fill
            && let Some(path) = PathBuilder::from_circle(cx, cy, r)
        {
            let p = paint(f);
            let b = path.bounds();
            let masked = self.mask_for((b.left() - 1.0, b.top() - 1.0, b.right() + 1.0, b.bottom() + 1.0));
            self.pixmap.fill_path(&path, &p, FillRule::Winding, Transform::identity(), if masked { self.mask.as_ref() } else { None });
        }
        if let Some((c, w)) = stroke {
            let mut pb = PathBuilder::new();
            pb.push_circle(cx, cy, r);
            self.stroke(pb, w, c, &[], false);
        }
    }

    /// A shaped line with its baseline origin at `(x, y)`, rotated by `angle` radians clockwise.
    pub fn glyphs(&mut self, x: f32, y: f32, size: f32, color: Rgb, line: &gridcraft_text::Line, angle: f32, italic: bool) {
        if line.glyphs.is_empty() || !finite(&[x, y, size]) || size <= 0.0 || size > 4000.0 || self.hidden() {
            return;
        }
        let (sn, cs) = if angle.is_finite() { angle.sin_cos() } else { (0.0, 1.0) };
        let _ = italic;
        let mut pb = PathBuilder::new();
        for g in line.glyphs.iter().take(20_000) {
            if !finite(&[g.x, g.dy]) {
                continue;
            }
            // Italic faces are drawn as they are; a face with no italic (Arabic) is slanted.
            let k = if g.slant { -0.2 } else { 0.0 };
            // Glyph space (x right, y down) → canvas: skew, then rotate about the line origin.
            let ox = x + g.x * cs - g.dy * sn;
            let oy = y + g.x * sn + g.dy * cs;
            let m = [cs, sn, k * cs - sn, k * sn + cs, ox, oy];
            let mut pen = GlyphPen { pb, m };
            gridcraft_text::outline(g.face, g.gid, size, &mut pen);
            pb = pen.pb;
        }
        let Some(path) = pb.finish() else { return };
        let p = paint(color);
        let b = path.bounds();
        let masked = self.mask_for((b.left() - 1.0, b.top() - 1.0, b.right() + 1.0, b.bottom() + 1.0));
        self.pixmap.fill_path(&path, &p, FillRule::Winding, Transform::identity(), if masked { self.mask.as_ref() } else { None });
    }

    /// Text at `(x, y)` (baseline) in the embedded faces, in the default family.
    pub fn text(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        bold: bool,
        italic: bool,
        color: Rgb,
        text: &str,
        angle: f32,
        base: Option<gridcraft_text::Dir>,
    ) {
        self.text_styled(x, y, size, &gridcraft_text::TextStyle { family: "", bold, italic }, color, text, angle, base);
    }

    /// Text at `(x, y)` (baseline) in `style`'s family (a font name, see
    /// `gridcraft_text::resolve_family`).
    #[allow(clippy::too_many_arguments)]
    pub fn text_styled(
        &mut self,
        x: f32,
        y: f32,
        size: f32,
        style: &gridcraft_text::TextStyle<'_>,
        color: Rgb,
        text: &str,
        angle: f32,
        base: Option<gridcraft_text::Dir>,
    ) {
        let line = gridcraft_text::shape_styled(text, size, style, base);
        self.glyphs(x, y, size, color, &line, angle, style.italic);
    }

    /// Draws an RGBA image (`pw`×`ph` pixels, straight alpha) into the box `(x, y, w, h)`.
    pub fn image_rgba(&mut self, x: f32, y: f32, w: f32, h: f32, rgba: &[u8], pw: u32, ph: u32) {
        let px = u64::from(pw) * u64::from(ph);
        if pw == 0 || ph == 0 || px > 40_000_000 || (rgba.len() as u64) < px * 4 || !finite(&[x, y, w, h]) || w <= 0.0 || h <= 0.0 || self.hidden() {
            return;
        }
        let Some(mut img) = Pixmap::new(pw, ph) else { return };
        for (dst, src) in img.pixels_mut().iter_mut().zip(rgba.chunks_exact(4)) {
            let Some(&[r, g, b, a]) = src.get(..4).and_then(|s| <&[u8; 4]>::try_from(s).ok()) else { continue };
            let c = tiny_skia::ColorU8::from_rgba(r, g, b, a).premultiply();
            *dst = c;
        }
        let t = Transform::from_row(w / pw as f32, 0.0, 0.0, h / ph as f32, x, y);
        let paint = PixmapPaint { quality: FilterQuality::Bilinear, ..PixmapPaint::default() };
        let masked = self.mask_for((x - 1.0, y - 1.0, x + w + 1.0, y + h + 1.0));
        self.pixmap.draw_pixmap(0, 0, img.as_ref(), &paint, t, if masked { self.mask.as_ref() } else { None });
    }

    /// The canvas as a PNG file (8-bit RGBA).
    pub fn png(&self) -> Vec<u8> {
        let mut data = Vec::with_capacity(self.pixmap.data().len());
        for p in self.pixmap.pixels() {
            let c = p.demultiply();
            data.extend_from_slice(&[c.red(), c.green(), c.blue(), c.alpha()]);
        }
        let mut out = Vec::new();
        {
            let mut enc = png::Encoder::new(&mut out, self.pixmap.width(), self.pixmap.height());
            enc.set_color(png::ColorType::Rgba);
            enc.set_depth(png::BitDepth::Eight);
            let Ok(mut w) = enc.write_header() else { return Vec::new() };
            if w.write_image_data(&data).is_err() {
                return Vec::new();
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]
    use super::*;

    /// WHIQ 0.7.6 (G1): a clipped draw costs what it draws, not the canvas. Each cell of a sheet
    /// picture is drawn under its own clip; a full-canvas mask copied and rebuilt per draw made a
    /// 252 × 264 sheet take minutes.
    #[test]
    #[allow(clippy::disallowed_methods)] // the clock, in a native test
    fn clipped_draws_do_not_cost_the_whole_canvas() {
        let (w, h) = (8000u32, 3000u32);
        let mut c = Canvas::new(w, h, [255, 255, 255]).unwrap();
        let t0 = std::time::Instant::now();
        for i in 0..2000u32 {
            let (x, y) = ((i % 100) as f32 * 64.0 + 0.5, (i / 100) as f32 * 20.0 + 0.5);
            c.save();
            c.clip_rect(x, y, 64.0, 20.0);
            c.fill_rect(x - 10.0, y, 80.0, 20.0, [200, 220, 255]);
            c.stroke_line(x - 5.0, y + 10.0, x + 70.0, y + 10.0, 1.0, [0, 0, 0]);
            c.polygon(&[[x, y], [x + 80.0, y], [x + 80.0, y + 30.0]], Some([255, 0, 0]), None);
            c.restore();
        }
        let took = t0.elapsed();
        assert!(took.as_secs_f64() < 3.0, "2,000 clipped cells took {took:?}");
        // And the clip still holds: nothing past the last cell's right edge (x + 64).
        let (x, y) = (99.0 * 64.0 + 0.5, 19.0 * 20.0 + 0.5);
        assert_eq!(c.pixel((x + 66.0) as u32, (y + 2.0) as u32), Some([255, 255, 255]), "clipped away");
        assert_ne!(c.pixel((x + 30.0) as u32, (y + 2.0) as u32), Some([255, 255, 255]), "drawn inside");
    }

    #[test]
    fn draws_shapes_and_clips() {
        let mut c = Canvas::new(100, 60, [255, 255, 255]).unwrap();
        c.fill_rect(10.0, 10.0, 20.0, 20.0, [255, 0, 0]);
        assert_eq!(c.pixel(15, 15), Some([255, 0, 0]));
        assert_eq!(c.pixel(5, 5), Some([255, 255, 255]));
        c.save();
        c.clip_rect(50.0, 0.0, 10.0, 60.0);
        c.fill_rect(40.0, 40.0, 40.0, 10.0, [0, 0, 255]);
        c.restore();
        assert_eq!(c.pixel(55, 45), Some([0, 0, 255]));
        assert_eq!(c.pixel(45, 45), Some([255, 255, 255]), "clipped away");
        c.stroke_line(0.0, 55.5, 100.0, 55.5, 1.0, [0, 0, 0]);
        assert!(c.pixel(50, 55).unwrap()[0] < 128);
        assert!(Canvas::new(0, 10, [0, 0, 0]).is_none());
        assert!(Canvas::new(100_000, 100_000, [0, 0, 0]).is_none());
        let png = c.png();
        assert!(png.starts_with(&[0x89, b'P', b'N', b'G']));
    }

    #[test]
    fn arabic_text_puts_ink_on_the_canvas() {
        if !gridcraft_text::fonts_for_tests() {
            return;
        }
        let mut c = Canvas::new(200, 50, [255, 255, 255]).unwrap();
        c.text(10.0, 35.0, 24.0, false, false, [0, 0, 0], "مرحبا", 0.0, None);
        let dark = (0..200).flat_map(|x| (0..50).map(move |y| (x, y))).filter(|(x, y)| c.pixel(*x, *y).is_some_and(|p| p[0] < 100)).count();
        assert!(dark > 80, "{dark} dark pixels");
        // Nothing drawn for hostile sizes.
        c.text(0.0, 0.0, f32::INFINITY, false, false, [0, 0, 0], "x", 0.0, None);
        c.image_rgba(0.0, 0.0, 10.0, 10.0, &[255; 16], 2, 2);
    }
}
