//! Annotations. Geometry is kept in the picture's logical coordinates and
//! painted with whatever painter is handed in (the live overlay or the
//! full-resolution output), so what you see is what gets saved.

use std::cell::RefCell;

use tiny_skia::PathBuilder;

use crate::geom::{IRect, Point, Rect};
use crate::image::Image;
use crate::keyseq;
use crate::render::Painter;
use crate::render::paint::{Align, Pen, polygon, rounded_rect_path};
use crate::render::text::{self, BOLD};
use crate::render::theme::{self, Color, OUTLINE_DARK, OUTLINE_LIGHT, SIZES};
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
    /// copy the text in an area (OCR)
    Ocr,
    Select,
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
    /// shade everything but the areas dragged out
    Spotlight,
    /// what's under an area, enlarged in it
    Magnify,
    Counter,
    /// the editor's: crop, cut out, rotate and resize the picture
    Crop,
}

/// (tool, id, label), in toolbar order.
pub const TOOLS: &[(Tool, &str, &str)] = &[
    (Tool::Region, "region", "Capture region"),
    (Tool::Record, "record", "Record screen"),
    (Tool::Ocr, "ocr", "Copy text from the screen (OCR)"),
    (Tool::Select, "select", "Select, move and resize drawings"),
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
    (Tool::Spotlight, "spotlight", "Spotlight: shade everything else"),
    (Tool::Magnify, "magnify", "Magnifier"),
    (Tool::Counter, "counter", "Counter"),
    (Tool::Crop, "crop", "Crop, cut out, rotate and resize"),
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
            Tool::Pen | Tool::Line | Tool::Arrow | Tool::Rect | Tool::Solid | Tool::Ellipse | Tool::Marker | Tool::Text | Tool::Counter | Tool::Magnify
        )
    }

    /// The tools that choose an area of the screen, not drawings: to
    /// capture, record or read.
    pub fn picks_area(self) -> bool {
        matches!(self, Tool::Region | Tool::Record | Tool::Ocr)
    }

    /// Tools dragged out from a corner (Shift/Ctrl/Alt change them).
    pub fn is_drag(self) -> bool {
        matches!(self, Tool::Line | Tool::Arrow | Tool::Rect | Tool::Solid | Tool::Ellipse | Tool::Pixelate | Tool::Blur | Tool::Spotlight | Tool::Magnify)
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
    pub fn new(pos: Point, line_like: bool) -> Drag {
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
pub fn snap45(d: Point) -> Point {
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
    pub style: TextStyle,
    /// For an edited copy (see Shape::replaces): how the text it replaces
    /// was, (text, pos, color, size).
    pub original: Option<(String, Point, Color, usize)>,
}

/// How a text looks: plain (its colour), on a label (white or black text
/// on a rounded box of its colour), or a callout (a label with a tail
/// pointing at `.0`).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub enum TextStyle {
    #[default]
    Plain,
    Label,
    Callout(Point),
}

impl TextStyle {
    /// The next of the text tool's styles (a click on it, or T, again).
    pub fn next(self) -> TextStyle {
        match self {
            TextStyle::Plain => TextStyle::Label,
            TextStyle::Label => TextStyle::Callout(Point::default()),
            TextStyle::Callout(_) => TextStyle::Plain,
        }
    }

    /// The text tool's icon in this style.
    pub fn icon(self) -> &'static str {
        match self {
            TextStyle::Plain => "text",
            TextStyle::Label => "text-label",
            TextStyle::Callout(_) => "text-callout",
        }
    }

    /// What the text tool does in this style.
    pub fn hint(self) -> &'static str {
        match self {
            TextStyle::Plain => "Text",
            TextStyle::Label => "Text on a label",
            TextStyle::Callout(_) => "Callout: drag from what it points at to where its text goes",
        }
    }
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
    Spotlight(Drag),
    Magnify(Drag),
    Text(TextShape),
    Counter {
        pos: Point,
        number: u32,
    },
    /// A deleted drawing: nothing, in place of the one it replaces, so undo
    /// brings that back.
    Removed,
    /// The editor's picture changed (cropped, turned, ...): an undo step.
    Step(std::rc::Rc<crate::ui::picture::Step>),
}

/// A drawing. One that's moved, resized, restyled, removed or its text
/// edited is replaced by a copy (`replaces`: the index of the one it
/// changes, in the same list, which is hidden meanwhile), so undo brings
/// the old one back by dropping the copy.
#[derive(Clone, Debug)]
pub struct Shape {
    pub kind: Kind,
    pub color: Color,
    pub size: usize,
    /// replaced by a changed copy, see `replaces`
    pub hidden: bool,
    pub replaces: Option<usize>,
    /// where it is in the stack of drawings across every monitor: they're
    /// painted (and picked) lowest first (0 until the session stamps it)
    pub order: u64,
}

