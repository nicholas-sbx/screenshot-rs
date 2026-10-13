//! Points and rectangles, in the two flavours the app needs: float ones in
//! logical pixels (what the pointer and the drawings use) and integer ones
//! in physical pixels (what pictures and buffers use).
//!
//! Unlike Qt's `QRect`, edges here are exclusive: `x2()` is `x + w`.

use std::ops::{Add, AddAssign, Div, Mul, Neg, Sub, SubAssign};

#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Point {
    pub x: f32,
    pub y: f32,
}

impl Point {
    pub const ZERO: Point = Point { x: 0.0, y: 0.0 };

    pub const fn new(x: f32, y: f32) -> Self {
        Point { x, y }
    }

    pub fn length(self) -> f32 {
        (self.x * self.x + self.y * self.y).sqrt()
    }

    pub fn manhattan(self) -> f32 {
        self.x.abs() + self.y.abs()
    }

    pub fn distance(self, other: Point) -> f32 {
        (self - other).length()
    }

    /// The nearest whole pixel (rounding half away from zero, as Qt does).
    pub fn round(self) -> IPoint {
        IPoint::new(self.x.round() as i32, self.y.round() as i32)
    }
}

impl Add for Point {
    type Output = Point;
    fn add(self, o: Point) -> Point {
        Point::new(self.x + o.x, self.y + o.y)
    }
}

impl AddAssign for Point {
    fn add_assign(&mut self, o: Point) {
        self.x += o.x;
        self.y += o.y;
    }
}

impl Sub for Point {
    type Output = Point;
    fn sub(self, o: Point) -> Point {
        Point::new(self.x - o.x, self.y - o.y)
    }
}

impl SubAssign for Point {
    fn sub_assign(&mut self, o: Point) {
        self.x -= o.x;
        self.y -= o.y;
    }
}

impl Mul<f32> for Point {
    type Output = Point;
    fn mul(self, k: f32) -> Point {
        Point::new(self.x * k, self.y * k)
    }
}

impl Div<f32> for Point {
    type Output = Point;
    fn div(self, k: f32) -> Point {
        Point::new(self.x / k, self.y / k)
    }
}

impl Neg for Point {
    type Output = Point;
    fn neg(self) -> Point {
        Point::new(-self.x, -self.y)
    }
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IPoint {
    pub x: i32,
    pub y: i32,
}

impl IPoint {
    pub const fn new(x: i32, y: i32) -> Self {
        IPoint { x, y }
    }

    pub fn to_f(self) -> Point {
        Point::new(self.x as f32, self.y as f32)
    }
}

impl Add for IPoint {
    type Output = IPoint;
    fn add(self, o: IPoint) -> IPoint {
        IPoint::new(self.x + o.x, self.y + o.y)
    }
}

impl Sub for IPoint {
    type Output = IPoint;
    fn sub(self, o: IPoint) -> IPoint {
        IPoint::new(self.x - o.x, self.y - o.y)
    }
}

/// A rectangle with float coordinates (logical pixels).
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct Rect {
    pub x: f32,
    pub y: f32,
    pub w: f32,
    pub h: f32,
}

impl Rect {
    pub const fn new(x: f32, y: f32, w: f32, h: f32) -> Self {
        Rect { x, y, w, h }
    }

    /// The rectangle spanned by two corners, in any order.
    pub fn from_points(a: Point, b: Point) -> Self {
        Rect::new(a.x.min(b.x), a.y.min(b.y), (a.x - b.x).abs(), (a.y - b.y).abs())
    }

    pub fn from_ltrb(l: f32, t: f32, r: f32, b: f32) -> Self {
        Rect::new(l, t, r - l, b - t)
    }

    pub fn left(&self) -> f32 {
        self.x
    }
    pub fn top(&self) -> f32 {
        self.y
    }
    pub fn right(&self) -> f32 {
        self.x + self.w
    }
    pub fn bottom(&self) -> f32 {
        self.y + self.h
    }
    pub fn center(&self) -> Point {
        Point::new(self.x + self.w / 2.0, self.y + self.h / 2.0)
    }
    pub fn top_left(&self) -> Point {
        Point::new(self.x, self.y)
    }
    pub fn size(&self) -> (f32, f32) {
        (self.w, self.h)
    }

    pub fn is_empty(&self) -> bool {
        self.w <= 0.0 || self.h <= 0.0
    }

