//! Custom-painted controls (nothing uses a desktop style): icon buttons,
//! colour swatches, the stroke-size button, labelled chips, dividers and
//! the drag grip, laid out in rows inside floating panels.

use crate::geom::{Point, Rect};
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD, MEDIUM};
use crate::render::theme::{self, Color};
use crate::render::{Painter, icons};

/// What a control is and how it looks.
#[derive(Clone, Debug, PartialEq)]
pub enum Look {
    /// An icon button: `size` square.
    Icon {
        icon: &'static str,
        size: f32,
    },
    /// One of the toolbar's colours.
    Swatch(Color),
    /// Your own colour, with a rainbow dot.
    CustomSwatch(Color),
    /// The three stroke sizes as dots, `0..=2` selected.
    Size(usize),
    /// A labelled button: text, an icon or the record dot, a key badge.
    Chip {
        text: String,
        icon: Option<&'static str>,
        primary: bool,
        key: String,
        on: Option<bool>,
    },
    /// A row of options, one selected (the recording format).
    Choice {
        options: Vec<(String, String, bool)>,
        value: String,
        hover: Option<usize>,
    },
    Divider,
    Grip,
}

/// One control in a panel, positioned relative to the panel.
#[derive(Clone, Debug)]
pub struct Item {
    pub id: &'static str,
    pub rect: Rect,
    pub look: Look,
    pub hint: String,
    pub enabled: bool,
    /// filled with the accent (the current tool)
    pub active: bool,
    /// faded: not used by the current tool
    pub dimmed: bool,
    /// an on/off toggle (Some(on))
    pub toggled: Option<bool>,
    /// the icon's colour when idle, if not the usual
    pub tint: Option<Color>,
    pub selected: bool,
}

impl Item {
    pub fn new(id: &'static str, look: Look, hint: impl Into<String>) -> Item {
        Item { id, rect: Rect::default(), look, hint: hint.into(), enabled: true, active: false, dimmed: false, toggled: None, tint: None, selected: false }
    }

    /// The fixed size this control takes in a row.
    pub fn size(&self) -> (f32, f32) {
        match &self.look {
            Look::Icon { size, .. } => (*size, *size),
            Look::Swatch(_) | Look::CustomSwatch(_) => (26.0, 36.0),
            Look::Size(_) => (40.0, 36.0),
            Look::Chip { text, icon, primary, key, .. } => (chip_width(text, icon.is_some() || *primary, key), 34.0),
            Look::Choice { options, .. } => (choice_widths(options).iter().sum::<f32>() + 6.0, 34.0),
            Look::Divider => (13.0, 36.0),
            Look::Grip => (18.0, 36.0),
        }
    }

    /// Does pressing it do something (vs. dragging the panel)?
    pub fn clickable(&self) -> bool {
        !matches!(self.look, Look::Divider | Look::Grip)
    }
}

fn chip_font() -> text::FontSpec {
    text::font(13.0, DEMIBOLD)
}

fn chip_key_font() -> text::FontSpec {
    text::font(11.0, MEDIUM)
}

pub fn chip_width(label: &str, with_icon: bool, key: &str) -> f32 {
    let mut w = text::width(label, chip_font()).round() + 24.0;
    if with_icon {
        w += 24.0;
    }
    if !key.is_empty() {
        w += text::width(key, chip_key_font()).round() + 16.0;
    }
    w
}

fn choice_font() -> text::FontSpec {
    text::font(12.0, text::BOLD)
}

pub fn choice_widths(options: &[(String, String, bool)]) -> Vec<f32> {
    options.iter().map(|(_, label, _)| text::width(label, choice_font()).round() + 22.0).collect()
}

/// Lay `items` out in a row from (`left`, `top`) with `spacing` between
/// them, centred vertically in `height`; hidden items are left out.
/// Returns the row's width (from 0, including `left`).
pub fn layout_row(items: &mut [Item], left: f32, top: f32, height: f32, spacing: f32) -> f32 {
    let mut x = left;
    let mut first = true;
    for it in items.iter_mut() {
        let (w, h) = it.size();
        if !first {
            x += spacing;
        }
        first = false;
        it.rect = Rect::new(x, top + ((height - h) / 2.0).round(), w, h);
        x += w;
    }
    x
}

/// Hover and press state of a panel's controls.
#[derive(Clone, Debug, Default, PartialEq, Eq, Hash)]
pub struct Pointer {
    pub hovered: Option<&'static str>,
    pub pressed: Option<&'static str>,
}

impl Pointer {
    pub fn is_hovered(&self, id: &str) -> bool {
        self.hovered == Some(id)
    }

    /// Shown pressed: pressed and the pointer still on it.
    pub fn is_down(&self, id: &str) -> bool {
        self.pressed == Some(id) && self.hovered == Some(id)
    }
}

