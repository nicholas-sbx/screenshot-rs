//! The other floating panels over a capture: a card on each code found,
//! the picker for your own colour, and the options under an area chosen
//! for recording.

use std::cell::RefCell;

use crate::geom::{Point, Rect};
use crate::image::Image;
use crate::keys::KeyPress;
use crate::render::Painter;
use crate::render::paint::{Align, Pen, sk};
use crate::render::text::{self, MEDIUM};
use crate::render::theme::{self, Color};
use crate::ui::textedit::{KeyResult, TextEdit};
use crate::ui::widgets::{Item, Look, Pointer, layout_row, paint_item, paint_panel};
use crate::util;

// -- code cards ---------------------------------------------------------------------

/// Slim card over a detected QR code or barcode: the decoded text plus
/// copy, open (links only) and dismiss.
#[derive(Clone, Debug)]
pub struct CodeChip {
    pub pos: Point,
    pub size: (f32, f32),
    pub preview: String,
    pub items: Vec<Item>,
    pub ptr: Pointer,
    pub grab: Option<Point>,
}

const CHIP_MAX_CHARS: usize = 30;

fn chip_font() -> text::FontSpec {
    text::mono(12.0, MEDIUM)
}

impl CodeChip {
    pub fn new(code_text: &str, is_link: bool) -> CodeChip {
        let shown = util::displayable(code_text);
        let flat = shown.split_whitespace().collect::<Vec<_>>().join(" ");
        let preview = util::elide(&flat, CHIP_MAX_CHARS);
        let full = util::elide(&shown, 200);
        let tw = text::width(&preview, chip_font()).round();
        let mut items = vec![Item::new("copy", Look::Icon { icon: "copy", size: 24.0 }, format!("Copy  ·  {full}"))];
        if is_link {
            items.push(Item::new("open", Look::Icon { icon: "open", size: 24.0 }, format!("Open  ·  {full}")));
        }
        items.push(Item::new("close", Look::Icon { icon: "close", size: 24.0 }, "Dismiss this code"));
        let right = layout_row(&mut items, 10.0 + tw + 8.0, 3.0, 24.0, 0.0);
        CodeChip { pos: Point::ZERO, size: (right + 3.0, 30.0), preview, items, ptr: Pointer::default(), grab: None }
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.pos.x, self.pos.y, self.size.0, self.size.1)
    }

    pub fn item_at(&self, at: Point) -> Option<&Item> {
        let local = at - self.pos;
        self.items.iter().find(|it| it.rect.contains(local))
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        let c = theme::c();
        let r = self.rect();
        paint_panel(p, r, 8.0);
        p.draw_text(Rect::new(r.x + 10.0, r.y, r.w, r.h), Align::Left, &self.preview, chip_font(), c.soft);
        for it in &self.items {
            paint_item(p, it, self.pos, &self.ptr);
        }
    }

    pub fn hint(&self) -> Option<String> {
        let id = self.ptr.hovered?;
        self.items.iter().find(|it| it.id == id).map(|it| it.hint.clone())
    }
}

/// Each code's card on its code, moved down (or up) clear of the cards
/// already placed, so none covers another. `codes` are the codes' boxes.
pub fn place_chips(chips: &mut [CodeChip], codes: &[Rect], bounds: Rect) {
    let bounds = bounds.adjusted(4.0, 4.0, -4.0, -4.0);
    let mut order: Vec<usize> = (0..chips.len().min(codes.len())).collect();
    order.sort_by(|&a, &b| {
        let ca = codes[a].center();
        let cb = codes[b].center();
        ca.y.partial_cmp(&cb.y).unwrap_or(std::cmp::Ordering::Equal).then(ca.x.partial_cmp(&cb.x).unwrap_or(std::cmp::Ordering::Equal))
    });
    let mut placed: Vec<Rect> = Vec::new();
    let n = order.len();
    for &i in &order {
        let c = codes[i].center();
        let (w, h) = chips[i].size;
        let mut home = Rect::new(c.x - w / 2.0, c.y - h / 2.0, w, h);
        home.x = home.x.min(bounds.right() - home.w).max(bounds.left());
        let step = h + 4.0;
        let mut spot = home;
        let mut found = false;
        for k in 1..(2 * n + 2) {
            if !placed.iter().any(|r| spot.adjusted(-2.0, -2.0, 2.0, 2.0).intersects(r)) && bounds.contains_rect(&spot) {
                found = true;
                break;
            }
            // down 1, up 1, down 2, ...
            let shift = (k.div_ceil(2)) as f32 * step * if k % 2 == 1 { 1.0 } else { -1.0 };
            spot = home.translated(Point::new(0.0, shift));
        }
        if !found {
            spot = home;
        }
        placed.push(spot);
        chips[i].pos = Point::new(spot.x.round(), spot.y.round());
    }
}

