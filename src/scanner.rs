//! Find QR codes and barcodes in a capture, off the UI thread.
//!
//! What's on screen is untrusted input, so the decoders are memory-safe
//! Rust (rxing). The work is split into small jobs shared out among the
//! cores: each 2D format on its own, as it is and light on dark (as in dark
//! themes), and the 1D codes in bands of rows. All read the same grey
//! picture (no copies, but for the light-on-dark one); a decoder that panics
//! on a strange picture loses only its own job's codes.
//!
//! (One reader for all the 2D formats, also trying each light on dark, takes
//! three or four times as long as the same done apart: every place it
//! looks, it tries them all, twice.)

use std::borrow::Cow;
use std::collections::HashSet;
use std::panic::{AssertUnwindSafe, catch_unwind};
use std::sync::atomic::{AtomicUsize, Ordering};

use rxing::common::HybridBinarizer;
use rxing::multi::{GenericMultipleBarcodeReader, MultipleBarcodeReader};
use rxing::{BarcodeFormat as F, BinaryBitmap, DecodeHints, Luma8Source, MultiUseMultiFormatReader};

use crate::geom::Point;
use crate::image::Image;
use crate::ui::overlay::Code;

/// The 2D formats, each scanned apart, also light on dark.
const MATRIX: &[F] = &[F::QR_CODE, F::DATA_MATRIX, F::AZTEC, F::PDF_417];

/// The 1D formats: EAN / UPC, and the others (dark on light).
const BARS: &[&[F]] = &[&[F::EAN_13, F::EAN_8, F::UPC_A, F::UPC_E], &[F::CODE_128, F::CODE_39, F::CODE_93, F::CODABAR, F::ITF, F::RSS_14, F::RSS_EXPANDED]];

/// The tallest band of rows the 1D codes are looked for in at once: a
/// barcode's bars run top to bottom, so any row across it reads it.
const BAND: u32 = 720;

/// One job: `formats` in rows `rows` (of the light-on-dark picture, `inverted`).
struct Job {
    formats: &'static [F],
    inverted: bool,
    rows: (u32, u32),
}

/// Longest text kept from a code (a QR code holds at most about 7000).
const MAX_TEXT: usize = 8192;

/// The codes in `img`, corners in its pixels. Empty when there are none,
/// or the picture is too small to hold one.
pub fn scan(img: &Image) -> Vec<Code> {
    let (w, h) = img.size();
    if w < 21 || h < 21 {
        return Vec::new();
    }
    let luma = img.to_luma();
    scan_luma(&luma, w, h)
}

/// The same for a grey picture, one byte a pixel.
pub fn scan_luma(luma: &[u8], w: u32, h: u32) -> Vec<Code> {
    if luma.len() < w as usize * h as usize {
        return Vec::new();
    }
    let luma = &luma[..w as usize * h as usize];
    let inverted: Vec<u8> = luma.iter().map(|v| 255 - v).collect();
    let jobs = jobs(h);
    let next = AtomicUsize::new(0);
    let threads = std::thread::available_parallelism().map_or(4, |n| n.get()).clamp(1, jobs.len());
    let found: Vec<Vec<Code>> = std::thread::scope(|s| {
        let handles: Vec<_> = (0..threads)
            .map(|_| {
                s.spawn(|| {
                    let mut codes = Vec::new();
                    while let Some(job) = jobs.get(next.fetch_add(1, Ordering::Relaxed)) {
                        let picture = if job.inverted { &inverted[..] } else { luma };
                        codes.extend(catch_unwind(AssertUnwindSafe(|| scan_job(picture, luma, w, job))).unwrap_or_default());
                    }
                    codes
                })
            })
            .collect();
        handles.into_iter().map(|t| t.join().unwrap_or_default()).collect()
    });
    let mut seen = HashSet::new();
    let mut codes = Vec::new();
    for code in found.into_iter().flatten() {
        let c = code_box_center(&code.corners);
        // the same code found twice (by its text and where it is)
        if seen.insert((code.text.clone(), (c.x / 8.0) as i32, (c.y / 8.0) as i32)) {
            codes.push(code);
        }
    }
    codes
}

/// The jobs for a picture `h` rows tall, the longest first (QR codes, the
/// ones most often found, take longest).
fn jobs(h: u32) -> Vec<Job> {
    let mut jobs = Vec::new();
    for inverted in [false, true] {
        jobs.push(Job { formats: &MATRIX[..1], inverted, rows: (0, h) });
    }
    // Bands overlapping a little, so a short barcode on the line between
    // two is whole in one of them.
    let bands = h.div_ceil(BAND).max(1);
    let step = h.div_ceil(bands);
    for formats in BARS {
        for i in 0..bands {
            let (a, b) = ((i * step).saturating_sub(32), ((i + 1) * step + 32).min(h));
            jobs.push(Job { formats, inverted: false, rows: (a, b) });
        }
    }
    for i in 1..MATRIX.len() {
        for inverted in [false, true] {
            jobs.push(Job { formats: &MATRIX[i..=i], inverted, rows: (0, h) });
        }
    }
    jobs
}