    /// Edges inclusive, as Qt's `QRectF::contains`.
    pub fn contains(&self, p: Point) -> bool {
        let r = self.normalized();
        p.x >= r.x && p.x <= r.right() && p.y >= r.y && p.y <= r.bottom()
    }

    pub fn contains_rect(&self, o: &Rect) -> bool {
        let r = self.normalized();
        let o = o.normalized();
        !r.is_empty() && !o.is_empty() && o.x >= r.x && o.y >= r.y && o.right() <= r.right() && o.bottom() <= r.bottom()
    }

    pub fn intersects(&self, o: &Rect) -> bool {
        let a = self.normalized();
        let b = o.normalized();
        !a.is_empty() && !b.is_empty() && a.x < b.right() && b.x < a.right() && a.y < b.bottom() && b.y < a.bottom()
    }

    /// The overlap, or an empty rectangle at the origin if there is none
    /// (as `QRectF::intersected`).
    pub fn intersected(&self, o: &Rect) -> Rect {
        let a = self.normalized();
        let b = o.normalized();
        let l = a.x.max(b.x);
        let t = a.y.max(b.y);
        let r = a.right().min(b.right());
        let btm = a.bottom().min(b.bottom());
        if l >= r || t >= btm {
            return Rect::default();
        }
        Rect::from_ltrb(l, t, r, btm)
    }

    pub fn united(&self, o: &Rect) -> Rect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        Rect::from_ltrb(self.x.min(o.x), self.y.min(o.y), self.right().max(o.right()), self.bottom().max(o.bottom()))
    }

    pub fn normalized(&self) -> Rect {
        let mut r = *self;
        if r.w < 0.0 {
            r.x += r.w;
            r.w = -r.w;
        }
        if r.h < 0.0 {
            r.y += r.h;
            r.h = -r.h;
        }
        r
    }

    pub fn translated(&self, d: Point) -> Rect {
        Rect::new(self.x + d.x, self.y + d.y, self.w, self.h)
    }

    /// Move the edges by (left, top, right, bottom), as `QRectF::adjusted`.
    pub fn adjusted(&self, l: f32, t: f32, r: f32, b: f32) -> Rect {
        Rect::new(self.x + l, self.y + t, self.w - l + r, self.h - t + b)
    }

    pub fn scaled(&self, k: f32) -> Rect {
        Rect::new(self.x * k, self.y * k, self.w * k, self.h * k)
    }

    /// The smallest integer rectangle containing this one (`toAlignedRect`).
    pub fn aligned(&self) -> IRect {
        let r = self.normalized();
        let x1 = r.x.floor() as i32;
        let y1 = r.y.floor() as i32;
        let x2 = r.right().ceil() as i32;
        let y2 = r.bottom().ceil() as i32;
        IRect::new(x1, y1, x2 - x1, y2 - y1)
    }

    /// Each value rounded, as `QRect(round(x), round(y), round(w), round(h))`.
    pub fn round(&self) -> IRect {
        IRect::new(self.x.round() as i32, self.y.round() as i32, self.w.round() as i32, self.h.round() as i32)
    }

    pub fn move_left(&mut self, x: f32) {
        self.x = x;
    }
    pub fn move_top(&mut self, y: f32) {
        self.y = y;
    }
    pub fn set_left(&mut self, l: f32) {
        let r = self.right();
        self.x = l;
        self.w = r - l;
    }
    pub fn set_right(&mut self, r: f32) {
        self.w = r - self.x;
    }
    pub fn set_top(&mut self, t: f32) {
        let b = self.bottom();
        self.y = t;
        self.h = b - t;
    }
    pub fn set_bottom(&mut self, b: f32) {
        self.h = b - self.y;
    }
}

/// A rectangle of whole pixels, edges exclusive (`x2 = x + w`).
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq, Hash)]
pub struct IRect {
    pub x: i32,
    pub y: i32,
    pub w: i32,
    pub h: i32,
}

impl IRect {
    pub const fn new(x: i32, y: i32, w: i32, h: i32) -> Self {
        IRect { x, y, w, h }
    }

    pub fn from_ltrb(l: i32, t: i32, r: i32, b: i32) -> Self {
        IRect::new(l, t, r - l, b - t)
    }

    pub fn x2(&self) -> i32 {
        self.x + self.w
    }