// -- a text field --------------------------------------------------------------------

/// A single-line text field, in the Flatshot look.
#[derive(Clone, Debug)]
pub struct LineEdit {
    pub rect: Rect,
    pub edit: TextEdit,
    pub focused: bool,
    pub font: text::FontSpec,
    pub placeholder: String,
    /// how far the text is scrolled left, when it's longer than the field
    pub scroll: f32,
}

impl LineEdit {
    pub fn new(rect: Rect, text_value: &str, font: text::FontSpec, max_chars: usize) -> LineEdit {
        LineEdit { rect, edit: TextEdit::single_line(text_value, max_chars), focused: false, font, placeholder: String::new(), scroll: 0.0 }
    }

    pub fn text(&self) -> &str {
        &self.edit.text
    }

    fn text_x(&self) -> f32 {
        self.rect.x + 9.0 - self.scroll
    }

    /// Keep the caret in view.
    fn follow(&mut self) {
        let inner = self.rect.w - 18.0;
        let caret = text::width(&self.edit.text[..self.edit.cursor], self.font);
        if caret - self.scroll > inner {
            self.scroll = caret - inner;
        } else if caret < self.scroll {
            self.scroll = caret;
        }
        let total = text::width(&self.edit.text, self.font);
        if total - self.scroll < inner {
            self.scroll = (total - inner).max(0.0);
        }
    }

    /// Where the caret is drawn.
    pub fn caret_rect(&self) -> Rect {
        let m = text::metrics(self.font);
        let cx = self.text_x() + text::width(&self.edit.text[..self.edit.cursor], self.font);
        Rect::new(cx.round(), self.rect.y + (self.rect.h - m.height) / 2.0, 1.0, m.height)
    }

    pub fn index_at(&self, x: f32) -> usize {
        let m = |s: &str| text::width(s, self.font);
        self.edit.col_at(0, x - self.text_x(), &m)
    }

    pub fn press(&mut self, at: Point, shift: bool) {
        self.focused = true;
        let i = self.index_at(at.x);
        self.edit.place(i, shift);
    }

    pub fn drag_to(&mut self, at: Point) {
        let i = self.index_at(at.x);
        self.edit.place(i, true);
        self.follow();
    }

    pub fn key(&mut self, press: &KeyPress) -> KeyResult {
        let font = self.font;
        let m = move |s: &str| text::width(s, font);
        let r = self.edit.key(press, &m);
        self.follow();
        r
    }

    pub fn paint(&self, p: &mut Painter<'_>, enabled: bool) {
        let c = theme::c();
        let border = if self.focused { c.muted } else { c.line };
        p.draw_rounded_rect(self.rect.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(c.ink), Some(Pen::new(border, 1.0)));
        p.save();
        p.clip_rect(self.rect.adjusted(1.0, 1.0, -1.0, -1.0));
        let m = text::metrics(self.font);
        let baseline = self.rect.y + (self.rect.h - m.height) / 2.0 + m.ascent;
        let x = self.text_x();
        if self.focused && self.edit.has_selection() {
            let (a, b) = self.edit.selection();
            let x0 = text::width(&self.edit.text[..a], self.font);
            let x1 = text::width(&self.edit.text[..b], self.font);
            p.fill_rect(Rect::new(x + x0, baseline - m.ascent, x1 - x0, m.height), c.hover);
        }
        if self.edit.text.is_empty() && !self.placeholder.is_empty() {
            p.draw_text_at(Point::new(x, baseline), &self.placeholder, self.font, c.muted);
        } else {
            p.draw_text_at(Point::new(x, baseline), &self.edit.text, self.font, if enabled { c.text } else { c.muted });
        }
        if self.focused && super::caret::shown() {
            p.fill_rect(self.caret_rect(), c.text);
        }
        p.restore();
    }
}

