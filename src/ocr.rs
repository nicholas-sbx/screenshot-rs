//! Copying text from the screen: the picture of an area is handed to
//! Tesseract (when it's installed) and the text it reads comes back.
//! Nothing of it runs, or is held in memory, but while an area is read.

use std::time::Duration;

use crate::image::Image;

/// How long Tesseract may take over an area.
const TIMEOUT: Duration = Duration::from_secs(60);
/// The most pixels handed over, enlarged (Tesseract slows with more).
const MAX_PIXELS: f32 = 12_000_000.0;
/// Screen pixels per logical pixel text is read best at: Tesseract wants
/// letters some 20 pixels high, and a screen's are often 7 to 10.
const READ_SCALE: f32 = 3.0;
/// The margin round the picture (Tesseract misses text touching its edges).
const MARGIN: u32 = 12;
/// The most text kept.
const MAX_TEXT: u64 = 4 << 20;

/// Why text can't be read here, if it can't.
pub fn problem() -> Option<String> {
    (!crate::util::have("tesseract"))
        .then(|| "Copying text needs Tesseract: install tesseract and its English data (tesseract-ocr-eng, or tesseract-data-eng)".into())
}

/// The text in `image` (`dpr` pixels per logical pixel), in `languages`
/// (Tesseract's codes, as "eng" or "eng+deu").
pub fn read(image: &Image, dpr: f32, languages: &str) -> Result<String, String> {
    let mut cmd = crate::util::command("tesseract").ok_or_else(|| problem().unwrap_or_default())?;
    // (a picture on stdin, the text on stdout; runs of spaces are kept, for code and tables)
    cmd.args(["stdin", "stdout", "--psm", "3", "-c", "preserve_interword_spaces=1"]);
    cmd.args(["-l", &crate::config::ocr_languages(languages)]);
    let out = crate::util::run_with_input(&mut cmd, pgm(&prepare(image, dpr)), TIMEOUT, MAX_TEXT).map_err(|e| format!("Tesseract: {e}"))?;
    Ok(tidy(&String::from_utf8_lossy(&out)))
}

/// A grey picture as Tesseract reads text best from it: dark on light
/// (light text on a dark theme is turned round), enlarged so letters are
/// big enough, with a margin of the background round it.
#[derive(Clone, Debug, PartialEq)]
pub struct Grey {
    pub width: u32,
    pub height: u32,
    pub pixels: Vec<u8>,
}

pub fn prepare(image: &Image, dpr: f32) -> Grey {
    let (w, h) = (image.width(), image.height());
    let mut grey: Vec<u8> = Vec::with_capacity(w as usize * h as usize);
    for y in 0..h {
        for px in image.row(y).as_chunks::<4>().0 {
            // (BGRA in memory: Rec. 709 weights, out of 256)
            let (b, g, r) = (px[0] as u32, px[1] as u32, px[2] as u32);
            grey.push(((r * 54 + g * 183 + b * 19) >> 8) as u8);
        }
    }
    // The background is what most of it is: dark, it's turned light.
    let mut counts = [0usize; 256];
    for &v in &grey {
        counts[v as usize] += 1;
    }
    let half = grey.len() / 2;
    let mut seen = 0;
    let median = counts.iter().position(|&n| {
        seen += n;
        seen > half
    });
    let mut background = median.unwrap_or(255) as u8;
    if background < 128 {
        for v in &mut grey {
            *v = 255 - *v;
        }
        background = 255 - background;
    }
    let k = (READ_SCALE / dpr.max(0.25)).min((MAX_PIXELS / (w as f32 * h as f32).max(1.0)).sqrt()).max(1.0);
    let (grey, w, h) = if k > 1.05 { enlarge(&grey, w, h, k) } else { (grey, w, h) };
    let (ow, oh) = (w + 2 * MARGIN, h + 2 * MARGIN);
    let mut pixels = vec![background; ow as usize * oh as usize];
    for y in 0..h as usize {
        let at = (y + MARGIN as usize) * ow as usize + MARGIN as usize;
        pixels[at..at + w as usize].copy_from_slice(&grey[y * w as usize..(y + 1) * w as usize]);
    }
    Grey { width: ow, height: oh, pixels }
}

