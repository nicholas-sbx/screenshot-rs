//! The editor's Background panel, beside the picture: its background,
//! padding, corners, shadow and shape, and presets. Each change shows at
//! once, and is the style Beautify after capture uses next.

use crate::beautify::{BACKGROUNDS, GRADIENTS, RATIOS, Style};
use crate::geom::{Point, Rect};
use crate::keys::KeyPress;
use crate::keyseq::key;
use crate::render::Painter;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD, MEDIUM, NORMAL};
use crate::render::theme;
use crate::ui::panels::LineEdit;
use crate::ui::textedit::KeyResult;

/// The panel's width.
pub const WIDTH: f32 = 272.0;
const PAD: f32 = 14.0;
const SLIDER_H: f32 = 24.0;
const COMBO_H: f32 = 32.0;
const BUTTON_H: f32 = 30.0;
const OPTION_H: f32 = 30.0;

/// The panel's dropdowns.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Combo {
    Background,
    Gradient,
    Ratio,
    Presets,
}

/// Its sliders: (style key, label, least, most, step).
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Slide {
    Padding,
    Radius,
    Shadow,
    Offset,
    Opacity,
}

impl Slide {
    fn range(self) -> (i64, i64, i64) {
        match self {
            Slide::Padding => (0, 200, 4),
            Slide::Radius => (0, 48, 2),
            Slide::Shadow => (0, 64, 2),
            Slide::Offset => (0, 40, 2),
            Slide::Opacity => (0, 100, 5),
        }
    }

    fn title(self) -> &'static str {
        match self {
            Slide::Padding => "Padding",
            Slide::Radius => "Rounded corners",
            Slide::Shadow => "Shadow",
            Slide::Offset => "Shadow offset",
            Slide::Opacity => "Shadow strength",
        }
    }

    fn value(self, s: &Style) -> i64 {
        match self {
            Slide::Padding => s.padding,
            Slide::Radius => s.radius,
            Slide::Shadow => s.shadow,
            Slide::Offset => s.shadow_offset,
            Slide::Opacity => s.shadow_opacity,
        }
    }

    fn label(self, s: &Style) -> String {
        let v = self.value(s);
        match self {
            Slide::Padding if s.padding_percent => format!("{v}%"),
            Slide::Shadow if v == 0 => "Off".into(),
            Slide::Opacity => format!("{v}%"),
            _ => format!("{v}px"),
        }
    }
}

/// What's at a place on the panel.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Hit {
    Combo(Combo),
    /// an open dropdown's option
    Option(usize),
    Slider(Slide),
    Percent,
    UseColour,
    ChooseImage,
    PresetName,
    SavePreset,
}

/// What the editor should do.
#[derive(Clone, Debug, PartialEq)]
pub enum Event {
    None,
    /// the style changed (the panel changed it in place)
    Changed,
    /// "Use the toolbar's colour"
    UseColour,
    ChooseImage,
    UsePreset(String),
    SavePreset(String),
    /// read the clipboard's text for the preset's name
    Paste,
}

/// What to lay out: (hit, its rectangle) and the text in between.
enum Part {
    Hit(Hit, Rect),
    Heading(String, f32),
    Text(Vec<String>, f32),
}

pub struct Panel {
    /// where it is in the window (logical)
    pub rect: Rect,
    scroll: f32,
    open: Option<Combo>,
    dragging: Option<Slide>,
    pub name: LineEdit,
    pub hovered: Option<Hit>,
}

fn heading_font() -> text::FontSpec {
    text::font(12.0, DEMIBOLD)
}

fn hint_font() -> text::FontSpec {
    text::font(12.0, NORMAL)
}

impl Default for Panel {
    fn default() -> Panel {
        let mut name = LineEdit::new(Rect::default(), "", text::font(13.0, NORMAL), 60);
        name.placeholder = "Preset name".into();
        Panel { rect: Rect::default(), scroll: 0.0, open: None, dragging: None, name, hovered: None }
    }
}

impl Panel {
    fn options(&self, combo: Combo, presets: &[String]) -> Vec<(String, String)> {
        match combo {
            Combo::Background => BACKGROUNDS.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect(),
            Combo::Gradient => GRADIENTS.iter().map(|g| (g.0.to_string(), capitalized(g.0))).collect(),
            Combo::Ratio => RATIOS.iter().map(|r| (r.0.to_string(), r.1.to_string())).collect(),
            Combo::Presets => std::iter::once((String::new(), "Presets…".to_string())).chain(presets.iter().map(|p| (p.clone(), p.clone()))).collect(),
        }
    }