// -- your own colour -------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq)]
enum Dragging {
    Shades,
    Hues,
}

/// The square of shades as drawn: (hue, width, height) and its pixels.
type Shades = ((u32, u32, u32), Image);

/// A square of shades (saturation across, brightness down), a hue bar, the
/// colour as hex to read or type, and an eyedropper for a colour on the
/// screen.
#[derive(Debug)]
pub struct ColorPicker {
    pub pos: Point,
    pub h: f32,
    pub s: f32,
    pub v: f32,
    dragging: Option<Dragging>,
    shades: RefCell<Option<Shades>>,
    pub hex: LineEdit,
    pub dropper: Item,
    pub ptr: Pointer,
}

pub const PICKER_SIZE: (f32, f32) = (216.0, 202.0);
const SHADES: Rect = Rect::new(10.0, 10.0, 196.0, 124.0);
const HUES: Rect = Rect::new(10.0, 144.0, 196.0, 12.0);

/// What a picker's input did.
#[derive(Clone, Debug, PartialEq)]
pub enum PickerEvent {
    None,
    /// the colour changed
    Color(Color),
    /// start the eyedropper
    Eyedropper,
    /// close it (Enter in the hex field)
    Close,
    Copy(String),
    Paste,
}

impl ColorPicker {
    pub fn new(color: Color) -> ColorPicker {
        let mut dropper = Item::new("dropper", Look::Icon { icon: "eyedropper", size: 28.0 }, "Take a colour from the screen  ·  then click a pixel");
        dropper.rect = Rect::new(178.0, 166.0, 28.0, 28.0);
        let mut picker = ColorPicker {
            pos: Point::ZERO,
            h: 0.0,
            s: 1.0,
            v: 1.0,
            dragging: None,
            shades: RefCell::new(None),
            hex: LineEdit::new(Rect::new(10.0, 166.0, 158.0, 28.0), "", text::mono(13.0, text::NORMAL), 7),
            dropper,
            ptr: Pointer::default(),
        };
        picker.set_color(color);
        picker
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.pos.x, self.pos.y, PICKER_SIZE.0, PICKER_SIZE.1)
    }

    /// The hex field's caret, while it's typed in.
    pub fn caret_rect(&self) -> Option<Rect> {
        self.hex.focused.then(|| self.hex.caret_rect().translated(self.pos))
    }

    pub fn color(&self) -> Color {
        Color::from_hsv(self.h, self.s, self.v)
    }

    pub fn set_color(&mut self, color: Color) {
        let (h, s, v) = color.hsv();
        if h >= 0.0 {
            self.h = h; // (greys have no hue: keep the one there was)
        }
        self.s = s;
        self.v = v;
        self.hex.edit.set_text(&color.hex());
    }

    fn changed(&mut self) -> PickerEvent {
        let color = self.color();
        self.hex.edit.set_text(&color.hex());
        PickerEvent::Color(color)
    }

    fn typed(&mut self) -> PickerEvent {
        let mut t = self.hex.text().trim().to_string();
        if !t.starts_with('#') {
            t.insert(0, '#');
        }
        match Color::parse(&t) {
            Some(color) if t.len() == 7 => {
                let (h, s, v) = color.hsv();
                if h >= 0.0 {
                    self.h = h;
                }
                self.s = s;
                self.v = v;
                PickerEvent::Color(color)
            }
            _ => PickerEvent::None,
        }
    }

    fn local(&self, at: Point) -> Point {
        at - self.pos
    }

    pub fn press(&mut self, at: Point, shift: bool) -> PickerEvent {
        let pos = self.local(at);
        if self.dropper.rect.contains(pos) {
            self.ptr.pressed = Some("dropper");
            return PickerEvent::None;
        }
        if self.hex.rect.contains(pos) {
            self.hex.press(pos, shift);
            return PickerEvent::None;
        }
        self.hex.focused = false;
        if SHADES.adjusted(-4.0, -4.0, 4.0, 4.0).contains(pos) {
            self.dragging = Some(Dragging::Shades);
        } else if HUES.adjusted(-4.0, -6.0, 4.0, 6.0).contains(pos) {
            self.dragging = Some(Dragging::Hues);
        }
        self.drag_to(pos)
    }

    pub fn motion(&mut self, at: Point, buttons_down: bool) -> PickerEvent {
        let pos = self.local(at);
        self.ptr.hovered = self.dropper.rect.contains(pos).then_some("dropper");
        if buttons_down && self.hex.focused && self.dragging.is_none() && self.ptr.pressed.is_none() {
            self.hex.drag_to(pos);
            return PickerEvent::None;
        }
        self.drag_to(pos)
    }

    pub fn release(&mut self, at: Point) -> PickerEvent {
        let pos = self.local(at);
        self.dragging = None;
        let clicked = self.ptr.pressed.take() == Some("dropper") && self.dropper.rect.contains(pos);
        if clicked { PickerEvent::Eyedropper } else { PickerEvent::None }
    }

    fn drag_to(&mut self, pos: Point) -> PickerEvent {
        let along = |lo: f32, len: f32, at: f32| ((at - lo) / len.max(1.0)).clamp(0.0, 1.0);
        match self.dragging {
            Some(Dragging::Shades) => {
                self.s = along(SHADES.x, SHADES.w, pos.x);
                self.v = 1.0 - along(SHADES.y, SHADES.h, pos.y);
            }
            Some(Dragging::Hues) => self.h = along(HUES.x, HUES.w, pos.x).min(0.9999),
            None => return PickerEvent::None,
        }
        self.changed()
    }

    /// A key while the hex field has the focus.
    pub fn key(&mut self, press: &KeyPress) -> PickerEvent {
        match self.hex.key(press) {
            KeyResult::Commit if press.key == crate::keyseq::key::ESCAPE => PickerEvent::None,
            KeyResult::Commit => PickerEvent::Close,
            KeyResult::Copy(s) => PickerEvent::Copy(s),
            KeyResult::Paste => PickerEvent::Paste,
            _ => self.typed(),
        }
    }

    pub fn paste(&mut self, s: &str) -> PickerEvent {
        self.hex.edit.paste(s.trim());
        self.typed()
    }

    pub fn hint(&self) -> Option<String> {
        (self.ptr.hovered == Some("dropper")).then(|| self.dropper.hint.clone())
    }

    fn shades_image(&self, scale: f32) -> Option<std::cell::Ref<'_, Option<Shades>>> {
        let (w, h) = ((SHADES.w * scale).round() as u32, (SHADES.h * scale).round() as u32);
        let key = ((self.h * 10000.0) as u32, w, h);
        if self.shades.borrow().as_ref().is_none_or(|(k, _)| *k != key) {
            let mut img = Image::new(w.max(1), h.max(1)).ok()?;
            for y in 0..h {
                for x in 0..w {
                    let s = x as f32 / (w - 1).max(1) as f32;
                    let v = 1.0 - y as f32 / (h - 1).max(1) as f32;
                    img.set_pixel(x, y, Color::from_hsv(self.h, s, v).premul());
                }
            }
            *self.shades.borrow_mut() = Some((key, img));
        }
        Some(self.shades.borrow())
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        let c = theme::c();
        let o = self.pos;
        paint_panel(p, self.rect(), 12.0);
        let shades = SHADES.translated(o);
        if let Some(cache) = self.shades_image(p.scale())
            && let Some((_, img)) = cache.as_ref()
        {
            p.save();
            p.clip_rounded(shades, 6.0);
            p.draw_image(img, shades, img.rect().to_f(), true);
            p.restore();
        }
        let hues = HUES.translated(o);
        let stops: Vec<tiny_skia::GradientStop> =
            (0..7).map(|i| tiny_skia::GradientStop::new(i as f32 / 6.0, sk(Color::from_hsv((i as f32 / 6.0) % 1.0, 1.0, 1.0)))).collect();
        if let (Some(shader), Some(path)) = (
            tiny_skia::LinearGradient::new(
                tiny_skia::Point::from_xy(hues.x, 0.0),
                tiny_skia::Point::from_xy(hues.right(), 0.0),
                stops,
                tiny_skia::SpreadMode::Pad,
                tiny_skia::Transform::identity(),
            ),
            crate::render::paint::rounded_rect_path(hues, 6.0),
        ) {
            p.fill_path_shader(&path, shader);
        }
        // Markers: a white ring with a dark edge shows on any colour.
        let at = Point::new(shades.x + self.s * shades.w, shades.y + (1.0 - self.v) * shades.h);
        ring(p, at, 6.0, self.color());
        ring(p, Point::new(hues.x + self.h * hues.w, hues.center().y), 7.0, Color::from_hsv(self.h, 1.0, 1.0));
        let mut hex = self.hex.clone();
        hex.rect = hex.rect.translated(o);
        hex.paint(p, true);
        paint_item(p, &self.dropper, o, &self.ptr);
        let _ = c;
    }
}

