//! The select tool: pick up a drawing again to move it, resize it (lines by
//! their ends), restyle or delete it. Shared by the capture overlay and the
//! annotation editor, each a `Drawings` host.
//!
//! Changes never edit a drawing: a changed copy replaces it (Shape::
//! replaces), so undo and redo work as for anything drawn.

use crate::geom::{Point, Rect};
use crate::keys::KeyPress;
use crate::keyseq::{ALT, CTRL, SHIFT, key};
use crate::render::Painter;
use crate::render::paint::Pen;
use crate::render::theme::{self, Color};
use crate::ui::session::CursorShape;
use crate::ui::shapes::{Kind, Shape};

/// A handle's size on the screen.
pub const HANDLE: f32 = 9.0;
/// The arrow keys' steps, without and with Shift.
const NUDGE: f32 = 1.0;
const BIG_NUDGE: f32 = 10.0;

/// The drawings a selection looks at: bottom to top.
pub trait Drawings {
    fn count(&self) -> usize;
    fn get(&self, i: usize) -> &Shape;
}

/// ... and changes, with a way to add one (an undo step).
pub trait DrawingsMut: Drawings {
    fn get_mut(&mut self, i: usize) -> &mut Shape;
    /// Add `shape` (a changed copy: its `replaces` says of which).
    fn commit(&mut self, shape: Shape);
}

impl Drawings for Vec<Shape> {
    fn count(&self) -> usize {
        self.len()
    }
    fn get(&self, i: usize) -> &Shape {
        &self[i]
    }
}

impl DrawingsMut for Vec<Shape> {
    fn get_mut(&mut self, i: usize) -> &mut Shape {
        &mut self[i]
    }
    fn commit(&mut self, shape: Shape) {
        self.push(shape);
    }
}

/// What a press with the select tool took hold of.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Grip {
    Move,
    /// a line's start (0) or end (1)
    End(usize),
    /// a handle of the box: the edges it moves
    Edges {
        left: bool,
        right: bool,
        top: bool,
        bottom: bool,
    },
}

impl Grip {
    pub fn cursor(self) -> CursorShape {
        match self {
            Grip::Move => CursorShape::Move,
            Grip::End(_) => CursorShape::Crosshair,
            Grip::Edges { left, right, top, bottom } => match (left || right, top || bottom) {
                (true, true) if (left && top) || (right && bottom) => CursorShape::ResizeNWSE,
                (true, true) => CursorShape::ResizeNESW,
                (true, false) => CursorShape::ResizeEW,
                _ => CursorShape::ResizeNS,
            },
        }
    }
}

/// A drag under way.
#[derive(Clone, Debug)]
struct Held {
    grip: Grip,
    start: Point,
    /// where the pointer is now
    at: Point,
    /// the drawing's index, and its box then
    index: usize,
    geometry: Rect,
    /// the changed copy, shown meanwhile
    preview: Option<Shape>,
}

#[derive(Clone, Debug, Default)]
pub struct Selection {
    index: Option<usize>,
    held: Option<Held>,
}

impl Selection {
    // -- what is selected ----------------------------------------------------

    /// The selected drawing's index, while it's still there to change (undo
    /// may have taken it away).
    pub fn index(&mut self, d: &dyn Drawings) -> Option<usize> {
        self.index = self.current(d);
        self.index
    }

    /// The same, without forgetting one that's gone.
    pub fn current(&self, d: &dyn Drawings) -> Option<usize> {
        if self.held.is_some() {
            return self.index;
        }
        self.index.filter(|&i| i < d.count() && !d.get(i).hidden && !d.get(i).is_removed())
    }

