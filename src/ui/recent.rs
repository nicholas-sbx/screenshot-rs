//! The Recent captures window: the captures saved lately (history.rs),
//! newest first, as cards with a picture of each and what's known about
//! it, to open, annotate, copy, find in their folder or take off the list.
//!
//! As the settings window: its state, input and painting, with nothing of
//! the window system. What needs the desktop is asked for with
//! [`Request`]s, and the pictures are made elsewhere (from the files, on a
//! thread) and handed in, only for the cards in view.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::geom::{Damage, IRect, Point, Rect};
use crate::history::Entry;
use crate::image::{Canvas, Image};
use crate::keys::KeyPress;
use crate::keyseq::{self, key};
use crate::render::Painter;
use crate::render::icons;
use crate::render::paint::{Align, Pen, rounded_rect_path};
use crate::render::text::{self, DEMIBOLD, MEDIUM, NORMAL};
use crate::render::theme::{self, Color};
use crate::ui::session::{Button, CursorShape};

const MARGIN: f32 = 24.0;
const GAP: f32 = 16.0;
/// The width a card aims for: a wider window has more of them across.
const CARD_W: f32 = 236.0;
/// Its picture's height for its width.
const THUMB_RATIO: f32 = 10.0 / 16.0;
/// Its three lines of text under the picture.
const TEXT_H: f32 = 64.0;
/// The title, its note and the buttons above the cards.
const HEADER_H: f32 = 84.0;
/// An action's button over a card's picture.
const ACTION: f32 = 28.0;
/// The scroll bar, and how far from the edge it's within reach.
const BAR_REACH: f32 = 12.0;
/// The most pictures kept made at once (those furthest from view go).
const MAX_THUMBS: usize = 48;
/// The biggest a card's picture is made, in pixels (it's drawn to fit).
pub const THUMB_MAX: (u32, u32) = (480, 300);
const DOUBLE_CLICK: Duration = Duration::from_millis(450);
/// How long "Clear list" waits for its second click.
const CONFIRM_FOR: Duration = Duration::from_secs(4);

/// What the window needs done outside itself.
#[derive(Clone, Debug, PartialEq)]
pub enum Request {
    /// make these cards' pictures, then hand each to `thumbnail`
    Thumbnails(Vec<Entry>),
    Open(PathBuf),
    Annotate(PathBuf),
    ShowInFolder(PathBuf),
    /// the picture (or a recording's file) on the clipboard
    Copy(Box<Entry>),
    /// take these off the list (their files stay)
    Remove(Vec<PathBuf>),
    /// empty the list (the files stay)
    Clear,
    OpenFolder,
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Action {
    Open,
    Annotate,
    Copy,
    Folder,
    Remove,
}

/// (action, icon, what it does)
const ACTIONS: [(Action, &str, &str); 5] = [
    (Action::Open, "open", "Open  ·  Enter or double-click"),
    (Action::Annotate, "pen", "Annotate in the editor"),
    (Action::Copy, "copy", "Copy  ·  Ctrl+C"),
    (Action::Folder, "folder", "Show in its folder"),
    (Action::Remove, "trash", "Take off the list (the file stays)  ·  Delete"),
];

#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    Card(usize),
    /// a card's action
    Action(usize, Action),
    OpenFolder,
    Clear,
}

enum Thumb {
    Asked,
    Ready(Arc<Image>),
    /// none to be had (no file, or not a picture)
    Missing,
}

pub struct Recent {
    entries: Vec<Entry>,
    /// whether each entry's file is still there
    present: Vec<bool>,
    thumbs: HashMap<PathBuf, Thumb>,
    view: (f32, f32),
    pub scale: f32,
    scroll: f32,
    pointer: Option<Point>,
    hovered: Option<Hit>,
    pressed: Option<Hit>,
    selected: Option<usize>,
    last_click: Option<(usize, Instant)>,
    /// the scroll bar's thumb being dragged, held this far below its top
    bar_drag: Option<f32>,
    /// "Clear list" was clicked once: until when a second click clears
    confirm_until: Option<Instant>,
    mods: i32,
    damaged: bool,
    /// parts to draw again when not all of it is (logical rectangles)
    touched: Vec<Rect>,
    requests: Vec<Request>,
}

impl Default for Recent {
    fn default() -> Self {
        Recent::new(Vec::new())
    }
}

impl Recent {
    pub fn new(entries: Vec<Entry>) -> Recent {
        let mut r = Recent {
            entries: Vec::new(),
            present: Vec::new(),
            thumbs: HashMap::new(),
            view: (900.0, 640.0),
            scale: 1.0,
            scroll: 0.0,
            pointer: None,
            hovered: None,
            pressed: None,
            selected: None,
            last_click: None,
            bar_drag: None,
            confirm_until: None,
            mods: 0,
            damaged: true,
            touched: Vec::new(),
            requests: Vec::new(),
        };
        r.set_entries(entries);
        r
    }

