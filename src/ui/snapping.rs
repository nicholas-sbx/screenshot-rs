//! Snapping a selection to edges in the picture. Edges that look like
//! parts of boxes win: long straight runs, and above all a vertical and a
//! horizontal one that meet (a box's corner, sharp or rounded).

use crate::image::Image;

/// An edge is looked at this far (logical px) either side of the pointer
/// along its length.
pub const SNAP_REACH: f32 = 40.0;
/// A vertical and a horizontal edge meeting: both ending there (an L, a
/// box's corner), one ending on the other (a T), or crossing.
const CORNER_BONUS: f64 = 1.6;
const TEE_BONUS: f64 = 1.25;
const CROSS_BONUS: f64 = 1.1;
/// A box's corner may be rounded up to this radius (logical px).
pub const ROUNDED_UP_TO: f32 = 24.0;

/// Python's round(): halves go to the even neighbour.
pub fn py_round(v: f32) -> i32 {
    v.round_ties_even() as i32
}

/// Two maps of the picture, one byte a pixel: how much each pixel differs
/// from the one to its left (vertical edges), and from the one above
/// (horizontal edges), in grey levels.
pub struct EdgeMaps {
    pub w: usize,
    pub h: usize,
    pub v: Vec<u8>,
    pub hz: Vec<u8>,
}

#[inline]
fn gray(b: i32, g: i32, r: i32) -> u8 {
    ((r * 11 + g * 16 + b * 5) >> 5) as u8
}

impl EdgeMaps {
    /// About 20 ms for a 4K screen (the Python version took 150).
    pub fn build(img: &Image) -> EdgeMaps {
        let (w, h) = (img.width() as usize, img.height() as usize);
        let mut v = vec![0u8; w * h];
        let mut hz = vec![0u8; w * h];
        let data = img.data();
        let stride = w * 4;
        for y in 0..h {
            let row = &data[y * stride..(y + 1) * stride];
            let above = if y > 0 { Some(&data[(y - 1) * stride..y * stride]) } else { None };
            let vrow = &mut v[y * w..(y + 1) * w];
            let hrow = &mut hz[y * w..(y + 1) * w];
            for x in 0..w {
                let p = &row[x * 4..x * 4 + 4];
                // (Where nothing was drawn over it, the difference is the pixel itself.)
                vrow[x] = if x > 0 {
                    let q = &row[(x - 1) * 4..x * 4];
                    gray((p[0] as i32 - q[0] as i32).abs(), (p[1] as i32 - q[1] as i32).abs(), (p[2] as i32 - q[2] as i32).abs())
                } else {
                    gray(p[0] as i32, p[1] as i32, p[2] as i32)
                };
                hrow[x] = match above {
                    Some(a) => {
                        let q = &a[x * 4..x * 4 + 4];
                        gray((p[0] as i32 - q[0] as i32).abs(), (p[1] as i32 - q[1] as i32).abs(), (p[2] as i32 - q[2] as i32).abs())
                    }
                    None => gray(p[0] as i32, p[1] as i32, p[2] as i32),
                };
            }
        }
        EdgeMaps { w, h, v, hz }
    }
}

/// A sampled range (start, stop, step), as Python's range.
#[derive(Clone, Copy, Debug)]
struct Range {
    start: i64,
    stop: i64,
    step: i64,
}

impl Range {
    fn len(&self) -> usize {
        if self.stop <= self.start { 0 } else { ((self.stop - self.start + self.step - 1) / self.step) as usize }
    }
    fn get(&self, i: usize) -> i64 {
        self.start + i as i64 * self.step
    }
}

/// An edge candidate: (score, position, run start, run end, the start is
/// an end, the end is an end, loose).
#[derive(Clone, Copy, Debug, PartialEq)]
struct Cand {
    score: f64,
    pos: i64,
    start: i64,
    end: i64,
    start_ends: bool,
    end_ends: bool,
    loose: bool,
}

impl Cand {
    fn key(&self) -> (f64, i64, i64, i64, bool, bool, bool) {
        (self.score, self.pos, self.start, self.end, self.start_ends, self.end_ends, self.loose)
    }
}

