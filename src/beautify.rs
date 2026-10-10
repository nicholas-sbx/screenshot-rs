//! Beautify: a capture on a background, with padding, rounded corners and a
//! soft shadow, for docs, slides and posts. Used by the annotation editor's
//! Background panel (with a live preview) and as an after-capture step (the
//! last style used, without any window).
//!
//! Sizes in a Style are logical pixels; `scale` (pixels per logical one)
//! makes them as big on a HiDPI capture as they look on screen. The result
//! is at the picture's full resolution.

use serde_json::{Map, Value};
use tiny_skia::{GradientStop, LinearGradient, Mask, PixmapPaint, SpreadMode, Transform};

use crate::geom::{IPoint, Rect};
use crate::image::Image;
use crate::render::Painter;
use crate::render::paint::{rounded_rect_path, sk};
use crate::render::theme::Color;

/// (key, label): what's behind the picture. "edge": the picture's own edge
/// colour.
pub const BACKGROUNDS: &[(&str, &str)] =
    &[("none", "Clear"), ("color", "Colour"), ("edge", "Edge colour"), ("gradient", "Gradient"), ("blur", "Blurred copy"), ("image", "Image")];
/// (key, label, width / height): the whole's shape, grown to it, never cut.
pub const RATIOS: &[(&str, &str, Option<f32>)] =
    &[("auto", "As the picture", None), ("1:1", "Square, 1:1", Some(1.0)), ("4:3", "4:3", Some(4.0 / 3.0)), ("16:9", "16:9", Some(16.0 / 9.0))];
/// Gradients from the themes' own colours: (from, to), top left to bottom right.
pub const GRADIENTS: &[(&str, u32, u32)] = &[
    ("ember", 0xFF6A3D, 0xFFC53D),
    ("graphite", 0x4C9EFF, 0x3DDC97),
    ("nord", 0x88C0D0, 0x5E81AC),
    ("dusk", 0xC08CFF, 0xFF6A9E),
    ("paper", 0xF5F4F0, 0xD2CEC4),
    ("night", 0x24242E, 0x121218),
];

#[derive(Clone, Debug, PartialEq)]
pub struct Style {
    /// see BACKGROUNDS
    pub background: String,
    /// for "color"
    pub color: String,
    /// see GRADIENTS
    pub gradient: String,
    /// a picture file, for "image"
    pub image: String,
    /// round the picture: logical px, or % of its longer side with padding_percent
    pub padding: i64,
    pub padding_percent: bool,
    /// its corners
    pub radius: i64,
    /// how soft (0: none)
    pub shadow: i64,
    /// downwards
    pub shadow_offset: i64,
    /// %
    pub shadow_opacity: i64,
    /// the whole: see RATIOS
    pub ratio: String,
}

impl Default for Style {
    fn default() -> Style {
        Style {
            background: "gradient".into(),
            color: "#1A1A22".into(),
            gradient: "ember".into(),
            image: String::new(),
            padding: 64,
            padding_percent: false,
            radius: 12,
            shadow: 24,
            shadow_offset: 10,
            shadow_opacity: 45,
            ratio: "auto".into(),
        }
    }
}