    /// The list as it is now (read again): the selection and the pictures
    /// made stay with their files.
    pub fn set_entries(&mut self, entries: Vec<Entry>) {
        let selected = self.selected.and_then(|i| self.entries.get(i)).map(|e| e.path.clone());
        self.present = entries.iter().map(|e| e.path.is_file()).collect();
        self.selected = selected.and_then(|p| entries.iter().position(|e| e.path == p));
        self.thumbs.retain(|p, _| entries.iter().any(|e| &e.path == p));
        self.entries = entries;
        self.clamp_scroll();
        self.hovered = self.pointer.and_then(|p| self.hit_at(p));
        self.damaged = true;
        self.want_thumbnails();
    }

    pub fn entries(&self) -> &[Entry] {
        &self.entries
    }

    pub fn set_view(&mut self, w: f32, h: f32) {
        if (w, h) != self.view {
            self.view = (w, h);
            self.clamp_scroll();
            self.damaged = true;
            self.want_thumbnails();
        }
    }

    pub fn set_scale(&mut self, scale: f32) {
        if (scale - self.scale).abs() > 1e-3 {
            self.scale = scale;
            self.damaged = true;
        }
    }

    pub fn device_size(&self) -> (u32, u32) {
        ((self.view.0 * self.scale).round() as u32, (self.view.1 * self.scale).round() as u32)
    }

    pub fn damage(&mut self) -> Damage {
        let touched = std::mem::take(&mut self.touched);
        if std::mem::take(&mut self.damaged) {
            return Damage::full();
        }
        let mut d = Damage::new();
        for r in touched {
            d.add_logical(r, self.scale);
        }
        d
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// A card's picture, made (None: there's none to be had).
    pub fn thumbnail(&mut self, path: &Path, picture: Option<Image>) {
        let Some(t) = self.thumbs.get_mut(path) else { return }; // (no longer wanted)
        *t = match picture {
            Some(img) => Thumb::Ready(Arc::new(img)),
            None => Thumb::Missing,
        };
        if let Some(i) = self.entries.iter().position(|e| e.path == path) {
            let g = self.grid();
            self.touched.push(g.card(i, self.scroll));
        }
    }

    /// When it next needs a look: "Clear list" going back to itself.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.confirm_until
    }

    pub fn on_timer(&mut self, now: Instant) {
        if self.confirm_until.is_some_and(|t| t <= now) {
            self.confirm_until = None;
            self.damaged = true;
        }
    }

    // -- layout ----------------------------------------------------------------------------

    fn grid(&self) -> Grid {
        let inner = (self.view.0 - 2.0 * MARGIN).max(CARD_W.min(self.view.0));
        let cols = (((inner + GAP) / (CARD_W + GAP)).floor() as usize).max(1);
        let card_w = ((inner - GAP * (cols - 1) as f32) / cols as f32).floor();
        let thumb_h = (card_w * THUMB_RATIO).round();
        Grid { cols, card_w, thumb_h, card_h: thumb_h + TEXT_H, rows: self.entries.len().div_ceil(cols) }
    }

    /// The page's height (all the cards, scrolled or not).
    fn page_height(&self) -> f32 {
        let g = self.grid();
        HEADER_H + g.rows as f32 * (g.card_h + GAP) + MARGIN
    }

    fn clamp_scroll(&mut self) {
        self.scroll = self.scroll.clamp(0.0, (self.page_height() - self.view.1).max(0.0));
    }

    fn header_buttons(&self) -> (Rect, Rect) {
        let clear_w = button_width(if self.confirm_until.is_some() { "Click again to clear" } else { "Clear list" });
        let clear = Rect::new(self.view.0 - MARGIN - clear_w, 24.0, clear_w, 30.0);
        let folder_w = button_width("Open folder");
        (Rect::new(clear.x - 8.0 - folder_w, 24.0, folder_w, 30.0), clear)
    }

    /// The action buttons over card `i`'s picture, right to left from its
    /// bottom right (only those that make sense for it).
    fn action_rects(&self, g: &Grid, i: usize) -> Vec<(Action, Rect)> {
        let card = g.card(i, self.scroll);
        let video = self.entries.get(i).is_some_and(Entry::is_video);
        let present = self.present.get(i).copied().unwrap_or(false);
        let shown: Vec<Action> =
            ACTIONS.iter().map(|a| a.0).filter(|a| present || *a == Action::Remove).filter(|a| !(video && *a == Action::Annotate)).collect();
        let n = shown.len() as f32;
        let y = card.y + 6.0 + g.thumb_h - 6.0 - ACTION - 6.0;
        let mut x = card.right() - 12.0 - n * ACTION - (n - 1.0) * 2.0;
        shown
            .into_iter()
            .map(|a| {
                let r = Rect::new(x, y, ACTION, ACTION);
                x += ACTION + 2.0;
                (a, r)
            })
            .collect()
    }

