//! Annotations. Geometry is kept in the picture's logical coordinates and
//! painted with whatever painter is handed in (the live overlay or the
//! full-resolution output), so what you see is what gets saved.

use std::cell::RefCell;

use tiny_skia::PathBuilder;

use crate::geom::{IRect, Point, Rect};
use crate::image::Image;
use crate::keyseq;
use crate::render::paint::{Align, Pen, polygon};
use crate::render::text::{self, BOLD};
use crate::render::theme::{self, Color, OUTLINE_DARK, OUTLINE_LIGHT, SIZES};
use crate::render::Painter;
use crate::ui::textedit::{KeyResult, Measure, TextEdit};

/// How the keys held while drawing change a shape: Shift squares it (or
/// snaps a line to 45°), Ctrl draws it out from its middle, Alt moves it
/// without changing its size (as in Krita).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Mods {
    pub constrain: bool,
    pub center: bool,
    pub moving: bool,
}

impl Mods {
    pub fn from_keys(mods: i32) -> Mods {
        Mods { constrain: mods & keyseq::SHIFT != 0, center: mods & keyseq::CTRL != 0, moving: mods & keyseq::ALT != 0 }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Tool {
    Region,
    Record,
    Pen,
    Line,
    Arrow,
    Rect,
    Solid,
    Ellipse,
    Marker,
    Text,
    Pixelate,
    Blur,
    Counter,
}

/// (tool, id, label), in toolbar order.
pub const TOOLS: &[(Tool, &str, &str)] = &[
    (Tool::Region, "region", "Capture region"),
    (Tool::Record, "record", "Record screen"),
    (Tool::Pen, "pen", "Pen"),
    (Tool::Line, "line", "Line"),
    (Tool::Arrow, "arrow", "Arrow"),
    (Tool::Rect, "rect", "Rectangle"),
    (Tool::Solid, "solid", "Filled rectangle"),
    (Tool::Ellipse, "ellipse", "Ellipse"),
    (Tool::Marker, "marker", "Highlighter"),
    (Tool::Text, "text", "Text"),
    (Tool::Pixelate, "pixelate", "Pixelate"),
    (Tool::Blur, "blur", "Blur"),
    (Tool::Counter, "counter", "Counter"),
];

impl Tool {
    pub fn id(self) -> &'static str {
        TOOLS.iter().find(|t| t.0 == self).map(|t| t.1).unwrap_or("region")
    }

    pub fn from_id(id: &str) -> Option<Tool> {
        TOOLS.iter().find(|t| t.1 == id).map(|t| t.0)
    }

    /// The tools that draw in the chosen colour.
    pub fn uses_color(self) -> bool {
        matches!(
            self,
            Tool::Pen | Tool::Line | Tool::Arrow | Tool::Rect | Tool::Solid | Tool::Ellipse | Tool::Marker | Tool::Text | Tool::Counter
        )
    }

    /// Tools dragged out from a corner (Shift/Ctrl/Alt change them).
    pub fn is_drag(self) -> bool {
        matches!(self, Tool::Line | Tool::Arrow | Tool::Rect | Tool::Solid | Tool::Ellipse | Tool::Pixelate | Tool::Blur)
    }
}

fn width_of(size: usize) -> f32 {
    SIZES[size.min(SIZES.len() - 1)]
}

/// A shape defined by a start and end point, dragged out from where the
/// press was (the anchor).
#[derive(Clone, Debug)]
pub struct Drag {
    pub start: Point,
    pub end: Point,
    pub anchor: Point,
    /// where the pointer was last
    pub pointer: Point,
    /// Lines snap to 45° with Shift; the rest become squares.
    line_like: bool,
}

impl Drag {
    fn new(pos: Point, line_like: bool) -> Drag {
        Drag { start: pos, end: pos, anchor: pos, pointer: pos, line_like }
    }