/// Edge candidates across the pointer: `lines` are the edge strengths
/// along each candidate position (`first`, `first` + 1, ...), sampled at
/// `along`. See the Python version's `_candidates`.
#[allow(clippy::too_many_arguments)]
fn candidates(lines: &[Vec<u8>], first: i64, at: i64, along: Range, at_along: i64, radius: i64, strong: i64, min_run: i64, rounding: i64) -> Vec<Cand> {
    let n = along.len();
    let at_index = (0..n).min_by_key(|&i| (along.get(i) - at_along).abs()).unwrap_or(0) as i64;
    let near = (radius / along.step.max(1)).max(1);
    let mut found = Vec::new();
    for (i, values) in lines.iter().enumerate() {
        let mut best: Option<(f64, i64, i64, bool, bool, bool)> = None;
        let mut start: Option<i64> = None;
        let mut end = 0i64;
        let (mut total, mut count, mut gap) = (0i64, 0i64, 0i64);
        let padded = values.iter().copied().chain([0u8, 0u8]);
        for (j, v) in padded.enumerate() {
            let j = j as i64;
            if v as i64 >= strong {
                if start.is_none() {
                    start = Some(j);
                    total = 0;
                    count = 0;
                }
                end = j;
                gap = 0;
                total += v as i64;
                count += 1;
            } else if let Some(s) = start {
                gap += 1;
                if gap > 1 {
                    // (a one-sample gap doesn't break a run)
                    let length = end - s + 1;
                    let reaches = s - near <= at_index && at_index <= end + near;
                    let loose = !reaches && s - near - rounding <= at_index && at_index <= end + near + rounding;
                    if length >= min_run && (reaches || loose) {
                        let score = total as f64 / count as f64 * (0.5 + 0.5 * (length as f64 / n as f64).min(1.0));
                        let better = match &best {
                            None => true,
                            Some(b) => (!loose, score) > (!b.5, b.0),
                        };
                        if better {
                            best = Some((score, along.get(s as usize), along.get(end as usize), s > 0, end < n as i64 - 1, loose));
                        }
                    }
                    start = None;
                }
            }
        }
        if let Some((score, s, e, se, ee, loose)) = best {
            let pos = first + i as i64;
            let score = score * (1.0 - 0.4 * (pos - at).abs() as f64 / (radius + 1) as f64);
            found.push(Cand { score, pos, start: s, end: e, start_ends: se, end_ends: ee, loose });
        }
    }
    found.sort_by(|a, b| b.key().partial_cmp(&a.key()).unwrap_or(std::cmp::Ordering::Equal));
    found.truncate(3);
    found
}

struct Edges<'a> {
    maps: &'a EdgeMaps,
    strong: i64,
}

impl Edges<'_> {
    /// Is there an edge at (cx, cy), or within `spread`? In the
    /// vertical-edge map (0), the horizontal one (1) or either.
    fn at(&self, cx: f64, cy: f64, which: &[usize], spread: i64, faint: bool) -> bool {
        let (cx, cy) = (cx.round_ties_even() as i64, cy.round_ties_even() as i64);
        let need = if faint { self.strong / 2 } else { self.strong };
        let (w, h) = (self.maps.w as i64, self.maps.h as i64);
        for row in (cy - spread).max(0)..(cy + spread + 1).min(h) {
            for &i in which {
                let buf = if i == 0 { &self.maps.v } else { &self.maps.hz };
                let lo = (cx - spread).max(0);
                let hi = (cx + spread + 1).min(w);
                if lo >= hi {
                    continue;
                }
                let slice = &buf[(row * w + lo) as usize..(row * w + hi) as usize];
                if slice.iter().copied().max().unwrap_or(0) as i64 >= need {
                    return true;
                }
            }
        }
        false
    }
}

/// The best vertical and horizontal edge together: the pair with the
/// highest score, boosted where the two meet, most where both end there
/// (the corner of a box, sharp or rounded).
fn pick(xs: &[Cand], ys: &[Cand], tolerance: i64, rounding: i64, edges: &Edges<'_>) -> (Option<i64>, Option<i64>) {
    let meets = |e: &Cand, at: i64| -> (bool, bool) {
        let reaches = e.start - tolerance <= at && at <= e.end + tolerance;
        let ends = (e.start_ends && (e.start - at).abs() <= tolerance) || (e.end_ends && (e.end - at).abs() <= tolerance);
        (reaches, ends)
    };
    let short = |e: &Cand, at: i64| -> (i64, i64) {
        if e.start_ends && at < e.start {
            return (e.start - at, 1);
        }
        if e.end_ends && at > e.end {
            return (at - e.end, -1);
        }
        (0, 0)
    };
    let rounded = |vx: &Cand, hy: &Cand| -> bool {
        let (x, y) = (vx.pos, hy.pos);
        let (down, dy) = short(vx, y);
        let (across, dx) = short(hy, x);
        if down.min(across) <= tolerance || down.max(across) > rounding {
            return false;
        }
        if ((down - across).abs() as f64) > (tolerance as f64).max((down + across) as f64 / 6.0) {
            return false;
        }
        let r = (down + across) as f64 / 2.0;
        let span = 2i64.max((r / 2.0).round_ties_even() as i64);
        let past = (tolerance + 1)..(tolerance + 1 + span);
        let n = past.end - past.start;
        let mut hits = 0;
        for k in past.clone() {
            hits += edges.at((x - dx * k) as f64, y as f64, &[1], 0, false) as i64;
        }
        for k in past {
            hits += edges.at(x as f64, (y - dy * k) as f64, &[0], 0, false) as i64;
        }
        if hits * 3 > 2 * n {
            return false; // a line carries on through it
        }
        let inset = r * (1.0 - 0.5f64.sqrt()); // the middle of the curve, in from the corner
        edges.at(x as f64 + dx as f64 * inset, y as f64 + dy as f64 * inset, &[0, 1], 1, true)
    };
    let mut best = (None, None);
    let mut best_score = 0.0f64;
    let xs_opt: Vec<Option<&Cand>> = xs.iter().map(Some).chain([None]).collect();
    let ys_opt: Vec<Option<&Cand>> = ys.iter().map(Some).chain([None]).collect();
    for vx in &xs_opt {
        for hy in &ys_opt {
            let round_corner = match (vx, hy) {
                (Some(v), Some(h)) if rounding > 0 => rounded(v, h),
                _ => false,
            };
            if !round_corner && (vx.is_some_and(|v| v.loose) || hy.is_some_and(|h| h.loose)) {
                continue; // one that stops short counts only as part of a rounded corner
            }
            let mut score = vx.map_or(0.0, |v| v.score) + hy.map_or(0.0, |h| h.score);
            if round_corner {
                score *= CORNER_BONUS;
            } else if let (Some(v), Some(h)) = (vx, hy) {
                let (reach_v, end_v) = meets(v, h.pos);
                let (reach_h, end_h) = meets(h, v.pos);
                if reach_v && reach_h {
                    score *= if end_v && end_h {
                        CORNER_BONUS
                    } else if end_v || end_h {
                        TEE_BONUS
                    } else {
                        CROSS_BONUS
                    };
                }
            }
            if score > best_score {
                best = (vx.map(|v| v.pos), hy.map(|h| h.pos));
                best_score = score;
            }
        }
    }
    best
}

