//! Hand-drawn line icons on a 24 x 24 grid, so no icon theme is involved.
//! Each is drawn once per size, colour and pixel offset, then copied: the
//! toolbar repaints whenever the crosshair passes behind it.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use tiny_skia::{PathBuilder, Transform};

use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::render::paint::{Cap, Join, Painter, Pen, polylines};
use crate::render::theme::Color;

/// Every icon's name.
pub const NAMES: &[&str] = &[
    "region", "pen", "line", "arrow", "rect", "solid", "ellipse", "marker", "text", "pixelate", "blur", "counter", "undo",
    "redo", "screen", "copy", "open", "close", "check", "codes", "codes-off", "pin", "record", "mic", "speaker",
    "cursor", "cursor-off", "pause", "resume", "stop", "trash", "magnet", "eyedropper", "fit", "save", "save-as",
];

struct Ctx<'a, 'b> {
    p: &'a mut Painter<'b>,
    pen: Pen,
    color: Color,
}

impl Ctx<'_, '_> {
    fn stroke(&mut self, path: Option<tiny_skia::Path>) {
        if let Some(path) = path {
            self.p.stroke_path(&path, self.pen);
        }
    }


    fn lines(&mut self, segs: &[&[(f32, f32)]]) {
        let path = polylines(segs);
        self.stroke(path);
    }

    fn closed(&mut self, pts: &[(f32, f32)]) -> Option<tiny_skia::Path> {
        let mut pb = PathBuilder::new();
        let (first, rest) = pts.split_first()?;
        pb.move_to(first.0, first.1);
        for p in rest {
            pb.line_to(p.0, p.1);
        }
        pb.close();
        pb.finish()
    }

    fn line(&mut self, a: (f32, f32), b: (f32, f32)) {
        let pen = self.pen;
        self.p.draw_line(Point::new(a.0, a.1), Point::new(b.0, b.1), pen);
    }

    fn rrect(&mut self, x: f32, y: f32, w: f32, h: f32, r: f32) {
        let pen = self.pen;
        self.p.stroke_rounded_rect(Rect::new(x, y, w, h), r, pen);
    }

    fn ellipse(&mut self, x: f32, y: f32, w: f32, h: f32) {
        let pen = self.pen;
        self.p.stroke_ellipse(Rect::new(x, y, w, h), pen);
    }
}

/// Qt's arcTo: from the current point, a line to the arc's start, then the
/// arc of the ellipse in `r` from `start` degrees through `sweep` degrees
/// (counter-clockwise on screen when positive).
pub fn arc_to(pb: &mut PathBuilder, r: Rect, start: f32, sweep: f32, move_first: bool) {
    let (cx, cy) = (r.x + r.w / 2.0, r.y + r.h / 2.0);
    let (rx, ry) = (r.w / 2.0, r.h / 2.0);
    let at = |deg: f32| {
        let a = deg.to_radians();
        (cx + rx * a.cos(), cy - ry * a.sin())
    };
    let (sx, sy) = at(start);
    if move_first {
        pb.move_to(sx, sy);
    } else {
        pb.line_to(sx, sy);
    }
    let segments = (sweep.abs() / 90.0).ceil().max(1.0) as i32;
    let step = sweep / segments as f32;
    for i in 0..segments {
        let a0 = (start + step * i as f32).to_radians();
        let a1 = (start + step * (i + 1) as f32).to_radians();
        let k = 4.0 / 3.0 * ((a1 - a0) / 4.0).tan();
        let (x0, y0) = (cx + rx * a0.cos(), cy - ry * a0.sin());
        let (x3, y3) = (cx + rx * a1.cos(), cy - ry * a1.sin());
        let (x1, y1) = (x0 - k * rx * a0.sin(), y0 - k * ry * a0.cos());
        let (x2, y2) = (x3 + k * rx * a1.sin(), y3 + k * ry * a1.cos());
        pb.cubic_to(x1, y1, x2, y2, x3, y3);
    }
}