pub const TEXT_SIZES_PX: [f32; 3] = [18.0, 26.0, 38.0];
const TEXT_PAD: f32 = 6.0;
const COUNTER_RADII: [f32; 3] = [11.0, 14.0, 18.0];
const BLUR_STRENGTH: [f32; 3] = [8.0, 12.0, 18.0];

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
            Tool::Spotlight => Kind::Spotlight(Drag::new(pos, false)),
            Tool::Magnify => Kind::Magnify(Drag::new(pos, false)),
            _ => return None,
        };
        Some(Shape { kind, color, size, hidden: false, replaces: None, order: 0 })
    }

    pub fn text(pos: Point, color: Color, size: usize, style: TextStyle) -> Shape {
        Shape {
            kind: Kind::Text(TextShape { pos, edit: TextEdit::default(), editing: true, style, original: None }),
            color,
            size,
            hidden: false,
            replaces: None,
            order: 0,
        }
    }

    pub fn counter(pos: Point, color: Color, size: usize, number: u32) -> Shape {
        Shape { kind: Kind::Counter { pos, number }, color, size, hidden: false, replaces: None, order: 0 }
    }

    /// A change to the editor's picture, as an undo step.
    pub fn step(step: std::rc::Rc<crate::ui::picture::Step>) -> Shape {
        Shape { kind: Kind::Step(step), color: Color::BLACK, size: 0, hidden: false, replaces: None, order: 0 }
    }

    /// Nothing, in place of drawing `index` (deleted).
    pub fn removed(of: &Shape, index: usize) -> Shape {
        Shape { kind: Kind::Removed, color: of.color, size: of.size, hidden: false, replaces: Some(index), order: 0 }
    }

    /// Nothing to see or pick up: a deleted drawing, or a step changing the picture.
    pub fn is_removed(&self) -> bool {
        matches!(self.kind, Kind::Removed | Kind::Step(_))
    }

    // -- picking it up again (the select tool) -----------------------------------

    /// Text and counters can only be moved.
    pub fn resizable(&self) -> bool {
        !matches!(self.kind, Kind::Text(_) | Kind::Counter { .. } | Kind::Removed | Kind::Step(_))
    }

    /// Resized by its two ends (lines, arrows), not a box.
    pub fn has_ends(&self) -> bool {
        matches!(self.kind, Kind::Line(_) | Kind::Arrow(_))
    }

    /// The box the shape's geometry fills (without its stroke).
    pub fn geometry(&self) -> Rect {
        match &self.kind {
            Kind::Stroke(s) => points_bounds(&s.points),
            Kind::Text(t) => label_box(t, self.size),
            Kind::Counter { pos, .. } => {
                let r = COUNTER_RADII[self.size.min(2)];
                Rect::new(pos.x - r, pos.y - r, 2.0 * r, 2.0 * r)
            }
            Kind::Removed | Kind::Step(_) => Rect::default(),
            _ => self.drag().map(Drag::rect).unwrap_or_default(),
        }
    }

    /// A spotlight's lit area.
    pub fn spotlight(&self) -> Option<Rect> {
        match &self.kind {
            Kind::Spotlight(d) if !self.hidden => Some(d.rect()),
            _ => None,
        }
    }

    /// How far its paint reaches past its geometry.
    pub fn reach(&self) -> f32 {
        let w = self.width();
        match &self.kind {
            Kind::Stroke(s) => stroke_reach(s, w),
            // (filled: no stroke past its box)
            Kind::Text(_)
            | Kind::Counter { .. }
            | Kind::Solid(_)
            | Kind::Pixelate(..)
            | Kind::Blur(..)
            | Kind::Spotlight(_)
            | Kind::Removed
            | Kind::Step(_) => 0.0,
            Kind::Magnify(_) => MAGNIFIER_FRAME / 2.0,
            _ => w / 2.0,
        }
    }

    /// Is `pos` on the shape: within its stroke (and `slack`) for lines
    /// and outlines, anywhere inside for filled ones.
    pub fn contains(&self, pos: Point, slack: f32) -> bool {
        let w = self.width();
        match &self.kind {
            Kind::Removed | Kind::Step(_) => false,
            Kind::Stroke(s) => {
                let reach = stroke_reach(s, w) + slack;
                match s.points[..] {
                    [only] => pos.distance(only) <= reach,
                    _ => s.points.windows(2).any(|ab| to_segment(pos, ab[0], ab[1]) <= reach),
                }
            }
            Kind::Line(d) => to_segment(pos, d.start, d.end) <= w / 2.0 + slack,
            Kind::Arrow(d) => {
                let head = (10.0 + w * 3.2).min(d.start.distance(d.end) * 0.6);
                to_segment(pos, d.start, d.end) <= w / 2.0 + slack || pos.distance(d.end) <= head + slack
            }
            Kind::Rect(d) => on_outline(pos, d.rect(), w / 2.0 + slack),
            Kind::Ellipse(d) => {
                let r = d.rect();
                let (rx, ry) = (r.w / 2.0, r.h / 2.0);
                if rx < 1.0 || ry < 1.0 {
                    return on_outline(pos, r, w / 2.0 + slack);
                }
                let v = pos - r.center();
                // How far out along its own radius, times the radius there:
                // near the line, about the distance to it.
                let k = (v.x / rx).hypot(v.y / ry);
                let radius = if k > 0.0 { v.length() / k } else { rx.min(ry) };
                (k - 1.0).abs() * radius <= w / 2.0 + slack
            }
            Kind::Counter { pos: c, .. } => pos.distance(*c) <= COUNTER_RADII[self.size.min(2)] + slack,
            _ => self.geometry().adjusted(-slack, -slack, slack, slack).contains(pos),
        }
    }

    /// A copy to change, in place of this one (its `replaces` is the
    /// caller's to set).
    fn copy(&self) -> Shape {
        let mut c = self.clone();
        c.hidden = false;
        if let Kind::Text(t) = &mut c.kind {
            t.editing = false;
            t.original = None;
        }
        c
    }

    fn map_points(&mut self, f: impl Fn(Point) -> Point) {
        match &mut self.kind {
            Kind::Stroke(s) => s.points.iter_mut().for_each(|p| *p = f(*p)),
            Kind::Text(t) => {
                t.pos = f(t.pos);
                if let TextStyle::Callout(tip) = &mut t.style {
                    *tip = f(*tip);
                }
            }
            Kind::Counter { pos, .. } => *pos = f(*pos),
            Kind::Removed | Kind::Step(_) => {}
            _ => {
                if let Some(d) = self.drag_mut() {
                    d.start = f(d.start);
                    d.end = f(d.end);
                    d.anchor = d.start;
                    d.pointer = d.end;
                }
            }
        }
    }

    /// A copy with each point moved through `f`.
    pub fn mapped(&self, f: impl Fn(Point) -> Point) -> Shape {
        let mut c = self.copy();
        c.map_points(f);
        c
    }

    /// A copy moved by `d`.
    pub fn moved(&self, d: Point) -> Shape {
        let mut c = self.copy();
        c.map_points(|p| p + d);
        c
    }

    /// The same moved by `d`, all as it is (a text being typed stays so).
    pub fn shifted(&self, d: Point) -> Shape {
        let mut c = self.clone();
        c.map_points(|p| p + d);
        c
    }

    /// A copy stretched from the box `old` to `new`.
    pub fn fitted(&self, old: Rect, new: Rect) -> Shape {
        let sx = if old.w != 0.0 { new.w / old.w } else { 1.0 };
        let sy = if old.h != 0.0 { new.h / old.h } else { 1.0 };
        let mut c = self.copy();
        c.map_points(|p| Point::new(new.x + (p.x - old.x) * sx, new.y + (p.y - old.y) * sy));
        c
    }

    /// A copy with its start (0) or end (1) at `pos` (lines and arrows).
    pub fn with_end(&self, which: usize, pos: Point) -> Shape {
        let mut c = self.copy();
        if let Some(d) = c.drag_mut() {
            if which == 0 {
                d.start = pos;
            } else {
                d.end = pos;
            }
            d.anchor = d.start;
            d.pointer = d.end;
        }
        c
    }

    /// A copy in another colour or size.
    pub fn restyled(&self, color: Option<Color>, size: Option<usize>) -> Shape {
        let mut c = self.copy();
        if let Some(color) = color {
            c.color = color;
        }
        if let Some(size) = size {
            c.size = size;
        }
        c
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
            Kind::Pixelate(d, _) | Kind::Blur(d, _) | Kind::Spotlight(d) | Kind::Magnify(d) => Some(d),
            _ => None,
        }
    }

    fn drag_mut(&mut self) -> Option<&mut Drag> {
        match &mut self.kind {
            Kind::Line(d) | Kind::Arrow(d) | Kind::Rect(d) | Kind::Solid(d) | Kind::Ellipse(d) => Some(d),
            Kind::Pixelate(d, _) | Kind::Blur(d, _) | Kind::Spotlight(d) | Kind::Magnify(d) => Some(d),
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
            Kind::Stroke(_) | Kind::Counter { .. } | Kind::Removed | Kind::Step(_) => true,
            Kind::Text(t) => !t.edit.text.trim().is_empty() || (t.original.is_some() && self.text_changed()),
            _ => self.drag().is_some_and(Drag::is_valid),
        }
    }

    /// A text with nothing (but spaces) in it.
    pub fn is_blank_text(&self) -> bool {
        matches!(&self.kind, Kind::Text(t) if t.edit.text.trim().is_empty())
    }

    /// For an edited copy of a text: is it any different from the original?
    pub fn text_changed(&self) -> bool {
        let Kind::Text(t) = &self.kind else { return true };
        match &t.original {
            None => true,
            Some((text, pos, color, size)) => (&t.edit.text, t.pos, self.color, self.size) != (text, *pos, *color, *size),
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
            // (it shades everything else)
            Kind::Spotlight(_) => Rect::new(-1e6, -1e6, 2e6, 2e6),
            Kind::Magnify(d) => d.rect().adjusted(-MAGNIFIER_FRAME - 8.0, -MAGNIFIER_FRAME - 6.0, MAGNIFIER_FRAME + 8.0, MAGNIFIER_FRAME + 10.0),
            Kind::Text(t) => {
                let b = label_box(t, self.size).adjusted(-TEXT_PAD - 3.0, -TEXT_PAD - 3.0, TEXT_PAD + 3.0, TEXT_PAD + 3.0);
                match t.style {
                    TextStyle::Callout(tip) => b.united(&Rect::new(tip.x - 3.0, tip.y - 3.0, 6.0, 6.0)),
                    _ => b,
                }
            }
            Kind::Counter { pos, .. } => {
                let r = COUNTER_RADII[self.size.min(2)] + 2.0;
                Rect::new(pos.x - r, pos.y - r, 2.0 * r, 2.0 * r)
            }
            Kind::Removed | Kind::Step(_) => Rect::default(),
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
            Kind::Spotlight(d) => paint_spotlights(p, &[d.rect()]),
            Kind::Magnify(d) => paint_magnifier(p, d.rect(), base, dpr, self.size, self.color),
            Kind::Text(t) => paint_text(p, t, self.color, self.size),
            Kind::Counter { pos, number } => {
                let r = COUNTER_RADII[self.size.min(2)];
                let bx = Rect::new(pos.x - r, pos.y - r, 2.0 * r, 2.0 * r);
                p.fill_ellipse(bx, self.color);
                let fg = if self.color.is_light() { OUTLINE_DARK } else { OUTLINE_LIGHT };
                p.draw_text(bx, Align::Center, &number.to_string(), text::font((r * 1.05).round(), BOLD), fg);
            }
            Kind::Removed | Kind::Step(_) => {}
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

/// Paint `shapes` in order, the spotlights together: where the first is
/// in the stack, everything but all of their areas is shaded once (so
/// they light up alike, overlapping or not); drawings above it aren't.
pub fn paint_all<'a>(p: &mut Painter<'_>, shapes: impl IntoIterator<Item = &'a Shape>, base: &Image, dpr: f32) {
    let shapes: Vec<&Shape> = shapes.into_iter().collect();
    let lit: Vec<Rect> = shapes.iter().filter_map(|s| s.spotlight()).collect();
    let mut shaded = false;
    for s in shapes {
        if matches!(s.kind, Kind::Spotlight(_)) {
            if !shaded && !lit.is_empty() {
                paint_spotlights(p, &lit);
                shaded = true;
            }
            continue;
        }
        s.paint(p, base, dpr);
    }
}

/// How dark a spotlight shades what isn't in it.
const SPOT_SHADE: Color = Color::rgba(0, 0, 0, 150);
const SPOT_RADIUS: f32 = 8.0;

/// Shade all of what `p` draws on but the areas `lit` (logical).
fn paint_spotlights(p: &mut Painter<'_>, lit: &[Rect]) {
    let (w, h) = (p.width(), p.height());
    let Some(mut mask) = tiny_skia::Mask::new(w, h) else { return };
    for r in lit {
        if let Some(path) = rounded_rect_path(r.normalized(), SPOT_RADIUS) {
            mask.fill_path(&path, tiny_skia::FillRule::Winding, true, p.transform());
        }
    }
    // (shaded where nothing is lit)
    for v in mask.data_mut() {
        *v = 255 - *v;
    }
    let mut paint = tiny_skia::Paint::default();
    paint.set_color(crate::render::paint::sk(SPOT_SHADE.with_alpha_f(SPOT_SHADE.a as f32 / 255.0 * p.opacity())));
    if let Some(all) = tiny_skia::Rect::from_xywh(0.0, 0.0, w as f32, h as f32) {
        p.pm.fill_rect(all, &paint, tiny_skia::Transform::identity(), Some(&mask));
    }
}

/// The magnifier's frame (logical pixels).
const MAGNIFIER_FRAME: f32 = 3.0;
/// How much it enlarges, by size.
const MAGNIFY: [f32; 3] = [2.0, 3.0, 4.0];

/// What's under the middle of `r` in `base`, enlarged to fill it, framed
/// in `color` (whole pixels: crisp, as a screen's are).
fn paint_magnifier(p: &mut Painter<'_>, r: Rect, base: &Image, dpr: f32, size: usize, color: Color) {
    let r = r.normalized();
    if r.w < 4.0 || r.h < 4.0 {
        return;
    }
    let k = MAGNIFY[size.min(2)];
    let (bw, bh) = (base.width() as f32, base.height() as f32);
    let (sw, sh) = ((r.w * dpr / k).min(bw), (r.h * dpr / k).min(bh));
    let c = r.center();
    let src = Rect::new((c.x * dpr - sw / 2.0).clamp(0.0, bw - sw), (c.y * dpr - sh / 2.0).clamp(0.0, bh - sh), sw, sh);
    // (a soft shadow under it, lifted off the picture)
    for i in 1..=4 {
        let d = i as f32 * 1.5;
        p.fill_rounded_rect(r.adjusted(-d, -d + 3.0, d, d + 3.0), 2.0 + d, Color::rgba(0, 0, 0, 14));
    }
    p.draw_image(base, r, src, false);
    p.stroke_rect(r, Pen::new(color, MAGNIFIER_FRAME));
}

/// Where a callout's text goes, pointing at `tip`, dragged to `to`: there
/// (a little above, the pointer at its middle), or up and to the right of
/// what it points at when it wasn't dragged.
pub fn callout_text_at(tip: Point, to: Point) -> Point {
    if to.distance(tip) < 12.0 { tip + Point::new(28.0, -64.0) } else { to - Point::new(0.0, 14.0) }
}

/// A text's box: its lines, and on a label (or a callout) the label round them.
fn label_box(t: &TextShape, size: usize) -> Rect {
    let b = text_bounds(t, size);
    match t.style {
        TextStyle::Plain => b,
        _ => {
            let pad = label_pad(size);
            b.adjusted(-pad, -pad * 0.55, pad, pad * 0.55)
        }
    }
}

fn label_pad(size: usize) -> f32 {
    (TEXT_SIZES_PX[size.min(2)] * 0.5).round()
}

/// A label (and a callout's tail) behind a text, in its colour.
fn paint_label(p: &mut Painter<'_>, t: &TextShape, color: Color, size: usize) {
    let bx = label_box(t, size);
    if let TextStyle::Callout(tip) = t.style {
        // A tail from the middle of the label to the tip, under the label:
        // only what's outside it shows.
        let c = bx.center();
        let d = tip - c;
        let len = d.length();
        if len > 1.0 {
            let half = (bx.h.min(bx.w) * 0.32).max(4.0);
            let n = Point::new(-d.y / len, d.x / len) * half;
            if let Some(path) = polygon(&[c + n, tip, c - n]) {
                p.fill_path(&path, color);
            }
        }
    }
    p.fill_rounded_rect(bx, (bx.h * 0.28).min(12.0), color);
}

/// Pixelate (`blur` false) or blur what's under `r` in `base`.
fn paint_cover(p: &mut Painter<'_>, r: Rect, base: &Image, dpr: f32, size: usize, cache: &CoverCache, blur: bool) {
    if r.w < 2.0 || r.h < 2.0 {
        return;
    }
    let src =
        IRect::new((r.x * dpr).round() as i32, (r.y * dpr).round() as i32, (r.w * dpr).round() as i32, (r.h * dpr).round() as i32).intersected(&base.rect());
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

// Pixelate and blur leave nothing exact behind to match: tools that read
// pixelated text back render candidates in the same font, pixelate them
// alike and pick the closest, which works because each block is exactly
// the average of its own pixels (and a blur is a fixed sum of them).
// Here each block (and each pixel of the blur's small picture) is the
// average of a window shifted a little at random, so where it was taken
// from is unknown just where the glyphs' edges are; it blends into its
// neighbours; and it gets a little random grain, the same on red, green
// and blue (so it reads as texture, not coloured specks). The shifts and
// the grain are what an attacker can't model. A filled rectangle is still
// the one sure way.

/// A pixelated block's side (logical pixels), by size.
const PIXEL_BLOCK: [f32; 3] = [10.0, 15.0, 22.0];

/// Blocks of the colour of what's under them and round them, grained.
pub fn pixelate_cover(base: &Image, src: IRect, dpr: f32, size: usize) -> Option<Image> {
    pixelate(base, src, dpr, size, Some(random_seed()))
}

/// Blurred past reading, grained.
pub fn blur_cover(base: &Image, src: IRect, dpr: f32, size: usize) -> Option<Image> {
    blur(base, src, dpr, size, Some(random_seed()))
}

/// As `pixelate_cover`, its grain from `seed` (None: none, as an attacker
/// would have to model it).
pub fn pixelate(base: &Image, src: IRect, dpr: f32, size: usize, seed: Option<u64>) -> Option<Image> {
    let block = (PIXEL_BLOCK[size.min(2)] * dpr).round().max(4.0);
    let crop = base.crop(src).ok()?;
    let nx = ((src.w as f32 / block).round() as u32).max(1);
    let ny = ((src.h as f32 / block).round() as u32).max(1);
    let mut rng = seed.map(Grain::new);
    let (small, detail) = averages(&crop, nx, ny, rng.as_mut())?;
    let mut small = tent(&small);
    if let Some(rng) = &mut rng {
        rng.add(&mut small, &tent_values(&detail, nx as usize), PIXEL_GRAIN);
    }
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

/// As `blur_cover`, its grain from `seed` (None: none).
pub fn blur(base: &Image, src: IRect, dpr: f32, size: usize, seed: Option<u64>) -> Option<Image> {
    let k = (BLUR_STRENGTH[size.min(2)] * dpr).round().max(2.0) as i32;
    // Blend in what's beside it, so the edges don't show.
    let around = src.adjusted(-2 * k, -2 * k, 2 * k, 2 * k).intersected(&base.rect());
    let crop = base.crop(around).ok()?;
    let sw = (around.w / k).max(1) as u32;
    let sh = (around.h / k).max(1) as u32;
    let mut rng = seed.map(Grain::new);
    let (small, detail) = averages(&crop, sw, sh, rng.as_mut())?;
    let mut small = tent(&tent(&small));
    if let Some(rng) = &mut rng {
        let detail = tent_values(&tent_values(&detail, sw as usize), sw as usize);
        rng.add(&mut small, &detail, BLUR_GRAIN);
    }
    let big = small.scaled_bilinear(around.w as u32, around.h as u32).ok()?;
    big.crop(src.translated(-around.x, -around.y)).ok()
}

/// How much grain (in levels of 255, either way) pixelate's blocks and the
/// blur's small picture get.
const PIXEL_GRAIN: f32 = 14.0;
const BLUR_GRAIN: f32 = 14.0;

/// A seed for the grain, from the kernel's generator.
fn random_seed() -> u64 {
    u64::from_str_radix(&crate::util::random_hex(8), 16).unwrap_or(0x9E37_79B9_7F4A_7C15)
}

/// How far (in blocks, either way) a block's window may be shifted.
const SHIFT: f32 = 0.33;

/// How much detail (the spread of a window's brightness, in levels) gets
/// all of the grain: flat colour gets none (it hides nothing), text gets
/// it all.
const DETAIL: f32 = 24.0;

/// `img` (opaque) as `nx` x `ny` blocks, each the average of the pixels in
/// its share of it; with `rng`, each window shifted at random (up to SHIFT
/// of a block, staying in the picture). And for each block, how much
/// detail is in it (0 to 1).
fn averages(img: &Image, nx: u32, ny: u32, mut rng: Option<&mut Grain>) -> Option<(Image, Vec<f32>)> {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let span = |i: i64, n: u32, len: i64| (i * len / n as i64, ((i + 1) * len / n as i64).max(i * len / n as i64 + 1).min(len));
    let mut out = Image::new(nx, ny).ok()?;
    let mut detail = Vec::with_capacity(nx as usize * ny as usize);
    for j in 0..ny as i64 {
        for i in 0..nx as i64 {
            let ((mut x0, mut x1), (mut y0, mut y1)) = (span(i, nx, w), span(j, ny, h));
            if let Some(rng) = rng.as_deref_mut() {
                (x0, x1) = shifted(x0, x1, w, rng.next());
                (y0, y1) = shifted(y0, y1, h, rng.next());
            }
            let (mut sum, mut luma, mut luma2) = ([0u64; 3], 0u64, 0u64);
            for y in y0..y1 {
                for px in img.row(y as u32)[x0 as usize * 4..x1 as usize * 4].as_chunks::<4>().0 {
                    for c in 0..3 {
                        sum[c] += px[c] as u64;
                    }
                    let l = (px[0] as u64 * 19 + px[1] as u64 * 183 + px[2] as u64 * 54) >> 8;
                    luma += l;
                    luma2 += l * l;
                }
            }
            let n = ((x1 - x0) * (y1 - y0)).max(1) as u64;
            let row = out.row_mut(j as u32);
            let at = i as usize * 4;
            for c in 0..3 {
                row[at + c] = ((sum[c] + n / 2) / n) as u8;
            }
            row[at + 3] = 255;
            let mean = luma as f32 / n as f32;
            let spread = (luma2 as f32 / n as f32 - mean * mean).max(0.0).sqrt();
            detail.push((spread / DETAIL).min(1.0));
        }
    }
    Some((out, detail))
}

/// `values` (rows `w` long) each blended into its neighbours, as `tent`.
fn tent_values(values: &[f32], w: usize) -> Vec<f32> {
    let h = values.len() / w.max(1);
    let at = |x: usize, y: usize| values[y * w + x];
    let mut out = Vec::with_capacity(values.len());
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
            let across = |y| at(x0, y) + 2.0 * at(x, y) + at(x1, y);
            out.push((across(y0) + 2.0 * across(y) + across(y1)) / 16.0);
        }
    }
    out
}

/// [lo, hi) moved by up to SHIFT of its length (`r` from 0 to 1: as far as
/// it goes one way to the other), staying in [0, max).
fn shifted(lo: i64, hi: i64, max: i64, r: f32) -> (i64, i64) {
    let d = ((r * 2.0 - 1.0) * SHIFT * (hi - lo) as f32).round() as i64;
    let d = d.clamp(-lo, max - hi);
    (lo + d, hi + d)
}

/// `img` (opaque) with each pixel blended into its neighbours: a 3 x 3
/// tent (1 2 1 across and down), the edges repeated.
fn tent(img: &Image) -> Image {
    let (w, h) = (img.width() as usize, img.height() as usize);
    let mut out = img.clone();
    let px = |x: usize, y: usize, c: usize| img.row(y as u32)[x * 4 + c] as u32;
    for y in 0..h {
        let (y0, y1) = (y.saturating_sub(1), (y + 1).min(h - 1));
        let row = out.row_mut(y as u32);
        for x in 0..w {
            let (x0, x1) = (x.saturating_sub(1), (x + 1).min(w - 1));
            for c in 0..3 {
                let across = |y| px(x0, y, c) + 2 * px(x, y, c) + px(x1, y, c);
                row[x * 4 + c] = ((across(y0) + 2 * across(y) + across(y1) + 8) / 16) as u8;
            }
        }
    }
    out
}

/// The random shifts and grain of a cover (xorshift64*: plenty for these;
/// it's seeded from the kernel's generator).
struct Grain(u64);

impl Grain {
    fn new(seed: u64) -> Grain {
        Grain(seed | 1)
    }

    /// The next number, from 0 to 1.
    fn next(&mut self) -> f32 {
        let s = &mut self.0;
        *s ^= *s >> 12;
        *s ^= *s << 25;
        *s ^= *s >> 27;
        (s.wrapping_mul(0x2545_F491_4F6C_DD1D) >> 40) as f32 / (1u64 << 24) as f32
    }

    /// Add grain to each pixel of `img` (the same on its three colours):
    /// up to `amount` levels where `detail` (one for each pixel, 0 to 1)
    /// says there's all of it, none where there's none.
    fn add(&mut self, img: &mut Image, detail: &[f32], amount: f32) {
        for y in 0..img.height() {
            let w = img.width() as usize;
            for x in 0..w {
                let k = detail.get(y as usize * w + x).copied().unwrap_or(1.0);
                let n = ((self.next() * 2.0 - 1.0) * amount * k).round() as i32;
                let row = img.row_mut(y);
                for c in 0..3 {
                    row[x * 4 + c] = (row[x * 4 + c] as i32 + n).clamp(0, 255) as u8;
                }
            }
        }
    }
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
        kind: Kind::Text(TextShape { pos: t.pos, edit, editing: true, style: t.style, original: Some((t.edit.text.clone(), t.pos, s.color, s.size)) }),
        color: s.color,
        size: s.size,
        hidden: false,
        replaces: Some(index),
        order: 0,
    })
}