/// Squares round `centre` whose edges all fall on whole screen pixels,
/// centred alike: `boxes(half, px)` is `half` logical px each way plus `px`
/// device pixels. (At 125 % or 150 % a ring drawn at logical coordinates
/// comes out a pixel thicker on one side.)
pub fn centred_boxes(p: &Painter<'_>, centre: Point) -> impl Fn(f32, i32) -> Rect + use<> {
    let dpr = p.scale();
    let d = p.map(centre);
    let (cx, cy) = (d.x.round(), d.y.round());
    let origin = p.map(Point::ZERO);
    move |half: f32, px: i32| {
        let h = ((half * dpr).round() as i32 + px).max(1) as f32;
        // Back to logical coordinates relative to the painter's origin.
        Rect::new((cx - h - origin.x) / dpr, (cy - h - origin.y) / dpr, 2.0 * h / dpr, 2.0 * h / dpr)
    }
}

/// The band between two rounded squares, filled.
pub fn fill_ring(p: &mut Painter<'_>, outer: Rect, inner: Rect, radius: f32, color: Color) {
    let inset = (outer.w - inner.w) / 2.0;
    let (Some(a), Some(b)) =
        (crate::render::paint::rounded_rect_path(outer, radius), crate::render::paint::rounded_rect_path(inner, (radius - inset).max(0.5)))
    else {
        return;
    };
    let mut pb = tiny_skia::PathBuilder::new();
    pb.push_path(&a);
    pb.push_path(&b);
    if let Some(path) = pb.finish() {
        p.fill_path_rule(&path, color, true);
    }
}

/// Paint `it` at `origin` (the panel's top left, logical).
pub fn paint_item(p: &mut Painter<'_>, it: &Item, origin: Point, ptr: &Pointer) {
    let c = theme::c();
    let r = it.rect.translated(origin);
    let hovered = ptr.is_hovered(it.id);
    let down = ptr.is_down(it.id);
    p.save();
    match &it.look {
        Look::Icon { icon, size } => {
            let bx = r.adjusted(1.0, 1.0, -1.0, -1.0);
            let mut fg = it.tint.unwrap_or(c.soft);
            if !it.enabled || (it.dimmed && !hovered) {
                p.set_opacity(0.3);
            }
            let fill = if it.active {
                fg = c.on_accent;
                Some(c.accent)
            } else if it.toggled == Some(true) && !down {
                fg = c.code;
                Some(if hovered { c.hover } else { c.raised })
            } else if down {
                fg = it.tint.unwrap_or(c.text);
                Some(c.line)
            } else if hovered {
                fg = it.tint.unwrap_or(c.text);
                Some(c.hover)
            } else {
                None
            };
            if let Some(fill) = fill {
                p.fill_rounded_rect(bx, (bx.w / 2.0).min(9.0), fill);
            }
            let inset = size * 0.22;
            icons::paint(p, icon, r.adjusted(inset, inset, -inset, -inset), fg);
        }
        Look::Swatch(_) | Look::CustomSwatch(_) => {
            if !it.enabled {
                p.set_opacity(0.3);
            }
            let color = match &it.look {
                Look::Swatch(c) | Look::CustomSwatch(c) => *c,
                _ => unreachable!(),
            };
            let bx = centred_boxes(p, r.center());
            if it.selected || hovered {
                fill_ring(p, bx(11.0, 0), bx(9.0, 0), 7.5, if it.selected { c.text } else { c.muted });
            }
            let inner = bx(7.0, 0);
            p.fill_rounded_rect(inner, 4.5, color);
            if color.is_light() == c.base.is_light() {
                // e.g. ink on a dark toolbar needs an edge
                fill_ring(p, inner, bx(7.0, -1), 4.5, c.line);
            }
            if matches!(it.look, Look::CustomSwatch(_)) {
                paint_rainbow_dot(p, r.center() + Point::new(6.0, 6.0), 3.6, c.base);
            }
        }
        Look::Size(selected) => {
            if !it.enabled {
                p.set_opacity(0.3);
            }
            if hovered {
                p.fill_rounded_rect(r.adjusted(1.0, 1.0, -1.0, -1.0), 9.0, c.hover);
            }
            const DOTS: [f32; 3] = [4.0, 7.0, 11.0];
            const GAP: f32 = 4.0;
            let ctr = r.center();
            let mut x = ctr.x - (DOTS.iter().sum::<f32>() + GAP * 2.0) / 2.0;
            for (i, d) in DOTS.iter().enumerate() {
                let color = if i == *selected { c.text } else { c.muted };
                p.fill_rounded_rect(Rect::new(x, ctr.y - d / 2.0, *d, *d), d / 3.0, color);
                x += d + GAP;
            }
        }
        Look::Chip { text: label, icon, primary, key, on } => {
            if !it.enabled {
                p.set_opacity(0.35);
            }
            let bx = r.adjusted(0.5, 0.5, -0.5, -0.5);
            let hover = hovered && it.enabled;
            let (fill, fg, icon_fg) = if *primary {
                (Some(if hover { c.accent.lighter(108) } else { c.accent }), c.on_accent, c.on_accent)
            } else if *on == Some(true) {
                (Some(if hover { c.hover } else { c.raised }), c.text, c.code)
            } else {
                let fg = if hover {
                    c.text
                } else if *on == Some(false) {
                    c.muted
                } else {
                    c.soft
                };
                (hover.then_some(c.hover), fg, fg)
            };
            if let Some(f) = fill {
                p.fill_rounded_rect(bx, 8.0, f);
            }
            let mut x = r.x + 12.0;
            if *primary {
                p.fill_ellipse(Rect::new(x, bx.center().y - 5.0, 10.0, 10.0), c.on_accent);
                x += 20.0;
            } else if let Some(icon) = icon {
                icons::paint(p, icon, Rect::new(x - 2.0, bx.center().y - 9.0, 18.0, 18.0), icon_fg);
                x += 22.0;
            }
            let tw = text::width(label, chip_font());
            p.draw_text(Rect::new(x, r.y, tw + 2.0, r.h), Align::Left, label, chip_font(), fg);
            if !key.is_empty() {
                let kw = text::width(key, chip_key_font()) + 10.0;
                let key_box = Rect::new(x + tw + 8.0, bx.center().y - 9.0, kw, 18.0);
                p.stroke_rounded_rect(key_box, 4.0, Pen::new(fg.with_alpha_f(0.4 * fg.a as f32 / 255.0), 1.0));
                p.draw_text(key_box, Align::Center, key, chip_key_font(), fg);
            }
        }
        Look::Choice { options, value, hover } => {
            p.fill_rounded_rect(r, 8.0, c.ink);
            let widths = choice_widths(options);
            let mut x = r.x + 3.0;
            for (i, ((v, label, available), w)) in options.iter().zip(widths).enumerate() {
                let cell = Rect::new(x, r.y + 3.0, w, r.h - 6.0);
                let selected = v == value;
                if selected || (*hover == Some(i) && *available) {
                    p.fill_rounded_rect(cell, 6.0, if selected { c.hover } else { c.raised });
                }
                p.save();
                p.set_opacity(if *available { 1.0 } else { 0.35 });
                p.draw_text(cell, Align::Center, label, choice_font(), if selected { c.text } else { c.muted });
                p.restore();
                x += w;
            }
        }
        Look::Divider => p.fill_rect(Rect::new(r.x + 6.0, r.y + 9.0, 1.0, 18.0), c.line),
        Look::Grip => {
            for col in 0..2 {
                for row in 0..3 {
                    p.fill_ellipse(Rect::new(r.x + 5.0 + col as f32 * 6.0, r.y + 11.0 + row as f32 * 6.0, 2.6, 2.6), c.muted);
                }
            }
        }
    }
    p.restore();
}