    pub fn extend(&mut self, pos: Point, m: Mods) {
        if m.moving {
            self.anchor += pos - self.pointer;
        }
        self.pointer = pos;
        let mut d = pos - self.anchor;
        if m.constrain {
            d = if self.line_like { snap45(d) } else { square(d) };
        }
        self.start = if m.center { self.anchor - d } else { self.anchor };
        self.end = self.anchor + d;
    }

    pub fn rect(&self) -> Rect {
        Rect::from_points(self.start, self.end)
    }

    pub fn is_valid(&self) -> bool {
        self.start.distance(self.end) > 2.0
    }
}

fn square(d: Point) -> Point {
    let side = d.x.abs().max(d.y.abs());
    let sign = |v: f32| if v < 0.0 { -1.0 } else { 1.0 };
    Point::new(side * sign(d.x), side * sign(d.y))
}

/// The same length, turned to the nearest multiple of 45° (QLineF::setAngle).
fn snap45(d: Point) -> Point {
    let len = d.length();
    // QLineF angles run counter-clockwise with y down.
    let angle = (-d.y).atan2(d.x).to_degrees();
    let snapped = ((angle / 45.0).round() * 45.0).to_radians();
    Point::new(len * snapped.cos(), -len * snapped.sin())
}

#[derive(Clone, Debug)]
pub struct Stroke {
    pub points: Vec<Point>,
    pub marker: bool,
}

/// What a cover was made from: where, how big, and the picture's identity.
type CoverKey = (IRect, usize, usize);

/// Mosaic or blur over what's underneath in the original picture, with
/// the result kept until the shape or the picture changes.
#[derive(Debug, Default)]
pub struct CoverCache {
    cache: RefCell<Option<(CoverKey, Image)>>,
}

impl Clone for CoverCache {
    fn clone(&self) -> Self {
        CoverCache::default()
    }
}

#[derive(Clone, Debug)]
pub struct TextShape {
    pub pos: Point,
    pub edit: TextEdit,
    pub editing: bool,
    /// The text this is an edited copy of (its index in the list), and
    /// how that one was: (text, pos, color, size).
    pub replaces: Option<(usize, String, Point, Color, usize)>,
}

#[derive(Clone, Debug)]
pub enum Kind {
    Stroke(Stroke),
    Line(Drag),
    Arrow(Drag),
    Rect(Drag),
    Solid(Drag),
    Ellipse(Drag),
    Pixelate(Drag, CoverCache),
    Blur(Drag, CoverCache),
    Text(TextShape),
    Counter { pos: Point, number: u32 },
}

#[derive(Clone, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub color: Color,
    pub size: usize,
    /// replaced by an edited copy (see TextShape::replaces)
    pub hidden: bool,
}

pub const TEXT_SIZES_PX: [f32; 3] = [18.0, 26.0, 38.0];
const TEXT_PAD: f32 = 6.0;
const COUNTER_RADII: [f32; 3] = [11.0, 14.0, 18.0];
const BLUR_STRENGTH: [f32; 3] = [5.0, 8.0, 12.0];

impl Shape {
    /// A new shape for a press of `tool` at `pos` (None for tools that
    /// don't drag: text and counters are made by their own code).
    pub fn create(tool: Tool, pos: Point, color: Color, size: usize) -> Option<Shape> {
        let kind = match tool {
            Tool::Pen => Kind::Stroke(Stroke { points: vec![pos], marker: false }),
            Tool::Marker => Kind::Stroke(Stroke { points: vec![pos], marker: true }),
            Tool::Line => Kind::Line(Drag::new(pos, true)),
            Tool::Arrow => Kind::Arrow(Drag::new(pos, true)),
            Tool::Rect => Kind::Rect(Drag::new(pos, false)),
            Tool::Solid => Kind::Solid(Drag::new(pos, false)),
            Tool::Ellipse => Kind::Ellipse(Drag::new(pos, false)),
            Tool::Pixelate => Kind::Pixelate(Drag::new(pos, false), CoverCache::default()),
            Tool::Blur => Kind::Blur(Drag::new(pos, false), CoverCache::default()),
            _ => return None,
        };
        Some(Shape { kind, color, size, hidden: false })
    }

