//! The countdown before a delayed capture: a small dial at the bottom of
//! each monitor, counting the seconds left with a ring running down. It
//! never takes the keyboard or the pointer (they go to what's under it),
//! so a menu opened for the capture stays open, and it's gone a moment
//! before the picture is taken, so it's never in it.

use std::time::{Duration, Instant};

use tiny_skia::PathBuilder;

use super::{App, Job};
use crate::geom::{Damage, IRect, Rect};
use crate::image::Canvas;
use crate::platform::{Owner, Painted};
use crate::render::Painter;
use crate::render::icons::arc_to;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD};
use crate::render::theme::{self, Color};

/// The dial's size (logical pixels).
const DIAL: f32 = 104.0;
/// How far above the bottom of the monitor it sits.
const ABOVE: i32 = 72;
/// How long before the picture it goes: off the screen by then, a desktop
/// that fades it out included.
const GONE_BEFORE: Duration = Duration::from_millis(350);
/// How often the ring moves on where the display doesn't pace it (X11; on
/// Wayland each frame shown asks for the next).
const FRAME: Duration = Duration::from_millis(16);
/// The steps the ring is drawn in, round the whole of it (a quarter of a
/// degree: well under a pixel, so it runs down smoothly).
const RING_STEPS: f32 = 1440.0;

/// A delayed capture counting down.
pub(super) struct Countdown {
    job: Job,
    started: Instant,
    /// when the picture is taken
    at: Instant,
    /// the settings are read again first (the tray app's; a one-off run
    /// keeps what its options changed)
    reload: bool,
    /// the dials shown, by monitor: the scale and what each was drawn with
    dials: Vec<(u8, f32, Option<Shows>)>,
}

/// What a dial shows: the seconds left, and its ring (in RING_STEPS).
type Shows = (u64, u16);

impl Countdown {
    /// The seconds left, and how much of the wait is (1 to 0), at `now`.
    fn left(&self, now: Instant) -> (f32, f32) {
        let total = self.at.saturating_duration_since(self.started).as_secs_f32().max(0.001);
        let left = self.at.saturating_duration_since(now).as_secs_f32();
        (left, (left / total).clamp(0.0, 1.0))
    }
}

struct DialPaint {
    scale: f32,
    seconds: u64,
    fraction: f32,
}

impl Painted for DialPaint {
    fn damage(&mut self, _: usize) -> Damage {
        Damage::full()
    }
    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), _: IRect) {
        target.data_mut().fill(0); // (see-through round it)
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        paint_dial(&mut p, self.seconds, self.fraction);
    }
    fn buffer_size(&self, _: usize) -> (u32, u32) {
        let s = (DIAL * self.scale).ceil() as u32;
        (s, s)
    }
}

/// The dial: the seconds left in the middle, a ring of how much of the
/// wait is left, from the top, clockwise.
pub fn paint_dial(p: &mut Painter<'_>, seconds: u64, fraction: f32) {
    let c = theme::c();
    let face = Rect::new(4.0, 4.0, DIAL - 8.0, DIAL - 8.0);
    p.fill_ellipse(face, Color::rgba(14, 14, 20, 236));
    p.stroke_ellipse(face, Pen::new(Color::rgba(255, 255, 255, 36), 1.0));
    let ring = face.adjusted(9.0, 9.0, -9.0, -9.0);
    p.stroke_ellipse(ring, Pen::new(Color::rgba(255, 255, 255, 40), 5.0));
    if fraction > 0.001 {
        let mut pb = PathBuilder::new();
        arc_to(&mut pb, ring, 90.0, -360.0 * fraction, true);
        if let Some(path) = pb.finish() {
            p.stroke_path(&path, Pen::round(c.accent, 5.0));
        }
    }
    p.draw_text(face, Align::Center, &seconds.to_string(), text::font(38.0, DEMIBOLD), Color::rgb(0xF2EEE6));
}

