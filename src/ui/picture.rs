//! Changing the picture itself in the annotation editor: crop (with a margin
//! where the crop reaches past its edges), cut out a slice, rotate, flip and
//! resize. Each takes the picture and its pixels per logical pixel and
//! gives a new one, plus how a point on the old picture moves onto the new
//! one (logical px): the drawings are moved with it, and stay drawings.

use std::sync::Arc;

use crate::geom::{IRect, Point, Rect};
use crate::image::Image;
use crate::render::theme::Color;
use crate::ui::shapes::{Kind, Shape};

/// The crop's shapes: (name, label, width / height; None: free, 0: the
/// picture's own).
pub const RATIOS: &[(&str, &str, Option<f32>)] = &[
    ("free", "Free", None),
    ("1:1", "1:1", Some(1.0)),
    ("4:3", "4:3", Some(4.0 / 3.0)),
    ("16:9", "16:9", Some(16.0 / 9.0)),
    ("original", "Original", Some(0.0)),
];

/// How a point on the old picture lands on the new one (logical px).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Moved {
    /// moved by minus this (a crop)
    Shift(Point),
    /// a band from `a` to `b` cut out of rows (y) or columns (x)
    Cut {
        rows: bool,
        a: f32,
        b: f32,
    },
    /// a quarter turn, the old picture `w` x `h`
    Turn {
        clockwise: bool,
        w: f32,
        h: f32,
    },
    /// mirrored left to right, or top to bottom
    Flip {
        horizontal: bool,
        w: f32,
        h: f32,
    },
    Scale(f32, f32),
}

impl Moved {
    pub fn apply(&self, p: Point) -> Point {
        match *self {
            Moved::Shift(d) => p - d,
            Moved::Cut { rows, a, b } => {
                let v = if rows { p.y } else { p.x };
                let v = if v <= a {
                    v
                } else if v < b {
                    a
                } else {
                    v - (b - a)
                };
                if rows { Point::new(p.x, v) } else { Point::new(v, p.y) }
            }
            Moved::Turn { clockwise: true, h, .. } => Point::new(h - p.y, p.x),
            Moved::Turn { clockwise: false, w, .. } => Point::new(p.y, w - p.x),
            Moved::Flip { horizontal: true, w, .. } => Point::new(w - p.x, p.y),
            Moved::Flip { horizontal: false, h, .. } => Point::new(p.x, h - p.y),
            Moved::Scale(sx, sy) => Point::new(p.x * sx, p.y * sy),
        }
    }
}

fn round(v: f32) -> i32 {
    v.round() as i32
}

/// `rect` (logical) of the picture. Where it reaches past the picture's
/// edges there's a margin: clear, or `margin`.
pub fn crop(img: &Image, dpr: f32, rect: Rect, margin: Option<Color>) -> Result<(Image, Moved), String> {
    let px = IRect::new(round(rect.x * dpr), round(rect.y * dpr), round(rect.w * dpr).max(1), round(rect.h * dpr).max(1));
    let mut out = Image::new(px.w as u32, px.h as u32)?;
    if let Some(c) = margin {
        out.fill(Color { a: 255, ..c }.premul());
    }
    out.blit(img, -px.x, -px.y);
    Ok((out, Moved::Shift(Point::new(px.x as f32 / dpr, px.y as f32 / dpr))))
}

/// Cut out the band between `start` and `end` (logical): rows across the
/// picture (`rows`), else columns, joining what's either side.
pub fn cut(img: &Image, dpr: f32, start: f32, end: f32, rows: bool) -> Result<(Image, Moved), String> {
    let (w, h) = img.size();
    let full = if rows { h } else { w } as f32 / dpr;
    let (lo, hi) = (start.min(end).max(0.0), start.max(end).min(full));
    let full_px = if rows { h } else { w } as i32;
    let (a, b) = (round(lo * dpr).clamp(0, full_px), round(hi * dpr).clamp(0, full_px));
    let gone = (b - a).max(0);
    if gone >= full_px {
        return Err("that would cut out all of it".into());
    }
    let mut out = if rows { Image::new(w, h - gone as u32)? } else { Image::new(w - gone as u32, h)? };
    if rows {
        if a > 0 {
            out.blit(&img.crop(IRect::new(0, 0, w as i32, a))?, 0, 0);
        }
        if b < h as i32 {
            out.blit(&img.crop(IRect::new(0, b, w as i32, h as i32 - b))?, 0, a);
        }
    } else {
        if a > 0 {
            out.blit(&img.crop(IRect::new(0, 0, a, h as i32))?, 0, 0);
        }
        if b < w as i32 {
            out.blit(&img.crop(IRect::new(b, 0, w as i32 - b, h as i32))?, a, 0);
        }
    }
    Ok((out, Moved::Cut { rows, a: a as f32 / dpr, b: b as f32 / dpr }))
}