    fn value(combo: Combo, s: &Style) -> String {
        match combo {
            Combo::Background => s.background.clone(),
            Combo::Gradient => s.gradient.clone(),
            Combo::Ratio => s.ratio.clone(),
            Combo::Presets => String::new(),
        }
    }

    /// Everything on the panel, top to bottom, scrolled.
    fn parts(&self, s: &Style, image_name: &str) -> (Vec<Part>, f32) {
        let x = self.rect.x + PAD;
        let w = WIDTH - 2.0 * PAD;
        let mut y = self.rect.y + 12.0 - self.scroll;
        let mut out = Vec::new();
        let heading = |out: &mut Vec<Part>, y: &mut f32, t: &str| {
            *y += 6.0;
            out.push(Part::Heading(t.to_string(), *y));
            *y += 20.0;
        };
        let hit = |out: &mut Vec<Part>, y: &mut f32, h: Hit, height: f32| {
            out.push(Part::Hit(h, Rect::new(x, *y, w, height)));
            *y += height + 6.0;
        };
        heading(&mut out, &mut y, "Background");
        hit(&mut out, &mut y, Hit::Combo(Combo::Background), COMBO_H);
        match s.background.as_str() {
            "gradient" => hit(&mut out, &mut y, Hit::Combo(Combo::Gradient), COMBO_H),
            "color" => hit(&mut out, &mut y, Hit::UseColour, BUTTON_H),
            "image" => {
                hit(&mut out, &mut y, Hit::ChooseImage, BUTTON_H);
                let lines = text::wrap(image_name, hint_font(), w);
                let n = lines.len() as f32;
                out.push(Part::Text(lines, y));
                y += n * 16.0 + 6.0;
            }
            _ => {}
        }
        for slide in [Slide::Padding, Slide::Radius, Slide::Shadow, Slide::Offset, Slide::Opacity] {
            heading(&mut out, &mut y, slide.title());
            hit(&mut out, &mut y, Hit::Slider(slide), SLIDER_H);
            if slide == Slide::Padding {
                out.push(Part::Text(vec!["Percentage of the longer side".into()], y + 2.0));
                out.push(Part::Hit(Hit::Percent, Rect::new(x + w - 38.0, y, 38.0, 20.0)));
                y += 26.0;
            }
        }
        heading(&mut out, &mut y, "Shape");
        hit(&mut out, &mut y, Hit::Combo(Combo::Ratio), COMBO_H);
        heading(&mut out, &mut y, "Presets");
        hit(&mut out, &mut y, Hit::Combo(Combo::Presets), COMBO_H);
        let save_w = text::width("Save", text::font(12.0, MEDIUM)) + 28.0;
        out.push(Part::Hit(Hit::PresetName, Rect::new(x, y, w - save_w - 6.0, 32.0)));
        out.push(Part::Hit(Hit::SavePreset, Rect::new(x + w - save_w, y + 1.0, save_w, BUTTON_H)));
        y += 32.0 + 14.0;
        let lines = text::wrap(
            "Save, Save as and Copy include the background. Settings → After capture → Beautify applies the last style to every capture.",
            hint_font(),
            w,
        );
        let n = lines.len() as f32;
        out.push(Part::Text(lines, y));
        y += n * 16.0 + 12.0;
        (out, y + self.scroll - self.rect.y)
    }

    /// The open dropdown's options: (index, rectangle), over everything.
    fn option_rects(&self, s: &Style, image_name: &str, presets: &[String]) -> Vec<(usize, Rect)> {
        let Some(open) = self.open else { return Vec::new() };
        let (parts, _) = self.parts(s, image_name);
        let Some(c) = parts.iter().find_map(|p| match p {
            Part::Hit(Hit::Combo(k), r) if *k == open => Some(*r),
            _ => None,
        }) else {
            return Vec::new();
        };
        let n = self.options(open, presets).len();
        let list_h = n as f32 * OPTION_H + 8.0;
        let top = if c.bottom() + 4.0 + list_h > self.rect.bottom() { (c.y - 4.0 - list_h).max(self.rect.y) } else { c.bottom() + 4.0 };
        (0..n).map(|i| (i, Rect::new(c.x + 4.0, top + 4.0 + i as f32 * OPTION_H, c.w - 8.0, OPTION_H))).collect()
    }