/// The number for a new counter: one more than the highest shown.
pub fn next_number<'a>(drawings: impl IntoIterator<Item = &'a Shape>) -> u32 {
    1 + drawings
        .into_iter()
        .filter(|s| !s.hidden)
        .filter_map(|s| match s.kind {
            Kind::Counter { number, .. } => Some(number),
            _ => None,
        })
        .max()
        .unwrap_or(0)
}

fn stroke_reach(s: &Stroke, width: f32) -> f32 {
    if s.marker { (width * 3.0 + 10.0) / 2.0 } else { width / 2.0 }
}

/// How far `pos` is from the segment a–b.
fn to_segment(pos: Point, a: Point, b: Point) -> f32 {
    let d = b - a;
    let len2 = d.x * d.x + d.y * d.y;
    let t = if len2 == 0.0 { 0.0 } else { (((pos.x - a.x) * d.x + (pos.y - a.y) * d.y) / len2).clamp(0.0, 1.0) };
    pos.distance(a + d * t)
}

fn on_outline(pos: Point, r: Rect, reach: f32) -> bool {
    let outer = r.adjusted(-reach, -reach, reach, reach);
    let inner = r.adjusted(reach, reach, -reach, -reach);
    outer.contains(pos) && !(inner.w > 0.0 && inner.h > 0.0 && inner.contains(pos))
}

