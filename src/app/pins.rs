//! Pins: captures kept on screen above the windows (as in Flameshot and
//! ShareX). Drag to move, scroll to zoom, double-click, middle-click or
//! Esc to close, right-click for a menu: copy, save, actual size, close.
//!
//! A pin is a layer-shell surface (above windows, placed exactly where the
//! capture was, never in the task bar), moved by changing its margins as
//! it's dragged. Where there's no layer-shell (GNOME), it's a borderless
//! window the desktop moves.

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::{App, Input};
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::{Canvas, Image};
use crate::keyseq::key;
use crate::platform::{Monitor, Owner, Painted};
use crate::render::Painter;
use crate::render::paint::Pen;
use crate::render::theme;
use crate::template::Shot;
use crate::ui::menu::{Choice, Entry, Menu};

const MIN_SIDE: f32 = 16.0;

#[derive(Clone, Copy, Debug)]
struct Drag {
    from: (i32, i32),
    grab: Point,
    moved: (f64, f64),
}
const DOUBLE_CLICK: Duration = Duration::from_millis(400);

pub struct Pin {
    pub id: u32,
    image: Arc<Image>,
    /// its natural size, logical pixels: that of the area captured
    base: (f32, f32),
    zoom: f32,
    monitor: Monitor,
    /// where its top left is, logical pixels from the monitor's
    at: (i32, i32),
    /// device pixels per logical one
    scale: f32,
    /// the picture at the size shown (when that isn't its own)
    scaled: Option<Image>,
    dirty: bool,
    /// a layer surface (else a window the desktop moves)
    layered: bool,
    /// being dragged: where it was and where the pointer was on it then,
    /// and how far the pointer has moved since
    drag: Option<Drag>,
    last_click: Option<(Instant, Point)>,
    escape_down: bool,
    pub menu: Option<Menu>,
}

impl Pin {
    /// Its size, logical pixels.
    fn size(&self) -> (u32, u32) {
        ((self.base.0 * self.zoom).round().max(MIN_SIDE) as u32, (self.base.1 * self.zoom).round().max(MIN_SIDE) as u32)
    }

    fn device_size(&self) -> (u32, u32) {
        let (w, h) = self.size();
        ((w as f32 * self.scale).round().max(1.0) as u32, (h as f32 * self.scale).round().max(1.0) as u32)
    }

    fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 1e-3 {
            self.scale = scale;
            self.scaled = None;
            self.dirty = true;
        }
    }

    /// The picture at the size shown.
    fn prepare(&mut self) {
        let (w, h) = self.device_size();
        if (self.image.width(), self.image.height()) == (w, h) || self.scaled.as_ref().is_some_and(|s| s.size() == (w, h)) {
            return;
        }
        let shrink = w <= self.image.width() && h <= self.image.height();
        let scaled = if shrink { self.image.scaled_area(w, h) } else { self.image.scaled_bilinear(w, h) };
        self.scaled = scaled.ok();
    }
}

impl Painted for Pin {
    fn damage(&mut self, _: usize) -> Damage {
        if !std::mem::take(&mut self.dirty) {
            return Damage::new();
        }
        self.prepare();
        Damage::full()
    }

    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let (w, h) = self.device_size();
        let src: &Image = match &self.scaled {
            Some(s) if s.size() == (w, h) => s,
            _ => &self.image,
        };
        paint_picture(src, self.size(), self.scale, target, origin, region);
    }

    fn buffer_size(&self, _: usize) -> (u32, u32) {
        self.device_size()
    }
}

/// A pin's picture (at the size shown) and its edge.
fn paint_picture(src: &Image, logical: (u32, u32), scale: f32, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
    // The picture, row by row into the part being drawn.
    let stride = target.stride();
    let cols = region.w.min(src.width() as i32 - region.x).max(0) as usize;
    for row in 0..region.h {
        let y = region.y + row;
        if y < 0 || y >= src.height() as i32 {
            continue;
        }
        let from = &src.row(y as u32)[region.x as usize * 4..(region.x as usize + cols) * 4];
        let at = (y - origin.1) as usize * stride + (region.x - origin.0) as usize * 4;
        target.data_mut()[at..at + cols * 4].copy_from_slice(from);
    }
    let mut p = Painter::for_region(target.pixmap_mut(), scale, origin);
    let edge = theme::c().accent.with_alpha(170);
    p.stroke_rect(Rect::new(0.5, 0.5, logical.0 as f32 - 1.0, logical.1 as f32 - 1.0), Pen::new(edge, 1.0));
}