fn draw_icon(c: &mut Ctx<'_, '_>, name: &str) {
    let color = c.color;
    match name {
        "region" => c.lines(&[
            &[(4.0, 9.0), (4.0, 4.0), (9.0, 4.0)],
            &[(15.0, 4.0), (20.0, 4.0), (20.0, 9.0)],
            &[(20.0, 15.0), (20.0, 20.0), (15.0, 20.0)],
            &[(9.0, 20.0), (4.0, 20.0), (4.0, 15.0)],
            &[(12.0, 9.5), (12.0, 14.5)],
            &[(9.5, 12.0), (14.5, 12.0)],
        ]),
        "pen" => {
            let body = c.closed(&[(4.0, 20.0), (5.0, 15.5), (15.5, 5.0), (19.0, 8.5), (8.5, 19.0)]);
            c.stroke(body);
            c.line((13.0, 7.5), (16.5, 11.0));
        }
        "line" => c.line((5.0, 19.0), (19.0, 5.0)),
        "arrow" => c.lines(&[&[(5.0, 19.0), (18.0, 6.0)], &[(10.0, 6.0), (18.0, 6.0), (18.0, 14.0)]]),
        "rect" => c.rrect(4.0, 6.0, 16.0, 12.0, 2.5),
        "solid" => {
            c.p.fill_rounded_rect(Rect::new(4.0, 6.0, 16.0, 12.0), 2.5, color);
            c.rrect(4.0, 6.0, 16.0, 12.0, 2.5);
        }
        "ellipse" => c.ellipse(4.0, 6.0, 16.0, 12.0),
        "marker" => {
            let body = c.closed(&[(9.0, 14.0), (16.0, 4.5), (20.0, 7.5), (13.0, 17.0)]);
            c.stroke(body);
            c.lines(&[&[(9.0, 14.0), (7.5, 17.5), (11.0, 17.5), (13.0, 17.0)]]);
            c.p.fill_rect(Rect::new(4.0, 19.5, 16.0, 2.0), color.with_alpha_f(color.a as f32 / 255.0 * 0.55));
        }
        "text" => c.lines(&[
            &[(6.0, 8.0), (6.0, 5.5), (18.0, 5.5), (18.0, 8.0)],
            &[(12.0, 5.5), (12.0, 19.0)],
            &[(9.5, 19.0), (14.5, 19.0)],
        ]),
        "pixelate" => {
            for i in 0..3 {
                for j in 0..3 {
                    let a = if (i + j) % 2 == 0 { 1.0 } else { 0.35 };
                    let cell = color.with_alpha_f(color.a as f32 / 255.0 * a);
                    c.p.fill_rounded_rect(Rect::new(4.0 + i as f32 * 5.6, 4.0 + j as f32 * 5.6, 4.6, 4.6), 1.0, cell);
                }
            }
        }
        "blur" => {
            // Dots fading out from the middle, as a blur spreads a point.
            for i in 0..4 {
                for j in 0..4 {
                    let d = (i as f32 - 1.5).abs() + (j as f32 - 1.5).abs();
                    let k = (d.round() as usize).clamp(1, 3) - 1;
                    let a = [1.0, 0.7, 0.4][k];
                    let r = [2.1, 1.6, 1.1][k];
                    let (x, y) = (5.5 + i as f32 * 4.33, 5.5 + j as f32 * 4.33);
                    c.p.fill_ellipse(Rect::new(x - r, y - r, 2.0 * r, 2.0 * r), color.with_alpha_f(color.a as f32 / 255.0 * a));
                }
            }
        }
        "eyedropper" => {
            c.lines(&[
                &[(4.0, 20.0), (5.5, 18.5)],
                &[(5.5, 18.5), (5.5, 16.0), (13.0, 8.5)],
                &[(5.5, 18.5), (8.0, 18.5), (15.5, 11.0)],
                &[(11.5, 7.0), (17.0, 12.5)],
            ]);
            c.p.save();
            c.p.translate(16.6, 7.4);
            c.p.rotate(45.0);
            c.rrect(-2.6, -4.2, 5.2, 6.4, 2.6);
            c.p.restore();
        }
        "save" => c.lines(&[
            &[(12.0, 4.0), (12.0, 14.0)],
            &[(7.5, 9.5), (12.0, 14.0), (16.5, 9.5)],
            &[(4.0, 14.0), (4.0, 19.5), (20.0, 19.5), (20.0, 14.0)],
        ]),
        "save-as" => {
            c.lines(&[&[(4.0, 14.0), (4.0, 19.5), (20.0, 19.5), (20.0, 14.0)]]);
            let body = c.closed(&[(8.0, 15.5), (8.7, 12.4), (15.6, 5.5), (18.5, 8.4), (11.6, 15.3)]);
            c.stroke(body);
        }
        "fit" => {
            c.ellipse(3.5, 3.5, 13.0, 13.0);
            c.line((14.6, 14.6), (20.5, 20.5));
            c.rrect(6.8, 7.6, 6.4, 4.8, 1.0);
        }
        "counter" => {
            c.ellipse(4.0, 4.0, 16.0, 16.0);
            c.lines(&[&[(10.5, 9.5), (12.5, 8.0), (12.5, 16.0)]]);
        }
        "undo" => undo(c),
        "redo" => {
            c.p.save();
            c.p.translate(24.0, 0.0);
            c.p.scale_by(-1.0, 1.0);
            undo(c);
            c.p.restore();
        }
        "screen" => {
            c.rrect(3.0, 4.0, 18.0, 12.0, 2.5);
            c.lines(&[&[(12.0, 16.0), (12.0, 20.0)], &[(8.0, 20.0), (16.0, 20.0)]]);
        }
        "copy" => {
            c.rrect(9.0, 9.0, 11.0, 11.0, 2.5);
            let mut pb = PathBuilder::new();
            pb.move_to(5.0, 15.0);
            pb.line_to(5.0, 6.5);
            pb.quad_to(5.0, 5.0, 6.5, 5.0);
            pb.line_to(15.0, 5.0);
            c.stroke(pb.finish());
        }
        "open" => c.lines(&[
            &[(10.0, 5.0), (5.5, 5.0), (5.0, 5.5), (5.0, 18.5), (5.5, 19.0), (18.5, 19.0), (19.0, 18.5), (19.0, 14.0)],
            &[(13.0, 5.0), (19.0, 5.0), (19.0, 11.0)],
            &[(19.0, 5.0), (11.0, 13.0)],
        ]),
        "close" => c.lines(&[&[(6.5, 6.5), (17.5, 17.5)], &[(17.5, 6.5), (6.5, 17.5)]]),
        "check" => c.lines(&[&[(5.0, 12.5), (10.0, 17.5), (19.0, 7.0)]]),
        "codes" => codes(c, color),
        "codes-off" => {
            let faded = color.with_alpha_f(color.a as f32 / 255.0 * 0.55);
            let pen = c.pen;
            c.pen = Pen { color: faded, ..pen };
            codes(c, faded);
            c.pen = pen;
            c.line((4.0, 20.0), (20.0, 4.0));
        }
        "pin" => {
            c.lines(&[&[(10.0, 4.5), (10.0, 9.5), (7.0, 13.5), (17.0, 13.5), (14.0, 9.5), (14.0, 4.5)]]);
            c.line((8.5, 4.5), (15.5, 4.5));
            c.line((12.0, 13.5), (12.0, 20.0));
        }
        "record" => {
            c.ellipse(4.0, 4.0, 16.0, 16.0);
            c.p.fill_ellipse(Rect::new(8.4, 8.4, 7.2, 7.2), color);
        }
        "mic" => {
            c.rrect(9.0, 3.5, 6.0, 11.0, 3.0);
            let mut pb = PathBuilder::new();
            pb.move_to(5.5, 11.5);
            pb.cubic_to(5.5, 20.0, 18.5, 20.0, 18.5, 11.5);
            c.stroke(pb.finish());
            c.line((12.0, 17.8), (12.0, 20.5));
        }
        "speaker" => {
            let body = c.closed(&[(4.0, 9.5), (7.5, 9.5), (12.0, 5.5), (12.0, 18.5), (7.5, 14.5), (4.0, 14.5)]);
            c.stroke(body);
            for r in [3.5f32, 6.5] {
                let mut pb = PathBuilder::new();
                arc_to(&mut pb, Rect::new(12.0 - r, 12.0 - r, 2.0 * r, 2.0 * r), 50.0, -100.0, true);
                c.stroke(pb.finish());
            }
        }
        "cursor" => {
            let body = c.closed(&[(6.0, 4.0), (18.0, 11.0), (12.5, 12.5), (10.0, 18.0)]);
            c.stroke(body);
        }
        "cursor-off" => {
            let faded = color.with_alpha_f(color.a as f32 / 255.0 * 0.55);
            let pen = c.pen;
            c.pen = Pen { color: faded, ..pen };
            let body = c.closed(&[(6.0, 4.0), (18.0, 11.0), (12.5, 12.5), (10.0, 18.0)]);
            c.stroke(body);
            c.pen = pen;
            c.line((4.0, 20.0), (20.0, 4.0));
        }
        "pause" => {
            c.line((9.0, 6.5), (9.0, 17.5));
            c.line((15.0, 6.5), (15.0, 17.5));
        }
        "resume" => {
            let body = c.closed(&[(8.0, 5.5), (18.5, 12.0), (8.0, 18.5)]);
            if let Some(b) = &body {
                c.p.fill_path(b, color);
            }
            c.stroke(body);
        }
        "stop" => c.p.fill_rounded_rect(Rect::new(7.0, 7.0, 10.0, 10.0), 2.0, color),
        "trash" => c.lines(&[
            &[(4.5, 7.0), (19.5, 7.0)],
            &[(9.5, 7.0), (9.5, 4.5), (14.5, 4.5), (14.5, 7.0)],
            &[(6.5, 7.0), (7.5, 19.5), (16.5, 19.5), (17.5, 7.0)],
        ]),
        "magnet" => {
            // A horseshoe magnet, tilted: a U-shaped bar with its poles marked off.
            c.p.save();
            c.p.translate(12.0, 12.0);
            c.p.rotate(45.0);
            c.p.translate(-12.0, -11.5);
            let mut pb = PathBuilder::new();
            pb.move_to(5.0, 3.5);
            pb.line_to(5.0, 12.0);
            arc_to(&mut pb, Rect::new(5.0, 5.0, 14.0, 14.0), 180.0, 180.0, false);
            pb.line_to(19.0, 3.5);
            pb.line_to(14.5, 3.5);
            pb.line_to(14.5, 12.0);
            arc_to(&mut pb, Rect::new(9.5, 9.5, 5.0, 5.0), 0.0, -180.0, false);
            pb.line_to(9.5, 3.5);
            pb.close();
            c.stroke(pb.finish());
            c.lines(&[&[(5.0, 7.5), (9.5, 7.5)], &[(14.5, 7.5), (19.0, 7.5)]]);
            c.p.restore();
        }
        _ => {}
    }
}