impl Style {
    /// From the config's form (unknown keys left out, odd values to the defaults).
    pub fn from_map(data: &Map<String, Value>) -> Style {
        let d = Style::default();
        let s = |k: &str, def: &str| data.get(k).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| def.to_string());
        let i = |k: &str, def: i64| data.get(k).and_then(|v| v.as_i64().or_else(|| v.as_f64().map(|f| f as i64))).unwrap_or(def);
        let mut style = Style {
            background: s("background", &d.background),
            color: s("color", &d.color),
            gradient: s("gradient", &d.gradient),
            image: s("image", &d.image),
            padding: i("padding", d.padding),
            padding_percent: data.get("padding_percent").and_then(Value::as_bool).unwrap_or(d.padding_percent),
            radius: i("radius", d.radius),
            shadow: i("shadow", d.shadow),
            shadow_offset: i("shadow_offset", d.shadow_offset),
            shadow_opacity: i("shadow_opacity", d.shadow_opacity),
            ratio: s("ratio", &d.ratio),
        };
        style.clean();
        style
    }

    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        m.insert("background".into(), self.background.clone().into());
        m.insert("color".into(), self.color.clone().into());
        m.insert("gradient".into(), self.gradient.clone().into());
        m.insert("image".into(), self.image.clone().into());
        m.insert("padding".into(), self.padding.into());
        m.insert("padding_percent".into(), self.padding_percent.into());
        m.insert("radius".into(), self.radius.into());
        m.insert("shadow".into(), self.shadow.into());
        m.insert("shadow_offset".into(), self.shadow_offset.into());
        m.insert("shadow_opacity".into(), self.shadow_opacity.into());
        m.insert("ratio".into(), self.ratio.clone().into());
        m
    }

    pub fn clean(&mut self) {
        if !BACKGROUNDS.iter().any(|b| b.0 == self.background) {
            self.background = "gradient".into();
        }
        if !GRADIENTS.iter().any(|g| g.0 == self.gradient) {
            self.gradient = "ember".into();
        }
        if !RATIOS.iter().any(|r| r.0 == self.ratio) {
            self.ratio = "auto".into();
        }
        if Color::parse(&self.color).is_none() {
            self.color = "#1A1A22".into();
        }
        self.padding = self.padding.clamp(0, 400);
        self.radius = self.radius.clamp(0, 64);
        self.shadow = self.shadow.clamp(0, 96);
        self.shadow_offset = self.shadow_offset.clamp(0, 64);
        self.shadow_opacity = self.shadow_opacity.clamp(0, 100);
    }
}

/// Already cut out with transparency round it (a window KWin took on its
/// own, with its shadow and rounded corners): no second shadow or corners
/// are added.
pub fn has_own_shape(img: &Image) -> bool {
    let (w, h) = img.size();
    if w < 2 || h < 2 {
        return false;
    }
    [(0, 0), (w - 1, 0), (0, h - 1), (w - 1, h - 1)].iter().all(|&(x, y)| img.pixel(x, y) >> 24 < 8)
}

/// The whole picture's size, and where the capture goes in it (pixels).
pub fn layout(size: (u32, u32), style: &Style, scale: f32) -> ((u32, u32), IPoint) {
    let (w, h) = (size.0 as i64, size.1 as i64);
    let pad = if style.padding_percent {
        (w.max(h) as f64 * style.padding as f64 / 100.0).round() as i64
    } else {
        (style.padding as f64 * scale as f64).round() as i64
    };
    let (mut ow, mut oh) = (w + 2 * pad, h + 2 * pad);
    if let Some(ratio) = RATIOS.iter().find(|r| r.0 == style.ratio).and_then(|r| r.2) {
        if (ow as f32 / oh as f32) < ratio {
            ow = (oh as f32 * ratio).round() as i64;
        } else {
            oh = (ow as f32 / ratio).round() as i64;
        }
    }
    let (ow, oh) = (ow.clamp(1, crate::image::MAX_SIDE as i64), oh.clamp(1, crate::image::MAX_SIDE as i64));
    // (on whole pixels: not resampled)
    (((ow) as u32, oh as u32), IPoint::new(((ow - w) / 2) as i32, ((oh - h) / 2) as i32))
}

/// `img` blurred: shrunk `k` times and grown back smoothly, twice (as the
/// Blur drawing does, quick even when big).
fn soft(img: &Image, k: f32) -> Result<Image, String> {
    if k <= 1.0 {
        return Ok(img.clone());
    }
    let (w, h) = img.size();
    let small = img.scaled_area(((w as f32 / k).round() as u32).max(1), ((h as f32 / k).round() as u32).max(1))?;
    let smaller = small.scaled_area((small.width() / 2).max(1), (small.height() / 2).max(1))?;
    smaller.scaled_bilinear(small.width(), small.height())?.scaled_bilinear(w, h)
}

/// The colour most of the picture's border has (to pad with, so the
/// margin looks like more of the picture).
pub fn edge_color(img: &Image) -> Color {
    let (w, h) = img.size();
    let step = ((w + h) / 400).max(1);
    let mut counts: std::collections::HashMap<(u8, u8, u8), u32> = std::collections::HashMap::new();
    let mut count = |x: u32, y: u32| {
        let c = Color::from_premul(img.pixel(x, y));
        *counts.entry((c.r / 8, c.g / 8, c.b / 8)).or_default() += 1;
    };
    for x in (0..w).step_by(step as usize) {
        count(x, 0);
        count(x, h - 1);
    }
    for y in (0..h).step_by(step as usize) {
        count(0, y);
        count(w - 1, y);
    }
    let (r, g, b) = counts.into_iter().max_by_key(|(k, n)| (*n, *k)).map(|(k, _)| k).unwrap_or((0, 0, 0));
    let up = |v: u8| (v as u32 * 8 + 4).min(255) as u8;
    Color::rgba(up(r), up(g), up(b), 255)
}

