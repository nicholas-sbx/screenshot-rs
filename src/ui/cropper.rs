//! The editor's crop tool: a frame round the part to keep, its edges dragged
//! in (or out past the picture, for a margin), at a fixed shape if asked; or,
//! when cutting out, a band dragged across the picture to remove.

use crate::geom::{Point, Rect};
use crate::keyseq::{CTRL, SHIFT};
use crate::render::Painter;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD};
use crate::render::theme::{self, Color, REC};
use crate::ui::picture::{RATIOS, fit_ratio};
use crate::ui::selection::{Grip, resized};
use crate::ui::session::CursorShape;

const HANDLE: f32 = 10.0;

/// What a press took hold of.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hold {
    Grip(Grip),
    /// a new frame, dragged out from here
    New(Point),
}

/// What letting go asks the editor to do.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Done {
    Nothing,
    /// cut out the band from `a` to `b` (logical): rows, else columns
    Cut {
        a: f32,
        b: f32,
        rows: bool,
    },
}

#[derive(Clone, Debug, Default)]
pub struct Cropper {
    /// what to keep (picture coordinates; may reach past it)
    pub rect: Option<Rect>,
    /// the picture's size (logical)
    size: (f32, f32),
    /// index in picture::RATIOS
    pub ratio: usize,
    /// a margin in your colour, else clear
    pub margin_colour: bool,
    /// cutting out rows (Some(true)) or columns (Some(false))
    pub cut: Option<bool>,
    held: Option<(Hold, Point, Rect)>,
    /// a slice being dragged across: (from, to)
    band: Option<(f32, f32)>,
}

impl Cropper {
    pub fn whole(&self) -> Rect {
        Rect::new(0.0, 0.0, self.size.0, self.size.1)
    }

    /// The frame to start from: all of the picture (at the crop's shape).
    fn default_rect(&self) -> Rect {
        match self.ratio_value() {
            Some(r) => fit_ratio(self.whole(), r),
            None => self.whole(),
        }
    }

    /// Start (again) on a picture of `size` (logical).
    pub fn start(&mut self, size: (f32, f32)) {
        self.size = size;
        self.rect = Some(self.default_rect());
        self.held = None;
        self.band = None;
        self.cut = None;
    }

    pub fn stop(&mut self) {
        self.rect = None;
        self.held = None;
        self.band = None;
        self.cut = None;
    }

    /// Is the frame anything but the whole picture?
    pub fn changed(&self) -> bool {
        let Some(r) = self.rect else { return false };
        let w = self.whole();
        [(r.left(), w.left()), (r.top(), w.top()), (r.right(), w.right()), (r.bottom(), w.bottom())].iter().any(|(a, b)| (a - b).abs() > 0.5)
    }

    pub fn busy(&self) -> bool {
        self.held.is_some() || self.band.is_some()
    }

    // -- the options -----------------------------------------------------------

    pub fn ratio_value(&self) -> Option<f32> {
        match RATIOS.get(self.ratio).and_then(|r| r.2) {
            Some(0.0) => (self.size.1 > 0.0).then(|| self.size.0 / self.size.1),
            other => other,
        }
    }

