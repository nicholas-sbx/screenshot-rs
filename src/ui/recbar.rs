//! The controls on screen while recording: a small bar with the time,
//! pause / resume, stop and discard, and red corner marks just outside the
//! recorded area. Painted here; the app puts each in a surface of its own,
//! outside the area, so none of it ends up in the video.

use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::Canvas;
use crate::render::Painter;
use crate::render::icons;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD};
use crate::render::theme::{self, REC};
use crate::ui::widgets::{self, Item, Look, Pointer};

/// Between the area and the corner marks, logical pixels.
pub const FRAME_GAP: i32 = 6;
/// A corner mark's arms: length and thickness.
pub const MARK: i32 = 20;
pub const MARK_WIDTH: i32 = 3;
const HEIGHT: f32 = 46.0;
const CLOCK_W: f32 = 76.0;

/// What a click on the bar asks for.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum BarAction {
    Pause,
    Stop,
    Discard,
}

pub struct RecBar {
    items: Vec<Item>,
    ptr: Pointer,
    pub scale: f32,
    /// the time shown
    clock: String,
    paused: bool,
    size: (f32, f32),
    damaged: bool,
}

impl RecBar {
    /// `can_pause`: segments can be joined; `mic`: the microphone is recorded.
    pub fn new(can_pause: bool, mic: bool) -> RecBar {
        let mut items = vec![Item::new("clock", Look::Divider, "")];
        if mic {
            items.push(Item::new("mic", Look::Divider, "The microphone is recorded"));
        }
        if can_pause {
            items.push(Item::new("pause", Look::Icon { icon: "pause", size: 36.0 }, "Pause"));
        }
        items.push(Item::new("stop", Look::Icon { icon: "stop", size: 36.0 }, "Stop and save"));
        items.push(Item::new("trash", Look::Icon { icon: "trash", size: 36.0 }, "Discard the recording"));
        let mut bar = RecBar { items, ptr: Pointer::default(), scale: 1.0, clock: "00:00".into(), paused: false, size: (0.0, HEIGHT), damaged: true };
        bar.layout();
        bar
    }

    fn layout(&mut self) {
        let mut x = 10.0;
        for (i, it) in self.items.iter_mut().enumerate() {
            let w = match it.id {
                "clock" => CLOCK_W,
                "mic" => 26.0,
                _ => 36.0,
            };
            if i > 0 {
                x += 2.0;
            }
            it.rect = Rect::new(x, 5.0, w, 36.0);
            x += w;
        }
        self.size = (x + 6.0, HEIGHT);
    }

    /// Its size, logical pixels.
    pub fn size(&self) -> (u32, u32) {
        (self.size.0.ceil() as u32, self.size.1 as u32)
    }

    pub fn device_size(&self) -> (u32, u32) {
        ((self.size.0 * self.scale).ceil() as u32, (self.size.1 * self.scale).ceil() as u32)
    }

    /// Draw all of it again (on a surface of its own).
    pub fn invalidate(&mut self) {
        self.damaged = true;
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 1e-3 {
            self.scale = scale;
            self.damaged = true;
        }
    }

    /// The time and whether it's paused, as they are now.
    pub fn set_state(&mut self, clock: String, paused: bool) {
        if clock != self.clock || paused != self.paused {
            self.clock = clock;
            self.paused = paused;
            if let Some(it) = self.items.iter_mut().find(|i| i.id == "pause") {
                it.look = Look::Icon { icon: if paused { "resume" } else { "pause" }, size: 36.0 };
                it.hint = if paused { "Resume" } else { "Pause" }.into();
            }
            self.damaged = true;
        }
    }

    pub fn damage(&mut self) -> Damage {
        if std::mem::take(&mut self.damaged) { Damage::full() } else { Damage::new() }
    }

    fn button_at(&self, at: Point) -> Option<&'static str> {
        self.items.iter().find(|i| matches!(i.look, Look::Icon { .. }) && i.rect.contains(at)).map(|i| i.id)
    }

    /// Is `at` on a button (else the bar can be dragged from there)?
    pub fn on_button(&self, at: Point) -> bool {
        self.button_at(at).is_some()
    }

    pub fn pointer_motion(&mut self, at: Option<Point>) {
        let hovered = at.and_then(|a| self.button_at(a));
        if hovered != self.ptr.hovered {
            self.ptr.hovered = hovered;
            self.damaged = true;
        }
    }

    /// A left button press or release at `at`: what it asks for, if anything.
    pub fn button(&mut self, pressed: bool, at: Point) -> Option<BarAction> {
        let on = self.button_at(at);
        self.damaged = true;
        if pressed {
            self.ptr.pressed = on;
            return None;
        }
        let was = self.ptr.pressed.take();
        match (was, on) {
            (Some(a), Some(b)) if a == b => match a {
                "pause" => Some(BarAction::Pause),
                "stop" => Some(BarAction::Stop),
                "trash" => Some(BarAction::Discard),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), _region: IRect) {
        let c = theme::c();
        target.data_mut().fill(0);
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        let r = Rect::new(0.0, 0.0, self.size.0, self.size.1);
        p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), r.h / 2.0 - 0.5, Some(c.base), Some(Pen::new(c.line, 1.0)));
        for it in &self.items {
            match it.id {
                "clock" => {
                    let font = text::mono(14.0, DEMIBOLD);
                    p.draw_text(it.rect, Align::Center, &self.clock, font, if self.paused { c.muted } else { c.text });
                }
                "mic" => icons::paint(&mut p, "mic", Rect::new(it.rect.x + 4.0, it.rect.y + 9.0, 18.0, 18.0), c.code),
                "stop" => paint_stop(&mut p, it.rect, self.ptr.is_hovered("stop"), self.ptr.is_down("stop")),
                _ => widgets::paint_item(&mut p, it, Point::ZERO, &self.ptr),
            }
        }
    }
}

