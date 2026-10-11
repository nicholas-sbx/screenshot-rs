//! The text caret's blink, the same in every window: on for half of each
//! period, counted from when it last moved (typing shows it at once), and
//! only in the window with the keyboard.

use std::time::Instant;

use crate::geom::Rect;
use crate::platform::Owner;
use crate::ui::caret::HALF;

/// The overlays are one window to the keyboard: it's on one of them, and
/// the text being typed can be on another.
fn same_window(a: Owner, b: Owner) -> bool {
    a == b || matches!((a, b), (Owner::Overlay(_), Owner::Overlay(_)))
}

#[derive(Debug)]
pub struct Blink {
    /// the window with the keyboard
    focus: Option<Owner>,
    /// where its caret is (window logical), when it has one
    caret: Option<(Owner, Rect)>,
    since: Instant,
    on: bool,
}

impl Default for Blink {
    fn default() -> Self {
        Blink { focus: None, caret: None, since: Instant::now(), on: true }
    }
}

impl Blink {
    /// The keyboard is on `focus`, and the caret at `caret`: what to
    /// repaint for it (the caret where it was and where it is, when it
    /// blinked, moved or the keyboard went elsewhere).
    pub fn update(&mut self, now: Instant, focus: Option<Owner>, caret: Option<(Owner, Rect)>) -> Vec<(Owner, Rect)> {
        let mut repaint = Vec::new();
        if focus != self.focus || caret != self.caret {
            repaint.extend(self.caret);
            repaint.extend(caret);
            self.focus = focus;
            self.caret = caret;
            self.since = now;
            self.on = true;
            return repaint;
        }
        let on = self.caret.is_none() || (now.saturating_duration_since(self.since).as_millis() / HALF.as_millis()).is_multiple_of(2);
        if on != self.on {
            self.on = on;
            repaint.extend(self.caret);
        }
        repaint
    }

    /// Is the caret drawn in `owner`?
    pub fn shown_in(&self, owner: Owner) -> bool {
        self.on && self.focus.is_some_and(|f| same_window(f, owner))
    }

    /// When it next blinks.
    pub fn deadline(&self) -> Option<Instant> {
        self.caret?;
        let n = self.since.elapsed().as_millis() / HALF.as_millis() + 1;
        Some(self.since + HALF * n as u32)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blinks_in_the_focused_window_only() {
        let t0 = Instant::now();
        let mut b = Blink::default();
        let r = Rect::new(10.0, 10.0, 1.0, 16.0);
        let ed = Owner::Editor(1);
        assert_eq!(b.update(t0, Some(ed), Some((ed, r))), vec![(ed, r)]);
        assert!(b.shown_in(ed) && !b.shown_in(Owner::Settings));
        assert!(b.update(t0 + HALF / 2, Some(ed), Some((ed, r))).is_empty());
        assert_eq!(b.update(t0 + HALF + HALF / 2, Some(ed), Some((ed, r))), vec![(ed, r)], "off");
        assert!(!b.shown_in(ed));
        assert_eq!(b.update(t0 + HALF * 2, Some(ed), Some((ed, r))), vec![(ed, r)], "on again");
        assert!(b.shown_in(ed));
        // Moved: on at once, the old and new places repainted.
        let moved = Rect::new(20.0, 10.0, 1.0, 16.0);
        b.update(t0 + HALF * 3, Some(ed), Some((ed, r)));
        assert!(!b.shown_in(ed));
        assert_eq!(b.update(t0 + HALF * 3, Some(ed), Some((ed, moved))), vec![(ed, r), (ed, moved)]);
        assert!(b.shown_in(ed));
        // The keyboard elsewhere: hidden.
        assert_eq!(b.update(t0 + HALF * 4, None, None), vec![(ed, moved)]);
        assert!(!b.shown_in(ed));
        assert_eq!(b.deadline(), None);
        // Overlays share the keyboard.
        b.update(t0, Some(Owner::Overlay(0)), Some((Owner::Overlay(1), r)));
        assert!(b.shown_in(Owner::Overlay(1)));
    }
}