    pub fn selected<'a>(&self, d: &'a dyn Drawings) -> Option<&'a Shape> {
        self.current(d).map(|i| d.get(i))
    }

    pub fn select(&mut self, index: Option<usize>) {
        self.index = index;
    }

    /// Deselect; true if something was selected (Esc does that first).
    pub fn clear(&mut self, d: &mut dyn DrawingsMut) -> bool {
        let had = self.index(d).is_some() || self.held.is_some();
        self.cancel(d);
        self.index = None;
        had
    }

    pub fn dragging(&self) -> bool {
        self.held.is_some()
    }

    /// The drawings at `pos`, topmost first (`scale`: screen pixels per unit).
    pub fn hits(d: &dyn Drawings, pos: Point, scale: f32) -> Vec<usize> {
        let slack = 5.0 / scale;
        (0..d.count()).rev().filter(|&i| !d.get(i).hidden && d.get(i).contains(pos, slack)).collect()
    }

    // -- handles ---------------------------------------------------------------

    fn handles(shape: &Shape, scale: f32) -> Vec<(Grip, Point)> {
        if shape.has_ends()
            && let Some(dr) = shape.drag()
        {
            return vec![(Grip::End(0), dr.start), (Grip::End(1), dr.end)];
        }
        if !shape.resizable() {
            return Vec::new();
        }
        let r = Self::frame(shape, scale);
        let xs = [(Some(false), r.left()), (None, r.center().x), (Some(true), r.right())];
        let ys = [(Some(false), r.top()), (None, r.center().y), (Some(true), r.bottom())];
        let mut out = Vec::with_capacity(8);
        for (yk, y) in ys {
            for (xk, x) in xs {
                if (xk, yk) != (None, None) {
                    let grip = Grip::Edges { left: xk == Some(false), right: xk == Some(true), top: yk == Some(false), bottom: yk == Some(true) };
                    out.push((grip, Point::new(x, y)));
                }
            }
        }
        out
    }

    /// The dashed box round a selected drawing: its geometry with room for
    /// its stroke.
    fn frame(shape: &Shape, scale: f32) -> Rect {
        let pad = shape.reach() + 4.0 / scale;
        shape.geometry().adjusted(-pad, -pad, pad, pad)
    }

    /// The selected drawing's handle at `pos`, if any: the nearest. On a
    /// drawing too small for its handles' reach to leave room to take it
    /// by (a finger can't see which it's on), only a handle itself counts
    /// inside it; elsewhere in it, it's moved.
    pub fn handle_at(&self, d: &dyn Drawings, pos: Point, scale: f32) -> Option<Grip> {
        let shape = self.selected(d)?;
        let grab = (HANDLE / 2.0 + 4.0) / scale;
        let frame = Self::frame(shape, scale);
        let crowded = frame.w < 4.0 * grab || frame.h < 4.0 * grab;
        let reach = if crowded && frame.contains(pos) && shape.contains(pos, 0.0) { HANDLE / 2.0 / scale } else { grab };
        Self::handles(shape, scale)
            .into_iter()
            .map(|(g, at)| (g, (pos.x - at.x).abs().max((pos.y - at.y).abs())))
            .filter(|(_, dist)| *dist <= reach)
            .min_by(|a, b| a.1.total_cmp(&b.1))
            .map(|(g, _)| g)
    }

    /// Is `pos` on the selected drawing (a handle, or the drawing itself)?
    pub fn holds(&self, d: &dyn Drawings, pos: Point, scale: f32) -> bool {
        self.handle_at(d, pos, scale).is_some() || self.current(d).is_some_and(|i| Self::hits(d, pos, scale).contains(&i))
    }

    /// The pointer's shape over `pos` with the select tool.
    pub fn cursor(&self, d: &dyn Drawings, pos: Point, scale: f32) -> CursorShape {
        if let Some(h) = &self.held {
            return h.grip.cursor();
        }
        if let Some(g) = self.handle_at(d, pos, scale) {
            return g.cursor();
        }
        if Self::hits(d, pos, scale).is_empty() { CursorShape::Arrow } else { CursorShape::Move }
    }

    // -- the pointer -------------------------------------------------------------

    /// A press with the select tool: on a handle, start resizing; on a
    /// drawing, select it (Alt: the next one under it) and start moving it;
    /// elsewhere, deselect. True if it took hold of something.
    pub fn press(&mut self, d: &mut dyn DrawingsMut, pos: Point, mods: i32, scale: f32) -> bool {
        let grip = match self.handle_at(d, pos, scale) {
            Some(g) => g,
            None => {
                let hits = Self::hits(d, pos, scale);
                if hits.is_empty() {
                    self.clear(d);
                    return false;
                }
                let current = self.index(d);
                match current.and_then(|c| hits.iter().position(|&h| h == c)) {
                    Some(at) if mods & ALT != 0 => self.index = Some(hits[(at + 1) % hits.len()]),
                    Some(_) => {}
                    None => self.index = Some(hits[0]),
                }
                Grip::Move
            }
        };
        let Some(index) = self.index else { return false };
        self.held = Some(Held { grip, start: pos, at: pos, index, geometry: d.get(index).geometry(), preview: None });
        true
    }

    /// The pointer moved with the button held; true if a drag is on.
    pub fn motion(&mut self, d: &mut dyn DrawingsMut, pos: Point, mods: i32) -> bool {
        let Some(h) = &mut self.held else { return false };
        h.at = pos;
        let keep_ratio = mods & SHIFT != 0;
        let center = mods & CTRL != 0;
        if h.preview.is_none() && (pos - h.start).manhattan() < 2.0 {
            return true; // (a click, not a drag yet)
        }
        let shape = d.get(h.index);
        let preview = match h.grip {
            Grip::Move => {
                let mut v = pos - h.start;
                if keep_ratio {
                    // Shift: straight across or straight down
                    v = if v.x.abs() >= v.y.abs() { Point::new(v.x, 0.0) } else { Point::new(0.0, v.y) };
                }
                shape.moved(v)
            }
            Grip::End(which) => {
                let Some(dr) = shape.drag() else { return true };
                let other = if which == 0 { dr.end } else { dr.start };
                let mut to = pos;
                if keep_ratio {
                    // Shift: 45° steps, as when drawing it
                    to = other + crate::ui::shapes::snap45(pos - other);
                }
                shape.with_end(which, to)
            }
            Grip::Edges { .. } => {
                let new = resized(h.geometry, h.grip, pos - h.start, keep_ratio, center);
                shape.fitted(h.geometry, new)
            }
        };
        h.preview = Some(preview);
        d.get_mut(h.index).hidden = true; // (the copy shows meanwhile)
        true
    }

    /// Shift or Ctrl pressed or let go mid-drag: the drag as they say now.
    pub fn modifiers(&mut self, d: &mut dyn DrawingsMut, mods: i32) {
        if let Some(h) = &self.held
            && h.preview.is_some()
        {
            let at = h.at;
            self.motion(d, at, mods);
        }
    }

    /// Let go: the changed copy replaces the drawing (one undo step).
    pub fn release(&mut self, d: &mut dyn DrawingsMut) -> bool {
        let Some(h) = self.held.take() else { return false };
        d.get_mut(h.index).hidden = false;
        if let Some(preview) = h.preview {
            self.replace(d, h.index, preview);
        }
        true
    }

    /// Drop a drag under way (Esc).
    pub fn cancel(&mut self, d: &mut dyn DrawingsMut) {
        if let Some(h) = self.held.take()
            && h.index < d.count()
        {
            d.get_mut(h.index).hidden = false;
        }
    }

    fn replace(&mut self, d: &mut dyn DrawingsMut, index: usize, mut new: Shape) {
        d.get_mut(index).hidden = true;
        new.replaces = Some(index);
        let removed = new.is_removed();
        d.commit(new);
        self.index = (!removed).then(|| d.count() - 1);
    }

    // -- keys and the toolbar ------------------------------------------------------

    /// Delete / Backspace removes the selected drawing; the arrow keys nudge
    /// it (Shift: further). True if the key was used.
    pub fn key(&mut self, d: &mut dyn DrawingsMut, k: &KeyPress) -> bool {
        if self.held.is_some() {
            return false;
        }
        let Some(i) = self.index(d) else { return false };
        if k.key == key::DELETE || k.key == key::BACKSPACE {
            let gone = Shape::removed(d.get(i), i);
            self.replace(d, i, gone);
            return true;
        }
        let step = if k.mods & SHIFT != 0 { BIG_NUDGE } else { NUDGE };
        let v = match k.key {
            key::LEFT => Point::new(-step, 0.0),
            key::RIGHT => Point::new(step, 0.0),
            key::UP => Point::new(0.0, -step),
            key::DOWN => Point::new(0.0, step),
            _ => return false,
        };
        let moved = d.get(i).moved(v);
        self.replace(d, i, moved);
        true
    }

    /// The toolbar's colour or size, for the selected drawing. True if there
    /// was one.
    pub fn restyle(&mut self, d: &mut dyn DrawingsMut, color: Option<Color>, size: Option<usize>) -> bool {
        let Some(i) = self.index(d) else { return false };
        let shape = d.get(i);
        // (pixelate and blur have no colour)
        let color = color.filter(|_| !matches!(shape.kind, Kind::Pixelate(..) | Kind::Blur(..)));
        if color.is_none_or(|c| c == shape.color) && size.is_none_or(|s| s == shape.size) {
            return true;
        }
        let changed = shape.restyled(color, size);
        self.replace(d, i, changed);
        true
    }

    // -- painting -----------------------------------------------------------------

    /// The copy being dragged, if any.
    pub fn moving(&self) -> Option<&Shape> {
        self.held.as_ref().and_then(|h| h.preview.as_ref())
    }

    /// The drawing the frame is around: the one selected, as it's being moved.
    pub fn framed(&self, d: &dyn Drawings) -> Option<Shape> {
        match self.moving() {
            Some(s) => Some(s.clone()),
            None => self.selected(d).cloned(),
        }
    }

    /// Where the frame and handles are drawn (logical), for repainting.
    pub fn frame_bounds(&self, d: &dyn Drawings, scale: f32) -> Option<Rect> {
        Some(Self::frame_bounds_of(&self.framed(d)?, scale))
    }

    /// Where the frame and handles around `shape` are drawn.
    pub fn frame_bounds_of(shape: &Shape, scale: f32) -> Rect {
        let h = HANDLE / scale + 2.0;
        let r = if shape.has_ends() { shape.geometry() } else { Self::frame(shape, scale) };
        r.adjusted(-h, -h, h, h)
    }

    /// The selected drawing's dashed frame and its handles. `scale`: screen
    /// pixels per unit of the painter (the editor's zoom).
    pub fn paint_frame(&self, d: &dyn Drawings, p: &mut Painter<'_>, scale: f32) {
        if let Some(shape) = self.framed(d) {
            Self::paint_frame_of(&shape, p, scale);
        }
    }

    /// The same around `shape` (another monitor's drawing, reaching here).
    pub fn paint_frame_of(shape: &Shape, p: &mut Painter<'_>, scale: f32) {
        let c = theme::c();
        let pen = Pen::new(c.accent, 1.5 / scale).dashed();
        match shape.drag().filter(|_| shape.has_ends()) {
            Some(dr) => p.draw_line(dr.start, dr.end, pen),
            None => p.stroke_rect(Self::frame(shape, scale), pen),
        }
        let h = HANDLE / scale;
        for (_, at) in Self::handles(shape, scale) {
            p.draw_rounded_rect(Rect::new(at.x - h / 2.0, at.y - h / 2.0, h, h), 3.0 / scale, Some(c.text), Some(Pen::new(c.accent, 1.5 / scale)));
        }
    }
}