    pub fn text(pos: Point, color: Color, size: usize) -> Shape {
        Shape {
            kind: Kind::Text(TextShape { pos, edit: TextEdit::default(), editing: true, replaces: None }),
            color,
            size,
            hidden: false,
        }
    }

    pub fn counter(pos: Point, color: Color, size: usize, number: u32) -> Shape {
        Shape { kind: Kind::Counter { pos, number }, color, size, hidden: false }
    }

    pub fn width(&self) -> f32 {
        width_of(self.size)
    }

    pub fn is_counter(&self) -> bool {
        matches!(self.kind, Kind::Counter { .. })
    }

    pub fn as_text(&self) -> Option<&TextShape> {
        match &self.kind {
            Kind::Text(t) => Some(t),
            _ => None,
        }
    }

    pub fn as_text_mut(&mut self) -> Option<&mut TextShape> {
        match &mut self.kind {
            Kind::Text(t) => Some(t),
            _ => None,
        }
    }

    /// The dragged-out kinds: they have an anchor and a pointer.
    pub fn drag(&self) -> Option<&Drag> {
        match &self.kind {
            Kind::Line(d) | Kind::Arrow(d) | Kind::Rect(d) | Kind::Solid(d) | Kind::Ellipse(d) => Some(d),
            Kind::Pixelate(d, _) | Kind::Blur(d, _) => Some(d),
            _ => None,
        }
    }

    fn drag_mut(&mut self) -> Option<&mut Drag> {
        match &mut self.kind {
            Kind::Line(d) | Kind::Arrow(d) | Kind::Rect(d) | Kind::Solid(d) | Kind::Ellipse(d) => Some(d),
            Kind::Pixelate(d, _) | Kind::Blur(d, _) => Some(d),
            _ => None,
        }
    }

    pub fn extend(&mut self, pos: Point, m: Mods) {
        match &mut self.kind {
            Kind::Stroke(s) => {
                if s.points.last().is_none_or(|last| last.distance(pos) >= 1.0) {
                    s.points.push(pos);
                }
            }
            _ => {
                if let Some(d) = self.drag_mut() {
                    d.extend(pos, m);
                }
            }
        }
    }

    pub fn is_valid(&self) -> bool {
        match &self.kind {
            Kind::Stroke(_) | Kind::Counter { .. } => true,
            Kind::Text(t) => !t.edit.text.trim().is_empty() || (t.replaces.is_some() && self.text_changed()),
            _ => self.drag().is_some_and(Drag::is_valid),
        }
    }

    /// For an edited copy of a text: is it any different from the original?
    pub fn text_changed(&self) -> bool {
        let Kind::Text(t) = &self.kind else { return true };
        match &t.replaces {
            None => true,
            Some((_, text, pos, color, size)) => (&t.edit.text, t.pos, self.color, self.size) != (text, *pos, *color, *size),
        }
    }

    /// The area it covers (logical), with room for its stroke.
    pub fn bounds(&self) -> Rect {
        let w = self.width();
        match &self.kind {
            Kind::Stroke(s) => {
                let pad = if s.marker { (w * 3.0 + 10.0) / 2.0 } else { w / 2.0 } + 2.0;
                points_bounds(&s.points).adjusted(-pad, -pad, pad, pad)
            }
            Kind::Line(d) => Rect::from_points(d.start, d.end).adjusted(-w - 2.0, -w - 2.0, w + 2.0, w + 2.0),
            Kind::Arrow(d) => {
                let head = 10.0 + w * 3.2;
                Rect::from_points(d.start, d.end).adjusted(-head, -head, head, head)
            }
            Kind::Rect(d) | Kind::Ellipse(d) => d.rect().adjusted(-w - 2.0, -w - 2.0, w + 2.0, w + 2.0),
            Kind::Solid(d) | Kind::Pixelate(d, _) | Kind::Blur(d, _) => d.rect().adjusted(-2.0, -2.0, 2.0, 2.0),
            Kind::Text(t) => text_bounds(t, self.size).adjusted(-TEXT_PAD - 3.0, -TEXT_PAD - 3.0, TEXT_PAD + 3.0, TEXT_PAD + 3.0),
            Kind::Counter { pos, .. } => {
                let r = COUNTER_RADII[self.size.min(2)] + 2.0;
                Rect::new(pos.x - r, pos.y - r, 2.0 * r, 2.0 * r)
            }
        }
    }