fn code_box_center(corners: &[Point]) -> Point {
    let n = corners.len().max(1) as f32;
    let (sx, sy) = corners.iter().fold((0.0, 0.0), |(x, y), p| (x + p.x, y + p.y));
    Point::new(sx / n, sy / n)
}

/// One job's codes, corners in the whole picture's pixels. `picture` is
/// what's read (`luma`, or it light on dark).
fn scan_job(picture: &[u8], luma: &[u8], w: u32, job: &Job) -> Vec<Code> {
    let (y0, y1) = job.rows;
    let h = luma.len() as u32 / w;
    let band = &picture[y0 as usize * w as usize..y1 as usize * w as usize];
    let Ok(source) = Luma8Source::new_with_slice(band, w, y1 - y0) else { return Vec::new() };
    let mut bitmap = BinaryBitmap::new(HybridBinarizer::new(source));
    let hints = DecodeHints { PossibleFormats: Some(job.formats.iter().copied().collect()), TryHarder: Some(true), ..Default::default() };
    let mut reader = GenericMultipleBarcodeReader::new(MultiUseMultiFormatReader::default());
    let Ok(results) = reader.decode_multiple_with_hints(&mut bitmap, &hints) else { return Vec::new() };
    let mut codes = Vec::new();
    for r in results {
        let text = r.getText();
        if text.is_empty() {
            continue;
        }
        let text = if text.len() > MAX_TEXT {
            let mut end = MAX_TEXT;
            while !text.is_char_boundary(end) {
                end -= 1;
            }
            &text[..end]
        } else {
            text
        };
        let pts: Vec<Point> = r.getPoints().iter().map(|p| Point::new(p.x, p.y + y0 as f32)).filter(|p| p.x.is_finite() && p.y.is_finite()).collect();
        if pts.is_empty() {
            continue;
        }
        let corners = corners_of(&pts, luma, w, h);
        codes.push(Code { text: text.to_string(), kind: kind_name(r.getBarcodeFormat()), corners });
    }
    codes
}

/// The four corners of a code from the points the decoder reports: 2D
/// codes give their corners; 1D codes give the line they were read along,
/// which grows up and down over its bars.
fn corners_of(pts: &[Point], luma: &[u8], w: u32, h: u32) -> Vec<Point> {
    let (mut x0, mut y0, mut x1, mut y1) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in pts {
        x0 = x0.min(p.x);
        y0 = y0.min(p.y);
        x1 = x1.max(p.x);
        y1 = y1.max(p.y);
    }
    if pts.len() >= 3 && x1 - x0 >= 4.0 && y1 - y0 >= 4.0 {
        return pts.to_vec();
    }
    if y1 - y0 < 4.0 && x1 - x0 >= 4.0 {
        let (top, bottom) = bars_extent(luma, w, h, x0, x1, (y0 + y1) / 2.0);
        y0 = y0.min(top);
        y1 = y1.max(bottom);
    } else if x1 - x0 < 4.0 && y1 - y0 >= 4.0 {
        // read top to bottom (a barcode on its side)
        let (left, right) = bars_extent_columns(luma, w, h, y0, y1, (x0 + x1) / 2.0);
        x0 = x0.min(left);
        x1 = x1.max(right);
    }
    if x1 - x0 < 4.0 {
        x0 -= 6.0;
        x1 += 6.0;
    }
    if y1 - y0 < 4.0 {
        y0 -= 6.0;
        y1 += 6.0;
    }
    vec![Point::new(x0, y0), Point::new(x1, y0), Point::new(x1, y1), Point::new(x0, y1)]
}

/// How far up and down the bars read along row `y` from `x0` to `x1` go:
/// rows that look the same (black and white in the same places).
fn bars_extent(luma: &[u8], w: u32, h: u32, x0: f32, x1: f32, y: f32) -> (f32, f32) {
    let (w, h) = (w as usize, h as usize);
    let (a, b) = ((x0.max(0.0) as usize).min(w - 1), (x1.max(0.0) as usize).min(w - 1));
    let y = (y.max(0.0) as usize).min(h - 1);
    if b <= a + 4 {
        return (y as f32, y as f32);
    }
    let row = |r: usize| Cow::Borrowed(&luma[r * w + a..=r * w + b]);
    extent(&row(y), h, y, (b - a) * 2, row)
}

fn bars_extent_columns(luma: &[u8], w: u32, h: u32, y0: f32, y1: f32, x: f32) -> (f32, f32) {
    let (w, h) = (w as usize, h as usize);
    let (a, b) = ((y0.max(0.0) as usize).min(h - 1), (y1.max(0.0) as usize).min(h - 1));
    let x = (x.max(0.0) as usize).min(w - 1);
    if b <= a + 4 {
        return (x as f32, x as f32);
    }
    let column = |c: usize| -> Cow<'_, [u8]> { Cow::Owned((a..=b).map(|r| luma[r * w + c]).collect()) };
    extent(&column(x), w, x, (b - a) * 2, column)
}