    pub fn hit(&self, at: Point, s: &Style, image_name: &str, presets: &[String]) -> Option<Hit> {
        if let Some((i, _)) = self.option_rects(s, image_name, presets).into_iter().find(|(_, r)| r.contains(at)) {
            return Some(Hit::Option(i));
        }
        if !self.rect.contains(at) {
            return None;
        }
        self.parts(s, image_name).0.into_iter().find_map(|p| match p {
            Part::Hit(h, r) if r.contains(at) => Some(h),
            _ => None,
        })
    }

    /// Keep the scroll within what there is.
    pub fn scroll_by(&mut self, d: f32, s: &Style, image_name: &str) {
        let (_, height) = self.parts(s, image_name);
        self.scroll = (self.scroll - d).clamp(0.0, (height - self.rect.h).max(0.0));
        self.open = None;
        self.place_name(s, image_name);
    }

    /// Put the panel at `rect` (and its fields where they go).
    pub fn place(&mut self, rect: Rect, s: &Style, image_name: &str) {
        self.rect = rect;
        self.place_name(s, image_name);
    }

    /// Where the preset name's field is (for clicks and its text).
    fn place_name(&mut self, s: &Style, image_name: &str) {
        if let Some(r) = self.parts(s, image_name).0.iter().find_map(|p| match p {
            Part::Hit(Hit::PresetName, r) => Some(*r),
            _ => None,
        }) {
            self.name.rect = r;
        }
    }

    pub fn is_open(&self) -> bool {
        self.open.is_some()
    }

    pub fn close(&mut self) -> bool {
        self.open.take().is_some()
    }

    pub fn press(&mut self, at: Point, s: &mut Style, image_name: &str, presets: &[String]) -> Event {
        self.place_name(s, image_name);
        let hit = self.hit(at, s, image_name, presets);
        if hit != Some(Hit::PresetName) {
            self.name.focused = false;
        }
        let was_open = self.open.take();
        match hit {
            Some(Hit::Option(i)) => {
                let Some(combo) = was_open else { return Event::None };
                let Some((v, _)) = self.options(combo, presets).get(i).cloned() else { return Event::None };
                match combo {
                    Combo::Background => s.background = v,
                    Combo::Gradient => s.gradient = v,
                    Combo::Ratio => s.ratio = v,
                    Combo::Presets if v.is_empty() => return Event::None,
                    Combo::Presets => return Event::UsePreset(v),
                }
                s.clean();
                Event::Changed
            }
            Some(Hit::Combo(c)) => {
                if was_open != Some(c) {
                    self.open = Some(c);
                }
                Event::None
            }
            Some(Hit::Slider(slide)) => {
                self.dragging = Some(slide);
                self.slide(at, s)
            }
            Some(Hit::Percent) => {
                s.padding_percent = !s.padding_percent;
                Event::Changed
            }
            Some(Hit::UseColour) => Event::UseColour,
            Some(Hit::ChooseImage) => Event::ChooseImage,
            Some(Hit::PresetName) => {
                self.name.press(at, false);
                Event::None
            }
            Some(Hit::SavePreset) => self.save_preset(),
            None => Event::None,
        }
    }

    fn save_preset(&mut self) -> Event {
        let name = self.name.text().trim().to_string();
        self.name = Panel::default().name;
        if name.is_empty() { Event::None } else { Event::SavePreset(name) }
    }

    /// The pointer moved; true if the panel needs drawing again.
    pub fn motion(&mut self, at: Point, s: &mut Style, image_name: &str, presets: &[String]) -> Event {
        if self.dragging.is_some() {
            return self.slide(at, s);
        }
        let h = self.hit(at, s, image_name, presets);
        if h != self.hovered {
            self.hovered = h;
        }
        Event::None
    }

    pub fn release(&mut self) {
        self.dragging = None;
    }

    pub fn dragging(&self) -> bool {
        self.dragging.is_some()
    }