    fn hit_at(&self, at: Point) -> Option<Hit> {
        if at.y < HEADER_H {
            let (folder, clear) = self.header_buttons();
            return if folder.contains(at) {
                Some(Hit::OpenFolder)
            } else if clear.contains(at) && !self.entries.is_empty() {
                Some(Hit::Clear)
            } else {
                None
            };
        }
        let g = self.grid();
        let i = self.visible_range(&g).find(|&i| g.card(i, self.scroll).contains(at))?;
        if (self.hovered.is_some_and(|h| matches!(h, Hit::Card(j) | Hit::Action(j, _) if j == i)) || self.selected == Some(i))
            && let Some((a, _)) = self.action_rects(&g, i).into_iter().find(|(_, r)| r.contains(at))
        {
            return Some(Hit::Action(i, a));
        }
        Some(Hit::Card(i))
    }

    /// The cards (partly) in view.
    fn visible_range(&self, g: &Grid) -> std::ops::Range<usize> {
        let row_h = g.card_h + GAP;
        let first = (((self.scroll - HEADER_H) / row_h).floor().max(0.0)) as usize;
        let last = (((self.scroll + self.view.1 - HEADER_H) / row_h).ceil().max(0.0)) as usize;
        (first * g.cols).min(self.entries.len())..((last + 1) * g.cols).min(self.entries.len())
    }

    /// Ask for the pictures of the cards in view (and a row past it), and
    /// let go of those furthest from it when too many are kept.
    fn want_thumbnails(&mut self) {
        let g = self.grid();
        let range = self.visible_range(&g);
        let wanted = range.start.saturating_sub(g.cols)..(range.end + g.cols).min(self.entries.len());
        let mut asked = Vec::new();
        for e in &self.entries[wanted.clone()] {
            if !self.thumbs.contains_key(&e.path) {
                self.thumbs.insert(e.path.clone(), Thumb::Asked);
                asked.push(e.clone());
            }
        }
        if !asked.is_empty() {
            self.requests.push(Request::Thumbnails(asked));
        }
        if self.thumbs.len() > MAX_THUMBS {
            let middle = (wanted.start + wanted.end) / 2;
            let mut far: Vec<(usize, PathBuf)> = self
                .entries
                .iter()
                .enumerate()
                .filter(|(i, e)| !wanted.contains(i) && self.thumbs.contains_key(&e.path))
                .map(|(i, e)| (i.abs_diff(middle), e.path.clone()))
                .collect();
            far.sort();
            while self.thumbs.len() > MAX_THUMBS {
                let Some((_, p)) = far.pop() else { break };
                self.thumbs.remove(&p);
            }
        }
    }

    // -- input -----------------------------------------------------------------------------

    pub fn cursor(&self) -> CursorShape {
        match self.hovered {
            Some(Hit::Action(..) | Hit::OpenFolder | Hit::Clear) => CursorShape::Hand,
            _ => CursorShape::Arrow,
        }
    }

    pub fn pointer_motion(&mut self, at: Point) {
        let near_bar = |p: Option<Point>| p.is_some_and(|p| p.x >= self.view.0 - BAR_REACH);
        if near_bar(self.pointer) != near_bar(Some(at)) {
            self.damaged = true; // (the bar widens under the pointer)
        }
        self.pointer = Some(at);
        if let Some(grab) = self.bar_drag {
            self.drag_thumb(at.y - grab);
            return;
        }
        let hovered = self.hit_at(at);
        self.hover(hovered);
    }

    pub fn pointer_leave(&mut self) {
        self.pointer = None;
        self.hover(None);
    }

    /// The pointer is over something else: what changed is drawn again.
    fn hover(&mut self, hovered: Option<Hit>) {
        if hovered == self.hovered {
            return;
        }
        let g = self.grid();
        for h in [self.hovered, hovered].into_iter().flatten() {
            match h {
                Hit::Card(i) | Hit::Action(i, _) => self.touched.push(g.card(i, self.scroll)),
                Hit::OpenFolder | Hit::Clear => self.touched.push(Rect::new(0.0, 0.0, self.view.0, HEADER_H)),
            }
        }
        self.hovered = hovered;
    }

    pub fn scroll_by(&mut self, steps: f32) {
        self.set_scroll(self.scroll - steps * 48.0);
    }

    fn set_scroll(&mut self, to: f32) {
        let was = self.scroll;
        self.scroll = to;
        self.clamp_scroll();
        if self.scroll != was {
            self.damaged = true;
            self.hovered = self.pointer.and_then(|p| self.hit_at(p));
            self.want_thumbnails();
        }
    }

    /// The scroll bar's thumb: its top and height, when the page is longer
    /// than the window.
    fn thumb(&self) -> Option<(f32, f32)> {
        let page = self.page_height();
        if page <= self.view.1 {
            return None;
        }
        let bar_h = (self.view.1 * self.view.1 / page).max(32.0);
        Some(((self.scroll / (page - self.view.1)) * (self.view.1 - bar_h), bar_h))
    }