    /// The device pixels this (logical) rectangle covers at `scale`, as
    /// compositors place a surface: each edge on the nearest one.
    pub fn device_edges(&self, scale: f32) -> IRect {
        let d = |v: i32| (v as f32 * scale).round() as i32;
        IRect::from_ltrb(d(self.x), d(self.y), d(self.x2()), d(self.y2()))
    }
    pub fn y2(&self) -> i32 {
        self.y + self.h
    }
    pub fn is_empty(&self) -> bool {
        self.w <= 0 || self.h <= 0
    }
    pub fn area(&self) -> i64 {
        if self.is_empty() { 0 } else { self.w as i64 * self.h as i64 }
    }
    pub fn top_left(&self) -> IPoint {
        IPoint::new(self.x, self.y)
    }
    pub fn center(&self) -> IPoint {
        IPoint::new(self.x + self.w / 2, self.y + self.h / 2)
    }

    pub fn contains(&self, p: IPoint) -> bool {
        p.x >= self.x && p.x < self.x2() && p.y >= self.y && p.y < self.y2()
    }

    pub fn contains_rect(&self, o: &IRect) -> bool {
        !o.is_empty() && o.x >= self.x && o.y >= self.y && o.x2() <= self.x2() && o.y2() <= self.y2()
    }

    pub fn intersects(&self, o: &IRect) -> bool {
        !self.is_empty() && !o.is_empty() && self.x < o.x2() && o.x < self.x2() && self.y < o.y2() && o.y < self.y2()
    }

    pub fn intersected(&self, o: &IRect) -> IRect {
        let l = self.x.max(o.x);
        let t = self.y.max(o.y);
        let r = self.x2().min(o.x2());
        let b = self.y2().min(o.y2());
        if l >= r || t >= b {
            return IRect::default();
        }
        IRect::from_ltrb(l, t, r, b)
    }

    pub fn united(&self, o: &IRect) -> IRect {
        if self.is_empty() {
            return *o;
        }
        if o.is_empty() {
            return *self;
        }
        IRect::from_ltrb(self.x.min(o.x), self.y.min(o.y), self.x2().max(o.x2()), self.y2().max(o.y2()))
    }

    pub fn translated(&self, dx: i32, dy: i32) -> IRect {
        IRect::new(self.x + dx, self.y + dy, self.w, self.h)
    }

    pub fn adjusted(&self, l: i32, t: i32, r: i32, b: i32) -> IRect {
        IRect::new(self.x + l, self.y + t, self.w - l + r, self.h - t + b)
    }

    pub fn to_f(&self) -> Rect {
        Rect::new(self.x as f32, self.y as f32, self.w as f32, self.h as f32)
    }

    /// The parts of `self` not covered by `hole`: up to four rectangles.
    pub fn subtract(&self, hole: &IRect) -> Vec<IRect> {
        if !self.intersects(hole) {
            return if self.is_empty() { vec![] } else { vec![*self] };
        }
        let mut out = Vec::with_capacity(4);
        if self.y < hole.y {
            out.push(IRect::from_ltrb(self.x, self.y, self.x2(), hole.y));
        }
        if self.y2() > hole.y2() {
            out.push(IRect::from_ltrb(self.x, hole.y2(), self.x2(), self.y2()));
        }
        let top = self.y.max(hole.y);
        let bottom = self.y2().min(hole.y2());
        if self.x < hole.x {
            out.push(IRect::from_ltrb(self.x, top, hole.x, bottom));
        }
        if self.x2() > hole.x2() {
            out.push(IRect::from_ltrb(hole.x2(), top, self.x2(), bottom));
        }
        out.retain(|r| !r.is_empty());
        out
    }

    /// Where `a` and `b` differ: the area in exactly one of them, as
    /// non-overlapping rectangles. What changes when a hole moves from
    /// `a` to `b`.
    pub fn xor(a: &IRect, b: &IRect) -> Vec<IRect> {
        let mut out = a.subtract(b);
        out.extend(b.subtract(a));
        out
    }
}

/// A set of damaged rectangles, merged as they come so it stays short.
#[derive(Clone, Debug, Default)]
pub struct Damage {
    rects: Vec<IRect>,
    full: bool,
}

impl Damage {
    /// More rectangles than this and they're merged into their bounds.
    const MAX: usize = 24;

    pub fn new() -> Self {
        Self::default()
    }

    pub fn full() -> Self {
        Damage { rects: Vec::new(), full: true }
    }

    pub fn is_full(&self) -> bool {
        self.full
    }

    pub fn is_empty(&self) -> bool {
        !self.full && self.rects.is_empty()
    }