/// `picture` grown or shrunk to cover `size`, its middle kept.
fn cover(picture: &Image, size: (u32, u32)) -> Result<Image, String> {
    let (pw, ph) = (picture.width() as f32, picture.height() as f32);
    let k = (size.0 as f32 / pw).max(size.1 as f32 / ph);
    let (sw, sh) = (((pw * k).round() as u32).max(size.0), ((ph * k).round() as u32).max(size.1));
    let scaled = picture.scaled_area(sw, sh)?;
    scaled.crop(crate::geom::IRect::new(((sw - size.0) / 2) as i32, ((sh - size.1) / 2) as i32, size.0 as i32, size.1 as i32))
}

fn fill_gradient(out: &mut Image, from: u32, to: u32) {
    let (w, h) = (out.width() as f32, out.height() as f32);
    let stops = vec![GradientStop::new(0.0, sk(Color::rgb(from))), GradientStop::new(1.0, sk(Color::rgb(to)))];
    let Some(shader) = LinearGradient::new(tiny_skia::Point::from_xy(0.0, 0.0), tiny_skia::Point::from_xy(w, h), stops, SpreadMode::Pad, Transform::identity())
    else {
        return out.fill(Color::rgb(from).premul());
    };
    let paint = tiny_skia::Paint { shader, ..Default::default() };
    if let Some(r) = tiny_skia::Rect::from_xywh(0.0, 0.0, w, h) {
        out.pixmap_mut().fill_rect(r, &paint, Transform::identity(), None);
    }
}

/// Everything but the capture itself: the background and the shadow, the
/// whole size (see layout), and where the capture goes on it.
pub fn backdrop(img: &Image, style: &Style, scale: f32) -> Result<(Image, IPoint), String> {
    let (size, at) = layout(img.size(), style, scale);
    let mut out = Image::new(size.0, size.1)?;
    match style.background.as_str() {
        "color" => out.fill(Color { a: 255, ..Color::parse(&style.color).unwrap_or(Color::rgb(0x1A1A22)) }.premul()),
        "edge" => out.fill(edge_color(img).premul()),
        "gradient" => {
            let g = GRADIENTS.iter().find(|g| g.0 == style.gradient).unwrap_or(&GRADIENTS[0]);
            fill_gradient(&mut out, g.1, g.2);
        }
        "blur" => {
            let mut copy = img.clone();
            copy.force_opaque();
            let k = (size.0.max(size.1) as f32 / 60.0).max(4.0);
            out = soft(&cover(&copy, size)?, k)?;
            // (a little darker, so the capture stands out)
            let mut p = Painter::new(out.pixmap_mut(), 1.0);
            p.fill_rect(Rect::new(0.0, 0.0, size.0 as f32, size.1 as f32), Color::rgba(0, 0, 0, 40));
        }
        "image" => match (!style.image.is_empty()).then(|| crate::codec::decode_file(std::path::Path::new(&style.image))) {
            Some(Ok(picture)) => out = cover(&picture, size)?,
            // (gone, or unreadable: a gradient instead)
            _ => {
                let g = GRADIENTS.iter().find(|g| g.0 == "night").unwrap_or(&GRADIENTS[0]);
                fill_gradient(&mut out, g.1, g.2);
            }
        },
        _ => {}
    }
    if style.shadow > 0 && style.shadow_opacity > 0 && !has_own_shape(img) {
        let blur = style.shadow as f32 * scale;
        let margin = (blur * 2.0).round() as u32;
        let mut mask = Image::new(img.width() + 2 * margin, img.height() + 2 * margin)?;
        {
            let mut p = Painter::new(mask.pixmap_mut(), 1.0);
            let alpha = (255.0 * style.shadow_opacity as f32 / 100.0).round() as u8;
            p.fill_rounded_rect(
                Rect::new(margin as f32, margin as f32, img.width() as f32, img.height() as f32),
                style.radius as f32 * scale,
                Color::rgba(0, 0, 0, alpha),
            );
        }
        let shadow = soft(&mask, (blur / 3.0).max(1.0))?;
        let (x, y) = (at.x - margin as i32, at.y - margin as i32 + (style.shadow_offset as f32 * scale).round() as i32);
        out.pixmap_mut().draw_pixmap(x, y, shadow.pixmap_ref(), &PixmapPaint::default(), Transform::identity(), None);
    }
    Ok((out, at))
}