    fn drag_thumb(&mut self, top: f32) {
        let Some((_, bar_h)) = self.thumb() else { return };
        let room = (self.view.1 - bar_h).max(1.0);
        self.set_scroll(top / room * (self.page_height() - self.view.1));
    }

    pub fn button(&mut self, button: Button, pressed: bool, at: Point, mods: i32) {
        self.mods = mods;
        self.pointer = Some(at);
        if button != Button::Left {
            return;
        }
        if !pressed {
            if self.bar_drag.take().is_some() {
                self.damaged = true;
                return;
            }
            let was = self.pressed.take();
            if let (Some(h), Some(now)) = (was, self.hit_at(at))
                && h == now
            {
                self.click(h);
            }
            return;
        }
        if at.x >= self.view.0 - BAR_REACH
            && let Some((top, bar_h)) = self.thumb()
        {
            let grab = if (top..top + bar_h).contains(&at.y) { at.y - top } else { bar_h / 2.0 };
            self.bar_drag = Some(grab);
            self.drag_thumb(at.y - grab);
            return;
        }
        self.pressed = self.hit_at(at);
        // (a card is chosen as it's pressed)
        if let Some(Hit::Card(i) | Hit::Action(i, _)) = self.pressed {
            self.select(Some(i));
        }
    }

    fn click(&mut self, h: Hit) {
        match h {
            Hit::Card(i) => {
                let now = Instant::now();
                let double = self.last_click.is_some_and(|(j, t)| j == i && now.duration_since(t) <= DOUBLE_CLICK);
                self.last_click = if double { None } else { Some((i, now)) };
                if double {
                    self.act(i, Action::Open);
                }
            }
            Hit::Action(i, a) => self.act(i, a),
            Hit::OpenFolder => self.requests.push(Request::OpenFolder),
            Hit::Clear => {
                if self.confirm_until.take().is_some() {
                    self.requests.push(Request::Clear);
                } else {
                    self.confirm_until = Some(Instant::now() + CONFIRM_FOR);
                }
                self.damaged = true;
            }
        }
    }

    fn act(&mut self, i: usize, a: Action) {
        let Some(e) = self.entries.get(i) else { return };
        let present = self.present.get(i).copied().unwrap_or(false);
        let request = match a {
            Action::Open if present => Request::Open(e.path.clone()),
            Action::Annotate if present && !e.is_video() => Request::Annotate(e.path.clone()),
            Action::Copy if present => Request::Copy(Box::new(e.clone())),
            Action::Folder if present => Request::ShowInFolder(e.path.clone()),
            Action::Remove => Request::Remove(vec![e.path.clone()]),
            _ => return,
        };
        self.requests.push(request);
    }

    fn select(&mut self, i: Option<usize>) {
        let i = i.filter(|&i| i < self.entries.len());
        if i == self.selected {
            return;
        }
        let g = self.grid();
        for j in [self.selected, i].into_iter().flatten() {
            self.touched.push(g.card(j, self.scroll));
        }
        self.selected = i;
        // (scrolled into view)
        if let Some(i) = i {
            let card = g.card(i, 0.0);
            if card.y - GAP < self.scroll + HEADER_H {
                self.set_scroll(card.y - HEADER_H - GAP);
            } else if card.bottom() + GAP > self.scroll + self.view.1 {
                self.set_scroll(card.bottom() + GAP - self.view.1);
            }
        }
    }

    pub fn modifiers(&mut self, mods: i32) {
        self.mods = mods;
    }

    pub fn key(&mut self, k: &KeyPress) {
        let ctrl = k.mods & keyseq::CTRL != 0;
        let n = self.entries.len();
        let cols = self.grid().cols;
        let at = self.selected;
        match k.key {
            key::ESCAPE => self.requests.push(Request::Close),
            key::W if ctrl => self.requests.push(Request::Close),
            key::RETURN | key::ENTER => {
                if let Some(i) = at {
                    self.act(i, Action::Open);
                }
            }
            key::DELETE | key::BACKSPACE => {
                if let Some(i) = at {
                    self.act(i, Action::Remove);
                }
            }
            key::C if ctrl => {
                if let Some(i) = at {
                    self.act(i, Action::Copy);
                }
            }
            key::LEFT => self.select(Some(at.map_or(0, |i| i.saturating_sub(1)))),
            key::RIGHT => self.select(Some(at.map_or(0, |i| (i + 1).min(n.saturating_sub(1))))),
            key::UP => self.select(Some(at.map_or(0, |i| if i >= cols { i - cols } else { i }))),
            key::DOWN => self.select(Some(at.map_or(0, |i| if i + cols < n { i + cols } else { i }))),
            key::HOME => self.select(Some(0)),
            key::END => self.select(n.checked_sub(1)),
            key::PAGE_UP => self.set_scroll(self.scroll - (self.view.1 - HEADER_H)),
            key::PAGE_DOWN => self.set_scroll(self.scroll + (self.view.1 - HEADER_H)),
            _ => {}
        }
    }