/// A quarter turn.
pub fn rotate(img: &Image, dpr: f32, clockwise: bool) -> Result<(Image, Moved), String> {
    let (w, h) = img.size();
    let mut out = Image::new_populated(h, w)?;
    for y in 0..h {
        let row = img.row(y);
        for x in 0..w {
            let (nx, ny) = if clockwise { (h - 1 - y, x) } else { (y, w - 1 - x) };
            let at = nx as usize * 4;
            out.row_mut(ny)[at..at + 4].copy_from_slice(&row[x as usize * 4..x as usize * 4 + 4]);
        }
    }
    Ok((out, Moved::Turn { clockwise, w: w as f32 / dpr, h: h as f32 / dpr }))
}

/// Mirror left to right (`horizontal`) or top to bottom.
pub fn flip(img: &Image, dpr: f32, horizontal: bool) -> Result<(Image, Moved), String> {
    let (w, h) = img.size();
    let mut out = Image::new_populated(w, h)?;
    for y in 0..h {
        let src = img.row(if horizontal { y } else { h - 1 - y });
        let dst = out.row_mut(y);
        if horizontal {
            for x in 0..w as usize {
                let from = (w as usize - 1 - x) * 4;
                dst[x * 4..x * 4 + 4].copy_from_slice(&src[from..from + 4]);
            }
        } else {
            dst.copy_from_slice(&src[..dst.len()]);
        }
    }
    Ok((out, Moved::Flip { horizontal, w: w as f32 / dpr, h: h as f32 / dpr }))
}

/// To `width` x `height` pixels.
pub fn resize(img: &Image, width: u32, height: u32) -> Result<(Image, Moved), String> {
    let out = img.scaled_area(width.max(1), height.max(1))?;
    let (sx, sy) = (out.width() as f32 / img.width() as f32, out.height() as f32 / img.height() as f32);
    Ok((out, Moved::Scale(sx, sy)))
}

/// The drawings moved onto the changed picture (each a copy, standing on
/// its own: undo puts back the picture and all of them). Text and counters
/// keep their size and reading direction: their middle moves.
pub fn move_drawings(drawings: &[Shape], m: Moved) -> Vec<Shape> {
    drawings
        .iter()
        .filter(|s| !s.hidden && !s.is_removed() && !matches!(s.kind, Kind::Step(_)))
        .map(|s| {
            let mut moved = match s.kind {
                Kind::Text(_) | Kind::Counter { .. } => {
                    let c = s.geometry().center();
                    s.moved(m.apply(c) - c)
                }
                _ => s.mapped(|p| m.apply(p)),
            };
            moved.replaces = None;
            moved
        })
        .collect()
}

/// A change to the picture, kept in the editor's list of drawings as an
/// undo step: undoing it puts back the picture and the drawings as they
/// were; redoing it, as they became.
#[derive(Debug)]
pub struct Step {
    /// (for the status line: "Cropped", "Rotated", ...)
    pub what: String,
    pub before: (Arc<Image>, Vec<(u64, Shape)>),
    pub after: (Arc<Image>, Vec<(u64, Shape)>),
}

