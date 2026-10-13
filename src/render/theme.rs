//! The Flatshot look: flat surfaces, rounded squares and a choice of
//! palettes. Nothing here reads from the desktop: the app looks the same
//! everywhere.

use std::sync::RwLock;

/// A colour with straight (not premultiplied) alpha.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash, Default)]
pub struct Color {
    pub r: u8,
    pub g: u8,
    pub b: u8,
    pub a: u8,
}

impl Color {
    pub const TRANSPARENT: Color = Color { r: 0, g: 0, b: 0, a: 0 };
    pub const WHITE: Color = Color::rgb(0xFFFFFF);
    pub const BLACK: Color = Color::rgb(0x000000);

    /// From 0xRRGGBB, opaque.
    pub const fn rgb(hex: u32) -> Color {
        Color { r: (hex >> 16) as u8, g: (hex >> 8) as u8, b: hex as u8, a: 255 }
    }

    pub const fn rgba(r: u8, g: u8, b: u8, a: u8) -> Color {
        Color { r, g, b, a }
    }

    pub const fn with_alpha(self, a: u8) -> Color {
        Color { a, ..self }
    }

    pub fn with_alpha_f(self, a: f32) -> Color {
        self.with_alpha((a.clamp(0.0, 1.0) * 255.0).round() as u8)
    }

    /// From "#RRGGBB" (or "RRGGBB").
    pub fn parse(text: &str) -> Option<Color> {
        let t = text.trim().trim_start_matches('#');
        if t.len() != 6 || !t.bytes().all(|c| c.is_ascii_hexdigit()) {
            return None;
        }
        u32::from_str_radix(t, 16).ok().map(Color::rgb)
    }

    /// "#RRGGBB", upper case.
    pub fn hex(self) -> String {
        format!("#{:02X}{:02X}{:02X}", self.r, self.g, self.b)
    }

    /// Packed, premultiplied 0xAARRGGBB (the pixel format of pictures).
    pub fn premul(self) -> u32 {
        let a = self.a as u32;
        let m = |c: u8| (c as u32 * a + 127) / 255;
        (a << 24) | (m(self.r) << 16) | (m(self.g) << 8) | m(self.b)
    }

    /// From a packed premultiplied pixel.
    pub fn from_premul(p: u32) -> Color {
        let (r, g, b, a) = crate::image::unpack(p);
        if a == 0 {
            return Color::TRANSPARENT;
        }
        let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
        Color::rgba(un(r), un(g), un(b), a)
    }

    /// Hue, saturation, value in 0..1 (hue -1 for greys, as Qt).
    pub fn hsv(self) -> (f32, f32, f32) {
        let r = self.r as f32 / 255.0;
        let g = self.g as f32 / 255.0;
        let b = self.b as f32 / 255.0;
        let max = r.max(g).max(b);
        let min = r.min(g).min(b);
        let d = max - min;
        let v = max;
        let s = if max > 0.0 { d / max } else { 0.0 };
        if d == 0.0 {
            return (-1.0, s, v);
        }
        let h = if max == r {
            ((g - b) / d).rem_euclid(6.0)
        } else if max == g {
            (b - r) / d + 2.0
        } else {
            (r - g) / d + 4.0
        } / 6.0;
        (h.rem_euclid(1.0), s, v)
    }

    pub fn from_hsv(h: f32, s: f32, v: f32) -> Color {
        let s = s.clamp(0.0, 1.0);
        let v = v.clamp(0.0, 1.0);
        let h = if h < 0.0 { 0.0 } else { h.rem_euclid(1.0) } * 6.0;
        let i = h.floor();
        let f = h - i;
        let p = v * (1.0 - s);
        let q = v * (1.0 - s * f);
        let t = v * (1.0 - s * (1.0 - f));
        let (r, g, b) = match i as i32 % 6 {
            0 => (v, t, p),
            1 => (q, v, p),
            2 => (p, v, t),
            3 => (p, q, v),
            4 => (t, p, v),
            _ => (v, p, q),
        };
        let c = |x: f32| (x * 255.0).round().clamp(0.0, 255.0) as u8;
        Color::rgba(c(r), c(g), c(b), 255)
    }