    /// What changed with the last point added to a stroke (cheap damage
    /// while drawing freehand), else the whole bounds.
    pub fn recent_bounds(&self) -> Rect {
        match &self.kind {
            Kind::Stroke(s) if s.points.len() > 3 => {
                let w = self.width();
                let pad = if s.marker { (w * 3.0 + 10.0) / 2.0 } else { w / 2.0 } + 2.0;
                points_bounds(&s.points[s.points.len() - 4..]).adjusted(-pad, -pad, pad, pad)
            }
            _ => self.bounds(),
        }
    }

    /// Paint on `p` (logical coordinates); `base` is the picture underneath
    /// and `dpr` its pixels per logical pixel (pixelate and blur use it).
    pub fn paint(&self, p: &mut Painter<'_>, base: &Image, dpr: f32) {
        let w = self.width();
        match &self.kind {
            Kind::Stroke(s) => {
                let (color, width) = if s.marker { (self.color.with_alpha(105), w * 3.0 + 10.0) } else { (self.color, w) };
                let pen = Pen::round(color, width);
                if s.points.len() == 1 {
                    p.draw_point(s.points[0], pen);
                    return;
                }
                // Smooth through midpoints with quadratic segments.
                let mut pb = PathBuilder::new();
                pb.move_to(s.points[0].x, s.points[0].y);
                for pair in s.points[1..].windows(2) {
                    let (a, b) = (pair[0], pair[1]);
                    let mid = (a + b) / 2.0;
                    pb.quad_to(a.x, a.y, mid.x, mid.y);
                }
                let last = s.points[s.points.len() - 1];
                pb.line_to(last.x, last.y);
                if let Some(path) = pb.finish() {
                    p.stroke_path(&path, pen);
                }
            }
            Kind::Line(d) => p.draw_line(d.start, d.end, Pen::round(self.color, w)),
            Kind::Arrow(d) => paint_arrow(p, d, self.color, w),
            Kind::Rect(d) => p.stroke_rounded_rect(d.rect(), 4.0, Pen::round(self.color, w)),
            Kind::Solid(d) => p.fill_rounded_rect(d.rect(), 3.0, self.color),
            Kind::Ellipse(d) => p.stroke_ellipse(d.rect(), Pen::round(self.color, w)),
            Kind::Pixelate(d, cache) => paint_cover(p, d.rect(), base, dpr, self.size, cache, false),
            Kind::Blur(d, cache) => paint_cover(p, d.rect(), base, dpr, self.size, cache, true),
            Kind::Text(t) => paint_text(p, t, self.color, self.size),
            Kind::Counter { pos, number } => {
                let r = COUNTER_RADII[self.size.min(2)];
                let bx = Rect::new(pos.x - r, pos.y - r, 2.0 * r, 2.0 * r);
                p.fill_ellipse(bx, self.color);
                let fg = if self.color.is_light() { OUTLINE_DARK } else { OUTLINE_LIGHT };
                p.draw_text(bx, Align::Center, &number.to_string(), text::font((r * 1.05).round(), BOLD), fg);
            }
        }
    }
}

fn points_bounds(pts: &[Point]) -> Rect {
    let mut l = f32::MAX;
    let mut t = f32::MAX;
    let mut r = f32::MIN;
    let mut b = f32::MIN;
    for p in pts {
        l = l.min(p.x);
        t = t.min(p.y);
        r = r.max(p.x);
        b = b.max(p.y);
    }
    if pts.is_empty() { Rect::default() } else { Rect::from_ltrb(l, t, r, b) }
}