fn undo(c: &mut Ctx<'_, '_>) {
    let mut pb = PathBuilder::new();
    pb.move_to(9.0, 6.0);
    pb.line_to(5.0, 10.0);
    pb.line_to(9.0, 14.0);
    pb.move_to(5.0, 10.0);
    pb.line_to(14.5, 10.0);
    pb.cubic_to(20.5, 10.0, 20.5, 19.0, 14.5, 19.0);
    pb.line_to(10.0, 19.0);
    c.stroke(pb.finish());
}

fn codes(c: &mut Ctx<'_, '_>, color: Color) {
    for (x, y) in [(4.0, 4.0), (14.0, 4.0), (4.0, 14.0)] {
        c.rrect(x, y, 6.0, 6.0, 1.5);
    }
    for (x, y) in [(15.5, 15.5), (18.5, 18.5), (15.5, 18.5), (18.5, 14.0), (6.0, 6.0), (16.0, 6.0), (6.0, 16.0)] {
        c.p.fill_rect(Rect::new(x - 1.1, y - 1.1, 2.2, 2.2), color);
    }
}

#[derive(Clone, Copy, PartialEq, Eq, Hash)]
struct Key {
    name: &'static str,
    w: u32,
    h: u32,
    color: Color,
    fx: u8,
    fy: u8,
}

