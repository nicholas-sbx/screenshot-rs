//! A small painter over tiny-skia, shaped like the QPainter calls the
//! Python version made: coordinates in logical pixels, a transform to the
//! device's, opacity, and clipping.
//!
//! Pictures are BGRA while tiny-skia works in RGBA: every colour handed to
//! tiny-skia has red and blue swapped (`sk`), which makes the bytes come
//! out right. Blending treats the channels alike, so nothing else changes.

use tiny_skia::{
    FillRule, FilterQuality, LineCap, LineJoin, Mask, Paint, PathBuilder, Pattern, PixmapMut, PixmapPaint, PixmapRef, Shader, SpreadMode, Stroke, StrokeDash,
    Transform,
};

use crate::geom::{IRect, Point, Rect};
use crate::image::Image;
use crate::render::text::{self, FontSpec};
use crate::render::theme::Color;

/// A colour for tiny-skia, with red and blue swapped for BGRA targets.
pub fn sk(c: Color) -> tiny_skia::Color {
    tiny_skia::Color::from_rgba8(c.b, c.g, c.r, c.a)
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Cap {
    Flat,
    Square,
    Round,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Join {
    Miter,
    Bevel,
    Round,
}

/// A pen: Qt's defaults are a square cap and a bevel join.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pen {
    pub color: Color,
    pub width: f32,
    pub cap: Cap,
    pub join: Join,
    pub dashed: bool,
}

impl Pen {
    pub fn new(color: Color, width: f32) -> Pen {
        Pen { color, width, cap: Cap::Square, join: Join::Bevel, dashed: false }
    }

    /// Round caps and joins (the drawing tools' pen).
    pub fn round(color: Color, width: f32) -> Pen {
        Pen { color, width, cap: Cap::Round, join: Join::Round, dashed: false }
    }

    pub fn with_join(mut self, join: Join) -> Pen {
        self.join = join;
        self
    }

    pub fn with_cap(mut self, cap: Cap) -> Pen {
        self.cap = cap;
        self
    }

    pub fn dashed(mut self) -> Pen {
        self.dashed = true;
        self
    }

    fn stroke(&self) -> Stroke {
        let mut s = Stroke {
            width: self.width.max(0.0),
            line_cap: match self.cap {
                Cap::Flat => LineCap::Butt,
                Cap::Square => LineCap::Square,
                Cap::Round => LineCap::Round,
            },
            line_join: match self.join {
                Join::Miter => LineJoin::Miter,
                Join::Bevel => LineJoin::Bevel,
                Join::Round => LineJoin::Round,
            },
            miter_limit: 2.0,
            dash: None,
        };
        if self.dashed {
            // Qt's DashLine: 4 on, 2 off, in pen widths.
            let w = self.width.max(1.0);
            s.dash = StrokeDash::new(vec![4.0 * w, 2.0 * w], 0.0);
        }
        s
    }
}

/// Text placement in a box, as Qt's alignment flags.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Align {
    Center,
    /// Left, centred vertically.
    Left,
    /// Right, centred vertically.
    Right,
}

#[derive(Clone)]
struct State {
    t: Transform,
    opacity: f32,
    clip: Option<IRect>,
    has_mask: bool,
}

pub struct Painter<'a> {
    pub pm: PixmapMut<'a>,
    t: Transform,
    opacity: f32,
    /// Device-space clip rectangle, when one is set.
    clip: Option<IRect>,
    mask: Option<Mask>,
    stack: Vec<(State, Option<Mask>)>,
    pub antialias: bool,
}

