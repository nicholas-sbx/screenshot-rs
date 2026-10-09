//! A popup menu (a pin's right-click menu), painted like the toolbar's
//! panels. It knows its entries, which one the pointer or the arrow keys
//! are on, and which was chosen.

use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::Canvas;
use crate::keys::KeyPress;
use crate::keyseq::key;
use crate::render::Painter;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, MEDIUM};
use crate::render::theme;

/// One entry; a separator has an empty key.
#[derive(Clone, Debug, PartialEq)]
pub struct Entry {
    pub key: &'static str,
    pub label: String,
    /// its keyboard shortcut, shown on the right
    pub shortcut: String,
}

impl Entry {
    pub fn new(key: &'static str, label: &str, shortcut: &str) -> Entry {
        Entry { key, label: label.into(), shortcut: shortcut.into() }
    }

    pub fn separator() -> Entry {
        Entry { key: "", label: String::new(), shortcut: String::new() }
    }

    fn is_separator(&self) -> bool {
        self.key.is_empty()
    }
}

/// What came of an input.
#[derive(Clone, Debug, PartialEq)]
pub enum Choice {
    None,
    Chosen(&'static str),
    /// closed without choosing
    Close,
}

const PAD: f32 = 5.0;
const ROW: f32 = 30.0;
const SEP: f32 = 9.0;
const RADIUS: f32 = 10.0;

fn label_font() -> text::FontSpec {
    text::font(13.0, MEDIUM)
}

fn shortcut_font() -> text::FontSpec {
    text::font(12.0, MEDIUM)
}

pub struct Menu {
    entries: Vec<Entry>,
    hovered: Option<usize>,
    width: f32,
    /// device pixels per logical one
    pub scale: f32,
    dirty: bool,
}

impl Menu {
    pub fn new(entries: Vec<Entry>) -> Menu {
        let widest = entries
            .iter()
            .map(|e| {
                let short = if e.shortcut.is_empty() { 0.0 } else { text::width(&e.shortcut, shortcut_font()) + 28.0 };
                text::width(&e.label, label_font()) + short
            })
            .fold(0.0f32, f32::max);
        let width = (widest + 2.0 * PAD + 28.0).ceil().max(160.0);
        Menu { entries, hovered: None, width, scale: 1.0, dirty: true }
    }

    /// Its size, logical pixels.
    pub fn size(&self) -> (u32, u32) {
        let h: f32 = self.entries.iter().map(|e| if e.is_separator() { SEP } else { ROW }).sum::<f32>() + 2.0 * PAD;
        (self.width as u32, h.ceil() as u32)
    }

    pub fn device_size(&self) -> (u32, u32) {
        let (w, h) = self.size();
        ((w as f32 * self.scale).round() as u32, (h as f32 * self.scale).round() as u32)
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 1e-3 {
            self.scale = scale;
            self.dirty = true;
        }
    }

    /// Each entry's place.
    fn rows(&self) -> Vec<Rect> {
        let mut y = PAD;
        self.entries
            .iter()
            .map(|e| {
                let h = if e.is_separator() { SEP } else { ROW };
                let r = Rect::new(PAD, y, self.width - 2.0 * PAD, h);
                y += h;
                r
            })
            .collect()
    }

    fn at(&self, p: Point) -> Option<usize> {
        self.rows().iter().position(|r| r.contains(p)).filter(|&i| !self.entries[i].is_separator())
    }

    fn hover(&mut self, i: Option<usize>) {
        if self.hovered != i {
            self.hovered = i;
            self.dirty = true;
        }
    }

    pub fn motion(&mut self, p: Point) {
        self.hover(self.at(p));
    }

    pub fn leave(&mut self) {
        self.hover(None);
    }

    /// A button let go at `p` (the menu chooses on release, so a right
    /// press can drag onto an entry and let go).
    pub fn release(&mut self, p: Point) -> Choice {
        match self.at(p) {
            Some(i) => Choice::Chosen(self.entries[i].key),
            None => Choice::None,
        }
    }