/// A pin's menu, painted on its popup.
pub(super) struct MenuPaint<'a>(pub(super) &'a mut Menu);

impl Painted for MenuPaint<'_> {
    fn damage(&mut self, _: usize) -> Damage {
        self.0.damage()
    }
    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        self.0.paint(target, origin, region);
    }
    fn buffer_size(&self, _: usize) -> (u32, u32) {
        self.0.device_size()
    }
}

fn menu_entries() -> Vec<Entry> {
    vec![
        Entry::new("copy", "Copy", "Ctrl+C"),
        Entry::new("save", "Save to screenshots folder", "Ctrl+S"),
        Entry::new("actual", "Actual size", ""),
        Entry::separator(),
        Entry::new("close", "Close", "Esc"),
    ]
}

impl App {
    /// Pin `image`. `at`: where it was captured (global logical pixels): the
    /// pin opens right there, at that size. `size`: the size it was
    /// captured at, when where can't be used (the monitors changed since).
    pub(super) fn pin(&mut self, image: Arc<Image>, at: Option<IRect>, size: Option<(u32, u32)>) {
        let monitors = self.dpy.monitors();
        let Some(first) = monitors.first() else { return };
        let mon = at
            .and_then(|a| {
                let centre = crate::geom::IPoint::new(a.x + a.w / 2, a.y + a.h / 2);
                monitors.iter().find(|m| m.rect.contains(centre))
            })
            .unwrap_or(first)
            .clone();
        let scale = mon.scale.max(1) as f32;
        let base = match (at, size) {
            (Some(a), _) => (a.w as f32, a.h as f32),
            (None, Some((w, h))) => (w as f32, h as f32),
            // Its pixels are no guide to its size on another monitor: as is.
            (None, None) => (image.width() as f32 / scale, image.height() as f32 / scale),
        };
        let zoom = 1.0f32.min(mon.rect.w as f32 * 0.9 / base.0.max(1.0)).min(mon.rect.h as f32 * 0.9 / base.1.max(1.0));
        let shown = ((base.0 * zoom).round() as i32, (base.1 * zoom).round() as i32);
        let place = match at {
            Some(a) => (a.x - mon.rect.x, a.y - mon.rect.y),
            None => ((mon.rect.w - shown.0) / 2, (mon.rect.h - shown.1) / 2),
        };
        self.next_pin += 1;
        let id = self.next_pin;
        let mut pin = Pin {
            id,
            image,
            base,
            zoom,
            monitor: mon.clone(),
            at: place,
            scale,
            scaled: None,
            dirty: true,
            layered: true,
            drag: None,
            last_click: None,
            escape_down: false,
            menu: None,
        };
        let size = pin.size();
        if !self.dpy.add_layer(Owner::Pin(id), &mon, place, size, "flatshot-rs-pin") {
            pin.layered = false;
            if let Err(e) = self.dpy.add_window(Owner::Pin(id), &format!("{} pin {id}", crate::APP_NAME), size, false) {
                crate::warn!("can't pin: {e}");
                return;
            }
        }
        self.pins.push(pin);
        self.dpy.flush();
    }

    pub(super) fn has_pins(&self) -> bool {
        !self.pins.is_empty()
    }

    fn pin_index(&self, id: u32) -> Option<usize> {
        self.pins.iter().position(|p| p.id == id)
    }

    pub(super) fn close_pin(&mut self, id: u32) {
        self.dpy.remove(Owner::Menu(id));
        self.dpy.remove(Owner::Pin(id));
        self.pins.retain(|p| p.id != id);
        crate::mem::trim();
    }