impl<'a> Painter<'a> {
    /// A painter for `pm`, whose device pixels are `scale` per logical pixel.
    pub fn new(pm: PixmapMut<'a>, scale: f32) -> Painter<'a> {
        Painter { pm, t: Transform::from_scale(scale, scale), opacity: 1.0, clip: None, mask: None, stack: Vec::new(), antialias: true }
    }

    /// For a buffer that holds the device area starting at `origin` (a
    /// damaged region drawn on its own): everything shifts by -origin.
    pub fn for_region(pm: PixmapMut<'a>, scale: f32, origin: (i32, i32)) -> Painter<'a> {
        let t = Transform::from_row(scale, 0.0, 0.0, scale, -origin.0 as f32, -origin.1 as f32);
        Painter { pm, t, opacity: 1.0, clip: None, mask: None, stack: Vec::new(), antialias: true }
    }

    pub fn width(&self) -> u32 {
        self.pm.width()
    }

    pub fn height(&self) -> u32 {
        self.pm.height()
    }

    pub fn transform(&self) -> Transform {
        self.t
    }

    /// Device pixels per logical pixel (the transform's scale).
    pub fn scale(&self) -> f32 {
        self.t.sx
    }

    pub fn save(&mut self) {
        let state = State { t: self.t, opacity: self.opacity, clip: self.clip, has_mask: self.mask.is_some() };
        self.stack.push((state, self.mask.clone()));
    }

    pub fn restore(&mut self) {
        if let Some((s, mask)) = self.stack.pop() {
            self.t = s.t;
            self.opacity = s.opacity;
            self.clip = s.clip;
            self.mask = if s.has_mask { mask } else { None };
        }
    }

    pub fn translate(&mut self, dx: f32, dy: f32) {
        self.t = self.t.pre_translate(dx, dy);
    }

    pub fn scale_by(&mut self, sx: f32, sy: f32) {
        self.t = self.t.pre_scale(sx, sy);
    }

    pub fn rotate(&mut self, degrees: f32) {
        self.t = self.t.pre_rotate(degrees);
    }

    /// Replace the whole transform.
    pub fn set_transform(&mut self, t: Transform) {
        self.t = t;
    }

    pub fn set_opacity(&mut self, o: f32) {
        self.opacity = o.clamp(0.0, 1.0);
    }

    pub fn opacity(&self) -> f32 {
        self.opacity
    }

    /// Map a logical point to device pixels.
    pub fn map(&self, p: Point) -> Point {
        let mut pts = [tiny_skia::Point::from_xy(p.x, p.y)];
        self.t.map_points(&mut pts);
        Point::new(pts[0].x, pts[0].y)
    }

    pub fn map_rect(&self, r: Rect) -> Rect {
        let a = self.map(r.top_left());
        let b = self.map(Point::new(r.right(), r.bottom()));
        Rect::from_points(a, b)
    }

    /// Clip further to `r` (logical). Axis-aligned: no mask needed unless a
    /// rounded clip is already set.
    pub fn clip_rect(&mut self, r: Rect) {
        let d = self.map_rect(r);
        let ir = IRect::from_ltrb(d.x.round() as i32, d.y.round() as i32, d.right().round() as i32, d.bottom().round() as i32);
        self.clip = Some(match self.clip {
            Some(c) => c.intersected(&ir),
            None => ir,
        });
        self.rebuild_mask_for_clip();
    }

    /// Clip to a rounded rectangle (logical): needs a mask.
    pub fn clip_rounded(&mut self, r: Rect, radius: f32) {
        let Some(path) = rounded_rect_path(r, radius) else { return };
        let (w, h) = (self.pm.width(), self.pm.height());
        let mut mask = match self.mask.take() {
            Some(m) => m,
            None => {
                let mut m = Mask::new(w, h).expect("mask of a valid pixmap");
                m.data_mut().fill(255);
                m
            }
        };
        // Intersect: draw the new shape into a fresh mask, then multiply.
        let mut shape = Mask::new(w, h).expect("mask of a valid pixmap");
        shape.fill_path(&path, FillRule::Winding, true, self.t);
        for (d, s) in mask.data_mut().iter_mut().zip(shape.data()) {
            *d = ((*d as u32 * *s as u32 + 127) / 255) as u8;
        }
        self.mask = Some(mask);
        self.rebuild_mask_for_clip();
    }

    fn rebuild_mask_for_clip(&mut self) {
        // With a mask in play the rectangle clip is folded into it.
        if let (Some(mask), Some(c)) = (self.mask.as_mut(), self.clip) {
            let (w, h) = (mask.width() as i32, mask.height() as i32);
            let data = mask.data_mut();
            for y in 0..h {
                for x in 0..w {
                    if !c.contains(crate::geom::IPoint::new(x, y)) {
                        data[(y * w + x) as usize] = 0;
                    }
                }
            }
        }
    }

    fn paint_for(&self, color: Color) -> Paint<'static> {
        let mut paint = Paint::default();
        let a = (color.a as f32 * self.opacity).round().clamp(0.0, 255.0) as u8;
        paint.set_color(sk(color.with_alpha(a)));
        paint.anti_alias = self.antialias;
        paint
    }

    /// Make sure a rectangle clip has a mask to draw through (tiny-skia
    /// clips with masks only). The rounded clip already is one. A clip
    /// around all of the pixmap needs none (and drawing without one is
    /// much faster: opaque fills are plain stores).
    fn ensure_mask(&mut self) {
        if self.mask.is_none()
            && let Some(c) = self.clip
        {
            let (w, h) = (self.pm.width(), self.pm.height());
            let all = IRect::new(0, 0, w as i32, h as i32);
            if c.intersected(&all) == all {
                return;
            }
            let mut m = Mask::new(w, h).expect("mask of a valid pixmap");
            let c = c.intersected(&all);
            for y in c.y..c.y2() {
                let row = &mut m.data_mut()[(y as usize * w as usize)..((y + 1) as usize * w as usize)];
                row[c.x as usize..c.x2() as usize].fill(255);
            }
            self.mask = Some(m);
        }
    }

    pub fn fill_rect(&mut self, r: Rect, color: Color) {
        if r.is_empty() || color.a == 0 {
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h) else { return };
        let paint = self.paint_for(color);
        self.ensure_mask();
        self.pm.fill_rect(rect, &paint, self.t, self.mask.as_ref());
    }

    /// A rectangle of whole device pixels, no anti-aliasing.
    pub fn fill_device_rect(&mut self, x: i32, y: i32, w: i32, h: i32, color: Color) {
        if w <= 0 || h <= 0 || color.a == 0 {
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(x as f32, y as f32, w as f32, h as f32) else { return };
        let mut paint = self.paint_for(color);
        paint.anti_alias = false;
        self.ensure_mask();
        self.pm.fill_rect(rect, &paint, Transform::identity(), self.mask.as_ref());
    }

    pub fn fill_path(&mut self, path: &tiny_skia::Path, color: Color) {
        if color.a == 0 {
            return;
        }
        let paint = self.paint_for(color);
        self.ensure_mask();
        self.pm.fill_path(path, &paint, FillRule::Winding, self.t, self.mask.as_ref());
    }

    pub fn fill_path_rule(&mut self, path: &tiny_skia::Path, color: Color, even_odd: bool) {
        if color.a == 0 {
            return;
        }
        let paint = self.paint_for(color);
        self.ensure_mask();
        let rule = if even_odd { FillRule::EvenOdd } else { FillRule::Winding };
        self.pm.fill_path(path, &paint, rule, self.t, self.mask.as_ref());
    }

    pub fn fill_path_shader(&mut self, path: &tiny_skia::Path, shader: Shader<'_>) {
        let mut paint = Paint { shader, anti_alias: self.antialias, ..Paint::default() };
        if self.opacity < 1.0 {
            paint.shader.apply_opacity(self.opacity);
        }
        self.ensure_mask();
        self.pm.fill_path(path, &paint, FillRule::Winding, self.t, self.mask.as_ref());
    }

    pub fn stroke_path(&mut self, path: &tiny_skia::Path, pen: Pen) {
        if pen.color.a == 0 || pen.width <= 0.0 {
            return;
        }
        let paint = self.paint_for(pen.color);
        let stroke = pen.stroke();
        self.ensure_mask();
        self.pm.stroke_path(path, &paint, &stroke, self.t, self.mask.as_ref());
    }

    pub fn fill_rounded_rect(&mut self, r: Rect, radius: f32, color: Color) {
        if let Some(p) = rounded_rect_path(r, radius) {
            self.fill_path(&p, color);
        }
    }

    pub fn stroke_rounded_rect(&mut self, r: Rect, radius: f32, pen: Pen) {
        if let Some(p) = rounded_rect_path(r, radius) {
            self.stroke_path(&p, pen);
        }
    }

    /// Fill and outline, as QPainter's drawRoundedRect with a brush and pen.
    pub fn draw_rounded_rect(&mut self, r: Rect, radius: f32, fill: Option<Color>, pen: Option<Pen>) {
        let Some(p) = rounded_rect_path(r, radius) else { return };
        if let Some(c) = fill {
            self.fill_path(&p, c);
        }
        if let Some(pen) = pen {
            self.stroke_path(&p, pen);
        }
    }

    pub fn stroke_rect(&mut self, r: Rect, pen: Pen) {
        let device_width = pen.width * self.t.sx.abs().max(self.t.sy.abs());
        if !pen.dashed && device_width <= 1.0 && pen.width > 0.0 {
            // tiny-skia draws strokes this thin as hairlines, whose edges
            // drift off the pixel grid: four thin bands are crisp.
            let h = pen.width / 2.0;
            let (x0, y0, x1, y1) = (r.x - h, r.y - h, r.x + r.w + h, r.y + r.h + h);
            self.fill_rect(Rect::new(x0, y0, x1 - x0, pen.width), pen.color);
            self.fill_rect(Rect::new(x0, y1 - pen.width, x1 - x0, pen.width), pen.color);
            self.fill_rect(Rect::new(x0, y0 + pen.width, pen.width, (y1 - y0 - 2.0 * pen.width).max(0.0)), pen.color);
            self.fill_rect(Rect::new(x1 - pen.width, y0 + pen.width, pen.width, (y1 - y0 - 2.0 * pen.width).max(0.0)), pen.color);
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w.max(0.0), r.h.max(0.0)) else { return };
        let p = PathBuilder::from_rect(rect);
        self.stroke_path(&p, pen);
    }

    pub fn draw_line(&mut self, a: Point, b: Point, pen: Pen) {
        let device_width = pen.width * self.t.sx.abs().max(self.t.sy.abs());
        if !pen.dashed && device_width <= 1.0 && pen.width > 0.0 && (a.x == b.x || a.y == b.y) {
            // (as in stroke_rect: crisp, where a hairline would drift)
            let (h, cap) = (pen.width / 2.0, if matches!(pen.cap, Cap::Flat) { 0.0 } else { pen.width / 2.0 });
            let r = if a.x == b.x {
                Rect::new(a.x - h, a.y.min(b.y) - cap, pen.width, (a.y - b.y).abs() + 2.0 * cap)
            } else {
                Rect::new(a.x.min(b.x) - cap, a.y - h, (a.x - b.x).abs() + 2.0 * cap, pen.width)
            };
            self.fill_rect(r, pen.color);
            return;
        }
        let mut pb = PathBuilder::new();
        pb.move_to(a.x, a.y);
        pb.line_to(b.x, b.y);
        if let Some(p) = pb.finish() {
            self.stroke_path(&p, pen);
        }
    }

    pub fn fill_ellipse(&mut self, r: Rect, color: Color) {
        if let Some(p) = ellipse_path(r) {
            self.fill_path(&p, color);
        }
    }

    pub fn stroke_ellipse(&mut self, r: Rect, pen: Pen) {
        if let Some(p) = ellipse_path(r) {
            self.stroke_path(&p, pen);
        }
    }

    pub fn draw_ellipse(&mut self, r: Rect, fill: Option<Color>, pen: Option<Pen>) {
        let Some(p) = ellipse_path(r) else { return };
        if let Some(c) = fill {
            self.fill_path(&p, c);
        }
        if let Some(pen) = pen {
            self.stroke_path(&p, pen);
        }
    }

    /// A dot as a round-capped pen draws a single point.
    pub fn draw_point(&mut self, at: Point, pen: Pen) {
        let r = pen.width / 2.0;
        match pen.cap {
            Cap::Round => self.fill_ellipse(Rect::new(at.x - r, at.y - r, 2.0 * r, 2.0 * r), pen.color),
            _ => self.fill_rect(Rect::new(at.x - r, at.y - r, 2.0 * r, 2.0 * r), pen.color),
        }
    }

    /// Draw the part `src` (picture pixels) of `img` into `dst` (logical).
    pub fn draw_image(&mut self, img: &Image, dst: Rect, src: Rect, smooth: bool) {
        self.draw_pixmap(img.pixmap_ref(), dst, src, smooth);
    }

    pub fn draw_pixmap(&mut self, pixmap: PixmapRef<'_>, dst: Rect, src: Rect, smooth: bool) {
        if dst.is_empty() || src.is_empty() {
            return;
        }
        let Some(rect) = tiny_skia::Rect::from_xywh(dst.x, dst.y, dst.w, dst.h) else { return };
        let sx = dst.w / src.w;
        let sy = dst.h / src.h;
        let pattern_t = Transform::from_row(sx, 0.0, 0.0, sy, dst.x - src.x * sx, dst.y - src.y * sy);
        let quality = if smooth { FilterQuality::Bilinear } else { FilterQuality::Nearest };
        let shader = Pattern::new(pixmap, SpreadMode::Pad, quality, self.opacity, pattern_t);
        let paint = Paint { shader, anti_alias: false, ..Paint::default() };
        self.ensure_mask();
        self.pm.fill_rect(rect, &paint, self.t, self.mask.as_ref());
    }

    /// A picture at a whole device pixel position, 1:1.
    pub fn blit_image(&mut self, img: &Image, x: i32, y: i32) {
        let paint = PixmapPaint { opacity: self.opacity, ..PixmapPaint::default() };
        self.ensure_mask();
        self.pm.draw_pixmap(x, y, img.pixmap_ref(), &paint, Transform::identity(), self.mask.as_ref());
    }

    // -- text -------------------------------------------------------------------

    /// One line of text with its baseline starting at `at` (logical).
    pub fn draw_text_at(&mut self, at: Point, s: &str, font: FontSpec, color: Color) {
        let line = text::shape(s, font);
        self.draw_line_at(&line, at, color);
    }

    pub fn draw_line_at(&mut self, line: &text::Line, at: Point, color: Color) {
        text::draw(&mut self.pm, self.t, line, at, color, self.opacity, self.clip, self.mask.as_ref());
    }

    /// One line of text placed in `r` (logical), as Qt's drawText(rect, flags).
    pub fn draw_text(&mut self, r: Rect, align: Align, s: &str, font: FontSpec, color: Color) {
        let line = text::shape(s, font);
        let m = text::metrics(font);
        let x = match align {
            Align::Center => r.x + (r.w - line.width) / 2.0,
            Align::Left => r.x,
            Align::Right => r.right() - line.width,
        };
        let baseline = r.y + (r.h - (m.ascent + m.descent)) / 2.0 + m.ascent;
        self.draw_line_at(&line, Point::new(x, baseline), color);
    }
}

/// A rounded rectangle path (radius clamped to fit, as Qt does).
pub fn rounded_rect_path(r: Rect, radius: f32) -> Option<tiny_skia::Path> {
    let r = r.normalized();
    if r.w <= 0.0 || r.h <= 0.0 {
        return None;
    }
    let rad = radius.min(r.w / 2.0).min(r.h / 2.0).max(0.0);
    if rad <= 0.01 {
        return Some(PathBuilder::from_rect(tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h)?));
    }
    // A quarter circle as a cubic: control points at k * radius.
    const K: f32 = 0.552_284_8;
    let (l, t, rt, b) = (r.x, r.y, r.right(), r.bottom());
    let c = rad * K;
    let mut pb = PathBuilder::new();
    pb.move_to(l + rad, t);
    pb.line_to(rt - rad, t);
    pb.cubic_to(rt - rad + c, t, rt, t + rad - c, rt, t + rad);
    pb.line_to(rt, b - rad);
    pb.cubic_to(rt, b - rad + c, rt - rad + c, b, rt - rad, b);
    pb.line_to(l + rad, b);
    pb.cubic_to(l + rad - c, b, l, b - rad + c, l, b - rad);
    pb.line_to(l, t + rad);
    pb.cubic_to(l, t + rad - c, l + rad - c, t, l + rad, t);
    pb.close();
    pb.finish()
}

pub fn ellipse_path(r: Rect) -> Option<tiny_skia::Path> {
    let r = r.normalized();
    PathBuilder::from_oval(tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h)?)
}

/// A path from lists of points; each list is one open polyline.
pub fn polylines(segments: &[&[(f32, f32)]]) -> Option<tiny_skia::Path> {
    let mut pb = PathBuilder::new();
    for seg in segments {
        let Some((first, rest)) = seg.split_first() else { continue };
        pb.move_to(first.0, first.1);
        for p in rest {
            pb.line_to(p.0, p.1);
        }
    }
    pb.finish()
}

/// A closed polygon.
pub fn polygon(points: &[Point]) -> Option<tiny_skia::Path> {
    let (first, rest) = points.split_first()?;
    let mut pb = PathBuilder::new();
    pb.move_to(first.x, first.y);
    for p in rest {
        pb.line_to(p.x, p.y);
    }
    pb.close();
    pb.finish()
}