fn ring(p: &mut Painter<'_>, at: Point, r: f32, fill: Color) {
    let bx = Rect::new(at.x - r, at.y - r, 2.0 * r, 2.0 * r);
    p.fill_ellipse(bx, fill);
    p.stroke_ellipse(bx, Pen::new(Color::rgba(0, 0, 0, 110), 4.0));
    p.stroke_ellipse(bx, Pen::new(Color::WHITE, 2.0));
}

// -- recording options -------------------------------------------------------------------

/// The recording options, as the panel shows them.
pub struct RecordView<'a> {
    pub format: &'a str,
    pub fps: i64,
    pub mic: bool,
    pub system_audio: bool,
    pub cursor: bool,
    /// (value, label, available)
    pub formats: Vec<(String, String, bool)>,
    /// why recording can't start, if it can't
    pub problem: Option<String>,
}

/// Under the area chosen for recording: start, sound and pointer options,
/// format and frame rate.
#[derive(Clone, Debug, Default)]
pub struct RecordPanel {
    pub pos: Point,
    pub size: (f32, f32),
    pub items: Vec<Item>,
    pub ptr: Pointer,
    pub choice_hover: Option<usize>,
}

impl RecordPanel {
    pub fn new(view: &RecordView<'_>) -> RecordPanel {
        let mut panel = RecordPanel::default();
        panel.refresh(view);
        panel
    }