/// The biggest `ratio` (width / height) box inside `rect`, centred.
pub fn fit_ratio(rect: Rect, ratio: f32) -> Rect {
    let (mut w, mut h) = (rect.w, rect.h);
    if w <= 0.0 || h <= 0.0 || ratio <= 0.0 {
        return rect;
    }
    if w / h > ratio {
        w = h * ratio;
    } else {
        h = w / ratio;
    }
    let c = rect.center();
    Rect::new(c.x - w / 2.0, c.y - h / 2.0, w, h)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pattern(w: u32, h: u32) -> Image {
        let mut img = Image::new(w, h).unwrap();
        for y in 0..h {
            for x in 0..w {
                img.set_pixel(x, y, 0xFF00_0000 | (x << 8) | y);
            }
        }
        img
    }

    #[test]
    fn cropping_with_a_margin() {
        let img = pattern(10, 10);
        let (out, m) = crop(&img, 1.0, Rect::new(-2.0, 5.0, 6.0, 8.0), Some(Color::rgb(0xFF0000))).unwrap();
        assert_eq!(out.size(), (6, 8));
        assert_eq!(out.pixel(0, 0), 0xFFFF_0000, "the margin in the colour");
        assert_eq!(out.pixel(2, 0), img.pixel(0, 5));
        assert_eq!(m.apply(Point::new(0.0, 5.0)), Point::new(2.0, 0.0));
        let (clear, _) = crop(&img, 1.0, Rect::new(-2.0, 0.0, 4.0, 4.0), None).unwrap();
        assert_eq!(clear.pixel(0, 0) >> 24, 0, "a clear margin");
    }

    #[test]
    fn cutting_out_a_band() {
        let img = pattern(10, 10);
        let (out, m) = cut(&img, 1.0, 6.0, 3.0, true).unwrap();
        assert_eq!(out.size(), (10, 7));
        assert_eq!(out.pixel(4, 2), img.pixel(4, 2));
        assert_eq!(out.pixel(4, 3), img.pixel(4, 6), "joined");
        assert_eq!(m.apply(Point::new(1.0, 8.0)), Point::new(1.0, 5.0));
        assert_eq!(m.apply(Point::new(1.0, 4.0)), Point::new(1.0, 3.0), "in the band: at the join");
        let (cols, _) = cut(&img, 1.0, 0.0, 2.0, false).unwrap();
        assert_eq!(cols.size(), (8, 10));
        assert_eq!(cols.pixel(0, 0), img.pixel(2, 0));
        let (top, _) = cut(&img, 1.0, 0.0, 2.0, true).unwrap();
        assert_eq!(top.pixel(0, 0), img.pixel(0, 2));
    }

    #[test]
    fn turning_and_mirroring() {
        let img = pattern(4, 2);
        let (cw, m) = rotate(&img, 1.0, true).unwrap();
        assert_eq!(cw.size(), (2, 4));
        // The top left goes to the top right.
        assert_eq!(cw.pixel(1, 0), img.pixel(0, 0));
        assert_eq!(m.apply(Point::new(0.0, 0.0)), Point::new(2.0, 0.0));
        let (ccw, m) = rotate(&img, 1.0, false).unwrap();
        assert_eq!(ccw.pixel(0, 3), img.pixel(0, 0));
        assert_eq!(m.apply(Point::new(0.0, 0.0)), Point::new(0.0, 4.0));
        let (h, _) = flip(&img, 1.0, true).unwrap();
        assert_eq!(h.pixel(0, 0), img.pixel(3, 0));
        let (v, m) = flip(&img, 1.0, false).unwrap();
        assert_eq!(v.pixel(0, 0), img.pixel(0, 1));
        assert_eq!(m.apply(Point::new(1.0, 0.5)), Point::new(1.0, 1.5));
        let (r, m) = resize(&img, 8, 1).unwrap();
        assert_eq!(r.size(), (8, 1));
        assert_eq!(m.apply(Point::new(1.0, 2.0)), Point::new(2.0, 1.0));
    }

    #[test]
    fn ratios() {
        assert_eq!(fit_ratio(Rect::new(0.0, 0.0, 200.0, 100.0), 1.0), Rect::new(50.0, 0.0, 100.0, 100.0));
        assert_eq!(fit_ratio(Rect::new(0.0, 0.0, 100.0, 100.0), 2.0), Rect::new(0.0, 25.0, 100.0, 50.0));
    }
}