fn paint_arrow(p: &mut Painter<'_>, d: &Drag, color: Color, w: f32) {
    let v = d.end - d.start;
    let length = v.length();
    if length < 1.0 {
        return;
    }
    let head = (10.0 + w * 3.2).min(length * 0.6);
    let unit = v / length;
    let normal = Point::new(-unit.y, unit.x);
    let back = d.end - unit * head;
    let tip = [d.end, back + normal * head * 0.55, back - normal * head * 0.55];
    p.draw_line(d.start, d.end - unit * head * 0.7, Pen::round(color, w));
    if let Some(path) = polygon(&tip) {
        p.fill_path(&path, color);
        p.stroke_path(&path, Pen::round(color, (w * 0.6).max(1.5)));
    }
}

/// Pixelate (`blur` false) or blur what's under `r` in `base`.
fn paint_cover(p: &mut Painter<'_>, r: Rect, base: &Image, dpr: f32, size: usize, cache: &CoverCache, blur: bool) {
    if r.w < 2.0 || r.h < 2.0 {
        return;
    }
    let src = IRect::new((r.x * dpr).round() as i32, (r.y * dpr).round() as i32, (r.w * dpr).round() as i32, (r.h * dpr).round() as i32)
        .intersected(&base.rect());
    if src.is_empty() {
        return;
    }
    let key = (src, size, base.data().as_ptr() as usize);
    let mut slot = cache.cache.borrow_mut();
    if slot.as_ref().is_none_or(|(k, _)| *k != key) {
        let cover = if blur { blur_cover(base, src, dpr, size) } else { pixelate_cover(base, src, dpr, size) };
        match cover {
            Some(c) => *slot = Some((key, c)),
            None => return,
        }
    }
    let Some((_, cover)) = slot.as_ref() else { return };
    let target = Rect::new(src.x as f32 / dpr, src.y as f32 / dpr, src.w as f32 / dpr, src.h as f32 / dpr);
    p.draw_image(cover, target, cover.rect().to_f(), false);
}

/// Blocks of the average colour of what they cover.
pub fn pixelate_cover(base: &Image, src: IRect, dpr: f32, size: usize) -> Option<Image> {
    let block = ((6.0 + size as f32 * 5.0) * dpr).round().max(4.0) as i32;
    let crop = base.crop(src).ok()?;
    let nx = (src.w / block).max(1) as u32;
    let ny = (src.h / block).max(1) as u32;
    let small = crop.scaled_area(nx, ny).ok()?;
    // Back up to size, each block one colour (nearest neighbour).
    let mut out = Image::new(src.w as u32, src.h as u32).ok()?;
    for y in 0..src.h as u32 {
        let sy = (y as u64 * ny as u64 / src.h as u64) as u32;
        let srow = small.row(sy.min(ny - 1)).to_vec();
        let drow = out.row_mut(y);
        for x in 0..src.w as usize {
            let sx = (x as u64 * nx as u64 / src.w as u64) as usize;
            let sx = sx.min(nx as usize - 1);
            drow[x * 4..x * 4 + 4].copy_from_slice(&srow[sx * 4..sx * 4 + 4]);
        }
    }
    Some(out)
}

/// Blurred past reading: shrunk (averaging), softened, grown back smoothly.
pub fn blur_cover(base: &Image, src: IRect, dpr: f32, size: usize) -> Option<Image> {
    let k = (BLUR_STRENGTH[size.min(2)] * dpr).round().max(2.0) as i32;
    // Blend in what's beside it, so the edges don't show.
    let around = src.adjusted(-2 * k, -2 * k, 2 * k, 2 * k).intersected(&base.rect());
    let crop = base.crop(around).ok()?;
    let sw = (around.w / k).max(1) as u32;
    let sh = (around.h / k).max(1) as u32;
    let small = crop.scaled_area(sw, sh).ok()?;
    let half = small.scaled_area((sw / 2).max(1), (sh / 2).max(1)).ok()?;
    let softer = half.scaled_bilinear(sw, sh).ok()?;
    let big = softer.scaled_bilinear(around.w as u32, around.h as u32).ok()?;
    big.crop(src.translated(-around.x, -around.y)).ok()
}