/// The rainbow dot on your own colour's swatch: a conical sweep of hues
/// from the top, ringed in the panel's colour.
pub fn paint_rainbow_dot(p: &mut Painter<'_>, centre: Point, radius: f32, edge: Color) {
    let d = p.map(centre);
    let s = p.scale();
    let rr = radius * s;
    let (x0, y0) = ((d.x - rr - 2.0).floor() as i32, (d.y - rr - 2.0).floor() as i32);
    let n = (rr * 2.0 + 5.0).ceil() as u32;
    let Ok(mut img) = crate::image::Image::new(n, n) else { return };
    for py in 0..n {
        for px in 0..n {
            let fx = x0 as f32 + px as f32 + 0.5 - d.x;
            let fy = y0 as f32 + py as f32 + 0.5 - d.y;
            let dist = (fx * fx + fy * fy).sqrt();
            let cov = (rr + 0.5 - dist).clamp(0.0, 1.0);
            if cov <= 0.0 {
                continue;
            }
            // QConicalGradient(c, 90): counter-clockwise from 12 o'clock.
            let angle = (-fy).atan2(fx).to_degrees();
            let t = ((angle - 90.0).rem_euclid(360.0)) / 360.0;
            let col = Color::from_hsv(t, 0.8, 1.0).with_alpha((cov * 255.0) as u8);
            img.set_pixel(px, py, col.premul());
        }
    }
    p.blit_image(&img, x0, y0);
    p.stroke_ellipse(Rect::new(centre.x - radius, centre.y - radius, 2.0 * radius, 2.0 * radius), Pen::new(edge, 1.5));
}

/// A floating panel's background: rounded, outlined.
pub fn paint_panel(p: &mut Painter<'_>, r: Rect, radius: f32) {
    let c = theme::c();
    p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), radius, Some(c.base), Some(Pen::new(c.line, 1.0)));
}