/// The red circle with a square: stop and save.
fn paint_stop(p: &mut Painter<'_>, r: Rect, hovered: bool, down: bool) {
    let c = theme::c();
    let color = if down {
        REC.darker(115)
    } else if hovered {
        REC.lighter(112)
    } else {
        REC
    };
    p.fill_ellipse(r.adjusted(2.0, 2.0, -2.0, -2.0), color);
    let s = r.w * 0.3;
    let ctr = r.center();
    p.fill_rounded_rect(Rect::new(ctr.x - s / 2.0, ctr.y - s / 2.0, s, s), 2.0, c.text);
}

/// Which corner a mark is at.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Corner {
    TopLeft,
    TopRight,
    BottomLeft,
    BottomRight,
}

pub const CORNERS: [Corner; 4] = [Corner::TopLeft, Corner::TopRight, Corner::BottomLeft, Corner::BottomRight];

/// Where the marks go around `area` (global logical pixels): just outside
/// it, so nothing Flatshot shows ends up in the video.
pub fn mark_rects(area: IRect) -> [(Corner, IRect); 4] {
    let out = area.adjusted(-FRAME_GAP - MARK_WIDTH, -FRAME_GAP - MARK_WIDTH, FRAME_GAP + MARK_WIDTH, FRAME_GAP + MARK_WIDTH);
    let (l, t, r, b) = (out.x, out.y, out.x2() - MARK, out.y2() - MARK);
    CORNERS.map(|c| {
        let (x, y) = match c {
            Corner::TopLeft => (l, t),
            Corner::TopRight => (r, t),
            Corner::BottomLeft => (l, b),
            Corner::BottomRight => (r, b),
        };
        (c, IRect::new(x, y, MARK, MARK))
    })
}

/// A mark's two arms, in its own square (the rest of it is see-through,
/// and lets clicks through).
pub fn arms(corner: Corner) -> [IRect; 2] {
    let left = matches!(corner, Corner::TopLeft | Corner::BottomLeft);
    let top = matches!(corner, Corner::TopLeft | Corner::TopRight);
    let x = if left { 0 } else { MARK - MARK_WIDTH };
    let y = if top { 0 } else { MARK - MARK_WIDTH };
    [IRect::new(0, y, MARK, MARK_WIDTH), IRect::new(x, 0, MARK_WIDTH, MARK)]
}

/// One red L, at `scale`.
pub fn paint_mark(corner: Corner, scale: f32, target: &mut Canvas<'_>, origin: (i32, i32)) {
    target.data_mut().fill(0);
    let mut p = Painter::for_region(target.pixmap_mut(), scale, origin);
    for arm in arms(corner) {
        p.fill_rect(arm.to_f(), REC);
    }
}

/// Where the bar goes: below the area, else above it, centred, on
/// `screen`; None when neither fits.
pub fn bar_spot(area: IRect, screen: IRect, size: (u32, u32)) -> Option<IRect> {
    let (w, h) = (size.0 as i32, size.1 as i32);
    let gap = FRAME_GAP + 12;
    let x = (area.center().x - w / 2).min(screen.x2() - w - 8).max(screen.x + 8);
    if area.y2() + gap + h <= screen.y2() - 8 {
        return Some(IRect::new(x, area.y2() + gap, w, h));
    }
    if area.y - gap - h >= screen.y + 8 {
        return Some(IRect::new(x, area.y - gap - h, w, h));
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bar_buttons() {
        let mut bar = RecBar::new(true, true);
        let (w, h) = bar.size();
        assert_eq!(h, 46);
        assert!(w > 200);
        let stop = bar.items.iter().find(|i| i.id == "stop").unwrap().rect.center();
        assert_eq!(bar.button(true, stop), None);
        assert_eq!(bar.button(false, stop), Some(BarAction::Stop));
        // Pressed on one, let go on another: nothing.
        let pause = bar.items.iter().find(|i| i.id == "pause").unwrap().rect.center();
        bar.button(true, pause);
        assert_eq!(bar.button(false, stop), None);
        // The clock is for dragging.
        let clock = bar.items[0].rect.center();
        assert!(!bar.on_button(clock));
        bar.set_state("00:05".into(), true);
        assert!(matches!(bar.items.iter().find(|i| i.id == "pause").unwrap().look, Look::Icon { icon: "resume", .. }));
    }

    #[test]
    fn marks_and_bar_stay_outside() {
        let area = IRect::new(100, 100, 400, 300);
        for (corner, r) in mark_rects(area) {
            for arm in arms(corner) {
                let arm = arm.translated(r.x, r.y);
                assert!(!arm.intersects(&area), "{corner:?}: {arm:?}");
                assert!(arm.intersects(&area.adjusted(-12, -12, 12, 12)), "{corner:?}: {arm:?}");
            }
        }
        let screen = IRect::new(0, 0, 1920, 1080);
        let spot = bar_spot(area, screen, (250, 46)).unwrap();
        assert!(!spot.intersects(&area) && screen.contains_rect(&spot));
        assert!(spot.y > area.y2());
        // No room below: above. No room at all: none.
        let low = IRect::new(100, 900, 400, 170);
        assert!(bar_spot(low, screen, (250, 46)).unwrap().y2() < low.y);
        assert_eq!(bar_spot(screen, screen, (250, 46)), None);
    }
}