    // -- painting --------------------------------------------------------------------------

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let c = theme::c();
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        p.fill_rect(Rect::new(0.0, 0.0, self.view.0, self.view.1), c.base);
        let k = self.scale;
        let region = Rect::new(region.x as f32 / k, region.y as f32 / k, region.w as f32 / k, region.h as f32 / k).adjusted(-2.0, -2.0, 2.0, 2.0);
        let g = self.grid();
        // The cards (under the header, which stays put).
        p.save();
        p.clip_rect(Rect::new(0.0, HEADER_H - 8.0, self.view.0, self.view.1));
        for i in self.visible_range(&g) {
            let card = g.card(i, self.scroll);
            if card.adjusted(-2.0, -2.0, 2.0, 2.0).intersects(&region) {
                self.paint_card(&mut p, &g, i, card);
            }
        }
        p.restore();
        if self.entries.is_empty() {
            let mid = Rect::new(MARGIN, HEADER_H + 40.0, self.view.0 - 2.0 * MARGIN, 24.0);
            p.draw_text(mid, Align::Center, "No captures yet", text::font(15.0, MEDIUM), c.text);
            let note = "Captures you save show up here, the newest first, with what was captured and where it came from.";
            let mut y = mid.bottom() + 6.0;
            for line in text::wrap(note, hint_font(), (self.view.0 - 2.0 * MARGIN).min(420.0)) {
                p.draw_text(Rect::new(MARGIN, y, self.view.0 - 2.0 * MARGIN, 18.0), Align::Center, &line, hint_font(), c.muted);
                y += 18.0;
            }
        }
        // The header.
        if region.y < HEADER_H {
            p.fill_rect(Rect::new(0.0, 0.0, self.view.0, HEADER_H - 8.0), c.base);
            p.draw_text(Rect::new(MARGIN, 22.0, self.view.0 - 2.0 * MARGIN, 26.0), Align::Left, "Recent captures", text::font(18.0, DEMIBOLD), c.text);
            let n = self.entries.len();
            let note = match n {
                0 => String::new(),
                1 => "1 capture. Its file stays where it is when it's taken off the list.".to_string(),
                n => format!("{n} captures, the newest first. Their files stay where they are when taken off the list."),
            };
            let (folder, clear) = self.header_buttons();
            p.draw_text(
                Rect::new(MARGIN, 50.0, folder.x - MARGIN - 12.0, 18.0),
                Align::Left,
                &text::elide_end(&note, hint_font(), folder.x - MARGIN - 12.0),
                hint_font(),
                c.muted,
            );
            paint_button(&mut p, folder, "Open folder", self.hovered == Some(Hit::OpenFolder), true, false);
            let confirm = self.confirm_until.is_some();
            paint_button(&mut p, clear, if confirm { "Click again to clear" } else { "Clear list" }, self.hovered == Some(Hit::Clear), n > 0, confirm);
        }
        // A thin scroll bar, when the page is longer than the window.
        if let Some((top, bar_h)) = self.thumb() {
            let wide = self.bar_drag.is_some() || self.pointer.is_some_and(|p| p.x >= self.view.0 - BAR_REACH);
            let (x, w) = if wide { (self.view.0 - 8.0, 6.0) } else { (self.view.0 - 6.0, 4.0) };
            p.fill_rounded_rect(Rect::new(x, top + 2.0, w, bar_h - 4.0), w / 2.0, if self.bar_drag.is_some() { c.muted } else { c.line });
        }
    }

    fn paint_card(&self, p: &mut Painter<'_>, g: &Grid, i: usize, card: Rect) {
        let c = theme::c();
        let Some(e) = self.entries.get(i) else { return };
        let present = self.present.get(i).copied().unwrap_or(false);
        let hovered = matches!(self.hovered, Some(Hit::Card(j) | Hit::Action(j, _)) if j == i);
        let selected = self.selected == Some(i);
        let edge = if selected { Pen::new(c.accent, 1.5) } else { Pen::new(if hovered { c.muted } else { c.line }, 1.0) };
        p.draw_rounded_rect(card.adjusted(0.5, 0.5, -0.5, -0.5), 10.0, Some(if hovered { c.hover } else { c.raised }), Some(edge));
        // The picture, fitted in its box.
        let bx = Rect::new(card.x + 6.0, card.y + 6.0, card.w - 12.0, g.thumb_h - 6.0);
        p.fill_rounded_rect(bx, 6.0, c.ink);
        match self.thumbs.get(&e.path) {
            Some(Thumb::Ready(img)) => {
                let (iw, ih) = (img.width() as f32, img.height() as f32);
                let k = (bx.w / iw).min(bx.h / ih);
                let (w, h) = (iw * k, ih * k);
                let dst = Rect::new(bx.x + (bx.w - w) / 2.0, bx.y + (bx.h - h) / 2.0, w, h);
                p.draw_image(img, dst, Rect::new(0.0, 0.0, iw, ih), true);
                // (its corners rounded as the box's, where it reaches them)
                corner_caps(p, bx, 6.0, if hovered { c.hover } else { c.raised });
            }
            Some(Thumb::Missing) | None if !present => {
                p.draw_text(bx, Align::Center, "File not found", hint_font(), c.muted);
            }
            Some(Thumb::Missing) => {
                let icon = if e.is_video() { "record" } else { "screen" };
                icons::paint(p, icon, Rect::new(bx.center().x - 16.0, bx.center().y - 16.0, 32.0, 32.0), c.muted);
            }
            _ => {}
        }
        if e.is_video() && present {
            // (a play mark: it's a recording)
            let r = Rect::new(bx.center().x - 20.0, bx.center().y - 20.0, 40.0, 40.0);
            p.fill_ellipse(r, Color::rgba(0, 0, 0, 150));
            icons::paint(p, "resume", r.adjusted(9.0, 9.0, -7.0, -9.0), Color::rgb(0xFFFFFF));
        }
        // What it is, on the picture's top left.
        let badge = kind_label(e);
        let bf = text::font(11.0, MEDIUM);
        let bw = text::width(&badge, bf).ceil() + 14.0;
        let br = Rect::new(bx.x + 6.0, bx.y + 6.0, bw.min(bx.w - 12.0), 20.0);
        p.fill_rounded_rect(br, 10.0, Color::rgba(0, 0, 0, 160));
        p.draw_text(br, Align::Center, &badge, bf, Color::rgb(0xF2EEE6));
        // Its actions, over the picture, while pointed at or chosen.
        let hovered_action = match self.hovered {
            Some(Hit::Action(j, a)) if j == i => Some(a),
            _ => None,
        };
        if hovered || selected {
            let rects = self.action_rects(g, i);
            if let (Some(first), Some(last)) = (rects.first(), rects.last()) {
                let strip = Rect::new(first.1.x - 4.0, first.1.y - 4.0, last.1.right() - first.1.x + 8.0, ACTION + 8.0);
                p.fill_rounded_rect(strip, 10.0, Color::rgba(0, 0, 0, 150));
            }
            for (a, r) in rects {
                if hovered_action == Some(a) {
                    p.fill_rounded_rect(r, 7.0, Color::rgba(255, 255, 255, 46));
                }
                let icon = ACTIONS.iter().find(|x| x.0 == a).map_or("open", |x| x.1);
                icons::paint(p, icon, r.adjusted(5.0, 5.0, -5.0, -5.0), Color::rgb(0xF2EEE6));
            }
        }
        // Its three lines: where it came from, what it is, when (or what the
        // action pointed at does).
        let tx = card.x + 12.0;
        let tw = card.w - 24.0;
        let mut y = card.y + g.thumb_h + 6.0;
        let name = source_label(e);
        p.draw_text(Rect::new(tx, y, tw, 20.0), Align::Left, &text::elide_end(&name, text::font(13.0, MEDIUM), tw), text::font(13.0, MEDIUM), c.text);
        y += 20.0;
        let (line2, line3) = match hovered_action.and_then(|a| ACTIONS.iter().find(|x| x.0 == a)) {
            Some(a) => (size_label(e), a.2.to_string()),
            None => (size_label(e), when_label(e)),
        };
        p.draw_text(Rect::new(tx, y, tw, 18.0), Align::Left, &text::elide_end(&line2, hint_font(), tw), hint_font(), c.muted);
        y += 18.0;
        let color = if hovered_action.is_some() { c.text } else { c.muted };
        p.draw_text(Rect::new(tx, y, tw, 18.0), Align::Left, &text::elide_end(&line3, hint_font(), tw), hint_font(), color);
    }
}