    /// Add `r`, logical pixels at `scale`, with a little to spare.
    pub fn add_logical(&mut self, r: Rect, scale: f32) {
        let r = r.adjusted(-2.0, -2.0, 2.0, 2.0);
        let (x0, y0) = ((r.x * scale).floor() as i32, (r.y * scale).floor() as i32);
        let (x1, y1) = ((r.right() * scale).ceil() as i32, (r.bottom() * scale).ceil() as i32);
        self.add(IRect::new(x0, y0, x1 - x0, y1 - y0));
    }

    pub fn set_full(&mut self) {
        self.full = true;
        self.rects.clear();
    }

    pub fn clear(&mut self) {
        self.full = false;
        self.rects.clear();
    }

    pub fn add(&mut self, r: IRect) {
        if self.full || r.is_empty() {
            return;
        }
        // Absorb anything it covers; skip it if something covers it.
        if self.rects.iter().any(|o| o.contains_rect(&r)) {
            return;
        }
        self.rects.retain(|o| !r.contains_rect(o));
        // Merge with a neighbour when that wastes little.
        for o in self.rects.iter_mut() {
            let u = o.united(&r);
            if u.area() <= (o.area() + r.area()) * 5 / 4 {
                *o = u;
                return;
            }
        }
        self.rects.push(r);
        if self.rects.len() > Self::MAX {
            let all = self.bounds();
            self.rects.clear();
            self.rects.push(all);
        }
    }

    pub fn add_all(&mut self, other: &Damage) {
        if other.full {
            self.set_full();
            return;
        }
        for r in &other.rects {
            self.add(*r);
        }
    }

    pub fn bounds(&self) -> IRect {
        self.rects.iter().fold(IRect::default(), |acc, r| acc.united(r))
    }

    /// The rectangles, clipped to `area` (everything in it when full).
    pub fn rects_in(&self, area: IRect) -> Vec<IRect> {
        if self.full {
            return vec![area];
        }
        self.rects.iter().map(|r| r.intersected(&area)).filter(|r| !r.is_empty()).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aligned_covers_fractions() {
        assert_eq!(Rect::new(0.5, 1.2, 2.0, 2.0).aligned(), IRect::new(0, 1, 3, 3));
        assert_eq!(Rect::new(-0.5, -0.5, 1.0, 1.0).aligned(), IRect::new(-1, -1, 2, 2));
    }

    #[test]
    fn contains_is_inclusive_for_floats_exclusive_for_ints() {
        assert!(Rect::new(0.0, 0.0, 10.0, 10.0).contains(Point::new(10.0, 10.0)));
        assert!(!IRect::new(0, 0, 10, 10).contains(IPoint::new(10, 5)));
        assert!(IRect::new(0, 0, 10, 10).contains(IPoint::new(9, 9)));
    }

    #[test]
    fn intersect_and_unite() {
        let a = IRect::new(0, 0, 10, 10);
        let b = IRect::new(5, 5, 10, 10);
        assert_eq!(a.intersected(&b), IRect::new(5, 5, 5, 5));
        assert_eq!(a.united(&b), IRect::new(0, 0, 15, 15));
        assert!(a.intersected(&IRect::new(20, 20, 1, 1)).is_empty());
    }

    #[test]
    fn xor_is_the_difference() {
        let a = IRect::new(0, 0, 10, 10);
        let b = IRect::new(0, 0, 10, 12);
        let parts = IRect::xor(&a, &b);
        assert_eq!(parts, vec![IRect::new(0, 10, 10, 2)]);
        let area: i64 = IRect::xor(&IRect::new(0, 0, 10, 10), &IRect::new(5, 5, 10, 10)).iter().map(|r| r.area()).sum();
        assert_eq!(area, 100 + 100 - 2 * 25);
    }

    #[test]
    fn damage_merges() {
        let mut d = Damage::new();
        d.add(IRect::new(0, 0, 10, 10));
        d.add(IRect::new(2, 2, 3, 3));
        assert_eq!(d.rects_in(IRect::new(0, 0, 100, 100)), vec![IRect::new(0, 0, 10, 10)]);
        d.add(IRect::new(10, 0, 10, 10));
        assert_eq!(d.rects_in(IRect::new(0, 0, 100, 100)), vec![IRect::new(0, 0, 20, 10)]);
        for i in 0..40 {
            d.add(IRect::new(i * 40, 200, 1, 1));
        }
        assert!(d.rects_in(IRect::new(0, 0, 4000, 4000)).len() <= Damage::MAX);
    }
}