    /// As `QColor::lighter(factor)`: value times factor/100, spilling over
    /// into less saturation.
    pub fn lighter(self, factor: u32) -> Color {
        if factor == 0 {
            return self;
        }
        if factor < 100 {
            return self.darker(10000 / factor);
        }
        let (h, s, v) = self.hsv();
        let mut s16 = (s * 65535.0) as i64;
        let mut v16 = (v * 65535.0) as i64 * factor as i64 / 100;
        if v16 > 65535 {
            s16 -= v16 - 65535;
            if s16 < 0 {
                s16 = 0;
            }
            v16 = 65535;
        }
        Color::from_hsv(h, s16 as f32 / 65535.0, v16 as f32 / 65535.0).with_alpha(self.a)
    }

    /// As `QColor::darker(factor)`.
    pub fn darker(self, factor: u32) -> Color {
        if factor == 0 {
            return self;
        }
        if factor < 100 {
            return self.lighter(10000 / factor);
        }
        let (h, s, v) = self.hsv();
        Color::from_hsv(h, s, v * 100.0 / factor as f32).with_alpha(self.a)
    }

    /// Light enough that dark text goes on it.
    pub fn is_light(self) -> bool {
        0.2126 * self.r as f32 / 255.0 + 0.7152 * self.g as f32 / 255.0 + 0.0722 * self.b as f32 / 255.0 > 0.55
    }
}

/// The palette in use. Painting reads it each time, so switching themes
/// shows on the next repaint.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Palette {
    /// deepest surface: input fields, the magnifier
    pub ink: Color,
    /// windows and panels
    pub base: Color,
    pub raised: Color,
    pub hover: Color,
    pub line: Color,
    pub text: Color,
    pub soft: Color,
    pub muted: Color,
    pub accent: Color,
    /// text and icons on the accent
    pub on_accent: Color,
    /// switch knobs
    pub knob: Color,
    pub code: Color,
    pub code_soft: Color,
    /// screen shading (its alpha comes from the settings)
    pub dim: Color,
}

/// name -> (label, ink, base, raised, hover, line, text, soft, muted, accent, on_accent, knob, code, dim)
pub const THEMES: &[(&str, &str, [u32; 13])] = &[
    ("ember", "Ember", [0x121218, 0x1A1A22, 0x24242E, 0x2E2E3A, 0x363642, 0xF2EEE6, 0xC9C5BD, 0x8B8898, 0xFF6A3D, 0x121218, 0xF2EEE6, 0x3DDC97, 0x0C0C12]),
    (
        "graphite",
        "Graphite",
        [0x111214, 0x18191C, 0x222327, 0x2C2D32, 0x35363C, 0xECEDEF, 0xC3C5CA, 0x868991, 0x4C9EFF, 0x0B0C0E, 0xECEDEF, 0x3DDC97, 0x0A0B0D],
    ),
    ("nord", "Nord", [0x242933, 0x2E3440, 0x3B4252, 0x434C5E, 0x4C566A, 0xECEFF4, 0xD8DEE9, 0x9099AB, 0x88C0D0, 0x242933, 0xECEFF4, 0xA3BE8C, 0x1C2028]),
    ("dusk", "Dusk", [0x15121D, 0x1C1826, 0x262131, 0x30293D, 0x3A3249, 0xF1ECF7, 0xCBC3D6, 0x8D8499, 0xC08CFF, 0x15121D, 0xF1ECF7, 0x5EE6B0, 0x0E0B14]),
    ("paper", "Paper", [0xFFFFFF, 0xF5F4F0, 0xEAE8E2, 0xE0DDD5, 0xD2CEC4, 0x1D1C21, 0x3E3C44, 0x78747F, 0xE4572E, 0xFFFFFF, 0xFFFFFF, 0x0E9F6E, 0x14141C]),
];