    /// A button pressed outside every entry closes it.
    pub fn press(&mut self, p: Point) -> Choice {
        let (w, h) = self.size();
        if p.x < 0.0 || p.y < 0.0 || p.x > w as f32 || p.y > h as f32 { Choice::Close } else { Choice::None }
    }

    pub fn key(&mut self, k: &KeyPress) -> Choice {
        let usable: Vec<usize> = (0..self.entries.len()).filter(|&i| !self.entries[i].is_separator()).collect();
        if usable.is_empty() {
            return Choice::Close;
        }
        let at = self.hovered.and_then(|h| usable.iter().position(|&i| i == h));
        let next = match k.key {
            key::ESCAPE => return Choice::Close,
            key::RETURN | key::ENTER | 0x20 => return at.map_or(Choice::None, |a| Choice::Chosen(self.entries[usable[a]].key)),
            key::DOWN => at.map_or(0, |a| (a + 1) % usable.len()),
            key::UP => at.map_or(usable.len() - 1, |a| (a + usable.len() - 1) % usable.len()),
            key::HOME => 0,
            key::END => usable.len() - 1,
            _ => return Choice::None,
        };
        self.hover(Some(usable[next]));
        Choice::None
    }

    pub fn damage(&mut self) -> Damage {
        if std::mem::take(&mut self.dirty) { Damage::full() } else { Damage::new() }
    }

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), _region: IRect) {
        target.data_mut().fill(0); // (see-through corners)
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        let c = theme::c();
        let (w, h) = self.size();
        let panel = Rect::new(0.0, 0.0, w as f32, h as f32);
        p.draw_rounded_rect(panel.adjusted(0.5, 0.5, -0.5, -0.5), RADIUS, Some(c.base), Some(Pen::new(c.line, 1.0)));
        for (i, (e, r)) in self.entries.iter().zip(self.rows()).enumerate() {
            if e.is_separator() {
                let y = r.y + r.h / 2.0;
                p.draw_line(Point::new(r.x + 6.0, y), Point::new(r.x + r.w - 6.0, y), Pen::new(c.line, 1.0));
                continue;
            }
            if self.hovered == Some(i) {
                p.fill_rounded_rect(r, 6.0, c.hover);
            }
            let text_r = Rect::new(r.x + 12.0, r.y, r.w - 24.0, r.h);
            p.draw_text(text_r, Align::Left, &e.label, label_font(), c.text);
            if !e.shortcut.is_empty() {
                p.draw_text(text_r, Align::Right, &e.shortcut, shortcut_font(), c.muted);
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn press(k: i32) -> KeyPress {
        KeyPress { key: k, mods: 0, text: String::new(), keysym: 0, repeat: false }
    }

    #[test]
    fn choosing() {
        let mut m = Menu::new(vec![Entry::new("copy", "Copy", "Ctrl+C"), Entry::separator(), Entry::new("close", "Close", "Esc")]);
        let (w, h) = m.size();
        assert!(w >= 160 && h == (2.0 * PAD + 2.0 * ROW + SEP) as u32);
        // The pointer: release over an entry chooses it, the separator nothing.
        assert_eq!(m.release(Point::new(20.0, PAD + 10.0)), Choice::Chosen("copy"));
        assert_eq!(m.release(Point::new(20.0, PAD + ROW + 3.0)), Choice::None);
        assert_eq!(m.press(Point::new(-5.0, 10.0)), Choice::Close);
        // The keys skip the separator and wrap around.
        assert_eq!(m.key(&press(key::DOWN)), Choice::None);
        assert_eq!(m.key(&press(key::DOWN)), Choice::None);
        assert_eq!(m.key(&press(key::RETURN)), Choice::Chosen("close"));
        m.key(&press(key::DOWN));
        assert_eq!(m.key(&press(key::RETURN)), Choice::Chosen("copy"));
        assert_eq!(m.key(&press(key::ESCAPE)), Choice::Close);
    }
}