    /// Input on pin `id`.
    pub(super) fn pin_input(&mut self, id: u32, ev: Input) {
        let Some(i) = self.pin_index(id) else { return };
        if self.pins[i].menu.is_some() && matches!(ev, Input::Key(_) | Input::KeyUp(_)) {
            // (its menu takes the keys, wherever the compositor sends them)
            return self.menu_input(id, ev);
        }
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        match ev {
            Input::Button(0x110, true, at) => {
                let at = p(at);
                let now = Instant::now();
                let pin = &mut self.pins[i];
                if let Some((t, q)) = pin.last_click
                    && now.duration_since(t) < DOUBLE_CLICK
                    && (q.x - at.x).abs() < 5.0
                    && (q.y - at.y).abs() < 5.0
                {
                    return self.close_pin(id);
                }
                pin.last_click = Some((now, at));
                if pin.layered {
                    pin.drag = Some(Drag { from: pin.at, grab: at, moved: (0.0, 0.0) });
                } else {
                    self.dpy.start_move(Owner::Pin(id));
                }
            }
            Input::Button(0x110, false, _) => {
                self.pins[i].drag = None;
            }
            Input::Button(0x112, true, _) => self.close_pin(id),
            Input::Button(0x111, true, at) => self.open_pin_menu(id, p(at)),
            Input::Motion(at, _) if !self.dpy.has_relative() => {
                // (no relative motion: where the pointer is on the pin, as
                // compositors report it while a button is held, relative
                // to where the pin was when it was pressed)
                let Some(d) = &mut self.pins[i].drag else { return };
                d.moved = ((at.0 as f32 - d.grab.x) as f64, (at.1 as f32 - d.grab.y) as f64);
                self.drag_pin(i);
            }
            Input::Scroll(steps) => {
                let pin = &mut self.pins[i];
                let zoom = (pin.zoom * 1.15f32.powf(steps)).clamp(0.1, 5.0);
                self.zoom_pin(i, zoom);
            }
            Input::Key(k) => {
                if k.key == key::ESCAPE {
                    // (closed when it's let go, which would otherwise go to the window below)
                    self.pins[i].escape_down = true;
                } else if k.ctrl() && (k.key == 'C' as i32) {
                    self.copy_pin(i);
                } else if k.ctrl() && (k.key == 'S' as i32) {
                    self.save_pin(i);
                }
            }
            Input::KeyUp(k) if k.key == key::ESCAPE && self.pins[i].escape_down => self.close_pin(id),
            _ => {}
        }
    }

    /// The pointer moved by (`dx`, `dy`): a pin being dragged follows.
    pub fn relative_motion(&mut self, dx: f64, dy: f64) {
        if self.drag_recbar(dx, dy) {
            return;
        }
        let Some(i) = self.pins.iter().position(|p| p.drag.is_some()) else { return };
        if let Some(d) = &mut self.pins[i].drag {
            d.moved = (d.moved.0 + dx, d.moved.1 + dy);
        }
        self.drag_pin(i);
    }

    /// Put a dragged pin where the pointer took it (kept at least partly
    /// on its monitor).
    fn drag_pin(&mut self, i: usize) {
        let pin = &mut self.pins[i];
        let Some(d) = pin.drag else { return };
        let (w, _) = pin.size();
        let r = pin.monitor.rect;
        let x = (d.from.0 as f64 + d.moved.0).round() as i32;
        let y = (d.from.1 as f64 + d.moved.1).round() as i32;
        let target = (x.clamp(24 - w as i32, r.w - 24), y.clamp(0, r.h - 24));
        if target != pin.at {
            pin.at = target;
            let (id, size) = (pin.id, pin.size());
            self.dpy.place_layer(Owner::Pin(id), target, size);
        }
    }

    fn zoom_pin(&mut self, i: usize, zoom: f32) {
        let pin = &mut self.pins[i];
        if (zoom - pin.zoom).abs() < 1e-4 {
            return;
        }
        pin.zoom = zoom;
        pin.scaled = None;
        pin.dirty = true;
        let (id, at, size, layered) = (pin.id, pin.at, pin.size(), pin.layered);
        if layered {
            self.dpy.place_layer(Owner::Pin(id), at, size);
        }
        self.dpy.set_logical_size(Owner::Pin(id), size);
    }

    fn open_pin_menu(&mut self, id: u32, at: Point) {
        self.dpy.remove(Owner::Menu(id));
        let Some(i) = self.pin_index(id) else { return };
        let mut menu = Menu::new(menu_entries());
        menu.set_scale(self.pins[i].scale);
        let (w, h) = menu.size();
        let anchor = IRect::new(at.x.round() as i32, at.y.round() as i32, 1, 1);
        match self.dpy.add_popup(Owner::Menu(id), Owner::Pin(id), anchor, (w, h)) {
            Ok(()) => self.pins[i].menu = Some(menu),
            Err(e) => crate::warn!("no menu: {e}"),
        }
    }