/// From line `at` (of `count`), how far lines like it go each way, up to
/// `max` lines; `line(i)` reads line i.
fn extent<'a>(first: &[u8], count: usize, at: usize, max: usize, line: impl Fn(usize) -> Cow<'a, [u8]>) -> (f32, f32) {
    let (lo, hi) = first.iter().fold((255u8, 0u8), |(lo, hi), &v| (lo.min(v), hi.max(v)));
    if hi.saturating_sub(lo) < 40 {
        return (at as f32, at as f32);
    }
    let mid = ((lo as u16 + hi as u16) / 2) as u8;
    let pattern: Vec<bool> = first.iter().map(|&v| v < mid).collect();
    let alike = |i: usize| {
        let l = line(i);
        let differ = l.iter().zip(&pattern).filter(|(v, dark)| (**v < mid) != **dark).count();
        differ * 100 <= pattern.len() * 12
    };
    let mut start = at;
    while start > 0 && at - start < max && alike(start - 1) {
        start -= 1;
    }
    let mut end = at;
    while end + 1 < count && end - at < max && alike(end + 1) {
        end += 1;
    }
    (start as f32, end as f32 + 1.0)
}

/// The format's name as zxing-cpp gives it.
fn kind_name(f: &F) -> String {
    match f {
        F::QR_CODE => "QRCode",
        F::DATA_MATRIX => "DataMatrix",
        F::AZTEC => "Aztec",
        F::PDF_417 => "PDF417",
        F::EAN_13 => "EAN13",
        F::EAN_8 => "EAN8",
        F::UPC_A => "UPCA",
        F::UPC_E => "UPCE",
        F::CODE_128 => "Code128",
        F::CODE_39 => "Code39",
        F::CODE_93 => "Code93",
        F::CODABAR => "Codabar",
        F::ITF => "ITF",
        F::RSS_14 => "DataBar",
        F::RSS_EXPANDED => "DataBarExpanded",
        _ => "Code",
    }
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::codec;

    fn asset(name: &str) -> Image {
        codec::decode_file(&std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("assets").join(name)).unwrap()
    }

    /// The self-test's fixtures on a grey desktop, as on a screen.
    pub fn desktop_with_codes() -> Image {
        let mut img = Image::filled(1280, 800, 0xFF5A5A5A).unwrap();
        for (name, x, y) in [("selftest-qr.png", 700, 160), ("selftest-ean13.png", 700, 480)] {
            let fixture = asset(name);
            let mut pad = Image::filled(fixture.width() + 40, fixture.height() + 40, 0xFFFFFFFF).unwrap();
            pad.blit(&fixture, 20, 20);
            img.blit(&pad, x - 20, y - 20);
        }
        img
    }

    #[test]
    fn finds_the_fixtures() {
        let img = desktop_with_codes();
        let t = std::time::Instant::now();
        let mut codes = scan(&img);
        eprintln!("scanned {}x{} in {:?}", img.width(), img.height(), t.elapsed());
        codes.sort_by(|a, b| a.text.cmp(&b.text));
        let texts: Vec<&str> = codes.iter().map(|c| c.text.as_str()).collect();
        assert_eq!(texts, ["4006381333931", "https://github.com/nicholas-sbx/screenshot"]);
        // The QR code's corners are its own; the barcode's box covers its bars.
        let qr = &codes[1];
        let (xs, ys): (Vec<f32>, Vec<f32>) = qr.corners.iter().map(|p| (p.x, p.y)).unzip();
        let min = |v: &[f32]| v.iter().copied().fold(f32::MAX, f32::min);
        let max = |v: &[f32]| v.iter().copied().fold(f32::MIN, f32::max);
        assert!(min(&xs) >= 700.0 && max(&xs) <= 885.0 && min(&ys) >= 160.0 && max(&ys) <= 345.0, "{:?}", qr.corners);
        let ean = &codes[0];
        let (ys,): (Vec<f32>,) = (ean.corners.iter().map(|p| p.y).collect(),);
        assert!(max(&ys) - min(&ys) > 60.0, "the barcode's box covers its bars: {:?}", ean.corners);
        assert_eq!(ean.kind, "EAN13");
    }

    #[test]
    fn light_on_dark_qr() {
        let mut img = desktop_with_codes();
        for px in img.data_mut().as_chunks_mut::<4>().0 {
            for v in &mut px[..3] {
                *v = 255 - *v;
            }
        }
        let codes = scan(&img);
        assert!(codes.iter().any(|c| c.text == "https://github.com/nicholas-sbx/screenshot"), "{codes:?}");
    }

    #[test]
    fn nothing_on_noise() {
        let mut img = Image::new(400, 300).unwrap();
        let mut seed = 0x1234_5678u32;
        for px in img.data_mut().as_chunks_mut::<4>().0 {
            seed ^= seed << 13;
            seed ^= seed >> 17;
            seed ^= seed << 5;
            px.copy_from_slice(&seed.to_le_bytes());
            px[3] = 255;
        }
        assert!(scan(&img).is_empty());
        assert!(scan(&Image::new(10, 10).unwrap()).is_empty());
    }
}