/// The cards' layout for the window's width.
struct Grid {
    cols: usize,
    card_w: f32,
    thumb_h: f32,
    card_h: f32,
    rows: usize,
}

impl Grid {
    /// Card `i` (window logical, with the page scrolled by `scroll`).
    fn card(&self, i: usize, scroll: f32) -> Rect {
        let (row, col) = (i / self.cols, i % self.cols);
        Rect::new(MARGIN + col as f32 * (self.card_w + GAP), HEADER_H + row as f32 * (self.card_h + GAP) - scroll, self.card_w, self.card_h)
    }
}

fn hint_font() -> text::FontSpec {
    text::font(12.0, NORMAL)
}

fn button_width(label: &str) -> f32 {
    text::width(label, text::font(12.0, MEDIUM)).ceil() + 24.0
}

fn paint_button(p: &mut Painter<'_>, r: Rect, label: &str, hovered: bool, enabled: bool, strong: bool) {
    let c = theme::c();
    let fill = if strong {
        c.accent
    } else if hovered && enabled {
        c.hover
    } else {
        c.raised
    };
    p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(fill), Some(Pen::new(if strong { c.accent } else { c.line }, 1.0)));
    let color = if strong {
        c.on_accent
    } else if enabled {
        c.text
    } else {
        c.muted
    };
    p.draw_text(r, Align::Center, label, text::font(12.0, MEDIUM), color);
}