    pub fn ratio_label(&self) -> &'static str {
        RATIOS.get(self.ratio).map_or("Free", |r| r.1)
    }

    pub fn next_ratio(&mut self) {
        self.ratio = (self.ratio + 1) % RATIOS.len();
        if let (Some(r), Some(rect)) = (self.ratio_value(), self.rect) {
            self.rect = Some(fit_ratio(rect, r));
        }
    }

    /// Cut out rows (`rows`) or columns, or stop doing that (the same again).
    pub fn set_cut(&mut self, rows: bool) {
        self.cut = if self.cut == Some(rows) { None } else { Some(rows) };
    }

    // -- the pointer ---------------------------------------------------------------

    fn handles(r: Rect) -> Vec<(Grip, Point)> {
        let xs = [(Some(false), r.left()), (None, r.center().x), (Some(true), r.right())];
        let ys = [(Some(false), r.top()), (None, r.center().y), (Some(true), r.bottom())];
        let mut out = Vec::with_capacity(8);
        for (yk, y) in ys {
            for (xk, x) in xs {
                if (xk, yk) != (None, None) {
                    out.push((
                        Grip::Edges { left: xk == Some(false), right: xk == Some(true), top: yk == Some(false), bottom: yk == Some(true) },
                        Point::new(x, y),
                    ));
                }
            }
        }
        out
    }

    fn handle_at(&self, pos: Point, scale: f32) -> Option<Grip> {
        let r = self.rect?;
        let grab = (HANDLE / 2.0 + 4.0) / scale;
        Self::handles(r).into_iter().find(|(_, at)| (pos.x - at.x).abs() <= grab && (pos.y - at.y).abs() <= grab).map(|(g, _)| g)
    }

    pub fn cursor(&self, pos: Point, scale: f32) -> CursorShape {
        if let Some(rows) = self.cut {
            return if rows { CursorShape::ResizeNS } else { CursorShape::ResizeEW };
        }
        if let Some((Hold::Grip(g), ..)) = self.held {
            return g.cursor();
        }
        if let Some(g) = self.handle_at(pos, scale) {
            return g.cursor();
        }
        if self.rect.is_some_and(|r| r.contains(pos)) && self.changed() { CursorShape::Move } else { CursorShape::Crosshair }
    }

    /// `snap`: an edge in the picture near a point (or the point).
    pub fn press(&mut self, pos: Point, mods: i32, scale: f32, snap: &dyn Fn(Point, i32) -> Point) {
        if let Some(rows) = self.cut {
            let v = if rows { pos.y } else { pos.x };
            self.band = Some((v, v));
            return;
        }
        let r = self.rect.unwrap_or_else(|| self.default_rect());
        self.held = Some(match self.handle_at(pos, scale) {
            Some(g) => (Hold::Grip(g), pos, r),
            None if r.contains(pos) && self.changed() => (Hold::Grip(Grip::Move), pos, r),
            None => (Hold::New(snap(pos, mods)), pos, r),
        });
    }

    /// The pointer moved with the button held: true if it changed something.
    pub fn motion(&mut self, pos: Point, mods: i32, snap: &dyn Fn(Point, i32) -> Point) -> bool {
        if let (Some((from, _)), Some(rows)) = (self.band, self.cut) {
            self.band = Some((from, if rows { pos.y } else { pos.x }));
            return true;
        }
        let Some((hold, start, r0)) = self.held else { return false };
        let ratio = self.ratio_value();
        let keep = ratio.is_some() || mods & SHIFT != 0;
        let center = mods & CTRL != 0;
        let r = match hold {
            Hold::New(origin) => {
                let end = snap(pos, mods);
                let mut d = end - origin;
                if keep {
                    let k = ratio.unwrap_or(1.0);
                    let sign = |v: f32| if v >= 0.0 { 1.0 } else { -1.0 };
                    d = if d.x.abs() / k > d.y.abs() { Point::new(d.x, d.x.abs() / k * sign(d.y)) } else { Point::new(d.y.abs() * k * sign(d.x), d.y) };
                }
                if center { Rect::from_points(origin - d, origin + d) } else { Rect::from_points(origin, origin + d) }
            }
            Hold::Grip(Grip::Move) => r0.translated(pos - start),
            Hold::Grip(g @ Grip::Edges { left, right, top, bottom }) => {
                let corner = Point::new(
                    if left {
                        r0.left()
                    } else if right {
                        r0.right()
                    } else {
                        r0.center().x
                    },
                    if top {
                        r0.top()
                    } else if bottom {
                        r0.bottom()
                    } else {
                        r0.center().y
                    },
                );
                // The edge keeps its distance from the pointer, then snaps to an edge in the picture.
                let d = snap(corner + (pos - start), mods) - corner;
                resized(r0, g, d, keep, center)
            }
            Hold::Grip(Grip::End(_)) => r0,
        };
        self.rect = Some(r.normalized());
        true
    }

    pub fn release(&mut self, dpr: f32) -> Done {
        if let Some((a, b)) = self.band.take() {
            if (b - a).abs() * dpr >= 1.0 {
                return Done::Cut { a, b, rows: self.cut == Some(true) };
            }
            return Done::Nothing;
        }
        if self.held.take().is_some() && self.rect.is_none_or(|r| r.w < 2.0 || r.h < 2.0) {
            self.rect = Some(self.default_rect()); // (a click: back to all of it)
        }
        Done::Nothing
    }

    /// Esc: stop cutting, or put the frame back round the whole picture;
    /// false if there was nothing to undo.
    pub fn cancel(&mut self) -> bool {
        if self.held.is_some() || self.band.is_some() {
            self.held = None;
            self.band = None;
        } else if self.cut.is_some() {
            self.cut = None;
        } else if self.rect.is_some_and(|r| r != self.default_rect()) {
            self.start(self.size);
        } else {
            return false;
        }
        true
    }

    // -- painting (in the canvas's own coordinates) ----------------------------------

    /// `offset`, `zoom`: where the picture is in `canvas`, and its scale;
    /// `colour`: the margin's when it has one.
    pub fn paint(&self, p: &mut Painter<'_>, canvas: Rect, offset: Point, zoom: f32, dpr: f32, colour: Color) {
        let c = theme::c();
        let frame = Rect::new(canvas.x + offset.x, canvas.y + offset.y, self.size.0 * zoom, self.size.1 * zoom);
        if let (Some((a, b)), Some(rows)) = (self.band, self.cut) {
            let (a, b) = (a.min(b), a.max(b));
            let band = if rows {
                Rect::new(frame.x, frame.y + a * zoom, frame.w, (b - a) * zoom)
            } else {
                Rect::new(frame.x + a * zoom, frame.y, (b - a) * zoom, frame.h)
            }
            .intersected(&frame);
            p.fill_rect(band, REC.with_alpha(90));
            p.stroke_rect(band, Pen::new(REC, 1.5).dashed());
        }
        let Some(rect) = self.rect.filter(|_| self.cut.is_none()) else { return };
        let r = Rect::new(frame.x + rect.x * zoom, frame.y + rect.y * zoom, rect.w * zoom, rect.h * zoom);
        // The margin, where the frame reaches past the picture.
        for part in outside(r, frame) {
            if self.margin_colour {
                p.fill_rect(part, Color { a: 255, ..colour });
            } else {
                checks(p, part);
            }
        }
        // Shade what's cut away, then the frame and its handles.
        if self.changed() {
            for part in outside(canvas, r) {
                p.fill_rect(part, c.dim.with_alpha(150));
            }
        }
        p.stroke_rect(r, Pen::new(c.accent, 2.0));
        for (_, at) in Self::handles(rect) {
            let v = Point::new(frame.x + at.x * zoom, frame.y + at.y * zoom);
            p.draw_rounded_rect(Rect::new(v.x - HANDLE / 2.0, v.y - HANDLE / 2.0, HANDLE, HANDLE), 3.0, Some(c.text), Some(Pen::new(c.accent, 1.5)));
        }
        // Its size in pixels.
        let label = format!("{} × {}", (rect.w * dpr).round() as i64, (rect.h * dpr).round() as i64);
        let f = text::font(12.0, DEMIBOLD);
        let w = text::width(&label, f) + 18.0;
        let mut bx = Rect::new(r.x, r.y - 30.0, w, 24.0);
        if bx.y < canvas.y + 4.0 {
            bx.y = r.y + 6.0;
        }
        p.fill_rounded_rect(bx, 6.0, c.accent);
        p.draw_text(bx, Align::Center, &label, f, c.on_accent);
    }
}