fn paint_text(p: &mut Painter<'_>, t: &TextShape, color: Color, size: usize) {
    let f = text_font(size);
    let m = text::metrics(f);
    let lines = t.edit.lines();
    let palette = theme::c();
    // On a label: the label in its colour, the text white or black on it.
    let color = if t.style == TextStyle::Plain {
        color
    } else {
        paint_label(p, t, color, size);
        if color.is_light() { OUTLINE_DARK } else { OUTLINE_LIGHT }
    };
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
                let x1 = t.edit.x_of(r, if r == rb { cb } else { line.len() }, &measure) + if r == rb { 0.0 } else { m.avg_char_width / 2.0 };
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
    if t.editing && super::caret::shown() {
        p.fill_rect(t.caret_rect(size), palette.accent);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn covers_leave_flat_colour_clean_and_grain_detail() {
        let flat = Image::filled(120, 80, 0xFF33_6699).unwrap();
        let r = IRect::new(0, 0, 120, 80);
        for img in [pixelate(&flat, r, 1.0, 1, Some(7)).unwrap(), blur(&flat, r, 1.0, 1, Some(7)).unwrap()] {
            assert!(img.data().as_chunks::<4>().0.iter().all(|px| *px == [0x99, 0x66, 0x33, 0xFF]), "grain on flat colour");
        }
        // Text: grained, the same for the same seed, not for another.
        crate::render::text::warm_up();
        let text = pin("7351", 1.0);
        let r = text.rect();
        let (a, b, c) =
            (pixelate(&text, r, 1.0, 0, Some(1)).unwrap(), pixelate(&text, r, 1.0, 0, Some(1)).unwrap(), pixelate(&text, r, 1.0, 0, Some(2)).unwrap());
        assert!(a.data() == b.data() && a.data() != c.data());
        let model = pixelate(&text, r, 1.0, 0, None).unwrap();
        assert!(model.data() != a.data());
    }

    /// A PIN in the app's font, dark on white.
    fn pin(digits: &str, dpr: f32) -> Image {
        let mut img = Image::filled((110.0 * dpr) as u32, (32.0 * dpr) as u32, 0xFFFF_FFFF).unwrap();
        {
            let mut p = Painter::new(img.pixmap_mut(), dpr);
            p.draw_text(
                Rect::new(6.0, 0.0, 100.0, 32.0),
                crate::render::paint::Align::Left,
                &format!("PIN {digits}"),
                text::font(16.0, text::NORMAL),
                Color::rgb(0x202020),
            );
        }
        img
    }

    #[test]
    fn covers_cant_be_matched_back() {
        // An attacker who knows the font and the algorithm (but not the
        // grain) covers each candidate alike and picks the closest: the
        // PIN mustn't come out on top among its neighbours. (Covers that
        // were exact averages put it first every time.)
        crate::render::text::warm_up();
        let distance = |a: &Image, b: &Image| a.data().iter().zip(b.data()).map(|(x, y)| (*x as i64 - *y as i64).pow(2)).sum::<i64>();
        for blurred in [false, true] {
            let cover = |img: &Image, seed| if blurred { blur(img, img.rect(), 1.0, 1, seed) } else { pixelate(img, img.rect(), 1.0, 1, seed) }.unwrap();
            let models: Vec<(String, Image)> = (7300..7500).map(|n| (n.to_string(), cover(&pin(&n.to_string(), 1.0), None))).collect();
            for seed in 1..=3 {
                let target = cover(&pin("7351", 1.0), Some(seed));
                let best = models.iter().min_by_key(|(_, m)| distance(m, &target)).map(|(n, _)| n.as_str());
                assert_ne!(best, Some("7351"), "matched back (blur: {blurred}, seed {seed})");
            }
        }
    }

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
        let mut t = Shape::text(Point::new(0.0, 0.0), Color::BLACK, 1, TextStyle::Plain);
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

    fn dragged(tool: Tool, a: Point, b: Point, color: Color) -> Shape {
        let mut s = Shape::create(tool, a, color, 1).unwrap();
        s.extend(b, Mods::default());
        s
    }

    #[test]
    fn spotlights_light_up_together_and_drawings_above_them_stay_bright() {
        let base = Image::filled(100, 100, 0xFFC0C0C0).unwrap();
        let red = Color::rgb(0xFF0000);
        let shapes = [
            dragged(Tool::Spotlight, Point::new(10.0, 10.0), Point::new(40.0, 40.0), red),
            dragged(Tool::Spotlight, Point::new(30.0, 30.0), Point::new(70.0, 60.0), red),
            dragged(Tool::Solid, Point::new(80.0, 80.0), Point::new(95.0, 95.0), red),
        ];
        let mut out = base.clone();
        {
            let mut p = Painter::new(out.pixmap_mut(), 1.0);
            paint_all(&mut p, &shapes, &base, 1.0);
        }
        // In either (their overlap too): as it was. Out of both: shaded, once.
        for (x, y) in [(20, 20), (35, 35), (60, 50)] {
            assert_eq!(out.pixel(x, y), 0xFFC0C0C0, "{x},{y} lit");
        }
        let shaded = out.pixel(5, 90);
        assert!(shaded >> 16 & 0xFF < 0x60, "{shaded:08X}");
        assert_eq!(out.pixel(50, 20), shaded);
        // (the box drawn after them isn't shaded)
        assert_eq!(out.pixel(88, 88), 0xFFFF0000);
        // Hidden, it shades nothing.
        let mut hidden = shapes[..1].to_vec();
        hidden[0].hidden = true;
        assert!(hidden[0].spotlight().is_none());
    }

    #[test]
    fn the_magnifier_enlarges_what_is_under_its_middle() {
        let mut base = Image::filled(100, 100, 0xFF000000).unwrap();
        // (a white dot 2 pixels across in the middle)
        for (x, y) in [(49, 49), (50, 49), (49, 50), (50, 50)] {
            base.set_pixel(x, y, 0xFFFFFFFF);
        }
        let m = dragged(Tool::Magnify, Point::new(30.0, 30.0), Point::new(70.0, 70.0), Color::rgb(0x00FF00));
        let mut out = base.clone();
        {
            let mut p = Painter::new(out.pixmap_mut(), 1.0);
            m.paint(&mut p, &base, 1.0);
        }
        // At 3x the dot is 6 across: white well off the middle, where it wasn't.
        assert_eq!(out.pixel(47, 47), 0xFFFFFFFF);
        assert_eq!(out.pixel(52, 52), 0xFFFFFFFF);
        assert_eq!(out.pixel(45, 45), 0xFF000000);
        // Framed in its colour.
        assert_eq!(out.pixel(30, 50), 0xFF00FF00);
    }

    #[test]
    fn a_callout_points_where_it_was_dragged_from_and_moves_as_one() {
        crate::render::text::warm_up();
        let tip = Point::new(20.0, 120.0);
        let to = Point::new(120.0, 40.0);
        let mut c = Shape::text(callout_text_at(tip, to), Color::rgb(0x2050FF), 1, TextStyle::Callout(tip));
        c.as_text_mut().unwrap().edit.insert("look");
        c.as_text_mut().unwrap().editing = false;
        let b = c.bounds();
        assert!(b.contains(tip) && b.contains(to), "{b:?}");
        // Moved: the tip goes with it.
        let m = c.moved(Point::new(10.0, 5.0));
        assert_eq!(m.as_text().unwrap().style, TextStyle::Callout(Point::new(30.0, 125.0)));
        // Painted: the label in its colour, its tail reaching the tip, the text white on it.
        let base = Image::filled(200, 160, 0xFF000000).unwrap();
        let mut out = base.clone();
        {
            let mut p = Painter::new(out.pixmap_mut(), 1.0);
            c.paint(&mut p, &base, 1.0);
        }
        let near_tip = tip + (label_box(c.as_text().unwrap(), 1).center() - tip) * 0.1;
        assert_eq!(out.pixel(near_tip.x as u32, near_tip.y as u32), 0xFF2050FF);
        let bx = label_box(c.as_text().unwrap(), 1);
        assert_eq!(out.pixel(bx.x as u32 + 3, bx.center().y as u32), 0xFF2050FF);
        // Not dragged: up and to the right of what it points at.
        assert_eq!(callout_text_at(tip, tip + Point::new(3.0, 2.0)), tip + Point::new(28.0, -64.0));
    }
}