/// Fill the corners of `r` outside its rounded corners (of `radius`) with
/// `color`, so what was drawn in it looks clipped to them.
fn corner_caps(p: &mut Painter<'_>, r: Rect, radius: f32, color: Color) {
    let Some(round) = rounded_rect_path(r, radius) else { return };
    let mut pb = tiny_skia::PathBuilder::new();
    if let Some(square) = tiny_skia::Rect::from_xywh(r.x, r.y, r.w, r.h) {
        pb.push_rect(square);
    }
    pb.push_path(&round);
    if let Some(caps) = pb.finish() {
        p.fill_path_rule(&caps, color, true);
    }
}

/// What a capture is, for its badge.
fn kind_label(e: &Entry) -> String {
    let kind = match e.kind.as_str() {
        "region" => "Region",
        "window" => "Window",
        "monitor" => "Monitor",
        "desktop" | "screens" => "All screens",
        "recording" => "Recording",
        "edited" => "Edited",
        "" => "Capture",
        other => other,
    };
    match e.seconds {
        Some(s) if e.is_video() => format!("{kind}  ·  {}", clock(s)),
        _ if e.edited && e.kind != "edited" => format!("{kind}  ·  edited"),
        _ => kind.to_string(),
    }
}

/// Where it came from: the window (app and title), else the monitor,
/// else its file's name.
fn source_label(e: &Entry) -> String {
    let title = crate::util::displayable(&e.title);
    let app = crate::util::displayable(&app_name(&e.app));
    match (app.is_empty(), title.trim().is_empty()) {
        (false, false) => format!("{app}  ·  {}", title.trim()),
        (false, true) => app,
        (true, false) => title.trim().to_string(),
        _ if !e.monitor.is_empty() => crate::util::displayable(&e.monitor),
        _ => crate::util::displayable(&e.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()),
    }
}

/// An app id as people know it: "org.kde.dolphin" is Dolphin.
fn app_name(app: &str) -> String {
    let last = app.rsplit(['.', '/']).next().unwrap_or(app);
    let mut cs = last.chars();
    match cs.next() {
        Some(f) => f.to_uppercase().chain(cs).collect(),
        None => String::new(),
    }
}

/// Its size in pixels and on disk, and its file type.
fn size_label(e: &Entry) -> String {
    let mut bits = Vec::new();
    if e.width > 0 && e.height > 0 {
        bits.push(format!("{} × {}", e.width, e.height));
    }
    if e.bytes > 0 {
        bits.push(human_size(e.bytes));
    }
    if let Some(ext) = e.path.extension() {
        bits.push(ext.to_string_lossy().to_uppercase());
    }
    bits.join("  ·  ")
}

/// When it was saved, and what else is known about it.
fn when_label(e: &Entry) -> String {
    let mut bits = vec![when(e.time)];
    match e.copied.as_str() {
        "image" if e.is_video() => bits.push("file copied".into()),
        "image" => bits.push("copied".into()),
        "path" => bits.push("path copied".into()),
        _ => {}
    }
    if e.codes > 0 {
        bits.push(format!("{} code{}", e.codes, if e.codes == 1 { "" } else { "s" }));
    }
    if e.drawings > 0 {
        bits.push(format!("{} drawing{}", e.drawings, if e.drawings == 1 { "" } else { "s" }));
    }
    if e.beautified {
        bits.push("beautified".into());
    }
    bits.join("  ·  ")
}

/// "Today, 14:32", "Yesterday, 09:05", "12 Oct, 18:40" or "3 Mar 2025".
fn when(ms: u64) -> String {
    let t = (ms / 1000) as libc::time_t;
    let now = (crate::util::now_ms() / 1000) as libc::time_t;
    let day = |t| crate::template::strftime("%Y-%m-%d", t, 0);
    let time = crate::template::strftime("%H:%M", t, 0);
    if day(t) == day(now) {
        format!("Today, {time}")
    } else if day(t) == day(now - 86_400) {
        format!("Yesterday, {time}")
    } else if crate::template::strftime("%Y", t, 0) == crate::template::strftime("%Y", now, 0) {
        format!("{}, {time}", crate::template::strftime("%d %b", t, 0).trim_start_matches('0'))
    } else {
        crate::template::strftime("%d %b %Y", t, 0).trim_start_matches('0').to_string()
    }
}