    pub fn refresh(&mut self, v: &RecordView<'_>) {
        let gif = v.format == "gif";
        let mut start = Item::new(
            "record",
            Look::Chip { text: "Record".into(), icon: None, primary: true, key: "Enter".into(), on: None },
            v.problem.clone().unwrap_or_else(|| "Start recording  ·  Enter".into()),
        );
        start.enabled = v.problem.is_none();
        let chip = |id: &'static str, label: &str, icon: &'static str, on: bool, hint: &str, enabled: bool| {
            let mut it = Item::new(
                id,
                Look::Chip { text: label.into(), icon: Some(icon), primary: false, key: String::new(), on: Some(on) },
                format!("{hint}  ·  {}", if on { "on" } else { "off" }),
            );
            it.enabled = enabled;
            it
        };
        let mut fps = Item::new(
            "fps",
            Look::Chip { text: if v.fps == 0 { "Native".into() } else { format!("{} fps", v.fps) }, icon: None, primary: false, key: String::new(), on: None },
            "Frames per second  ·  click to change",
        );
        fps.enabled = true;
        self.items = vec![
            start,
            Item::new("divider.1", Look::Divider, ""),
            chip("mic", "Mic", "mic", v.mic && !gif, "Record the microphone", !gif),
            chip("system", "System", "speaker", v.system_audio && !gif, "Record the computer's sound", !gif),
            chip("cursor", "Cursor", "cursor", v.cursor, "Show the mouse pointer", true),
            Item::new("divider.2", Look::Divider, ""),
            Item::new(
                "format",
                Look::Choice { options: v.formats.clone(), value: v.format.to_string(), hover: self.choice_hover },
                "File type  ·  GIFs have no sound",
            ),
            fps,
            Item::new("close", Look::Icon { icon: "close", size: 34.0 }, "Choose another area  ·  Esc"),
        ];
        let w = layout_row(&mut self.items, 6.0, 6.0, 34.0, 4.0) + 6.0;
        self.size = (w, 46.0);
    }