    fn slide(&mut self, at: Point, s: &mut Style) -> Event {
        let Some(slide) = self.dragging else { return Event::None };
        let (lo, hi, step) = slide.range();
        let track = Rect::new(self.rect.x + PAD + 7.0, 0.0, WIDTH - 2.0 * PAD - 44.0 - 14.0, 0.0);
        let t = ((at.x - track.x) / track.w.max(1.0)).clamp(0.0, 1.0);
        let v = ((lo as f32 + t * (hi - lo) as f32) / step as f32).round() as i64 * step;
        let v = v.clamp(lo, hi);
        let field = match slide {
            Slide::Padding => &mut s.padding,
            Slide::Radius => &mut s.radius,
            Slide::Shadow => &mut s.shadow,
            Slide::Offset => &mut s.shadow_offset,
            Slide::Opacity => &mut s.shadow_opacity,
        };
        if *field == v {
            return Event::None;
        }
        *field = v;
        Event::Changed
    }

    pub fn wants_keys(&self) -> bool {
        self.name.focused
    }

    pub fn key(&mut self, k: &KeyPress) -> Event {
        match k.key {
            key::RETURN | key::ENTER => self.save_preset(),
            key::ESCAPE => {
                self.name.focused = false;
                Event::None
            }
            _ => match self.name.key(k) {
                KeyResult::Paste => Event::Paste,
                _ => Event::None,
            },
        }
    }

    pub fn insert(&mut self, s: &str) {
        if self.name.focused {
            self.name.edit.insert(s);
        }
    }

    pub fn paint(&self, p: &mut Painter<'_>, s: &Style, image_name: &str, presets: &[String]) {
        let c = theme::c();
        p.fill_rect(self.rect, c.base);
        p.fill_rect(Rect::new(self.rect.x, self.rect.y, 1.0, self.rect.h), c.line);
        p.save();
        p.clip_rect(self.rect);
        let x = self.rect.x + PAD;
        let w = WIDTH - 2.0 * PAD;
        let (parts, _) = self.parts(s, image_name);
        for part in &parts {
            match part {
                Part::Heading(t, y) => p.draw_text(Rect::new(x, *y, w, 20.0), Align::Left, t, heading_font(), c.soft),
                Part::Text(lines, y) => {
                    for (i, l) in lines.iter().enumerate() {
                        p.draw_text(Rect::new(x, y + i as f32 * 16.0, w, 16.0), Align::Left, l, hint_font(), c.muted);
                    }
                }
                Part::Hit(h, r) => self.paint_hit(p, *h, *r, s),
            }
        }
        // The open dropdown, over everything.
        let options = self.option_rects(s, image_name, presets);
        if let (Some(open), Some(first), Some(last)) = (self.open, options.first(), options.last()) {
            let bg = Rect::from_ltrb(first.1.x - 4.0, first.1.y - 4.0, first.1.right() + 4.0, last.1.bottom() + 4.0);
            p.draw_rounded_rect(bg, 8.0, Some(c.raised), Some(Pen::new(c.line, 1.0)));
            let value = Self::value(open, s);
            for ((i, r), (v, label)) in options.iter().zip(self.options(open, presets)) {
                if self.hovered == Some(Hit::Option(*i)) || (v == value && open != Combo::Presets) {
                    p.fill_rounded_rect(*r, 6.0, c.hover);
                }
                p.draw_text(r.adjusted(10.0, 0.0, 0.0, 0.0), Align::Left, &label, text::font(13.0, MEDIUM), c.text);
            }
        }
        p.restore();
    }