    /// Input on pin `id`'s menu.
    pub(super) fn menu_input(&mut self, id: u32, ev: Input) {
        let Some(i) = self.pin_index(id) else { return };
        let Some(menu) = self.pins[i].menu.as_mut() else { return };
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        let choice = match ev {
            Input::Enter(at) | Input::Motion(at, _) => {
                menu.motion(p(at));
                Choice::None
            }
            Input::Leave => {
                menu.leave();
                Choice::None
            }
            Input::Button(_, true, at) => menu.press(p(at)),
            Input::Button(_, false, at) => menu.release(p(at)),
            Input::Key(k) => menu.key(&k),
            _ => Choice::None,
        };
        match choice {
            Choice::None => {}
            Choice::Close => self.close_menu(id),
            Choice::Chosen(action) => {
                self.close_menu(id);
                let Some(i) = self.pin_index(id) else { return };
                match action {
                    "copy" => self.copy_pin(i),
                    "save" => self.save_pin(i),
                    "actual" => self.zoom_pin(i, 1.0),
                    "close" => self.close_pin(id),
                    _ => {}
                }
            }
        }
    }

    pub(super) fn close_menu(&mut self, id: u32) {
        self.dpy.remove(Owner::Menu(id));
        if let Some(i) = self.pin_index(id) {
            self.pins[i].menu = None;
        }
    }

    fn copy_pin(&mut self, i: usize) {
        match crate::codec::encode_png(&self.pins[i].image, false) {
            Ok(png) => {
                let slot = crate::clipboard::Slot::ready(Arc::new(png));
                if !self.dpy.offer(vec!["image/png".into()], slot) {
                    crate::warn!("could not copy: no clipboard");
                }
            }
            Err(e) => crate::warn!("{e}"),
        }
    }

    fn save_pin(&mut self, i: usize) {
        let cfg = crate::config::load();
        match crate::deliver::save(&self.pins[i].image, &cfg, None, &Shot::new("region")) {
            Ok((path, _, _)) => crate::util::print_line(&path.to_string_lossy()),
            Err(e) => crate::warn!("{e}"),
        }
    }

    /// The colour theme changed: the pins' edges follow.
    pub(super) fn restyle_pins(&mut self) {
        for pin in &mut self.pins {
            pin.dirty = true;
        }
    }

    /// Draw the pins (and their menus) that changed.
    pub(super) fn redraw_pins(&mut self) {
        for owner in self.dpy.owners() {
            let Some((_, scale)) = self.dpy.surface(owner) else { continue };
            match owner {
                Owner::Pin(id) => {
                    if let Some(pin) = self.pins.iter_mut().find(|p| p.id == id) {
                        pin.set_scale(scale);
                        self.dpy.redraw(owner, 0, pin);
                    }
                }
                Owner::Menu(id) => {
                    if let Some(menu) = self.pins.iter_mut().find(|p| p.id == id).and_then(|p| p.menu.as_mut()) {
                        menu.set_scale(scale);
                        self.dpy.redraw(owner, 0, &mut MenuPaint(menu));
                    }
                }
                _ => {}
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn edge_is_one_crisp_pixel() {
        let image = Image::filled(200, 100, 0xFF3F_3F3F).unwrap();
        let mut buf = vec![0u8; 200 * 100 * 4];
        let mut c = Canvas::new(&mut buf, 200, 100).unwrap();
        paint_picture(&image, (200, 100), 1.0, &mut c, (0, 0), IRect::new(0, 0, 200, 100));
        let px = |x: usize, y: usize| u32::from_le_bytes(buf[(y * 200 + x) * 4..(y * 200 + x) * 4 + 4].try_into().unwrap());
        for y in [1, 50, 98] {
            assert_ne!(px(0, y), 0xFF3F_3F3F, "edge at row {y}");
            assert_eq!(px(1, y), 0xFF3F_3F3F, "inside at row {y}");
        }
        // A part drawn on its own matches.
        let mut part = vec![0u8; 20 * 10 * 4];
        let mut c = Canvas::new(&mut part, 20, 10).unwrap();
        paint_picture(&image, (200, 100), 1.0, &mut c, (0, 45), IRect::new(0, 45, 20, 10));
        assert_eq!(&part[..20 * 4], &buf[45 * 800..45 * 800 + 80]);
    }
}