// -- text ---------------------------------------------------------------------------

fn text_font(size: usize) -> text::FontSpec {
    text::font(TEXT_SIZES_PX[size.min(2)], BOLD)
}

/// Measures lines of a text shape's font.
pub struct TextMeasure(pub text::FontSpec);

impl Measure for TextMeasure {
    fn width(&self, s: &str) -> f32 {
        text::width(s, self.0)
    }
}

/// The text's box (at least a caret's width when empty).
pub fn text_bounds(t: &TextShape, size: usize) -> Rect {
    let f = text_font(size);
    let m = text::metrics(f);
    let lines = t.edit.lines();
    let w = lines.iter().map(|l| text::width(l, f)).fold(2.0f32, f32::max);
    let h = m.height + (lines.len() as f32 - 1.0) * m.line_spacing;
    Rect::new(t.pos.x, t.pos.y, w, h)
}

impl TextShape {
    pub fn contains(&self, at: Point, size: usize) -> bool {
        text_bounds(self, size).adjusted(-TEXT_PAD, -TEXT_PAD, TEXT_PAD, TEXT_PAD).contains(at)
    }

    pub fn caret_rect(&self, size: usize) -> Rect {
        let f = text_font(size);
        let m = text::metrics(f);
        let (row, col) = self.edit.row_col(self.edit.cursor);
        let x = self.edit.x_of(row, col, &TextMeasure(f));
        Rect::new(self.pos.x + x - 1.0, self.pos.y + row as f32 * m.line_spacing, 2.0, m.height)
    }

    /// The caret position nearest to `at`.
    pub fn index_at(&self, at: Point, size: usize) -> usize {
        let f = text_font(size);
        let m = text::metrics(f);
        let rows = self.edit.lines().len();
        let row = (((at.y - self.pos.y) / m.line_spacing).floor().max(0.0) as usize).min(rows.saturating_sub(1));
        let col = self.edit.col_at(row, at.x - self.pos.x, &TextMeasure(f));
        self.edit.index(row, col)
    }

    pub fn key(&mut self, press: &crate::keys::KeyPress, size: usize) -> KeyResult {
        self.edit.key(press, &TextMeasure(text_font(size)))
    }
}

/// A copy of text shape `s` (at `index` in its list) to edit in place of
/// it, so undo can bring the original back.
pub fn copy_for_editing(s: &Shape, index: usize) -> Option<Shape> {
    let t = s.as_text()?;
    let mut edit = TextEdit::new(&t.edit.text);
    edit.place(edit.text.len(), false);
    Some(Shape {
        kind: Kind::Text(TextShape {
            pos: t.pos,
            edit,
            editing: true,
            replaces: Some((index, t.edit.text.clone(), t.pos, s.color, s.size)),
        }),
        color: s.color,
        size: s.size,
        hidden: false,
    })
}