impl Palette {
    pub fn named(name: &str) -> Palette {
        let c = THEMES.iter().find(|t| t.0 == name).unwrap_or(&THEMES[0]).2;
        let code = Color::rgb(c[11]);
        Palette {
            ink: Color::rgb(c[0]),
            base: Color::rgb(c[1]),
            raised: Color::rgb(c[2]),
            hover: Color::rgb(c[3]),
            line: Color::rgb(c[4]),
            text: Color::rgb(c[5]),
            soft: Color::rgb(c[6]),
            muted: Color::rgb(c[7]),
            accent: Color::rgb(c[8]),
            on_accent: Color::rgb(c[9]),
            knob: Color::rgb(c[10]),
            code,
            code_soft: code.with_alpha(40),
            dim: Color::rgb(c[12]).with_alpha(150),
        }
    }
}

static CURRENT: RwLock<Option<Palette>> = RwLock::new(None);

/// The palette in use.
pub fn c() -> Palette {
    match *CURRENT.read().unwrap_or_else(|e| e.into_inner()) {
        Some(p) => p,
        None => Palette::named("ember"),
    }
}

/// Switch the palette (unknown names fall back to Ember).
pub fn use_theme(name: &str) {
    *CURRENT.write().unwrap_or_else(|e| e.into_inner()) = Some(Palette::named(name));
}

/// Annotation outlines are part of the saved image: they never follow the theme.
pub const OUTLINE_LIGHT: Color = Color::rgb(0xF2EEE6);
pub const OUTLINE_DARK: Color = Color::rgb(0x121218);

/// Screen recording: the record button's dot, the stop button and the
/// frame around the area being recorded. The same red in every theme.
pub const REC: Color = Color::rgb(0xFF4747);

/// The toolbar's colours to start with, in order (keys 1-7); the
/// settings can change them (`config::DEFAULT_PALETTE`, as hex).
pub const SWATCHES: [Color; 7] = [
    Color::rgb(0xFF6A3D), // ember
    Color::rgb(0xFFC53D), // amber
    Color::rgb(0x3DDC97), // mint
    Color::rgb(0x3DB2FF), // sky
    Color::rgb(0x9B7BFF), // violet
    Color::rgb(0xF2EEE6), // bone
    Color::rgb(0x121218), // ink
];

/// Stroke widths for the three size steps.
pub const SIZES: [f32; 3] = [2.0, 4.0, 7.0];

pub const FONT_FAMILIES: &[&str] = &["Inter", "Manrope", "IBM Plex Sans", "Noto Sans", "DejaVu Sans"];
pub const MONO_FAMILIES: &[&str] = &["JetBrains Mono", "IBM Plex Mono", "Noto Sans Mono", "DejaVu Sans Mono"];

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn colors() {
        assert_eq!(Color::parse("#ff6a3d"), Some(Color::rgb(0xFF6A3D)));
        assert_eq!(Color::rgb(0xFF6A3D).hex(), "#FF6A3D");
        assert_eq!(Color::rgba(255, 0, 0, 128).premul(), 0x80800000);
        let (h, s, v) = Color::rgb(0xFF0000).hsv();
        assert!((h - 0.0).abs() < 1e-4 && s == 1.0 && v == 1.0);
        assert_eq!(Color::from_hsv(1.0 / 3.0, 1.0, 1.0), Color::rgb(0x00FF00));
        assert!(Color::rgb(0xF2EEE6).is_light());
        assert!(!Color::rgb(0x121218).is_light());
        // Qt: QColor("#FF6A3D").lighter(108) keeps hue, desaturates.
        let l = Color::rgb(0xFF6A3D).lighter(108);
        assert_eq!(l.r, 255);
        assert!(l.g > 0x6A && l.b > 0x3D);
        let d = Color::rgb(0xFF4747).darker(115);
        assert!(d.r < 255);
    }

    #[test]
    fn default_swatches() {
        let hex: Vec<String> = SWATCHES.iter().map(|c| c.hex()).collect();
        assert_eq!(hex, crate::config::DEFAULT_PALETTE);
    }

    #[test]
    fn palettes() {
        use_theme("nord");
        assert_eq!(c().accent, Color::rgb(0x88C0D0));
        use_theme("nonsense");
        assert_eq!(c().accent, Color::rgb(0xFF6A3D));
        assert_eq!(c().dim.a, 150);
        assert_eq!(c().code_soft.a, 40);
    }
}