    pub fn rect(&self) -> Rect {
        Rect::new(self.pos.x, self.pos.y, self.size.0, self.size.1)
    }

    pub fn item_at(&self, at: Point) -> Option<&Item> {
        let local = at - self.pos;
        self.items.iter().find(|it| it.rect.contains(local))
    }

    /// Which format cell is at `at` (in the format control).
    pub fn choice_at(&self, at: Point) -> Option<usize> {
        let it = self.items.iter().find(|it| it.id == "format")?;
        let Look::Choice { options, .. } = &it.look else { return None };
        let local = at - self.pos - it.rect.top_left();
        let mut x = 3.0;
        for (i, w) in crate::ui::widgets::choice_widths(options).into_iter().enumerate() {
            if Rect::new(x, 3.0, w, it.rect.h - 6.0).contains(local) {
                return Some(i);
            }
            x += w;
        }
        None
    }

    pub fn paint(&self, p: &mut Painter<'_>) {
        paint_panel(p, self.rect(), 12.0);
        for it in &self.items {
            paint_item(p, it, self.pos, &self.ptr);
        }
    }

    pub fn hint(&self) -> Option<String> {
        let id = self.ptr.hovered?;
        self.items.iter().find(|it| it.id == id).map(|it| it.hint.clone())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chips_never_overlap() {
        let mut chips: Vec<CodeChip> = (0..4).map(|i| CodeChip::new(&format!("https://example.com/{i}"), true)).collect();
        let codes: Vec<Rect> = (0..4).map(|i| Rect::new(100.0 + i as f32 * 3.0, 100.0, 50.0, 50.0)).collect();
        place_chips(&mut chips, &codes, Rect::new(0.0, 0.0, 800.0, 600.0));
        for i in 0..chips.len() {
            for j in i + 1..chips.len() {
                assert!(!chips[i].rect().intersects(&chips[j].rect()), "{i} and {j} overlap");
            }
        }
        assert!(chips[0].items.iter().any(|it| it.id == "open"));
        assert!(!CodeChip::new("4006381333931", false).items.iter().any(|it| it.id == "open"));
    }

    #[test]
    fn chip_text_is_made_safe() {
        let chip = CodeChip::new("https://evil.example/\u{202E}gnp.exe", true);
        assert!(!chip.preview.contains('\u{202E}'));
    }

    #[test]
    fn picker_round_trip() {
        let mut p = ColorPicker::new(Color::rgb(0xFF4FA3));
        assert_eq!(p.color().hex(), "#FF4FA3");
        p.pos = Point::new(100.0, 100.0);
        let e = p.press(Point::new(100.0 + 10.0 + 196.0, 100.0 + 10.0), false);
        assert!(matches!(e, PickerEvent::Color(_)));
        assert_eq!((p.s, p.v), (1.0, 1.0));
        p.release(Point::new(0.0, 0.0));
        p.hex.focused = true;
        p.hex.edit.select_all();
        let typed = KeyPress { key: 0x31, text: "#00ff00".into(), ..Default::default() };
        assert_eq!(p.key(&typed), PickerEvent::Color(Color::rgb(0x00FF00)));
    }
}