/// Where (`x`, `y`) (logical, at `dpr` picture pixels each) snaps to, in
/// logical coordinates: either axis may stay as it was.
pub fn snap(maps: &EdgeMaps, x: f32, y: f32, dpr: f32, distance: i64, sensitivity: i64) -> (f32, f32) {
    let (w, h) = (maps.w as i64, maps.h as i64);
    let px = (x * dpr) as i64;
    let py = (y * dpr) as i64;
    if !(0 <= px && px < w && 0 <= py && py < h) || x < 0.0 || y < 0.0 {
        return (x, y);
    }
    let sens = sensitivity.clamp(1, 10);
    let radius = py_round(distance as f32 * dpr).max(2) as i64;
    let reach = py_round(SNAP_REACH * dpr).max(8) as i64;
    let step = py_round(dpr).max(1) as i64; // about one logical pixel apart along an edge
    let rounding = py_round(ROUNDED_UP_TO * dpr) as i64;
    let strong = 10 + (10 - sens) * 4; // how much a pixel must differ across the edge
    let min_run = (py_round((8.0 + (10 - sens) as f32 * 2.2) * dpr / step as f32) as i64).max(3);

    // Vertical edges near x: each candidate column, sampled down the rows around y.
    let (c0, c1) = ((px - radius).max(1), (px + radius + 1).min(w));
    let rows = Range { start: (py - reach).max(0), stop: (py + reach + 1).min(h), step };
    let columns: Vec<Vec<u8>> = (c0..c1).map(|c| (0..rows.len()).map(|i| maps.v[(rows.get(i) * w + c) as usize]).collect()).collect();
    let xs = candidates(&columns, c0, px, rows, py, radius, strong, min_run, rounding / step);
    // Horizontal edges near y: each candidate row, sampled along the columns around x.
    let (r0, r1) = ((py - radius).max(1), (py + radius + 1).min(h));
    let cols = Range { start: (px - reach).max(0), stop: (px + reach + 1).min(w), step };
    let lines: Vec<Vec<u8>> = (r0..r1).map(|r| (0..cols.len()).map(|i| maps.hz[(r * w + cols.get(i)) as usize]).collect()).collect();
    let ys = candidates(&lines, r0, py, cols, px, radius, strong, min_run, rounding / step);

    let edges = Edges { maps, strong };
    let (sx, sy) = pick(&xs, &ys, (step * 2).max(2), rounding, &edges);
    (sx.map_or(x, |v| v as f32 / dpr), sy.map_or(y, |v| v as f32 / dpr))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A white picture with a dark box (20..80 x 30..70).
    fn boxed() -> Image {
        let mut img = Image::filled(120, 100, 0xFFFFFFFF).unwrap();
        for y in 30..70 {
            for x in 20..80 {
                img.set_pixel(x, y, 0xFF202020);
            }
        }
        img
    }

    #[test]
    fn snaps_to_a_box_corner() {
        let maps = EdgeMaps::build(&boxed());
        // Near the top-left corner: both axes snap to it.
        let (x, y) = snap(&maps, 23.0, 27.0, 1.0, 10, 5);
        assert_eq!((x, y), (20.0, 30.0));
        // Far from any edge: stays.
        let (x, y) = snap(&maps, 100.0, 10.0, 1.0, 10, 5);
        assert_eq!((x, y), (100.0, 10.0));
        // Near the right edge, mid-height: x snaps, y stays.
        let (x, y) = snap(&maps, 76.0, 50.0, 1.0, 10, 5);
        assert_eq!((x, y), (80.0, 50.0));
    }

    #[test]
    fn rounding_is_pythons() {
        assert_eq!(py_round(2.5), 2);
        assert_eq!(py_round(3.5), 4);
        assert_eq!(py_round(12.5), 12);
    }
}