/// The parts of `r` outside `hole`, as up to four rectangles.
fn outside(r: Rect, hole: Rect) -> Vec<Rect> {
    let h = hole.intersected(&r);
    if h.is_empty() {
        return if r.is_empty() { Vec::new() } else { vec![r] };
    }
    [
        Rect::from_ltrb(r.left(), r.top(), r.right(), h.top()),
        Rect::from_ltrb(r.left(), h.bottom(), r.right(), r.bottom()),
        Rect::from_ltrb(r.left(), h.top(), h.left(), h.bottom()),
        Rect::from_ltrb(h.right(), h.top(), r.right(), h.bottom()),
    ]
    .into_iter()
    .filter(|p| p.w > 0.0 && p.h > 0.0)
    .collect()
}

/// A grey checkerboard, for what's transparent.
fn checks(p: &mut Painter<'_>, r: Rect) {
    p.fill_rect(r, Color::rgb(0x9A9A9A));
    p.save();
    p.clip_rect(r);
    let light = Color::rgb(0xCFCFCF);
    let (x0, y0) = ((r.x / 8.0).floor() as i64, (r.y / 8.0).floor() as i64);
    let (x1, y1) = ((r.right() / 8.0).ceil() as i64, (r.bottom() / 8.0).ceil() as i64);
    for y in y0..y1 {
        for x in x0..x1 {
            if (x + y).rem_euclid(2) == 0 {
                p.fill_rect(Rect::new(x as f32 * 8.0, y as f32 * 8.0, 8.0, 8.0), light);
            }
        }
    }
    p.restore();
}