/// `grey` (w x h) `k` times as large, smoothly.
fn enlarge(grey: &[u8], w: u32, h: u32, k: f32) -> (Vec<u8>, u32, u32) {
    let (nw, nh) = (((w as f32 * k).round() as u32).max(1), ((h as f32 * k).round() as u32).max(1));
    let (sx, sy) = (w as f32 / nw as f32, h as f32 / nh as f32);
    // Each column's two source columns and how much of the second.
    let cols: Vec<(usize, usize, u32)> = (0..nw)
        .map(|x| {
            let fx = ((x as f32 + 0.5) * sx - 0.5).clamp(0.0, (w - 1) as f32);
            let x0 = fx.floor() as usize;
            (x0, (x0 + 1).min(w as usize - 1), ((fx - x0 as f32) * 256.0) as u32)
        })
        .collect();
    let mut out = Vec::with_capacity(nw as usize * nh as usize);
    for y in 0..nh {
        let fy = ((y as f32 + 0.5) * sy - 0.5).clamp(0.0, (h - 1) as f32);
        let y0 = fy.floor() as usize;
        let y1 = (y0 + 1).min(h as usize - 1);
        let wy = ((fy - y0 as f32) * 256.0) as u32;
        let (r0, r1) = (&grey[y0 * w as usize..][..w as usize], &grey[y1 * w as usize..][..w as usize]);
        for &(x0, x1, wx) in &cols {
            let top = r0[x0] as u32 * (256 - wx) + r0[x1] as u32 * wx;
            let bottom = r1[x0] as u32 * (256 - wx) + r1[x1] as u32 * wx;
            out.push(((top * (256 - wy) + bottom * wy + (1 << 15)) >> 16) as u8);
        }
    }
    (out, nw, nh)
}

/// The picture as a PGM file (which Tesseract reads from stdin).
fn pgm(g: &Grey) -> Vec<u8> {
    let mut out = format!("P5\n{} {}\n255\n", g.width, g.height).into_bytes();
    out.extend_from_slice(&g.pixels);
    out
}

/// Tesseract's text tidied: no spaces at the ends of lines, no page
/// break, at most one empty line in a row, none at either end.
pub fn tidy(text: &str) -> String {
    let mut out: Vec<&str> = Vec::new();
    for line in text.split('\n').map(|l| l.trim_end_matches(|c: char| c.is_whitespace() || c == '\u{c}')) {
        if line.is_empty() && out.last().is_none_or(|l| l.is_empty()) {
            continue;
        }
        out.push(line);
    }
    while out.last().is_some_and(|l| l.is_empty()) {
        out.pop();
    }
    out.join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dark_themes_are_turned_light_and_small_text_enlarged() {
        // A dark picture with a light mark: inverted, three times as large
        // at a scale of 1, with the margin round it in the background.
        let mut img = Image::filled(20, 10, 0xFF20_2020).unwrap();
        img.set_pixel(5, 5, 0xFFF0_F0F0);
        let g = prepare(&img, 1.0);
        assert_eq!((g.width, g.height), (60 + 2 * MARGIN, 30 + 2 * MARGIN));
        assert_eq!(g.pixels[0], 255 - 0x20);
        let mark = g.pixels[(MARGIN as usize + 16) * g.width as usize + MARGIN as usize + 16];
        assert!(mark < 64, "{mark}");
        // Light already, at a scale of 3: as it is.
        let img = Image::filled(20, 10, 0xFFFF_FFFF).unwrap();
        let g = prepare(&img, 3.0);
        assert_eq!((g.width, g.height), (20 + 2 * MARGIN, 10 + 2 * MARGIN));
        assert!(g.pixels.iter().all(|&v| v == 255));
        // Huge areas aren't enlarged past what Tesseract reads quickly.
        let img = Image::filled(4000, 2000, 0xFFFF_FFFF).unwrap();
        let g = prepare(&img, 1.0);
        assert!((g.width as f32 * g.height as f32) < MAX_PIXELS * 1.05);
        assert!(g.width > 4000);
    }

    #[test]
    fn text_is_tidied() {
        assert_eq!(tidy("Hello  world  \n\n\n\nSecond line\n\n\u{c}"), "Hello  world\n\nSecond line");
        assert_eq!(tidy("\n\n  indented\n"), "  indented");
        assert_eq!(tidy("\u{c}"), "");
    }

    #[test]
    fn reads_text_when_tesseract_is_there() {
        if problem().is_some() {
            return;
        }
        // "FLATSHOT" drawn in our own font, light on dark, at a scale of 1.
        crate::render::text::warm_up();
        let mut img = Image::filled(260, 40, 0xFF1E_1E24).unwrap();
        {
            let mut p = crate::render::Painter::new(img.pixmap_mut(), 1.0);
            let font = crate::render::text::font(18.0, crate::render::text::NORMAL);
            p.draw_text(
                crate::geom::Rect::new(10.0, 0.0, 240.0, 40.0),
                crate::render::paint::Align::Left,
                "Copy this text",
                font,
                crate::render::theme::Color::rgb(0xEEEEEE),
            );
        }
        let text = read(&img, 1.0, "eng").expect("read");
        assert_eq!(text, "Copy this text");
    }
}