/// The capture's corner radius in pixels (none for one already shaped).
pub fn corners(style: &Style, img: &Image, scale: f32) -> f32 {
    if has_own_shape(img) { 0.0 } else { style.radius as f32 * scale }
}

/// `img` on `backdrop` at `at`, its corners rounded by `radius` (pixels).
pub fn place(backdrop: &mut Image, img: &Image, at: IPoint, radius: f32) {
    let mask = (radius > 0.0)
        .then(|| {
            let path = rounded_rect_path(Rect::new(at.x as f32, at.y as f32, img.width() as f32, img.height() as f32), radius)?;
            let mut m = Mask::new(backdrop.width(), backdrop.height())?;
            m.fill_path(&path, tiny_skia::FillRule::Winding, true, Transform::identity());
            Some(m)
        })
        .flatten();
    backdrop.pixmap_mut().draw_pixmap(at.x, at.y, img.pixmap_ref(), &PixmapPaint::default(), Transform::identity(), mask.as_ref());
}

/// The capture beautified, at full resolution.
pub fn render(img: &Image, style: &Style, scale: f32) -> Result<Image, String> {
    let (mut out, at) = backdrop(img, style, scale)?;
    place(&mut out, img, at, corners(style, img, scale));
    if style.background != "none" && style.background != "image" {
        out.force_opaque(); // (opaque all over)
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn style_round_trip() {
        let mut s = Style { padding: 999, gradient: "nope".into(), ..Default::default() };
        s.clean();
        assert_eq!((s.padding, s.gradient.as_str()), (400, "ember"));
        assert_eq!(Style::from_map(&s.to_map()), s);
        assert_eq!(Style::from_map(&Map::new()), Style::default());
    }

    #[test]
    fn layouts() {
        let s = Style { padding: 10, ..Default::default() };
        assert_eq!(layout((100, 50), &s, 2.0), ((140, 90), IPoint::new(20, 20)));
        let square = Style { padding: 0, ratio: "1:1".into(), ..Default::default() };
        assert_eq!(layout((100, 50), &square, 1.0), ((100, 100), IPoint::new(0, 25)));
        let percent = Style { padding: 10, padding_percent: true, ..Default::default() };
        assert_eq!(layout((200, 100), &percent, 1.0).0, (240, 140));
    }

    #[test]
    fn beautified() {
        let img = Image::filled(100, 60, 0xFF20_4060).unwrap();
        let style = Style { padding: 20, radius: 10, shadow: 8, ..Default::default() };
        let out = render(&img, &style, 1.0).unwrap();
        assert_eq!(out.size(), (140, 100));
        assert!(!out.has_alpha(), "a gradient is opaque all over");
        let corner = Color::from_premul(out.pixel(0, 0));
        assert!(corner.r > 240 && corner.b < 100, "ember's orange at the top left: {corner:?}");
        assert_eq!(out.pixel(70, 50), 0xFF20_4060, "the picture in the middle");
        assert_ne!(out.pixel(21, 21), 0xFF20_4060, "its corner rounded");
        // Clear: transparent round it.
        let clear = render(&img, &Style { background: "none".into(), shadow: 0, ..style.clone() }, 1.0).unwrap();
        assert_eq!(clear.pixel(0, 0) >> 24, 0);
        // A window with its own shape gets no second corners.
        let mut window = Image::filled(50, 50, 0xFF00_FF00).unwrap();
        window.set_pixel(0, 0, 0);
        window.set_pixel(49, 0, 0);
        window.set_pixel(0, 49, 0);
        window.set_pixel(49, 49, 0);
        assert!(has_own_shape(&window) && corners(&style, &window, 1.0) == 0.0);
        // The edge colour: what most of the border is.
        let mut framed = Image::filled(40, 40, 0xFFFF_FFFF).unwrap();
        framed.set_pixel(0, 0, 0xFF00_0000);
        let e = edge_color(&framed);
        assert!(e.r > 240 && e.g > 240 && e.b > 240, "{e:?}");
    }
}