    fn paint_hit(&self, p: &mut Painter<'_>, h: Hit, r: Rect, s: &Style) {
        let c = theme::c();
        let hovered = self.hovered == Some(h);
        let button = |p: &mut Painter<'_>, label: &str| {
            p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(if hovered { c.hover } else { c.raised }), Some(Pen::new(c.line, 1.0)));
            p.draw_text(r, Align::Center, label, text::font(12.0, MEDIUM), c.text);
        };
        match h {
            Hit::Combo(combo) => {
                let open = self.open == Some(combo);
                p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(c.ink), Some(Pen::new(if open || hovered { c.muted } else { c.line }, 1.0)));
                let value = Self::value(combo, s);
                let label = match combo {
                    Combo::Presets => "Presets…".to_string(),
                    _ => self.options(combo, &[]).into_iter().find(|o| o.0 == value).map(|o| o.1).unwrap_or(value),
                };
                p.draw_text(Rect::new(r.x + 10.0, r.y, r.w - 36.0, r.h), Align::Left, &label, text::font(13.0, NORMAL), c.text);
                let cx = Point::new(r.right() - 14.0, r.center().y);
                let pen = Pen::new(c.muted, 1.4);
                p.draw_line(Point::new(cx.x - 3.5, cx.y - 1.5), Point::new(cx.x, cx.y + 2.0), pen);
                p.draw_line(Point::new(cx.x, cx.y + 2.0), Point::new(cx.x + 3.5, cx.y - 1.5), pen);
            }
            Hit::Slider(slide) => {
                let (lo, hi, _) = slide.range();
                let track = Rect::new(r.x + 7.0, r.center().y - 2.0, r.w - 44.0 - 14.0, 4.0);
                p.fill_rounded_rect(track, 2.0, c.line);
                let t = (slide.value(s) - lo) as f32 / (hi - lo).max(1) as f32;
                let kx = track.x + track.w * t;
                p.fill_rounded_rect(Rect::new(track.x, track.y, kx - track.x, track.h), 2.0, c.soft);
                p.fill_ellipse(Rect::new(kx - 7.0, r.center().y - 7.0, 14.0, 14.0), c.text);
                p.draw_text(Rect::new(r.right() - 40.0, r.y, 40.0, r.h), Align::Right, &slide.label(s), hint_font(), c.muted);
            }
            Hit::Percent => {
                let on = s.padding_percent;
                p.fill_rounded_rect(r, 10.0, if on { c.accent } else { c.line });
                let kx = if on { r.right() - 18.0 } else { r.x + 2.0 };
                p.fill_ellipse(Rect::new(kx, r.y + 2.0, 16.0, 16.0), if on { c.knob } else { c.text });
            }
            Hit::UseColour => button(p, "Use the toolbar's colour"),
            Hit::ChooseImage => button(p, "Choose an image…"),
            Hit::SavePreset => button(p, "Save"),
            Hit::PresetName => self.name.paint(p, true),
            Hit::Option(_) => {}
        }
    }
}

fn capitalized(s: &str) -> String {
    let mut c = s.chars();
    c.next().map(|f| f.to_uppercase().collect::<String>() + c.as_str()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn panel() -> Panel {
        Panel { rect: Rect::new(600.0, 48.0, WIDTH, 700.0), ..Default::default() }
    }

    fn centre(p: &Panel, s: &Style, want: Hit) -> Point {
        p.parts(s, "")
            .0
            .into_iter()
            .find_map(|part| match part {
                Part::Hit(h, r) if h == want => Some(r.center()),
                _ => None,
            })
            .expect("laid out")
    }

    #[test]
    fn choosing_and_sliding() {
        let mut p = panel();
        let mut s = Style::default();
        // The background's dropdown, then "Colour".
        let at = centre(&p, &s, Hit::Combo(Combo::Background));
        assert_eq!(p.press(at, &mut s, "", &[]), Event::None);
        let colour = p.option_rects(&s, "", &[])[1].1.center();
        assert_eq!(p.press(colour, &mut s, "", &[]), Event::Changed);
        assert_eq!(s.background, "color");
        assert_eq!(p.press(centre(&p, &s, Hit::UseColour), &mut s, "", &[]), Event::UseColour);
        // The padding's slider, all the way.
        let r = centre(&p, &s, Hit::Slider(Slide::Padding));
        p.press(r, &mut s, "", &[]);
        assert_eq!(p.motion(Point::new(r.x + 500.0, r.y), &mut s, "", &[]), Event::Changed, "dragged past its end");
        p.release();
        assert_eq!(s.padding, 200);
        assert_eq!(p.press(centre(&p, &s, Hit::Percent), &mut s, "", &[]), Event::Changed);
        assert!(s.padding_percent && Slide::Padding.label(&s) == "200%");
        // A preset named and saved.
        p.press(centre(&p, &s, Hit::PresetName), &mut s, "", &[]);
        p.insert("house");
        assert_eq!(p.key(&KeyPress { key: key::RETURN, ..Default::default() }), Event::SavePreset("house".into()));
        let presets = vec!["house".to_string()];
        p.press(centre(&p, &s, Hit::Combo(Combo::Presets)), &mut s, "", &presets);
        let first = p.option_rects(&s, "", &presets)[1].1.center();
        assert_eq!(p.press(first, &mut s, "", &presets), Event::UsePreset("house".into()));
    }
}