fn clock(seconds: f64) -> String {
    let s = seconds.round() as u64;
    if s >= 3600 { format!("{}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60) } else { format!("{}:{:02}", s / 60, s % 60) }
}

fn human_size(n: u64) -> String {
    if n >= 1_000_000 { format!("{:.1} MB", n as f64 / 1e6) } else { format!("{} KB", (n as f64 / 1e3).round().max(1.0)) }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(n: usize) -> Vec<Entry> {
        (0..n)
            .map(|i| Entry {
                path: PathBuf::from(format!("/nonexistent/{i}.png")),
                time: crate::util::now_ms(),
                kind: "region".into(),
                width: 800,
                height: 600,
                bytes: 123_456,
                app: "org.kde.dolphin".into(),
                title: "Home".into(),
                ..Default::default()
            })
            .collect()
    }

    fn center(r: &Recent, h: Hit) -> Point {
        let g = r.grid();
        match h {
            Hit::Card(i) => g.card(i, r.scroll).center(),
            Hit::Action(i, a) => r.action_rects(&g, i).into_iter().find(|x| x.0 == a).expect("action").1.center(),
            Hit::OpenFolder => r.header_buttons().0.center(),
            Hit::Clear => r.header_buttons().1.center(),
        }
    }

    fn click_on(r: &mut Recent, h: Hit) {
        let at = center(r, h);
        r.pointer_motion(at);
        r.button(Button::Left, true, at, 0);
        r.button(Button::Left, false, at, 0);
    }

    #[test]
    fn cards_lay_out_by_the_window_width() {
        let mut r = Recent::new(entries(40));
        r.set_view(900.0, 640.0);
        let g = r.grid();
        assert_eq!(g.cols, 3);
        assert_eq!(g.rows, 14);
        r.set_view(520.0, 640.0);
        assert_eq!(r.grid().cols, 1);
        // Only the cards in view (and a row past it) are asked for.
        let asked: usize = r.take_requests().iter().map(|q| if let Request::Thumbnails(v) = q { v.len() } else { 0 }).sum();
        assert!(asked > 0 && asked < 20, "{asked}");
        // Scrolled to the end: the last ones are.
        r.scroll_by(-1000.0);
        let asked: Vec<Entry> = r.take_requests().into_iter().flat_map(|q| if let Request::Thumbnails(v) = q { v } else { Vec::new() }).collect();
        assert!(asked.iter().any(|e| e.path.ends_with("39.png")), "{:?}", asked.len());
    }

    #[test]
    fn choosing_and_acting_on_cards() {
        let mut r = Recent::new(entries(5));
        r.set_view(900.0, 640.0);
        r.take_requests();
        // A click chooses; the file's gone, so only Remove is offered.
        click_on(&mut r, Hit::Card(1));
        assert_eq!(r.selected, Some(1));
        assert_eq!(r.action_rects(&r.grid(), 1).iter().map(|a| a.0).collect::<Vec<_>>(), [Action::Remove]);
        click_on(&mut r, Hit::Action(1, Action::Remove));
        assert_eq!(r.take_requests(), [Request::Remove(vec![PathBuf::from("/nonexistent/1.png")])]);
        // Keys move the choice; Delete takes it off.
        r.key(&KeyPress { key: key::RIGHT, ..Default::default() });
        assert_eq!(r.selected, Some(2));
        r.key(&KeyPress { key: key::DOWN, ..Default::default() });
        assert_eq!(r.selected, Some(2), "no card below");
        r.key(&KeyPress { key: key::DELETE, ..Default::default() });
        assert_eq!(r.take_requests(), [Request::Remove(vec![PathBuf::from("/nonexistent/2.png")])]);
        // Clear list wants a second click.
        click_on(&mut r, Hit::Clear);
        assert!(r.take_requests().is_empty() && r.confirm_until.is_some());
        click_on(&mut r, Hit::Clear);
        assert_eq!(r.take_requests(), [Request::Clear]);
    }

    #[test]
    fn a_present_file_offers_everything() {
        let dir = std::env::temp_dir().join(format!("flatshot-recent-{}", crate::util::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("shot.png");
        std::fs::write(&file, b"x").unwrap();
        let mut e = entries(1).remove(0);
        e.path = file.clone();
        let mut r = Recent::new(vec![e.clone()]);
        r.set_view(900.0, 640.0);
        click_on(&mut r, Hit::Card(0));
        assert_eq!(r.action_rects(&r.grid(), 0).len(), 5);
        click_on(&mut r, Hit::Card(0));
        assert!(r.take_requests().contains(&Request::Open(file.clone())), "a double-click opens");
        click_on(&mut r, Hit::Action(0, Action::Annotate));
        assert_eq!(r.take_requests(), [Request::Annotate(file.clone())]);
        // A recording isn't annotated.
        e.kind = "recording".into();
        r.set_entries(vec![e]);
        assert!(!r.action_rects(&r.grid(), 0).iter().any(|a| a.0 == Action::Annotate));
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn labels() {
        let e = &entries(1)[0];
        assert_eq!(source_label(e), "Dolphin  ·  Home");
        assert_eq!(size_label(e), "800 × 600  ·  123 KB  ·  PNG");
        assert!(when_label(e).starts_with("Today, "));
        let v = Entry { kind: "recording".into(), seconds: Some(75.4), ..e.clone() };
        assert_eq!(kind_label(&v), "Recording  ·  1:15");
        assert_eq!(clock(3725.0), "1:02:05");
    }
}