impl App {
    /// Capture `job` after `delay`, counting down on screen (where a dial
    /// can be placed; just waiting where it can't). One at a time: a new
    /// one takes the place of the one counting.
    pub(super) fn count_down(&mut self, job: Job, delay: Duration, reload: bool) {
        self.stop_countdown();
        let now = Instant::now();
        let mut cd = Countdown { job, started: now, at: now + delay, reload, dials: Vec::new() };
        if delay > GONE_BEFORE && self.dpy.can_place() {
            let size = DIAL.ceil() as u32;
            for (i, m) in self.dpy.monitors().iter().enumerate().take(u8::MAX as usize) {
                let at = ((m.rect.w - size as i32) / 2, m.rect.h - size as i32 - ABOVE);
                if self.dpy.add_control(Owner::Countdown(i as u8), m, at, (size, size), "flatshot-countdown", true) {
                    cd.dials.push((i as u8, 0.0, None));
                }
            }
            self.dpy.flush();
        }
        self.countdown = Some(cd);
    }

    fn hide_dials(&mut self) {
        let Some(cd) = &mut self.countdown else { return };
        for (i, _, _) in std::mem::take(&mut cd.dials) {
            self.dpy.remove(Owner::Countdown(i));
        }
        self.dpy.flush();
    }

    /// No capture counting down any more.
    pub(super) fn stop_countdown(&mut self) {
        self.hide_dials();
        self.countdown = None;
    }

    pub(super) fn countdown_deadline(&self) -> Option<Instant> {
        let cd = self.countdown.as_ref()?;
        if cd.dials.is_empty() {
            return Some(cd.at);
        }
        Some((Instant::now() + FRAME).min(cd.at - GONE_BEFORE))
    }

    /// The dials go a moment before the picture; then it's taken.
    pub(super) fn poll_countdown(&mut self) {
        let Some(cd) = &self.countdown else { return };
        let now = Instant::now();
        if now >= cd.at {
            self.hide_dials();
            let Some(mut cd) = self.countdown.take() else { return };
            cd.job.asked = now;
            if cd.reload {
                self.capture_later(cd.job, Duration::ZERO);
            } else {
                self.start_capture(cd.job);
            }
        } else if now + GONE_BEFORE >= cd.at && !cd.dials.is_empty() {
            self.hide_dials();
        }
    }

    /// Draw the dials whose second or ring changed.
    pub(super) fn redraw_countdown(&mut self) {
        let Some(cd) = &mut self.countdown else { return };
        let (left, fraction) = cd.left(Instant::now());
        let seconds = left.ceil().max(1.0) as u64;
        let key = (seconds, (fraction * RING_STEPS).round() as u16);
        for (i, scale, drawn) in &mut cd.dials {
            let Some((_, s)) = self.dpy.surface(Owner::Countdown(*i)) else { continue };
            if *drawn == Some(key) && *scale == s {
                continue;
            }
            if self.dpy.redraw(Owner::Countdown(*i), 0, &mut DialPaint { scale: s, seconds, fraction }) {
                (*scale, *drawn) = (s, Some(key));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::image::Image;

    #[test]
    fn the_dial_shows_the_seconds_and_the_ring_runs_down() {
        crate::render::text::warm_up();
        let draw = |fraction: f32| {
            let mut img = Image::new(DIAL as u32, DIAL as u32).unwrap();
            {
                let mut p = Painter::new(img.pixmap_mut(), 1.0);
                paint_dial(&mut p, 3, fraction);
            }
            img
        };
        let (full, half) = (draw(1.0), draw(0.5));
        // See-through round the face; the ring's left half lit when full, not at half.
        assert_eq!(full.pixel(0, 0) >> 24, 0);
        // (the ring's middle: 4 + 9 in from each side)
        let left_of_ring = (13, (DIAL / 2.0) as u32);
        assert_ne!(full.pixel(left_of_ring.0, left_of_ring.1), half.pixel(left_of_ring.0, left_of_ring.1));
        let right_of_ring = (DIAL as u32 - 13, (DIAL / 2.0) as u32);
        assert_eq!(full.pixel(right_of_ring.0, right_of_ring.1), half.pixel(right_of_ring.0, right_of_ring.1));
    }
}