static CACHE: Mutex<Option<HashMap<Key, Arc<Image>>>> = Mutex::new(None);

/// Draw icon `name` filling `rect` (logical), in `color`.
pub fn paint(p: &mut Painter<'_>, name: &str, rect: Rect, color: Color) {
    let Some(&name) = NAMES.iter().find(|n| **n == name) else { return };
    let d = p.map_rect(rect);
    if d.is_empty() {
        return;
    }
    let (fx, fy) = (d.x - d.x.floor(), d.y - d.y.floor());
    let key = Key {
        name,
        w: (d.w * 4.0).round() as u32,
        h: (d.h * 4.0).round() as u32,
        color,
        fx: (fx * 4.0).round() as u8,
        fy: (fy * 4.0).round() as u8,
    };
    let cached = {
        let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
        cache.get_or_insert_with(HashMap::new).get(&key).cloned()
    };
    let img = match cached {
        Some(img) => img,
        None => {
            let Some(img) = render(name, key) else { return };
            let img = Arc::new(img);
            let mut cache = CACHE.lock().unwrap_or_else(|e| e.into_inner());
            let map = cache.get_or_insert_with(HashMap::new);
            if map.len() > 512 {
                map.clear();
            }
            map.insert(key, img.clone());
            img
        }
    };
    p.blit_image(&img, d.x.floor() as i32, d.y.floor() as i32);
}

fn render(name: &'static str, key: Key) -> Option<Image> {
    let (w, h) = (key.w as f32 / 4.0, key.h as f32 / 4.0);
    let (fx, fy) = (key.fx as f32 / 4.0, key.fy as f32 / 4.0);
    let mut img = Image::new((w + fx).ceil() as u32 + 2, (h + fy).ceil() as u32 + 2).ok()?;
    {
        let mut p = Painter::new(img.pixmap_mut(), 1.0);
        p.set_transform(Transform::from_row(w / 24.0, 0.0, 0.0, h / 24.0, fx, fy));
        let pen = Pen::new(key.color, 1.9).with_cap(Cap::Round).with_join(Join::Round);
        let mut ctx = Ctx { p: &mut p, pen, color: key.color };
        draw_icon(&mut ctx, name);
    }
    Some(img)
}

/// Draw an icon straight onto `p` (no cache): for one-off pictures like
/// the tray icon.
pub fn draw_direct(p: &mut Painter<'_>, name: &str, rect: Rect, color: Color) {
    p.save();
    p.translate(rect.x, rect.y);
    p.scale_by(rect.w / 24.0, rect.h / 24.0);
    let pen = Pen::new(color, 1.9).with_cap(Cap::Round).with_join(Join::Round);
    let mut ctx = Ctx { p, pen, color };
    draw_icon(&mut ctx, name);
    p.restore();
}