#[cfg(test)]
mod tests {
    use super::*;

    fn none(p: Point, _: i32) -> Point {
        p
    }

    #[test]
    fn framing() {
        let mut c = Cropper::default();
        c.start((400.0, 300.0));
        assert!(!c.changed());
        // The right edge dragged in by its handle.
        c.press(Point::new(400.0, 150.0), 0, 1.0, &none);
        c.motion(Point::new(300.0, 150.0), 0, &none);
        assert_eq!(c.release(1.0), Done::Nothing);
        assert_eq!(c.rect, Some(Rect::new(0.0, 0.0, 300.0, 300.0)));
        assert!(c.changed());
        // Esc puts it back.
        assert!(c.cancel() && !c.changed());
        assert!(!c.cancel());
        // 1:1 from the whole picture: the biggest square, centred.
        c.next_ratio();
        assert_eq!(c.ratio_label(), "1:1");
        c.start((400.0, 300.0));
        assert_eq!(c.rect, Some(Rect::new(50.0, 0.0, 300.0, 300.0)));
        // Dragged out past the picture: a margin.
        c.ratio = 0;
        c.start((400.0, 300.0));
        c.press(Point::new(0.0, 0.0), 0, 1.0, &none);
        c.motion(Point::new(-40.0, -20.0), 0, &none);
        c.release(1.0);
        assert_eq!(c.rect, Some(Rect::new(-40.0, -20.0, 440.0, 320.0)));
    }

    #[test]
    fn cutting_out() {
        let mut c = Cropper::default();
        c.start((400.0, 300.0));
        c.set_cut(true);
        c.press(Point::new(10.0, 100.0), 0, 1.0, &none);
        c.motion(Point::new(10.0, 140.0), 0, &none);
        assert_eq!(c.release(1.0), Done::Cut { a: 100.0, b: 140.0, rows: true });
        c.set_cut(true);
        assert_eq!(c.cut, None, "the same button again stops it");
    }

    #[test]
    fn outside_parts() {
        let parts = outside(Rect::new(0.0, 0.0, 10.0, 10.0), Rect::new(2.0, 2.0, 6.0, 6.0));
        let area: f32 = parts.iter().map(|r| r.w * r.h).sum();
        assert_eq!(area, 100.0 - 36.0);
        assert_eq!(outside(Rect::new(0.0, 0.0, 5.0, 5.0), Rect::new(0.0, 0.0, 5.0, 5.0)), Vec::<Rect>::new());
    }
}