/// `r` with the edges `grip` names moved by `d` (as far as the pointer
/// moved: the handle stays under it). `keep_ratio` (Shift) keeps its shape,
/// `center` (Ctrl) moves the opposite edges the other way.
pub fn resized(r: Rect, grip: Grip, d: Point, keep_ratio: bool, center: bool) -> Rect {
    let Grip::Edges { left: gl, right: gr, top: gt, bottom: gb } = grip else { return r };
    let (mut left, mut top, mut right, mut bottom) = (r.left(), r.top(), r.right(), r.bottom());
    let c = r.center();
    if gl {
        left += d.x;
        if center {
            right = 2.0 * c.x - left;
        }
    }
    if gr {
        right += d.x;
        if center {
            left = 2.0 * c.x - right;
        }
    }
    if gt {
        top += d.y;
        if center {
            bottom = 2.0 * c.y - top;
        }
    }
    if gb {
        bottom += d.y;
        if center {
            top = 2.0 * c.y - bottom;
        }
    }
    if keep_ratio && r.w > 0.0 && r.h > 0.0 {
        let ratio = r.w / r.h;
        let (mut w, mut h) = (right - left, bottom - top);
        let sign = |v: f32| if v >= 0.0 { 1.0 } else { -1.0 };
        let sideways = (gl || gr) && !(gt || gb);
        let upright = (gt || gb) && !(gl || gr);
        if sideways {
            h = w.abs() / ratio * sign(h);
        } else if upright {
            w = h.abs() * ratio * sign(w);
        } else if w.abs() / ratio > h.abs() {
            h = w.abs() / ratio * sign(h);
        } else {
            w = h.abs() * ratio * sign(w);
        }
        // Grow away from the edges that stay (or the middle).
        let ax = if center || upright {
            c.x
        } else if gl {
            r.right()
        } else {
            r.left()
        };
        let ay = if center || sideways {
            c.y
        } else if gt {
            r.bottom()
        } else {
            r.top()
        };
        let fx = if center || upright {
            0.5
        } else if gl {
            1.0
        } else {
            0.0
        };
        let fy = if center || sideways {
            0.5
        } else if gt {
            1.0
        } else {
            0.0
        };
        left = ax - w * fx;
        right = ax + w * (1.0 - fx);
        top = ay - h * fy;
        bottom = ay + h * (1.0 - fy);
    }
    Rect::from_points(Point::new(left, top), Point::new(right, bottom))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::shapes::{Mods, Tool};

    fn rect_shape(r: Rect) -> Shape {
        let mut s = Shape::create(Tool::Rect, r.top_left(), Color::BLACK, 1).unwrap();
        s.extend(Point::new(r.right(), r.bottom()), Mods::default());
        s
    }

    fn press(k: i32, mods: i32) -> KeyPress {
        KeyPress { key: k, mods, text: String::new(), keysym: 0, repeat: false }
    }

    #[test]
    fn resizing() {
        let r = Rect::new(10.0, 10.0, 40.0, 20.0);
        let br = Grip::Edges { left: false, right: true, top: false, bottom: true };
        assert_eq!(resized(r, br, Point::new(10.0, 10.0), false, false), Rect::new(10.0, 10.0, 50.0, 30.0));
        // Ctrl: about the middle.
        assert_eq!(resized(r, br, Point::new(10.0, 10.0), false, true), Rect::new(0.0, 0.0, 60.0, 40.0));
        // Shift: its shape kept (2:1), grown from the top left.
        assert_eq!(resized(r, br, Point::new(40.0, 0.0), true, false), Rect::new(10.0, 10.0, 80.0, 40.0));
        let right = Grip::Edges { left: false, right: true, top: false, bottom: false };
        assert_eq!(resized(r, right, Point::new(40.0, 0.0), true, false), Rect::new(10.0, 0.0, 80.0, 40.0));
    }

    #[test]
    fn select_move_and_undo_steps() {
        let mut d: Vec<Shape> = vec![rect_shape(Rect::new(10.0, 10.0, 40.0, 20.0)), rect_shape(Rect::new(100.0, 100.0, 20.0, 20.0))];
        let mut sel = Selection::default();
        // A click inside an outline misses it; on its line, it's picked.
        assert!(!sel.press(&mut d, Point::new(30.0, 20.0), 0, 1.0));
        assert!(sel.press(&mut d, Point::new(10.0, 20.0), 0, 1.0));
        assert_eq!(sel.index(&d), Some(0));
        sel.motion(&mut d, Point::new(20.0, 25.0), 0);
        assert!(d[0].hidden && sel.moving().is_some());
        sel.release(&mut d);
        assert_eq!(d.len(), 3);
        assert!(d[0].hidden && d[2].replaces == Some(0));
        assert_eq!(d[2].geometry(), Rect::new(20.0, 15.0, 40.0, 20.0));
        assert_eq!(sel.index(&d), Some(2));
        // Nudged, then deleted: each a copy in place of the last.
        assert!(sel.key(&mut d, &press(key::RIGHT, SHIFT)));
        assert_eq!(d[3].geometry().x, 30.0);
        assert!(sel.key(&mut d, &press(key::DELETE, 0)));
        assert!(d[4].is_removed() && d[3].hidden && sel.index(&d).is_none());
        // A resize by the bottom right handle (just outside the box's line).
        sel.press(&mut d, Point::new(100.0, 110.0), 0, 1.0);
        let corner = Selection::frame(&d[1], 1.0);
        sel.release(&mut d);
        sel.press(&mut d, Point::new(corner.right(), corner.bottom()), 0, 1.0);
        sel.motion(&mut d, Point::new(corner.right() + 20.0, corner.bottom() + 10.0), 0);
        sel.release(&mut d);
        assert_eq!(d.last().unwrap().geometry(), Rect::new(100.0, 100.0, 40.0, 30.0));
        // Restyled: the colour and size of a copy.
        assert!(sel.restyle(&mut d, Some(Color::rgb(0xFF0000)), Some(2)));
        let last = d.last().unwrap();
        assert_eq!((last.color, last.size), (Color::rgb(0xFF0000), 2));
    }

    #[test]
    fn small_drawings_are_moved_not_resized() {
        // So small its handles' reach covers it: taken by its line, it moves.
        let mut d: Vec<Shape> = vec![rect_shape(Rect::new(10.0, 10.0, 16.0, 16.0))];
        let mut sel = Selection::default();
        sel.press(&mut d, Point::new(10.0, 18.0), 0, 1.0);
        sel.release(&mut d);
        assert_eq!(sel.handle_at(&d, Point::new(10.0, 18.0), 1.0), None);
        sel.press(&mut d, Point::new(10.0, 18.0), 0, 1.0);
        sel.motion(&mut d, Point::new(30.0, 28.0), 0);
        sel.release(&mut d);
        assert_eq!(d.last().unwrap().geometry(), Rect::new(30.0, 20.0, 16.0, 16.0));
        // Its handles themselves still resize it; the nearest one wins.
        let frame = Selection::frame(d.last().unwrap(), 1.0);
        let corner = Point::new(frame.right(), frame.bottom());
        assert_eq!(sel.handle_at(&d, corner, 1.0), Some(Grip::Edges { left: false, right: true, top: false, bottom: true }));
        // Shift pressed mid-drag applies at once.
        sel.press(&mut d, corner, 0, 1.0);
        sel.motion(&mut d, corner + Point::new(20.0, 4.0), 0);
        sel.modifiers(&mut d, SHIFT);
        let g = sel.moving().unwrap().geometry();
        assert!((g.w - g.h).abs() < 0.01, "{g:?}");
    }

    #[test]
    fn lines_by_their_ends() {
        let mut line = Shape::create(Tool::Line, Point::new(0.0, 0.0), Color::BLACK, 1).unwrap();
        line.extend(Point::new(100.0, 0.0), Mods::default());
        let mut d = vec![line];
        let mut sel = Selection::default();
        sel.press(&mut d, Point::new(50.0, 1.0), 0, 1.0);
        sel.release(&mut d);
        assert_eq!(sel.handle_at(&d, Point::new(100.0, 0.0), 1.0), Some(Grip::End(1)));
        sel.press(&mut d, Point::new(100.0, 0.0), 0, 1.0);
        // Shift: 45° steps.
        sel.motion(&mut d, Point::new(60.0, 41.0), SHIFT);
        sel.release(&mut d);
        let end = d.last().unwrap().drag().unwrap().end;
        assert!((end.x - end.y).abs() < 0.01, "{end:?}");
    }

    #[test]
    fn alt_picks_the_one_under() {
        let mut d = vec![rect_shape(Rect::new(0.0, 0.0, 50.0, 50.0)), rect_shape(Rect::new(0.0, 0.0, 60.0, 60.0))];
        let mut sel = Selection::default();
        sel.press(&mut d, Point::new(0.0, 12.0), 0, 1.0);
        sel.release(&mut d);
        assert_eq!(sel.index(&d), Some(1));
        sel.press(&mut d, Point::new(0.0, 12.0), ALT, 1.0);
        sel.release(&mut d);
        assert_eq!(sel.index(&d), Some(0));
    }
}