fn paint_text(p: &mut Painter<'_>, t: &TextShape, color: Color, size: usize) {
    let f = text_font(size);
    let m = text::metrics(f);
    let lines = t.edit.lines();
    let palette = theme::c();
    if t.editing {
        // The edit box, and what's selected.
        let bx = text_bounds(t, size).adjusted(-TEXT_PAD, -TEXT_PAD / 2.0, TEXT_PAD, TEXT_PAD / 2.0);
        p.stroke_rounded_rect(bx, 4.0, Pen::new(palette.accent.with_alpha(170), 1.2).dashed());
        let (a, b) = t.edit.selection();
        if a != b {
            let shade = palette.accent.with_alpha(90);
            let (ra, ca) = t.edit.row_col(a);
            let (rb, cb) = t.edit.row_col(b);
            let measure = TextMeasure(f);
            for (r, line) in lines.iter().enumerate().take(rb + 1).skip(ra) {
                let x0 = t.edit.x_of(r, if r == ra { ca } else { 0 }, &measure);
                let x1 = t.edit.x_of(r, if r == rb { cb } else { line.len() }, &measure)
                    + if r == rb { 0.0 } else { m.avg_char_width / 2.0 };
                p.fill_rect(Rect::new(t.pos.x + x0, t.pos.y + r as f32 * m.line_spacing, x1 - x0, m.height), shade);
            }
        }
    }
    for (i, line) in lines.iter().enumerate() {
        let shaped = text::shape(line, f);
        let at = Point::new(t.pos.x, t.pos.y + m.ascent + i as f32 * m.line_spacing);
        let (path, pictures) = text::outline_path(&shaped, at);
        if let Some(path) = path {
            p.fill_path(&path, color);
        }
        for (glyph, r) in pictures {
            if let Some(color_px) = &glyph.color
                && let Ok(img) = Image::from_bgra(glyph.w, glyph.h, color_px.clone())
            {
                p.draw_image(&img, r, img.rect().to_f(), true);
            }
        }
    }
    if t.editing {
        p.fill_rect(t.caret_rect(size), palette.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drags_and_modifiers() {
        let mut s = Shape::create(Tool::Rect, Point::new(10.0, 10.0), Color::BLACK, 1).unwrap();
        s.extend(Point::new(30.0, 20.0), Mods::default());
        assert_eq!(s.drag().unwrap().rect(), Rect::new(10.0, 10.0, 20.0, 10.0));
        s.extend(Point::new(30.0, 20.0), Mods { constrain: true, ..Default::default() });
        assert_eq!(s.drag().unwrap().rect(), Rect::new(10.0, 10.0, 20.0, 20.0));
        s.extend(Point::new(30.0, 20.0), Mods { center: true, ..Default::default() });
        assert_eq!(s.drag().unwrap().rect(), Rect::new(-10.0, 0.0, 40.0, 20.0));
        // Alt moves it: the anchor follows the pointer.
        s.extend(Point::new(40.0, 30.0), Mods { moving: true, ..Default::default() });
        assert_eq!(s.drag().unwrap().rect(), Rect::new(20.0, 20.0, 20.0, 10.0));
        let mut l = Shape::create(Tool::Line, Point::new(0.0, 0.0), Color::BLACK, 1).unwrap();
        l.extend(Point::new(10.0, 1.0), Mods { constrain: true, ..Default::default() });
        let d = l.drag().unwrap();
        assert!((d.end.y).abs() < 1e-4 && (d.end.x - 10.0499).abs() < 1e-3, "{:?}", d.end);
        l.extend(Point::new(10.0, -9.0), Mods { constrain: true, ..Default::default() });
        let d = l.drag().unwrap();
        assert!((d.end.x + d.end.y).abs() < 1e-3, "45 degrees: {:?}", d.end);
    }

    #[test]
    fn validity() {
        let s = Shape::create(Tool::Arrow, Point::new(0.0, 0.0), Color::BLACK, 0).unwrap();
        assert!(!s.is_valid());
        let mut t = Shape::text(Point::new(0.0, 0.0), Color::BLACK, 1);
        assert!(!t.is_valid());
        t.as_text_mut().unwrap().edit.insert("hi");
        assert!(t.is_valid());
        let copy = copy_for_editing(&t, 0).unwrap();
        assert!(!copy.text_changed());
    }

    #[test]
    fn covers() {
        let mut base = Image::new(64, 64).unwrap();
        for y in 0..64 {
            for x in 0..64 {
                base.set_pixel(x, y, if (x / 4 + y / 4) % 2 == 0 { 0xFF000000 } else { 0xFFFFFFFF });
            }
        }
        let p = pixelate_cover(&base, IRect::new(0, 0, 40, 40), 1.0, 0).unwrap();
        assert_eq!(p.size(), (40, 40));
        // A block is one colour.
        assert_eq!(p.pixel(0, 0), p.pixel(5, 5));
        let b = blur_cover(&base, IRect::new(10, 10, 30, 30), 1.0, 1).unwrap();
        assert_eq!(b.size(), (30, 30));
        let (r, _, _, _) = crate::image::unpack(b.pixel(15, 15));
        assert!(r > 40 && r < 215, "blurred to grey: {r}");
    }
}
