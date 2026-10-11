//! One capture: the frozen screens with an overlay on each, drawing and
//! choosing, until it ends in an `Outcome` for the app to deliver.
//!
//! The session knows nothing of Wayland or X11: the platform hands it the
//! pointer, keys and timers, and paints what the overlays say changed. So
//! it runs the same headless, in the self-test.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::geom::{IPoint, IRect, Point, Rect};
use crate::image::{self, Canvas, Image};
use crate::keys::{KeyPress, Where};
use crate::keyseq::{self, key};
use crate::render::Painter;
use crate::render::theme::{Color, SIZES};
use crate::scanner;
use crate::template::Shot;
use crate::timing::{self, Clock};
use crate::ui::overlay::{Code, Grab, Handle, LOUPE_ZOOM, Output, Overlay, Window};
use crate::ui::panels::{CodeChip, ColorPicker, PickerEvent, RecordPanel, RecordView, place_chips};
use crate::ui::selection::Selection;
use crate::ui::shapes::{self, Mods, Shape, Tool, copy_for_editing};
use crate::ui::snapping::{self, EdgeMaps, py_round};
use crate::ui::state::{CUSTOM, Desktop, State};
use crate::ui::textedit::KeyResult;
use crate::ui::toolbar::{CodeScan, Toolbar, View};
use crate::ui::widgets::Look;

/// The recording formats, as the panel offers them.
pub const RECORD_FORMATS: &[(&str, &str)] = &[("mp4", "MP4"), ("webm", "WebM"), ("gif", "GIF")];
const SCAN_AFTER: Duration = Duration::from_millis(60);
const RAINBOW_EVERY: Duration = Duration::from_millis(50);

/// What was asked for.
#[derive(Clone, Debug, Default)]
pub struct Request {
    /// pin the result to the screen instead of saving it
    pub pin: bool,
    /// look for QR codes and barcodes
    pub scan: bool,
    /// open with the record tool
    pub record: bool,
    /// a picture shown for annotating, not the screen
    pub image: bool,
}

/// A monitor and what was captured on it.
pub struct Screen {
    pub output: Output,
    /// its own pixels
    pub picture: Arc<Image>,
    /// The mouse pointer, as captured: where its pixels go in `picture`,
    /// and those pixels with the pointer in them.
    pub pointer: Option<(IPoint, Arc<Image>)>,
}

/// Everything a session starts from.
pub struct Setup {
    pub cfg: Config,
    pub request: Request,
    pub screens: Vec<Screen>,
    /// pictures with the pointer were taken too (the toolbar can show it)
    pub pointer_taken: bool,
    /// where the pointer is (global logical), when known
    pub cursor: Option<Point>,
    /// the primary monitor (an index into `screens`)
    pub primary: usize,
    /// nothing is being recorded, so this capture may record
    pub can_record: bool,
}

/// What the recorder can do here (the app knows; tests make it up).
pub trait RecordSupport {
    /// Why recording in `format` can't work here (what to install), if it can't.
    fn problem(&mut self, format: &str) -> Option<String>;
    /// Can it take an area across monitors?
    fn can_span(&self) -> bool;
}

/// A recording to start, for the app to run.
#[derive(Debug)]
pub struct RecordJob {
    /// global logical pixels
    pub area: IRect,
    /// the monitor it was chosen on, and its pixels per logical pixel
    pub output: Output,
    pub scale: f32,
    pub shot: Shot,
    pub thumbnail: Image,
    pub format: String,
    pub fps: i64,
    pub mic: bool,
    pub system_audio: bool,
    pub cursor: bool,
}

/// How a session ended.
#[derive(Debug)]
pub enum Outcome {
    /// A picture to save / copy (or pin). `at`: where it came from, in
    /// global logical pixels (None for a picture that wasn't the screen).
    Image {
        image: Image,
        shot: Shot,
        at: Option<IRect>,
        pin: bool,
    },
    /// The colour under the pointer, to copy.
    Color {
        hex: String,
        rgb: (u8, u8, u8),
    },
    /// A code's text, to copy.
    CodeText(String),
    /// A code's link, to open.
    OpenLink(String),
    Record(Box<RecordJob>),
    Cancelled,
    Failed(String),
}

/// Something for the app to do while the session goes on.
#[derive(Clone, Debug, PartialEq)]
pub enum Effect {
    /// Put this text on the clipboard.
    Clipboard(String),
    /// Read the clipboard's text and hand it to `paste`.
    Paste,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Button {
    Left,
    Right,
    Middle,
    Other,
}

/// The mouse pointer's look.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CursorShape {
    Crosshair,
    Text,
    Move,
    Hand,
    Arrow,
    ResizeNS,
    ResizeEW,
    ResizeNWSE,
    ResizeNESW,
}

/// Wakes the app's event loop from another thread (work done in the
/// background is waiting in `poll`).
pub type Waker = Arc<dyn Fn() + Send + Sync>;

enum Background {
    Codes(usize, Vec<Code>),
    Edges(usize, Arc<EdgeMaps>),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Timer {
    Countdown,
    Rainbow,
    Scan,
}

/// The panels floating over an overlay.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Float {
    Toolbar,
    Chip(usize),
    Panel,
    Picker,
}

/// What the pointer is over: a panel, its control, a cell of the format choice.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Hover {
    float: Option<Float>,
    item: Option<&'static str>,
    choice: Option<usize>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum PasteTarget {
    Text,
    Picker(usize),
}

#[derive(Clone, Debug)]
enum RecordOption {
    Format(String),
    Fps(i64),
    Mic(bool),
    SystemAudio(bool),
    Cursor(bool),
}

pub struct Session {
    pub st: State,
    pub overlays: Vec<Overlay>,
    pub request: Request,
    pub done: bool,
    outcome: Option<Outcome>,
    effects: Vec<Effect>,
    record_support: Box<dyn RecordSupport>,
    problems: Vec<(String, Option<String>)>,
    desktop: Option<Desktop>,
    codes: Vec<Code>,
    /// codes are to be looked for, once shown (hidden at the start: not yet)
    scan_pending: bool,
    /// pictures still being looked through for codes
    scans_left: usize,
    /// the overlay whose select tool holds a drawing being dragged (which
    /// may have been picked up from another monitor)
    drag_owner: Option<usize>,
    /// overlays in the order things were drawn on them, for undo
    history: Vec<usize>,
    redo_stack: Vec<usize>,
    /// Esc went down here; it acts when it comes up
    escape_down: bool,
    countdown_overlay: Option<usize>,
    /// a selection may cross monitors (the setting, and more than one)
    may_span: bool,
    /// all the monitors (global logical)
    virtual_rect: IRect,
    timers: Vec<(Instant, Timer)>,
    started: Instant,
    pub clock: Clock,
    tx: mpsc::Sender<Background>,
    rx: mpsc::Receiver<Background>,
    waker: Waker,
    /// set when the session ends: background work stops early
    cancelled: Arc<AtomicBool>,
    /// where the pointer really is on each overlay (cursor_pos stays put
    /// for a finger on a panel)
    pointer: Vec<Option<Point>>,
    hover: Vec<Hover>,
    paste_target: Option<PasteTarget>,
}

impl Session {
    pub fn new(setup: Setup, record_support: Box<dyn RecordSupport>, waker: Waker) -> Session {
        let Setup { cfg, request, screens, pointer_taken, cursor, primary, can_record } = setup;
        let mut st = State::new(cfg);
        st.can_record = can_record && !request.image;
        if st.tool == Tool::Record && !st.can_record {
            st.tool = Tool::Region;
        }
        if request.record && st.can_record {
            st.tool = Tool::Record;
        }
        st.colour_tool = if st.tool.uses_color() { st.tool } else { Tool::Pen };
        st.pin_mode = request.pin;
        st.pointer_available = pointer_taken && !request.image;
        let what = if request.image {
            "image"
        } else if st.tool == Tool::Record {
            "recording"
        } else {
            "region capture"
        };
        let mut overlays = Vec::with_capacity(screens.len());
        let mut virtual_rect = IRect::default();
        for (i, s) in screens.into_iter().enumerate() {
            virtual_rect = virtual_rect.united(&s.output.rect);
            let g = s.output.rect;
            let fit = (s.picture.width() as f32 / g.w.max(1) as f32).max(s.picture.height() as f32 / g.h.max(1) as f32).max(1.0);
            let mut ov = Overlay::new(i, s.output, s.picture, IPoint::new(0, 0));
            if request.image {
                ov = ov.with_dpr(fit);
            }
            ov.pointer_patch = s.pointer;
            overlays.push(ov);
        }
        let n = overlays.len();
        let (tx, rx) = mpsc::channel();
        let may_span = st.cfg.span_monitors && !request.image && n > 1;
        let mut s = Session {
            st,
            overlays,
            request,
            done: false,
            outcome: None,
            effects: Vec::new(),
            record_support,
            problems: Vec::new(),
            desktop: None,
            codes: Vec::new(),
            history: Vec::new(),
            redo_stack: Vec::new(),
            escape_down: false,
            countdown_overlay: None,
            may_span,
            virtual_rect,
            timers: Vec::new(),
            started: Instant::now(),
            clock: Clock::new(what),
            tx,
            rx,
            waker,
            cancelled: Arc::new(AtomicBool::new(false)),
            scan_pending: false,
            scans_left: 0,
            drag_owner: None,
            pointer: vec![None; n],
            hover: vec![Hover::default(); n],
            paste_target: None,
        };
        if n == 0 {
            s.fail("no screens to show");
            return s;
        }
        s.update_spans();
        for o in 0..n {
            let tb = Toolbar::new(&toolbar_view(&s.st, CodeScan::Found(0), false, false));
            s.overlays[o].toolbar = Some(tb);
            s.place_floating(o);
        }
        // Until the pointer enters an overlay: the monitor it's on, if
        // known, else the primary one.
        let start = if s.st.cfg.toolbar_follows_mouse { cursor.and_then(|c| s.overlay_at(c)) } else { None };
        let first = start.unwrap_or(primary.min(n - 1));
        s.st.pointer_overlay = Some(first);
        s.st.toolbar_overlay = Some(first);
        s.refresh();
        if let Some(c) = cursor {
            s.point_at(c);
        }
        if s.request.scan {
            // Codes are looked for while shown: now (once the overlay is on
            // screen, so scanning doesn't hold it up), else once they are.
            s.scan_pending = true;
            if s.st.codes_visible {
                s.schedule(Timer::Scan, SCAN_AFTER);
            }
            s.refresh_toolbars();
        }
        if s.st.snap_edges {
            for o in 0..n {
                s.snap_changed(o);
            }
        }
        if s.st.cfg.rainbow {
            s.schedule(Timer::Rainbow, RAINBOW_EVERY);
        }
        s.clock.step("overlays ready", format!("{n} monitor{}", if n == 1 { "" } else { "s" }));
        s
    }

    // -- what the app reads ------------------------------------------------------

    /// How the session ended, once it has (taken once).
    pub fn take_outcome(&mut self) -> Option<Outcome> {
        self.outcome.take()
    }

    pub fn take_effects(&mut self) -> Vec<Effect> {
        std::mem::take(&mut self.effects)
    }

    /// When `on_timer` should next run.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.timers.iter().map(|t| t.0).min()
    }

    /// The parts of overlay `o` to repaint now (device pixels of its buffer).
    pub fn damage(&mut self, o: usize) -> crate::geom::Damage {
        self.share_drawings(o);
        let Some(ov) = self.overlays.get_mut(o) else { return Default::default() };
        ov.update_damage(&self.st);
        std::mem::take(&mut ov.damage)
    }

    /// Paint `region` (device pixels) of overlay `o` into `target`, whose
    /// top left is at `origin`.
    pub fn paint(&self, o: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        if let Some(ov) = self.overlays.get(o) {
            ov.paint(&self.st, target, origin, region);
        }
    }

    /// The pointer's look over overlay `o`.
    pub fn cursor(&self, o: usize) -> CursorShape {
        let Some(ov) = self.overlays.get(o) else { return CursorShape::Arrow };
        let canvas = || {
            if let Some((h, ..)) = ov.rec_drag {
                return handle_cursor(h);
            }
            // (text being selected by dragging)
            if ov.text_drag.is_some() && self.st.text_edit.is_some() {
                return CursorShape::Text;
            }
            if self.st.tool == Tool::Record
                && ov.sel_origin.is_none()
                && let Some(h) = ov.cursor_pos.and_then(|p| ov.handle_at(p))
            {
                return handle_cursor(h);
            }
            if self.st.tool == Tool::Select && !self.st.eyedropper {
                return ov.cursor_pos.map_or(CursorShape::Arrow, |p| ov.selection.cursor(&ov.annotations, p, 1.0));
            }
            if self.st.tool == Tool::Text && !self.st.eyedropper { CursorShape::Text } else { CursorShape::Crosshair }
        };
        let float = match ov.grab {
            Grab::Toolbar => Some(Float::Toolbar),
            Grab::Chip(i) => Some(Float::Chip(i)),
            Grab::Panel => Some(Float::Panel),
            Grab::Picker => Some(Float::Picker),
            Grab::Canvas => None,
            Grab::None => self.hover[o].float,
        };
        let item = self.hover[o].item;
        match float {
            None => canvas(),
            Some(Float::Toolbar) => match item {
                Some("grip") => CursorShape::Move,
                Some(id) if !id.starts_with("divider") => CursorShape::Hand,
                _ => canvas(),
            },
            Some(Float::Chip(_)) | Some(Float::Panel) => match item {
                Some(id) if !id.starts_with("divider") => CursorShape::Hand,
                _ => CursorShape::Arrow,
            },
            Some(Float::Picker) => match item {
                Some("hex") => CursorShape::Text,
                Some("dropper") => CursorShape::Hand,
                _ => CursorShape::Arrow,
            },
        }
    }

    /// Where the caret of the text being typed on overlay `o` is (for an
    /// input method's candidate window), if one is.
    pub fn text_input(&self, o: usize) -> Option<Rect> {
        if let Some(p) = self.focused_picker() {
            return if p == o { self.overlays[p].picker.as_ref()?.caret_rect() } else { None };
        }
        let (eo, shape) = self.st.text_edit.as_ref()?;
        (*eo == o).then(|| shape.as_text().map(|t| t.caret_rect(shape.size)))?
    }

    /// Is something typed into: a text, or the picker's hex field?
    pub fn wants_text(&self) -> bool {
        self.st.text_edit.is_some() || self.focused_picker().is_some()
    }

    // -- events from the app -------------------------------------------------------

    /// Work done in the background (codes found, edge maps) is ready.
    pub fn poll(&mut self) {
        while let Ok(msg) = self.rx.try_recv() {
            match msg {
                Background::Codes(o, codes) => self.codes_found(o, codes),
                Background::Edges(o, maps) => {
                    if !self.done
                        && let Some(ov) = self.overlays.get_mut(o)
                    {
                        timing::log(format!("snapping: edge maps for {} × {}", maps.w, maps.h));
                        ov.edge_maps = Some(maps);
                    }
                }
            }
        }
    }

    pub fn on_timer(&mut self, now: Instant) {
        let due: Vec<Timer> = self.timers.iter().filter(|t| t.0 <= now).map(|t| t.1).collect();
        self.timers.retain(|t| t.0 > now);
        for t in due {
            if self.done {
                return;
            }
            match t {
                Timer::Countdown => self.tick(),
                Timer::Rainbow => {
                    self.st.rainbow_t = now.duration_since(self.started).as_secs_f32();
                    self.schedule(Timer::Rainbow, RAINBOW_EVERY);
                }
                Timer::Scan => self.start_scan(),
            }
        }
    }

    /// The compositor described the desktop: its windows, the active one,
    /// where the pointer is.
    pub fn desktop_found(&mut self, desktop: Desktop) {
        if self.done {
            return;
        }
        timing::log(format!(
            "{}: windows known at {:.0} ms, {} window{}",
            self.clock.what,
            self.clock.since_start(),
            desktop.windows.len(),
            if desktop.windows.len() == 1 { "" } else { "s" }
        ));
        let cursor = desktop.cursor;
        self.st.windows = desktop.windows.clone();
        self.desktop = Some(desktop);
        if let Some(c) = cursor
            && self.overlays.iter().all(|o| o.cursor_pos.is_none())
        {
            self.point_at(c);
        }
        if !self.st.cfg.detect_windows {
            return;
        }
        for o in 0..self.overlays.len() {
            self.set_windows(o);
        }
        self.refresh();
    }

    /// Codes found on overlay `o`'s picture (corners in its pixels).
    pub fn codes_found(&mut self, o: usize, codes: Vec<Code>) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        self.scans_left = self.scans_left.saturating_sub(1);
        let dpr = self.overlays[o].dpr;
        for code in codes {
            let local: Vec<Point> = code.corners.iter().map(|p| *p / dpr).collect();
            let chip = CodeChip::new(&code.text, code.is_link());
            self.overlays[o].codes.push((self.codes.len(), local));
            self.overlays[o].chips.push(chip);
            self.codes.push(code);
        }
        self.place_floating(o);
        self.refresh();
    }

    pub fn pointer_enter(&mut self, o: usize, pos: Point) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        self.pointer[o] = Some(pos);
        self.activate(o);
        // (the overlay may have appeared under a pointer that hasn't moved:
        // the crosshair and magnifier show there now, not on the first move)
        self.overlay_point_at(o, pos);
        self.update_float_hover(o);
    }

    pub fn pointer_leave(&mut self, o: usize) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        self.pointer[o] = None;
        let ov = &mut self.overlays[o];
        ov.cursor_pos = None;
        ov.hover_window = None;
        ov.snap_point = None;
        self.set_hover(o, Hover::default());
    }

    /// The pointer moved to `pos` (logical, on overlay `o`); `touch`: a
    /// finger, not a mouse.
    pub fn pointer_motion(&mut self, o: usize, pos: Point, mods: i32, touch: bool) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        self.pointer[o] = Some(pos);
        match self.overlays[o].grab {
            Grab::Canvas => self.canvas_motion(o, pos, mods),
            Grab::None => {
                let hover = self.hover_at(o, pos);
                self.set_hover(o, hover);
                match hover.float {
                    None => self.canvas_motion(o, pos, mods),
                    Some(f) => {
                        if f == Float::Picker
                            && let Some(p) = &mut self.overlays[o].picker
                        {
                            p.motion(pos, false);
                        }
                        if !touch {
                            self.track(o, pos);
                        }
                    }
                }
            }
            Grab::Toolbar => {
                let ov = &mut self.overlays[o];
                let (w, h) = (ov.width(), ov.height());
                let mut moved = false;
                if let Some(tb) = &mut ov.toolbar {
                    if let Some(g) = tb.grab {
                        let target = (pos - g).round();
                        tb.pos = Point::new((target.x as f32).min(w - tb.size.0 - 8.0).max(8.0), (target.y as f32).min(h - tb.size.1 - 8.0).max(8.0));
                        moved = true;
                    } else {
                        let over = tb.item_at(pos).map(|it| it.id);
                        tb.ptr.hovered = over.filter(|id| Some(*id) == tb.ptr.pressed);
                    }
                }
                if moved && ov.picker_open() {
                    self.show_picker(o); // (it stays under its swatch)
                }
                if !touch {
                    self.track(o, pos);
                }
            }
            Grab::Chip(i) => {
                let ov = &mut self.overlays[o];
                let (w, h) = (ov.width(), ov.height());
                if let Some(chip) = ov.chips.get_mut(i) {
                    if let Some(g) = chip.grab {
                        let target = (pos - g).round();
                        chip.pos = Point::new((target.x as f32).min(w - chip.size.0 - 8.0).max(8.0), (target.y as f32).min(h - chip.size.1 - 8.0).max(8.0));
                    } else {
                        let over = chip.item_at(pos).map(|it| it.id);
                        chip.ptr.hovered = over.filter(|id| Some(*id) == chip.ptr.pressed);
                    }
                }
                if !touch {
                    self.track(o, pos);
                }
            }
            Grab::Panel => {
                if let Some(panel) = &mut self.overlays[o].panel {
                    let over = panel.item_at(pos).map(|it| it.id);
                    panel.ptr.hovered = over.filter(|id| Some(*id) == panel.ptr.pressed);
                }
                if !touch {
                    self.track(o, pos);
                }
            }
            Grab::Picker => {
                let ev = self.overlays[o].picker.as_mut().map(|p| p.motion(pos, true)).unwrap_or(PickerEvent::None);
                self.picker_event(o, ev);
                if !touch {
                    self.track(o, pos);
                }
            }
        }
    }

    pub fn button(&mut self, o: usize, button: Button, pressed: bool, pos: Point, mods: i32) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        self.pointer[o] = Some(pos);
        if pressed {
            self.press(o, button, pos, mods);
        } else {
            self.release(o, button, pos);
        }
    }

    /// The wheel turned `steps` notches (up is positive): the magnifier zooms.
    pub fn scroll(&mut self, o: usize, steps: f32) {
        if self.done || o >= self.overlays.len() || steps == 0.0 {
            return;
        }
        if !(matches!(self.st.tool, Tool::Region | Tool::Record) && self.st.cfg.show_loupe) {
            return;
        }
        self.st.loupe_zoom = (self.st.loupe_zoom * 1.25f32.powf(steps)).clamp(LOUPE_ZOOM.0, LOUPE_ZOOM.1);
    }

    /// Shift, Ctrl or Alt pressed or let go: a shape being drawn changes at
    /// once, without waiting for the pointer to move.
    pub fn modifiers(&mut self, mods: i32) {
        if self.done {
            return;
        }
        let spans = self.st.spans;
        let mut moved = None;
        for o in 0..self.overlays.len() {
            let bounds = self.drag_bounds(o);
            let ov = &mut self.overlays[o];
            // The area being dragged out: squared or centred as the keys say.
            if let Some(d) = &mut ov.sel_origin {
                let mut m = Mods::from_keys(mods);
                m.moving = false;
                let at = d.pointer;
                d.extend(at, m);
                ov.sel_rect = Some(clip_selection(d.rect(), bounds));
                moved = Some(o);
            }
            if let Some(shape) = &mut ov.active
                && let Some(d) = shape.drag()
            {
                let at = d.pointer;
                let mut m = Mods::from_keys(mods);
                m.moving = false; // (moving needs the pointer to move)
                shape.extend(at, m);
            }
        }
        if let Some(o) = moved
            && spans
        {
            self.selection_moved(o, None);
        }
    }

    pub fn key(&mut self, o: usize, press: &KeyPress) {
        if self.done || o >= self.overlays.len() {
            return;
        }
        if is_modifier(press.key) {
            // (they change a shape being drawn: see `modifiers`)
            return;
        }
        // The picker's hex field has the keys while it has the focus; Esc
        // goes on to close it.
        if press.key != key::ESCAPE
            && let Some(po) = self.focused_picker()
        {
            let ev = self.overlays[po].picker.as_mut().map(|p| p.key(press)).unwrap_or(PickerEvent::None);
            self.picker_event(po, ev);
            return;
        }
        // With several overlays the keyboard focus can be on any of them:
        // act on the monitor the pointer is on.
        let target = self.st.pointer_overlay.filter(|&p| p < self.overlays.len()).unwrap_or(o);
        let ctrl = press.ctrl();
        let enter = press.key == key::RETURN || press.key == key::ENTER;

        if self.st.text_edit.is_some() {
            let action = if ctrl { self.st.keymap.action(press, Where::Capture) } else { None };
            let result = {
                let (_, shape) = self.st.text_edit.as_mut().expect("checked");
                let size = shape.size;
                let Some(t) = shape.as_text_mut() else { return };
                if matches!(action, Some("undo") | Some("redo")) && t.edit.undo_edit(action == Some("redo")) { KeyResult::Used } else { t.key(press, size) }
            };
            match result {
                KeyResult::Ignored => {}
                KeyResult::Used => return,
                KeyResult::Commit => {
                    self.commit_text();
                    return;
                }
                KeyResult::Copy(s) => {
                    self.effects.push(Effect::Clipboard(s));
                    return;
                }
                KeyResult::Paste => {
                    self.paste_target = Some(PasteTarget::Text);
                    self.effects.push(Effect::Paste);
                    return;
                }
            }
        }

        if press.key == key::ESCAPE {
            // Acted on when the key comes up: closing on the way down hands
            // the release to the window underneath.
            self.escape_down = !press.repeat || self.escape_down;
            return;
        }
        if self.st.countdown.is_some() {
            return;
        }
        if self.st.tool == Tool::Select
            && let Some(so) = self.selected_on()
        {
            let len = self.overlays[so].annotations.len();
            let ov = &mut self.overlays[so];
            if ov.selection.key(&mut ov.annotations, press) {
                self.selection_changed(so, len); // (Delete, or an arrow key nudging it)
                return;
            }
        }
        let save_keys = ctrl && (press.key == key::S || press.key == key::C);
        if self.st.tool == Tool::Record && enter {
            match (0..self.overlays.len()).find(|&i| self.overlays[i].rec_rect.is_some()) {
                Some(armed) => self.start_countdown(armed),
                None => self.whole_screen(target),
            }
        } else if self.st.tool == Tool::Record && save_keys {
        } else if let Some(action) = self.st.keymap.action(press, Where::Capture) {
            self.do_action(action, target);
        } else if ctrl && press.key == key::Y {
            self.redo(); // (besides the Redo key)
        } else if enter || save_keys {
            self.capture(target, None, None);
        }
    }

    pub fn key_release(&mut self, _o: usize, press: &KeyPress) {
        if self.done || press.key != key::ESCAPE || press.repeat || !self.escape_down {
            return;
        }
        self.escape_down = false;
        if self.st.countdown.is_some() {
            self.cancel_countdown();
        } else if self.st.eyedropper {
            self.close_picker(); // (its hint says Esc stops it)
        } else {
            self.close_picker(); // it closes, and Esc still does what it does
            if !(0..self.overlays.len()).any(|o| self.cancel_gesture(o)) {
                self.cancel();
            }
        }
    }

    /// Text from an input method (compose, dead keys, CJK, emoji).
    pub fn commit_string(&mut self, text: &str) {
        if self.done || text.is_empty() {
            return;
        }
        if let Some((_, shape)) = &mut self.st.text_edit
            && let Some(t) = shape.as_text_mut()
        {
            t.edit.insert(text);
        } else if let Some(po) = self.focused_picker() {
            let ev = self.overlays[po].picker.as_mut().map(|p| p.paste(text)).unwrap_or(PickerEvent::None);
            self.picker_event(po, ev);
        }
    }

    /// The clipboard's text, asked for with `Effect::Paste`.
    pub fn paste(&mut self, text: &str) {
        if self.done {
            return;
        }
        match self.paste_target.take() {
            Some(PasteTarget::Text) => {
                if let Some((_, shape)) = &mut self.st.text_edit
                    && let Some(t) = shape.as_text_mut()
                {
                    t.edit.paste(text);
                }
            }
            Some(PasteTarget::Picker(o)) => {
                let ev = self.overlays.get_mut(o).and_then(|ov| ov.picker.as_mut()).map(|p| p.paste(text)).unwrap_or(PickerEvent::None);
                self.picker_event(o, ev);
            }
            None => {}
        }
    }

    // -- which monitor ---------------------------------------------------------------

    fn overlay_at(&self, at: Point) -> Option<usize> {
        self.overlays.iter().position(|o| o.output.rect.to_f().contains(at))
    }

    /// Show the crosshair and magnifier at `at` (global logical) before the
    /// pointer has moved.
    fn point_at(&mut self, at: Point) {
        for o in 0..self.overlays.len() {
            let g = self.overlays[o].output.rect.to_f();
            if g.contains(at) {
                self.overlay_point_at(o, at - g.top_left());
            }
        }
    }

    fn overlay_point_at(&mut self, o: usize, pos: Point) {
        let ov = &self.overlays[o];
        if ov.cursor_pos.is_some() || !ov.rect().contains(pos) {
            return;
        }
        self.overlays[o].cursor_pos = Some(pos);
        self.activate(o);
        self.update_hover(o);
    }

    /// The pointer is now over overlay `o`.
    fn activate(&mut self, o: usize) {
        if self.st.pointer_overlay == Some(o) || self.done {
            return;
        }
        self.st.pointer_overlay = Some(o);
        if self.st.cfg.toolbar_follows_mouse && !self.overlays.iter().any(|ov| ov.sel_rect.is_some() || ov.rec_rect.is_some()) {
            let moving = self.st.toolbar_overlay != Some(o);
            let picker_open = self.overlays.iter().any(|ov| ov.picker_open());
            self.st.toolbar_overlay = Some(o);
            self.sync_toolbars();
            if moving && picker_open {
                // your colour's picker goes along with the toolbar
                for (i, ov) in self.overlays.iter_mut().enumerate() {
                    if i != o {
                        ov.picker_visible = false;
                    }
                }
                self.show_picker(o);
            }
        }
    }

    fn sync_toolbars(&mut self) {
        let dragging = self.overlays.iter().any(|o| o.sel_rect.is_some());
        let countdown = self.st.countdown.is_some();
        let target = self.st.toolbar_overlay;
        for (i, o) in self.overlays.iter_mut().enumerate() {
            o.toolbar_visible = o.toolbar.is_some() && target == Some(i) && !dragging && !countdown;
        }
    }

    /// The crosshair follows the pointer over the panels too (not a finger
    /// on a button).
    fn track(&mut self, o: usize, pos: Point) {
        self.overlays[o].cursor_pos = Some(pos);
        self.overlays[o].snap_point = None;
        self.activate(o);
        self.update_hover(o);
    }

    // -- selections across monitors ------------------------------------------------

    fn update_spans(&mut self) {
        self.st.spans = self.may_span
            && match self.st.tool {
                Tool::Region => true,
                Tool::Record => self.record_support.can_span(),
                _ => false,
            };
    }

    /// Where a selection may reach on overlay `o`: its monitor, or the whole desktop.
    fn drag_bounds(&self, o: usize) -> Rect {
        let g = self.overlays[o].output.rect;
        if self.st.spans { self.virtual_rect.translated(-g.x, -g.y).to_f() } else { self.overlays[o].rect() }
    }

    /// The area to record, chosen on `origin`, changed: show its part on
    /// the other monitors.
    fn area_moved(&mut self, origin: usize) {
        let g = self.overlays[origin].output.rect.to_f();
        let area = self.overlays[origin].rec_rect.map(|r| r.translated(g.top_left()));
        for (i, ov) in self.overlays.iter_mut().enumerate() {
            if i != origin {
                let og = ov.output.rect.to_f();
                ov.span_rect = area.filter(|a| a.intersects(&og)).map(|a| a.translated(-og.top_left()));
            }
        }
    }

    /// `origin`'s selection changed: show its part on the other monitors,
    /// and the crosshair and magnifier where the pointer is.
    fn selection_moved(&mut self, origin: usize, pointer: Option<Point>) {
        let g = self.overlays[origin].output.rect.to_f();
        let sel = self.overlays[origin].sel_rect.map(|r| r.translated(g.top_left()));
        let at = pointer.map(|p| p + g.top_left());
        for (i, ov) in self.overlays.iter_mut().enumerate() {
            if i == origin {
                continue;
            }
            let og = ov.output.rect;
            let ogf = og.to_f();
            ov.span_rect = sel.filter(|s| s.intersects(&ogf)).map(|s| s.translated(-ogf.top_left()));
            ov.span_pointer = at.filter(|a| og.contains(a.round())).map(|a| a - ogf.top_left());
        }
    }

    fn hover_changed(&mut self, origin: usize, window: Option<usize>) {
        if !self.st.spans {
            return;
        }
        for (i, ov) in self.overlays.iter_mut().enumerate() {
            if i != origin {
                ov.span_hover = window.and_then(|w| ov.windows.iter().find(|(_, x)| *x == w).copied());
            }
        }
    }

    /// Capture `area` (global logical), which crosses monitors: from every
    /// monitor's picture, with its drawings.
    fn capture_span(&mut self, area: IRect, window: Option<usize>) {
        if self.done {
            return;
        }
        self.commit_text();
        self.clock.step("chosen", "");
        let area = area.intersected(&self.virtual_rect);
        let image = match self.render_desktop(area) {
            Ok(img) => img,
            Err(e) => return self.fail(&e),
        };
        self.clock.step("drawn", format!("{} × {} across monitors", image.width(), image.height()));
        let mut shot = Shot::new(if window.is_some() { "window" } else { "region" });
        self.name_source(&mut shot, window);
        let pin = self.st.pin_mode;
        self.close_overlays();
        self.finish(Outcome::Image { image, shot, at: Some(area), pin });
    }

    /// `area` (global logical) of the whole desktop with the drawings, at
    /// the highest of the monitors' scales.
    fn render_desktop(&self, area: IRect) -> Result<Image, String> {
        let scale = self.overlays.iter().map(|o| o.dpr).fold(1.0f32, f32::max);
        let (w, h) = ((area.w as f32 * scale).round().max(1.0) as u32, (area.h as f32 * scale).round().max(1.0) as u32);
        let mut out = Image::filled(w, h, 0xFF00_0000)?;
        for ov in &self.overlays {
            let g = ov.output.rect;
            let part = g.intersected(&area);
            if part.is_empty() {
                continue;
            }
            let d = ov.dpr;
            let src =
                IRect::new(py_round((part.x - g.x) as f32 * d), py_round((part.y - g.y) as f32 * d), py_round(part.w as f32 * d), py_round(part.h as f32 * d))
                    .intersected(&ov.base.rect());
            let dst = IRect::new(
                ((part.x - area.x) as f32 * scale).round() as i32,
                ((part.y - area.y) as f32 * scale).round() as i32,
                (part.w as f32 * scale).round() as i32,
                (part.h as f32 * scale).round() as i32,
            );
            if src.is_empty() || dst.is_empty() {
                continue;
            }
            let piece = ov.base.crop(src)?;
            if (piece.width() as i32, piece.height() as i32) == (dst.w, dst.h) {
                out.blit(&piece, dst.x, dst.y);
            } else {
                out.blit(&piece.scaled_area(dst.w as u32, dst.h as u32)?, dst.x, dst.y);
            }
        }
        // The drawings, wherever they reach: pixelate and blur take what's
        // under them from this picture of the whole area, so one reaching
        // across monitors covers both parts.
        if self.overlays.iter().any(|o| o.annotations.iter().any(|s| !s.hidden)) {
            let under = out.clone();
            let mut p = Painter::new(out.pixmap_mut(), scale);
            for ov in &self.overlays {
                let g = ov.output.rect;
                let d = Point::new((g.x - area.x) as f32, (g.y - area.y) as f32);
                for shape in ov.annotations.iter().filter(|s| !s.hidden) {
                    shape.moved(d).paint(&mut p, &under, scale);
                }
            }
        }
        out.force_opaque();
        Ok(out)
    }

    /// The app and title of what was captured: the window chosen, else the active one.
    fn name_source(&self, shot: &mut Shot, window: Option<usize>) {
        let source = window.and_then(|w| self.st.windows.get(w)).or(self.desktop.as_ref().and_then(|d| d.active.as_ref()));
        if let Some(w) = source {
            shot.app = w.app.clone();
            shot.title = w.title.clone();
        }
    }

    // -- tool state --------------------------------------------------------------------

    /// Bring every overlay up to date with the state.
    pub fn refresh(&mut self) {
        if self.done {
            return;
        }
        self.update_spans();
        if self.st.tool != Tool::Record {
            for ov in &mut self.overlays {
                ov.rec_rect = None;
                ov.rec_window = None;
                ov.rec_drag = None;
            }
        }
        self.st.any_armed = self.overlays.iter().any(|o| o.rec_rect.is_some());
        self.st.record_problem = if self.st.tool == Tool::Record { self.record_problem() } else { None };
        self.sync_toolbars();
        self.refresh_toolbars();
        for o in 0..self.overlays.len() {
            self.place_panel(o);
            if self.overlays[o].cursor_pos.is_some() {
                self.update_hover(o);
            }
            self.update_float_hover(o);
        }
    }

    fn refresh_toolbars(&mut self) {
        let (can_undo, can_redo) = (self.can_undo(), self.can_redo());
        let view = toolbar_view(&self.st, self.code_scan(), can_undo, can_redo);
        for ov in &mut self.overlays {
            if let Some(tb) = &mut ov.toolbar {
                tb.refresh(&view);
            }
        }
    }

    pub fn set_tool(&mut self, tool: Tool) {
        if tool == Tool::Record && !self.st.can_record {
            return;
        }
        self.commit_text();
        if self.st.tool == Tool::Record && tool != Tool::Record {
            self.disarm();
        }
        if tool != Tool::Select {
            self.select_on(None);
        }
        self.st.tool = tool;
        if tool.uses_color() {
            self.st.colour_tool = tool;
        }
        self.refresh();
    }

    /// Choose a colour; from a tool that has none (capture, pixelate), go
    /// back to the last drawing tool, so the colour is used.
    pub fn set_color(&mut self, index: usize) {
        if self.st.tool == Tool::Record {
            return;
        }
        if self.st.tool == Tool::Select
            && let Some(so) = self.selected_on()
        {
            // (it changes the selected drawing)
            self.st.color_index = index.min(CUSTOM);
            let color = self.st.color();
            self.restyle_selected(so, Some(color), None);
            return;
        }
        if !self.st.tool.uses_color() {
            self.st.tool = self.st.colour_tool;
        }
        self.st.color_index = index.min(CUSTOM);
        let color = self.st.color();
        if let Some((_, shape)) = &mut self.st.text_edit {
            shape.color = color;
        }
        self.refresh();
    }

    pub fn set_custom_color(&mut self, color: Color) {
        self.st.custom_color = Color { a: 255, ..color };
        self.set_color(CUSTOM);
    }

    fn toggle_picker(&mut self, o: usize) {
        if self.overlays[o].picker_open() {
            self.close_picker();
        } else if self.st.tool != Tool::Record {
            self.set_color(CUSTOM);
            self.show_picker(o);
        }
    }

    /// Close the picker (or stop the eyedropper); true if either was open.
    pub fn close_picker(&mut self) -> bool {
        let was_open = self.st.eyedropper || self.overlays.iter().any(|o| o.picker_open());
        self.st.eyedropper = false;
        for o in 0..self.overlays.len() {
            self.hide_picker(o);
        }
        let hex = self.st.custom_color.hex();
        if hex != self.st.cfg.custom_color {
            self.st.cfg.custom_color = hex.clone();
            config::update(move |c| c.custom_color = hex);
        }
        if was_open {
            self.refresh();
        }
        was_open
    }

    fn start_eyedropper(&mut self) {
        for o in 0..self.overlays.len() {
            self.hide_picker(o);
        }
        self.st.eyedropper = true;
        self.refresh();
    }

    /// The eyedropper's click: the pixel there becomes your own colour,
    /// and the picker comes back to adjust it.
    fn take_color(&mut self, o: usize, pos: Point) {
        let ov = &self.overlays[o];
        let px = pixel_at(&ov.base, pos, ov.dpr);
        self.st.eyedropper = false;
        self.set_custom_color(Color::from_premul(px));
        let t = self.st.toolbar_overlay.filter(|&t| t < self.overlays.len()).unwrap_or(o);
        self.show_picker(t);
    }

    fn cycle_size(&mut self) {
        self.set_size((self.st.size + 1) % SIZES.len());
    }

    pub fn set_size(&mut self, size: usize) {
        self.st.size = size.min(SIZES.len() - 1);
        if let Some((_, shape)) = &mut self.st.text_edit {
            shape.size = self.st.size;
        }
        if self.st.tool == Tool::Select
            && let Some(so) = self.selected_on()
        {
            return self.restyle_selected(so, None, Some(self.st.size));
        }
        self.refresh();
    }

    /// Beautify this capture and the next ones (Settings → After capture →
    /// Beautify), or not.
    fn toggle_beautify(&mut self) {
        if self.st.tool == Tool::Record {
            return;
        }
        let on = !self.st.cfg.beautify;
        self.st.cfg.beautify = on;
        config::update(move |c| c.beautify = on);
        self.refresh();
    }

    fn toggle_pin(&mut self) {
        self.st.pin_mode = !self.st.pin_mode;
        self.refresh();
    }

    /// Snap selections to edges in the picture; remembered for next time.
    fn toggle_snap(&mut self) {
        self.st.snap_edges = !self.st.snap_edges;
        let on = self.st.snap_edges;
        self.st.cfg.snap_edges = on;
        config::update(move |c| c.snap_edges = on);
        for o in 0..self.overlays.len() {
            self.snap_changed(o);
        }
        self.refresh();
    }

    /// Show the mouse pointer in the picture, or not (as captured).
    fn toggle_pointer(&mut self) {
        if !self.st.pointer_available || self.st.tool == Tool::Record || self.done {
            return;
        }
        self.st.show_pointer = !self.st.show_pointer;
        let show = self.st.show_pointer;
        for ov in &mut self.overlays {
            ov.show_pointer(show);
        }
        self.refresh();
    }

    /// Show or hide the codes found; remembered for next time.
    fn toggle_codes(&mut self) {
        self.st.codes_visible = !self.st.codes_visible;
        let on = self.st.codes_visible;
        if on {
            self.start_scan(); // (hidden at the start, so not looked for yet)
        }
        self.st.cfg.show_codes = on;
        config::update(move |c| c.show_codes = on);
        self.refresh();
    }

    /// How the search for codes stands.
    pub fn code_scan(&self) -> CodeScan {
        if self.scans_left > 0 {
            CodeScan::Scanning
        } else if self.scan_pending {
            CodeScan::NotYet
        } else {
            CodeScan::Found(self.code_count())
        }
    }

    pub fn code_count(&self) -> usize {
        self.overlays.iter().map(|o| o.codes.len()).sum()
    }

    fn dismiss_code(&mut self, o: usize, i: usize) {
        let ov = &mut self.overlays[o];
        if i < ov.codes.len() && i < ov.chips.len() {
            ov.codes.remove(i);
            ov.chips.remove(i);
            ov.code_hover = match ov.code_hover {
                Some(h) if h == i => None,
                Some(h) if h > i => Some(h - 1),
                other => other,
            };
        }
        self.set_hover(o, Hover::default());
        self.st.hint = None;
        self.refresh();
    }

    fn next_number(&self) -> u32 {
        // (one more than the highest shown: a moved counter keeps its number)
        shapes::next_number(self.overlays.iter().flat_map(|o| o.annotations.iter()))
    }

    // -- recording -----------------------------------------------------------------------

    fn problem_for(&mut self, format: &str) -> Option<String> {
        if let Some((_, p)) = self.problems.iter().find(|(f, _)| f == format) {
            return p.clone();
        }
        let p = self.record_support.problem(format);
        self.problems.push((format.to_string(), p.clone()));
        p
    }

    /// Why recording in the chosen format can't work here.
    fn record_problem(&mut self) -> Option<String> {
        let format = self.st.cfg.record_format.clone();
        self.problem_for(&format)
    }

    /// Change and remember a recording option.
    fn set_record_option(&mut self, opt: RecordOption) {
        let cfg = &mut self.st.cfg;
        match &opt {
            RecordOption::Format(f) => cfg.record_format = f.clone(),
            RecordOption::Fps(v) => cfg.record_fps = *v,
            RecordOption::Mic(v) => cfg.record_mic = *v,
            RecordOption::SystemAudio(v) => cfg.record_system_audio = *v,
            RecordOption::Cursor(v) => cfg.record_cursor = *v,
        }
        config::update(move |c| match opt {
            RecordOption::Format(f) => c.record_format = f,
            RecordOption::Fps(v) => c.record_fps = v,
            RecordOption::Mic(v) => c.record_mic = v,
            RecordOption::SystemAudio(v) => c.record_system_audio = v,
            RecordOption::Cursor(v) => c.record_cursor = v,
        });
        self.refresh();
    }

    /// Forget the area chosen for recording (on every monitor).
    fn disarm(&mut self) {
        for ov in &mut self.overlays {
            ov.rec_rect = None;
            ov.rec_window = None;
            ov.span_rect = None;
        }
    }

    /// The toolbar's screen button: capture, or choose the screen to record.
    fn whole_screen(&mut self, o: usize) {
        if self.st.tool == Tool::Record {
            self.disarm();
            let r = self.overlays[o].rect();
            self.arm(o, r, None);
        } else {
            self.capture(o, None, None);
        }
    }

    fn start_countdown(&mut self, o: usize) {
        if self.done || self.overlays[o].rec_rect.is_none() || self.st.countdown.is_some() {
            return;
        }
        if let Some(problem) = self.record_problem() {
            self.st.hint = Some(problem);
            return;
        }
        if self.st.cfg.record_countdown <= 0 {
            self.start_recording(o);
            return;
        }
        self.st.countdown = Some(self.st.cfg.record_countdown);
        self.countdown_overlay = Some(o);
        self.schedule(Timer::Countdown, Duration::from_secs(1));
        self.refresh();
    }

    fn cancel_countdown(&mut self) -> bool {
        if self.st.countdown.is_none() {
            return false;
        }
        self.timers.retain(|t| t.1 != Timer::Countdown);
        self.st.countdown = None;
        self.countdown_overlay = None;
        self.refresh();
        true
    }

    fn tick(&mut self) {
        let Some(n) = self.st.countdown else { return };
        if n > 1 {
            self.st.countdown = Some(n - 1);
            self.schedule(Timer::Countdown, Duration::from_secs(1));
            self.refresh();
            return;
        }
        let o = self.countdown_overlay.take();
        self.st.countdown = None;
        if let Some(o) = o {
            self.start_recording(o);
        }
    }

    /// Close the overlays and hand the area chosen on `o` to the recorder.
    fn start_recording(&mut self, o: usize) {
        let Some(rect) = self.overlays.get(o).and_then(|ov| ov.rec_rect) else { return };
        if self.done {
            return;
        }
        let ov = &self.overlays[o];
        let g = ov.output.rect;
        let local = rect.aligned();
        let area = local.translated(g.x, g.y);
        let window = ov.rec_window;
        let full = local == IRect::new(0, 0, g.w, g.h);
        let mut shot = Shot::new(if window.is_some() {
            "window"
        } else if full {
            "monitor"
        } else {
            "region"
        });
        shot.monitor = ov.output.name.clone();
        self.name_source(&mut shot, window);
        let spanning = !g.contains_rect(&area);
        let thumbnail = if spanning && self.may_span { self.render_desktop(area) } else { ov.render(Some(rect)) };
        let thumbnail = match thumbnail {
            Ok(t) => t,
            Err(e) => return self.fail(&e),
        };
        let cfg = &self.st.cfg;
        let job = RecordJob {
            area,
            output: ov.output.clone(),
            scale: ov.dpr,
            shot,
            thumbnail,
            format: cfg.record_format.clone(),
            fps: cfg.record_fps,
            mic: cfg.record_mic,
            system_audio: cfg.record_system_audio,
            cursor: cfg.record_cursor,
        };
        self.clock.step("recording area chosen", format!("{} × {}, {}, {} fps", area.w, area.h, job.format, job.fps));
        self.close_overlays();
        self.finish(Outcome::Record(Box::new(job)));
    }

    // -- history -------------------------------------------------------------------------

    /// Something was drawn on overlay `o`.
    fn record(&mut self, o: usize) {
        self.history.push(o);
        for &r in &self.redo_stack {
            if let Some(ov) = self.overlays.get_mut(r) {
                ov.undone.clear();
            }
        }
        self.redo_stack.clear();
        self.refresh_toolbars();
    }

    fn commit_shape(&mut self, o: usize, shape: Shape) {
        self.overlays[o].commit(shape);
        self.record(o);
    }

    // Drawings cross monitors: each overlay owns the drawings begun on it,
    // and also paints the others' that reach it.

    /// Where a point on overlay `from` is on overlay `to` (each in its own
    /// logical coordinates).
    fn to_overlay(&self, from: usize, to: usize, p: Point) -> Point {
        let (a, b) = (self.overlays[from].output.rect, self.overlays[to].output.rect);
        p + Point::new((a.x - b.x) as f32, (a.y - b.y) as f32)
    }

    /// Each overlay, topmost drawings first (the other overlays' are painted
    /// over this one's own), with `pos` (on `o`) in its coordinates.
    fn topmost_first(&self, o: usize, pos: Point) -> Vec<(usize, Point)> {
        let mut order: Vec<usize> = (0..self.overlays.len()).filter(|&k| k != o).rev().collect();
        order.push(o);
        order.into_iter().map(|k| (k, self.to_overlay(o, k, pos))).collect()
    }

    /// The topmost text at `pos` on `o`, wherever it was typed: (its
    /// overlay, its index there, `pos` in that overlay's coordinates).
    fn text_at_any(&self, o: usize, pos: Point) -> Option<(usize, usize, Point)> {
        self.topmost_first(o, pos).into_iter().find_map(|(k, local)| self.overlays[k].text_at(local).map(|i| (k, i, local)))
    }

    /// The overlay whose select tool takes a press at `pos` on `o`: the one
    /// with a drawing selected, on its handles or the drawing; else the one
    /// owning the topmost drawing there.
    fn select_owner(&self, o: usize, pos: Point) -> usize {
        if let Some(k) = (0..self.overlays.len()).find(|&k| self.overlays[k].selection.current(&self.overlays[k].annotations).is_some()) {
            let local = self.to_overlay(o, k, pos);
            if self.overlays[k].selection.holds(&self.overlays[k].annotations, local, 1.0) {
                return k;
            }
        }
        self.topmost_first(o, pos).into_iter().find(|(k, local)| !Selection::hits(&self.overlays[*k].annotations, *local, 1.0).is_empty()).map_or(o, |(k, _)| k)
    }

    /// Bring overlay `o`'s copies of the other monitors' drawings up to date:
    /// those done (again only when they change), and those under way.
    fn share_drawings(&mut self, o: usize) {
        if self.overlays.len() < 2 || o >= self.overlays.len() {
            return;
        }
        let here = self.overlays[o].rect();
        let others: Vec<usize> = (0..self.overlays.len()).filter(|&k| k != o).collect();
        let key = {
            use std::hash::{Hash, Hasher};
            let mut h = std::collections::hash_map::DefaultHasher::new();
            for &k in &others {
                let ov = &self.overlays[k];
                (k, ov.annotations.len()).hash(&mut h);
                for s in &ov.annotations {
                    s.hidden.hash(&mut h);
                }
            }
            h.finish()
        };
        let offsets: Vec<Point> = (0..self.overlays.len()).map(|k| self.to_overlay(k, o, Point::ZERO)).collect();
        let moved_here = |k: usize, s: &Shape| {
            let m = s.moved(offsets[k]);
            m.bounds().intersects(&here).then_some(m)
        };
        if key != self.overlays[o].foreign_key {
            let foreign: Vec<Shape> = others
                .iter()
                .flat_map(|&k| self.overlays[k].annotations.iter().filter(|s| !s.hidden && !s.is_removed()).map(move |s| (k, s)))
                .filter_map(|(k, s)| moved_here(k, s))
                .collect();
            self.overlays[o].foreign = foreign;
            self.overlays[o].foreign_key = key;
        }
        let mut live: Vec<Shape> = Vec::new();
        for &k in &others {
            let ov = &self.overlays[k];
            for s in ov.active.iter().chain(ov.selection.moving()) {
                live.extend(moved_here(k, s));
            }
        }
        // (the text being typed as it is: its box, caret and what's selected)
        if let Some((k, s)) = &self.st.text_edit
            && *k != o
        {
            let s = s.shifted(offsets[*k]);
            live.extend(s.bounds().intersects(&here).then_some(s));
        }
        self.overlays[o].foreign_live = live;
        let frame = others.iter().find_map(|&k| self.overlays[k].selection.framed(&self.overlays[k].annotations).map(|s| s.shifted(offsets[k])));
        self.overlays[o].foreign_frame = frame.filter(|s| Selection::frame_bounds_of(s, 1.0).intersects(&here));
    }

    // The select tool: one drawing selected at a time, on one monitor.

    /// Selecting on overlay `o` (None: nowhere): drop what's selected on the others.
    fn select_on(&mut self, o: Option<usize>) {
        for (i, ov) in self.overlays.iter_mut().enumerate() {
            if Some(i) != o {
                ov.selection.clear(&mut ov.annotations);
            }
        }
    }

    /// The overlay with a drawing selected, if any.
    fn selected_on(&mut self) -> Option<usize> {
        (0..self.overlays.len()).find(|&i| {
            let ov = &mut self.overlays[i];
            ov.selection.index(&ov.annotations).is_some()
        })
    }

    /// The selection on `o` changed its drawings (they were `before` long):
    /// a changed copy is an undo step.
    fn selection_changed(&mut self, o: usize, before: usize) {
        if self.overlays[o].annotations.len() > before {
            self.overlays[o].undone.clear();
            self.record(o);
        }
        self.refresh();
    }

    fn restyle_selected(&mut self, o: usize, color: Option<Color>, size: Option<usize>) {
        let len = self.overlays[o].annotations.len();
        let ov = &mut self.overlays[o];
        ov.selection.restyle(&mut ov.annotations, color, size);
        self.selection_changed(o, len);
    }

    pub fn can_undo(&self) -> bool {
        !self.history.is_empty() || self.st.text_edit.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.redo_stack.is_empty()
    }

    pub fn undo(&mut self) {
        self.commit_text();
        self.select_on(None);
        if let Some(o) = self.history.pop()
            && self.overlays[o].undo()
        {
            self.redo_stack.push(o);
        }
        self.refresh();
    }

    pub fn redo(&mut self) {
        self.commit_text();
        self.select_on(None);
        if let Some(o) = self.redo_stack.pop()
            && self.overlays[o].redo()
        {
            self.history.push(o);
        }
        self.refresh();
    }

    // -- text ----------------------------------------------------------------------------

    fn begin_text(&mut self, o: usize, shape: Shape) {
        self.commit_text();
        if let Some(orig) = shape.replaces.and_then(|i| self.overlays[o].annotations.get_mut(i)) {
            orig.hidden = true; // (the copy is shown while it's edited)
        }
        self.st.text_edit = Some((o, shape));
        self.refresh_toolbars();
    }

    /// Finish the text being typed; true if there was one.
    pub fn commit_text(&mut self) -> bool {
        let Some((o, mut shape)) = self.st.text_edit.take() else { return false };
        let replaces = shape.replaces;
        if let Some(t) = shape.as_text_mut() {
            t.editing = false;
        }
        if o < self.overlays.len() {
            if shape.is_valid() && shape.text_changed() {
                self.commit_shape(o, shape);
            } else if let Some(i) = replaces
                && let Some(orig) = self.overlays[o].annotations.get_mut(i)
            {
                orig.hidden = false; // edited back to as it was
            }
        }
        self.refresh_toolbars();
        true
    }

    // -- keys ------------------------------------------------------------------------------

    /// A key's action (keys::BINDINGS) while capturing.
    fn do_action(&mut self, action: &str, target: usize) {
        match action {
            "undo" => self.undo(),
            "redo" => self.redo(),
            "codes" => self.toggle_codes(),
            "pin" => self.toggle_pin(),
            "beautify" => self.toggle_beautify(),
            "snap" => self.toggle_snap(),
            "pointer" => self.toggle_pointer(),
            "copy_color" => self.copy_color(target),
            "size.down" => self.set_size(self.st.size.saturating_sub(1)),
            "size.up" => self.set_size(self.st.size + 1),
            _ => {
                if let Some(t) = action.strip_prefix("tool.").and_then(Tool::from_id) {
                    self.set_tool(t);
                } else if let Some(i) = action.strip_prefix("color.").and_then(|n| n.parse::<usize>().ok()) {
                    self.set_color(i.saturating_sub(1));
                }
            }
        }
    }

    // -- finishing -------------------------------------------------------------------------

    fn finish(&mut self, outcome: Outcome) {
        if self.outcome.is_none() {
            self.outcome = Some(outcome);
        }
        self.done = true;
    }

    /// Take the overlays down and let go of every picture.
    fn close_overlays(&mut self) {
        self.done = true;
        self.cancelled.store(true, Ordering::Relaxed);
        self.timers.clear();
        self.overlays.clear();
        self.history.clear();
        self.redo_stack.clear();
        self.st.text_edit = None;
        self.st.pointer_overlay = None;
        self.st.toolbar_overlay = None;
        self.countdown_overlay = None;
        self.codes.clear();
    }

    /// Capture `rect` (logical, None: the whole monitor) of overlay `o`;
    /// `window` when it's a window that was clicked.
    fn capture(&mut self, o: usize, rect: Option<Rect>, window: Option<usize>) {
        if self.done {
            return;
        }
        self.commit_text();
        // (the other monitors' drawings that reach this one, as they are now)
        self.share_drawings(o);
        self.clock.step("chosen", "");
        let ov = &self.overlays[o];
        let drawn = ov.annotations.len();
        let mut shot = Shot::new(if window.is_some() {
            "window"
        } else if rect.is_some() {
            "region"
        } else {
            "monitor"
        });
        shot.monitor = ov.output.name.clone();
        let g = ov.output.rect;
        let at = if self.request.image { None } else { Some(rect.map(|r| r.aligned()).unwrap_or(IRect::new(0, 0, g.w, g.h)).translated(g.x, g.y)) };
        self.name_source(&mut shot, window);
        let pin = self.st.pin_mode;
        // The overlay goes (and with it what else held its picture), so a
        // whole screen with nothing drawn is handed over without a copy.
        let ov = self.overlays.swap_remove(o);
        self.close_overlays();
        let image = match ov.into_render(rect) {
            Ok(img) => img,
            Err(e) => return self.fail(&e),
        };
        self.clock.step("drawn", format!("{} × {}, {drawn} drawing{}", image.width(), image.height(), if drawn == 1 { "" } else { "s" }));
        self.finish(Outcome::Image { image, shot, at, pin });
    }

    /// Copy the colour under the pointer (the one the magnifier shows).
    fn copy_color(&mut self, o: usize) {
        let ov = &self.overlays[o];
        let Some(pos) = ov.cursor_pos else { return };
        let px = pixel_at(&ov.base, pos, ov.dpr);
        let (r, g, b, _) = image::unpack(px);
        let hex = image::hex(px);
        self.close_overlays();
        self.finish(Outcome::Color { hex, rgb: (r, g, b) });
    }

    fn copy_code(&mut self, o: usize, i: usize) {
        let Some(text) = self.overlays[o].codes.get(i).and_then(|(c, _)| self.codes.get(*c)).map(|c| c.text.clone()) else { return };
        self.close_overlays();
        self.finish(Outcome::CodeText(text));
    }

    fn open_code(&mut self, o: usize, i: usize) {
        let Some(code) = self.overlays[o].codes.get(i).and_then(|(c, _)| self.codes.get(*c)) else { return };
        if !code.is_link() {
            return; // (only links get an Open button)
        }
        let url = code.text.trim().to_string();
        self.close_overlays();
        self.finish(Outcome::OpenLink(url));
    }

    pub fn cancel(&mut self) {
        self.close_overlays();
        self.finish(Outcome::Cancelled);
    }

    pub fn fail(&mut self, message: &str) {
        self.close_overlays();
        self.finish(Outcome::Failed(message.to_string()));
    }

    // -- background work and timers ----------------------------------------------------------

    fn schedule(&mut self, t: Timer, after: Duration) {
        self.timers.retain(|x| x.1 != t);
        self.timers.push((Instant::now() + after, t));
    }

    /// Look for codes on each monitor's picture, one after another (the
    /// pointer's first), in the background; once a capture (later calls do nothing).
    fn start_scan(&mut self) {
        if !std::mem::take(&mut self.scan_pending) || self.done {
            return;
        }
        self.timers.retain(|x| x.1 != Timer::Scan);
        self.scans_left = self.overlays.len();
        self.refresh_toolbars();
        let mut pictures: Vec<(usize, Arc<Image>)> = self.overlays.iter().map(|o| (o.index, o.plain.clone().unwrap_or_else(|| o.base.clone()))).collect();
        // The monitor being looked at first.
        let first = self.st.pointer_overlay;
        pictures.sort_by_key(|(o, _)| Some(*o) != first);
        let (tx, waker, cancelled) = (self.tx.clone(), self.waker.clone(), self.cancelled.clone());
        let started = std::thread::Builder::new().name("flatshot-scan".into()).spawn(move || {
            for (o, img) in pictures {
                if cancelled.load(Ordering::Relaxed) {
                    return; // (the rest of the pictures are let go now)
                }
                let t = Instant::now();
                let codes = scanner::scan(&img);
                drop(img);
                timing::log(format!("code scan: {} found on screen {o} in {:.0} ms", codes.len(), t.elapsed().as_secs_f64() * 1000.0));
                if tx.send(Background::Codes(o, codes)).is_err() {
                    return;
                }
                waker();
            }
        });
        if let Err(e) = started {
            self.scans_left = 0;
            eprintln!("flatshot-rs: could not start looking for codes: {e}");
        }
    }

    /// Snapping was turned on or off. When it's on, build the edge maps
    /// (once per overlay), in the background so nothing waits.
    fn snap_changed(&mut self, o: usize) {
        let ov = &mut self.overlays[o];
        ov.snap_point = None;
        if !self.st.snap_edges || ov.edge_maps.is_some() || ov.edges_requested {
            return;
        }
        ov.edges_requested = true;
        let img = ov.base.clone();
        let (tx, waker, cancelled) = (self.tx.clone(), self.waker.clone(), self.cancelled.clone());
        let _ = std::thread::Builder::new().name("flatshot-edges".into()).spawn(move || {
            if cancelled.load(Ordering::Relaxed) {
                return;
            }
            let maps = EdgeMaps::build(&img);
            drop(img);
            if tx.send(Background::Edges(o, Arc::new(maps))).is_ok() {
                waker();
            }
        });
    }

    /// `pos` moved onto nearby edges in the picture, if any (Ctrl: as is).
    fn snapped(&mut self, o: usize, pos: Point, mods: i32) -> Point {
        if !self.st.snap_edges || mods & keyseq::CTRL != 0 {
            return pos;
        }
        let ov = &self.overlays[o];
        let Some(maps) = &ov.edge_maps else {
            self.snap_changed(o); // still being built: place freely until then
            return pos;
        };
        let cfg = &self.st.cfg;
        let (x, y) = snapping::snap(maps, pos.x, pos.y, ov.dpr, cfg.snap_distance, cfg.snap_sensitivity);
        Point::new(x, y)
    }

    // -- one overlay ---------------------------------------------------------------------------

    fn place_floating(&mut self, o: usize) {
        let ov = &mut self.overlays[o];
        let w = ov.width();
        if let Some(tb) = &mut ov.toolbar
            && !tb.placed
            && w > tb.size.0
        {
            tb.place(w);
        }
        let boxes: Vec<Rect> = ov.codes.iter().map(|(_, poly)| poly_bounds(poly)).collect();
        let bounds = ov.rect();
        place_chips(&mut ov.chips, &boxes, bounds);
        self.place_panel(o);
    }

    /// Under the area chosen for recording, else above it, else inside.
    fn place_panel(&mut self, o: usize) {
        let show = {
            let ov = &self.overlays[o];
            ov.rec_rect.is_some() && ov.rec_drag.is_none() && self.st.countdown.is_none()
        };
        if !show {
            return;
        }
        let formats: Vec<(String, String, bool)> = RECORD_FORMATS.iter().map(|(v, l)| (v.to_string(), l.to_string(), self.problem_for(v).is_none())).collect();
        let problem = self.record_problem();
        let cfg = &self.st.cfg;
        let view = RecordView {
            format: &cfg.record_format,
            fps: cfg.record_fps,
            mic: cfg.record_mic,
            system_audio: cfg.record_system_audio,
            cursor: cfg.record_cursor,
            formats,
            problem,
        };
        let ov = &mut self.overlays[o];
        let (w, h) = (ov.width(), ov.height());
        let Some(r) = ov.rec_rect else { return };
        let panel = ov.panel.get_or_insert_with(RecordPanel::default);
        panel.refresh(&view);
        let (pw, ph) = panel.size;
        let x = (r.center().x - pw / 2.0).min(w - pw - 8.0).max(8.0);
        let y = if r.bottom() + 16.0 + ph <= h - 8.0 {
            r.bottom() + 16.0
        } else if r.top() - 16.0 - ph >= 8.0 {
            r.top() - 16.0 - ph
        } else {
            r.bottom() - ph - 60.0
        };
        panel.pos = Point::new(py_round(x) as f32, py_round(y) as f32);
    }

    /// The picker for your own colour, under its swatch on the toolbar.
    fn show_picker(&mut self, o: usize) {
        let color = self.st.custom_color;
        let ov = &mut self.overlays[o];
        let (w, h) = (ov.width(), ov.height());
        let picker = ov.picker.get_or_insert_with(|| ColorPicker::new(color));
        picker.set_color(color);
        let (pw, ph) = (picker.rect().w, picker.rect().h);
        let (mut x, mut y) = (8.0, 8.0);
        if let Some(tb) = &ov.toolbar {
            let bar = tb.rect();
            if let Some(sw) = tb.items.iter().find(|it| it.id == "color.8") {
                x = (tb.pos.x + sw.rect.x + (sw.rect.w / 2.0).floor()).floor() - (pw / 2.0).floor();
            }
            y = if bar.bottom() + 8.0 + ph <= h - 8.0 { bar.bottom() + 8.0 } else { bar.top() - 8.0 - ph };
        }
        picker.pos = Point::new(x.min(w - pw - 8.0).max(8.0), y.max(8.0));
        ov.picker_visible = true;
    }

    fn hide_picker(&mut self, o: usize) {
        let ov = &mut self.overlays[o];
        if ov.picker_open() {
            ov.picker_visible = false;
        }
        if let Some(p) = &mut ov.picker {
            p.hex.focused = false;
        }
    }

    fn focused_picker(&self) -> Option<usize> {
        self.overlays.iter().position(|o| o.picker_open() && o.picker.as_ref().is_some_and(|p| p.hex.focused))
    }

    fn picker_event(&mut self, o: usize, ev: PickerEvent) {
        match ev {
            PickerEvent::None => {}
            PickerEvent::Color(c) => self.set_custom_color(c),
            PickerEvent::Eyedropper => self.start_eyedropper(),
            PickerEvent::Close => {
                self.close_picker();
            }
            PickerEvent::Copy(s) => self.effects.push(Effect::Clipboard(s)),
            PickerEvent::Paste => {
                self.paste_target = Some(PasteTarget::Picker(o));
                self.effects.push(Effect::Paste);
            }
        }
    }

    /// Window bounds (global logical) on overlay `o`'s monitor.
    fn set_windows(&mut self, o: usize) {
        let g = self.overlays[o].output.rect;
        let bounds = Rect::new(0.0, 0.0, g.w as f32, g.h as f32);
        let list = self
            .st
            .windows
            .iter()
            .enumerate()
            .filter_map(|(i, w)| {
                let r = w.rect.translated(-g.x, -g.y).to_f().intersected(&bounds);
                (r.w >= 8.0 && r.h >= 8.0).then_some((r, i))
            })
            .collect();
        self.overlays[o].windows = list;
        self.update_hover(o);
    }

    /// The window under the pointer (when choosing an area), and the code.
    fn update_hover(&mut self, o: usize) {
        let shown = self.overlays[o].chips_shown(&self.st);
        if !self.overlays[o].chips.is_empty() {
            self.overlays[o].update_code_hover(shown);
        }
        let ov = &self.overlays[o];
        let mut hover = None;
        if matches!(self.st.tool, Tool::Region | Tool::Record)
            && ov.sel_rect.is_none()
            && ov.rec_rect.is_none()
            && let Some(pos) = ov.cursor_pos
            && !ov.over_floating(0.0, shown)
        {
            hover = ov.windows.iter().find(|(r, _)| r.contains(pos)).copied();
        }
        if hover != self.overlays[o].hover_window {
            self.overlays[o].hover_window = hover;
            self.hover_changed(o, hover.map(|h| h.1));
        }
    }

    fn cancel_gesture(&mut self, o: usize) -> bool {
        let ov = &mut self.overlays[o];
        if ov.selection.clear(&mut ov.annotations) {
            self.refresh();
            return true;
        }
        if ov.sel_rect.is_some() || ov.active.is_some() || ov.rec_drag.is_some() {
            let spanned = ov.sel_rect.is_some() && self.st.spans;
            ov.sel_origin = None;
            ov.sel_rect = None;
            ov.active = None;
            ov.rec_drag = None;
            ov.text_drag = None;
            if spanned {
                self.selection_moved(o, None);
            }
            self.refresh();
            return true;
        }
        if ov.rec_rect.is_some() {
            ov.rec_rect = None;
            ov.rec_window = None;
            self.area_moved(o);
            self.refresh();
            return true;
        }
        false
    }

    /// Choose `rect` (logical, overlay `o`'s coordinates) to record. It may
    /// cross onto other monitors where the recorder can take that.
    fn arm(&mut self, o: usize, rect: Rect, window: Option<usize>) {
        let bounds = self.drag_bounds(o);
        self.overlays[o].rec_rect = Some(rect.intersected(&bounds));
        self.overlays[o].rec_window = window;
        if self.st.spans {
            self.selection_moved(o, None); // (the drag that chose it is over)
        }
        self.area_moved(o);
        self.refresh();
    }

    /// The recording area on `o`, moved over onto another monitor: it's that
    /// monitor's now (most of it is there), with its handles and panel.
    fn rehome_area(&mut self, o: usize) {
        let Some(r) = self.overlays[o].rec_rect else { return };
        let g = self.overlays[o].output.rect.to_f();
        let area = r.translated(g.top_left());
        let size = |k: usize| {
            let i = area.intersected(&self.overlays[k].output.rect.to_f());
            i.w.max(0.0) * i.h.max(0.0)
        };
        let Some(k) = (0..self.overlays.len()).max_by(|&a, &b| size(a).total_cmp(&size(b))).filter(|&k| k != o && size(k) > size(o)) else { return };
        let to = self.overlays[k].output.rect.to_f();
        let from = &mut self.overlays[o];
        from.rec_rect = None;
        from.rec_window = None;
        from.panel = None;
        let ov = &mut self.overlays[k];
        ov.rec_rect = Some(area.translated(-to.top_left()));
        ov.span_rect = None;
        self.area_moved(k);
    }

    /// Move or resize the recording area; a resized edge goes to `snapped`
    /// when snapping found an edge there. `center` (Ctrl): the opposite edge
    /// moves the other way, so the area stays centred.
    fn drag_area(&mut self, o: usize, pos: Point, snapped: Option<Point>, center: bool) {
        let Some((handle, start, r0)) = self.overlays[o].rec_drag else { return };
        let d = pos - start;
        let bounds = self.drag_bounds(o);
        let r = match handle {
            Handle::Move => {
                let mut r = r0.translated(d);
                r.x = r.x.min(bounds.right() - r.w).max(bounds.left());
                r.y = r.y.min(bounds.bottom() - r.h).max(bounds.top());
                r
            }
            Handle::Edges { left, right, top, bottom } if center => {
                // The dragged edge follows the pointer as without Ctrl; the other mirrors it.
                let c = r0.center();
                let mut r = r0;
                if left || right {
                    let edge = if left { r0.left() } else { r0.right() } + d.x;
                    let half = (edge - c.x).abs().min(c.x - bounds.left()).min(bounds.right() - c.x);
                    r.x = c.x - half;
                    r.w = 2.0 * half;
                }
                if top || bottom {
                    let edge = if top { r0.top() } else { r0.bottom() } + d.y;
                    let half = (edge - c.y).abs().min(c.y - bounds.top()).min(bounds.bottom() - c.y);
                    r.y = c.y - half;
                    r.h = 2.0 * half;
                }
                r
            }
            Handle::Edges { left, right, top, bottom } => {
                let sx = snapped.filter(|s| s.x != pos.x).map(|s| s.x);
                let sy = snapped.filter(|s| s.y != pos.y).map(|s| s.y);
                let mut r = r0;
                if left {
                    r.set_left(sx.unwrap_or(r0.left() + d.x));
                }
                if right {
                    r.set_right(sx.unwrap_or(r0.right() + d.x));
                }
                if top {
                    r.set_top(sy.unwrap_or(r0.top() + d.y));
                }
                if bottom {
                    r.set_bottom(sy.unwrap_or(r0.bottom() + d.y));
                }
                r.normalized().intersected(&bounds)
            }
        };
        self.overlays[o].rec_rect = Some(r);
        self.overlays[o].rec_window = None;
        self.area_moved(o);
    }

    // -- the pointer on the panels -----------------------------------------------------------------

    /// The panel at `pos` on overlay `o`, topmost first: the picker, the
    /// recording panel, the toolbar, the code cards (the hovered one on top).
    fn float_at(&self, o: usize, pos: Point) -> Option<Float> {
        let ov = &self.overlays[o];
        if ov.picker_open() && ov.picker.as_ref().is_some_and(|p| p.rect().contains(pos)) {
            return Some(Float::Picker);
        }
        if ov.rec_rect.is_some() && ov.rec_drag.is_none() && self.st.countdown.is_none() && ov.panel.as_ref().is_some_and(|p| p.rect().contains(pos)) {
            return Some(Float::Panel);
        }
        if ov.toolbar_visible && ov.toolbar.as_ref().is_some_and(|t| t.rect().contains(pos)) {
            return Some(Float::Toolbar);
        }
        if ov.chips_shown(&self.st) {
            if let Some(h) = ov.code_hover
                && ov.chips.get(h).is_some_and(|c| c.rect().contains(pos))
            {
                return Some(Float::Chip(h));
            }
            if let Some(i) = (0..ov.chips.len()).rev().find(|&i| ov.chips[i].rect().contains(pos)) {
                return Some(Float::Chip(i));
            }
        }
        None
    }

    fn hover_at(&self, o: usize, pos: Point) -> Hover {
        let Some(float) = self.float_at(o, pos) else { return Hover::default() };
        let ov = &self.overlays[o];
        let mut h = Hover { float: Some(float), item: None, choice: None };
        match float {
            Float::Toolbar => h.item = ov.toolbar.as_ref().and_then(|t| t.item_at(pos)).map(|it| it.id),
            Float::Chip(i) => h.item = ov.chips.get(i).and_then(|c| c.item_at(pos)).map(|it| it.id),
            Float::Panel => {
                if let Some(p) = &ov.panel {
                    h.item = p.item_at(pos).map(|it| it.id);
                    if h.item == Some("format") {
                        h.choice = p.choice_at(pos);
                    }
                }
            }
            Float::Picker => {
                if let Some(p) = &ov.picker {
                    let local = pos - p.pos;
                    h.item = if p.dropper.rect.contains(local) {
                        Some("dropper")
                    } else if p.hex.rect.contains(local) {
                        Some("hex")
                    } else {
                        None
                    };
                }
            }
        }
        h
    }

    /// Recheck what the pointer is over (a panel may have moved, appeared or gone).
    fn update_float_hover(&mut self, o: usize) {
        if self.overlays[o].grab != Grab::None {
            return;
        }
        let h = match self.pointer[o] {
            Some(pos) => self.hover_at(o, pos),
            None => Hover::default(),
        };
        self.set_hover(o, h);
    }

    fn set_hover(&mut self, o: usize, h: Hover) {
        let old = self.hover[o];
        if old == h {
            return;
        }
        self.hover[o] = h;
        let ov = &mut self.overlays[o];
        if let Some(tb) = &mut ov.toolbar {
            tb.ptr.hovered = if h.float == Some(Float::Toolbar) { h.item } else { None };
        }
        for (i, chip) in ov.chips.iter_mut().enumerate() {
            chip.ptr.hovered = if h.float == Some(Float::Chip(i)) { h.item } else { None };
        }
        if let Some(p) = &mut ov.panel {
            p.ptr.hovered = if h.float == Some(Float::Panel) { h.item } else { None };
            let choice = if h.float == Some(Float::Panel) && h.item == Some("format") { h.choice } else { None };
            if p.choice_hover != choice {
                p.choice_hover = choice;
                for it in &mut p.items {
                    if let Look::Choice { hover, .. } = &mut it.look {
                        *hover = choice;
                    }
                }
            }
        }
        if let Some(p) = &mut ov.picker {
            p.ptr.hovered = if h.float == Some(Float::Picker) && h.item == Some("dropper") { Some("dropper") } else { None };
        }
        // A control's hint shows while the pointer is on it.
        if (old.float, old.item) != (h.float, h.item) {
            self.st.hint = self.item_hint(o, h);
        }
    }

    fn item_hint(&self, o: usize, h: Hover) -> Option<String> {
        let ov = &self.overlays[o];
        let id = h.item?;
        let hint = match h.float? {
            Float::Toolbar => ov.toolbar.as_ref()?.items.iter().find(|it| it.id == id && it.clickable())?.hint.clone(),
            Float::Chip(i) => ov.chips.get(i)?.items.iter().find(|it| it.id == id)?.hint.clone(),
            Float::Panel => ov.panel.as_ref()?.items.iter().find(|it| it.id == id && it.clickable())?.hint.clone(),
            Float::Picker => (id == "dropper").then(|| ov.picker.as_ref().map(|p| p.dropper.hint.clone()))??,
        };
        (!hint.is_empty()).then_some(hint)
    }

    fn on_custom_swatch(&self, o: usize, pos: Point) -> bool {
        let ov = &self.overlays[o];
        ov.toolbar_visible
            && ov.toolbar.as_ref().and_then(|t| t.item_at(pos)).is_some_and(|it| it.id == "color.8")
            && self.float_at(o, pos) == Some(Float::Toolbar)
    }

    fn press(&mut self, o: usize, button: Button, pos: Point, mods: i32) {
        // The picker closes on a press anywhere but on it or its swatch
        // (and the press goes on to do what it does).
        if self.overlays.iter().any(|ov| ov.picker_open()) && self.float_at(o, pos) != Some(Float::Picker) && !self.on_custom_swatch(o, pos) {
            self.close_picker();
        }
        let hover = self.hover_at(o, pos);
        self.set_hover(o, hover);
        match hover.float {
            None => {
                self.overlays[o].grab = Grab::Canvas;
                self.canvas_press(o, button, pos, mods);
            }
            Some(Float::Toolbar) => {
                if button != Button::Left {
                    return;
                }
                let ov = &mut self.overlays[o];
                ov.grab = Grab::Toolbar;
                if let Some(tb) = &mut ov.toolbar {
                    match tb.item_at(pos) {
                        Some(it) if it.clickable() && it.enabled => tb.ptr.pressed = Some(it.id),
                        _ => tb.grab = Some(pos - tb.pos), // (a disabled button drags the toolbar)
                    }
                }
            }
            Some(Float::Chip(i)) => {
                if button != Button::Left {
                    return;
                }
                let ov = &mut self.overlays[o];
                ov.grab = Grab::Chip(i);
                if let Some(chip) = ov.chips.get_mut(i) {
                    match chip.item_at(pos) {
                        Some(it) if it.clickable() && it.enabled => chip.ptr.pressed = Some(it.id),
                        _ => chip.grab = Some(pos - chip.pos),
                    }
                }
            }
            Some(Float::Panel) => {
                if button == Button::Right {
                    // (as on the picture: the area goes)
                    self.overlays[o].grab = Grab::Canvas;
                    self.canvas_press(o, button, pos, mods);
                    return;
                }
                if button != Button::Left {
                    return;
                }
                self.overlays[o].grab = Grab::Panel;
                let mut choose = None;
                if let Some(panel) = &mut self.overlays[o].panel {
                    match panel.item_at(pos).map(|it| (it.id, it.clickable() && it.enabled, it.look.clone())) {
                        Some(("format", _, Look::Choice { options, value, .. })) => {
                            // a format is chosen on the press
                            if let Some(c) = panel.choice_at(pos)
                                && let Some((v, _, available)) = options.get(c)
                                && *available
                                && *v != value
                            {
                                choose = Some(v.clone());
                            }
                        }
                        Some((id, true, _)) => panel.ptr.pressed = Some(id),
                        _ => {}
                    }
                }
                if let Some(f) = choose {
                    self.set_record_option(RecordOption::Format(f));
                }
            }
            Some(Float::Picker) => {
                if button != Button::Left {
                    return;
                }
                self.overlays[o].grab = Grab::Picker;
                let shift = mods & keyseq::SHIFT != 0;
                let ev = self.overlays[o].picker.as_mut().map(|p| p.press(pos, shift)).unwrap_or(PickerEvent::None);
                self.picker_event(o, ev);
            }
        }
    }

    fn release(&mut self, o: usize, button: Button, pos: Point) {
        let grab = self.overlays[o].grab;
        if button == Button::Left || grab == Grab::Canvas {
            self.overlays[o].grab = Grab::None;
        }
        match grab {
            Grab::None => {}
            Grab::Canvas => self.canvas_release(o, button, pos),
            Grab::Toolbar => {
                let mut clicked = None;
                if let Some(tb) = &mut self.overlays[o].toolbar {
                    tb.grab = None;
                    if let Some(id) = tb.ptr.pressed.take()
                        && tb.item_at(pos).is_some_and(|it| it.id == id && it.enabled)
                    {
                        clicked = Some(id);
                    }
                }
                if let Some(id) = clicked {
                    self.toolbar_click(o, id);
                }
            }
            Grab::Chip(i) => {
                let mut clicked = None;
                if let Some(chip) = self.overlays[o].chips.get_mut(i) {
                    chip.grab = None;
                    if let Some(id) = chip.ptr.pressed.take()
                        && chip.item_at(pos).is_some_and(|it| it.id == id)
                    {
                        clicked = Some(id);
                    }
                }
                match clicked {
                    Some("copy") => self.copy_code(o, i),
                    Some("open") => self.open_code(o, i),
                    Some("close") => self.dismiss_code(o, i),
                    _ => {}
                }
            }
            Grab::Panel => {
                let mut clicked = None;
                if let Some(panel) = &mut self.overlays[o].panel
                    && let Some(id) = panel.ptr.pressed.take()
                    && panel.item_at(pos).is_some_and(|it| it.id == id && it.enabled)
                {
                    clicked = Some(id);
                }
                if let Some(id) = clicked {
                    self.panel_click(o, id);
                }
            }
            Grab::Picker => {
                let ev = self.overlays.get_mut(o).and_then(|ov| ov.picker.as_mut()).map(|p| p.release(pos)).unwrap_or(PickerEvent::None);
                self.picker_event(o, ev);
            }
        }
        if !self.done && o < self.overlays.len() {
            self.update_float_hover(o);
        }
    }

    fn toolbar_click(&mut self, o: usize, id: &str) {
        match id {
            "color.8" => self.toggle_picker(o),
            "size" => self.cycle_size(),
            "codes" => self.toggle_codes(),
            "snap" => self.toggle_snap(),
            "pointer" => self.toggle_pointer(),
            "undo" => self.undo(),
            "redo" => self.redo(),
            "beautify" => self.toggle_beautify(),
            "pin" => self.toggle_pin(),
            "screen" => self.whole_screen(o),
            "close" => self.cancel(),
            _ => {
                if let Some(i) = id.strip_prefix("color.").and_then(|n| n.parse::<usize>().ok()) {
                    self.set_color(i.saturating_sub(1));
                } else if let Some(t) = Tool::from_id(id) {
                    self.set_tool(t);
                }
            }
        }
    }

    fn panel_click(&mut self, o: usize, id: &str) {
        let cfg = &self.st.cfg;
        match id {
            "record" => self.start_countdown(o),
            "mic" => self.set_record_option(RecordOption::Mic(!cfg.record_mic)),
            "system" => self.set_record_option(RecordOption::SystemAudio(!cfg.record_system_audio)),
            "cursor" => self.set_record_option(RecordOption::Cursor(!cfg.record_cursor)),
            "fps" => {
                let choices = config::RECORD_FPS;
                let next = choices.iter().position(|&c| c == cfg.record_fps).map(|i| choices[(i + 1) % choices.len()]).unwrap_or(30);
                self.set_record_option(RecordOption::Fps(next));
            }
            "close" => {
                self.cancel_gesture(o);
            }
            _ => {}
        }
    }

    // -- the pointer on the picture ------------------------------------------------------------------

    fn canvas_press(&mut self, o: usize, button: Button, pos: Point, mods: i32) {
        if button == Button::Right {
            if !(self.cancel_countdown() || self.close_picker() || self.cancel_gesture(o)) {
                self.cancel();
            }
            return;
        }
        if button != Button::Left {
            return;
        }
        if self.st.eyedropper {
            self.take_color(o, pos);
            return;
        }
        self.close_picker(); // a click elsewhere closes it, and does what it would anyway
        // In the text being typed: the caret goes there (Shift: selects to
        // there); a drag moves the text.
        if let Some(eo) = self.st.text_edit.as_ref().map(|(eo, _)| *eo).filter(|&eo| eo < self.overlays.len()) {
            let local = self.to_overlay(o, eo, pos);
            if let Some((_, shape)) = &mut self.st.text_edit {
                let size = shape.size;
                if let Some(t) = shape.as_text_mut()
                    && t.contains(local, size)
                {
                    let i = t.index_at(local, size);
                    t.edit.place(i, mods & keyseq::SHIFT != 0);
                    self.overlays[o].text_drag = Some((pos, t.pos));
                    return;
                }
            }
        }
        if self.commit_text() && self.st.tool == Tool::Text && self.text_at_any(o, pos).is_none() {
            return; // the first click just finishes the text being typed
        }
        let tool = self.st.tool;
        if tool == Tool::Record && self.st.countdown.is_some() {
            return;
        }
        let handle = if tool == Tool::Record { self.overlays[o].handle_at(pos) } else { None };
        if let Some(h) = handle
            && let Some(r) = self.overlays[o].rec_rect
        {
            self.overlays[o].rec_drag = Some((h, pos, r));
            self.refresh();
        } else if matches!(tool, Tool::Region | Tool::Record) {
            self.disarm();
            let pos = self.snapped(o, pos, mods);
            self.overlays[o].sel_origin = Some(crate::ui::shapes::Drag::new(pos, false));
            self.overlays[o].sel_rect = Some(Rect::new(pos.x, pos.y, 0.0, 0.0));
            self.refresh();
        } else if tool == Tool::Text {
            // (a text reaching here from another monitor is edited there)
            let (k, shape) = match self.text_at_any(o, pos) {
                Some((k, i, local)) => {
                    // edit it again (a copy, so undo brings back the original)
                    let Some(mut s) = copy_for_editing(&self.overlays[k].annotations[i], i) else { return };
                    let size = s.size;
                    if let Some(t) = s.as_text_mut() {
                        let at = t.index_at(local, size);
                        t.edit.place(at, false);
                        self.overlays[o].text_drag = Some((pos, t.pos));
                    }
                    (k, s)
                }
                None => (o, Shape::text(pos, self.st.color(), self.st.size)),
            };
            self.begin_text(k, shape);
        } else if tool == Tool::Counter {
            let n = self.next_number();
            self.commit_shape(o, Shape::counter(pos, self.st.color(), self.st.size, n));
        } else if tool == Tool::Select {
            // (a drawing reaching here from another monitor is picked up there)
            let k = self.select_owner(o, pos);
            let local = self.to_overlay(o, k, pos);
            self.select_on(Some(k));
            self.drag_owner = Some(k);
            let ov = &mut self.overlays[k];
            ov.selection.press(&mut ov.annotations, local, mods, 1.0);
            self.refresh();
        } else {
            self.overlays[o].active = Shape::create(tool, pos, self.st.color(), self.st.size);
        }
    }

    fn canvas_motion(&mut self, o: usize, pos: Point, mods: i32) {
        self.overlays[o].cursor_pos = Some(pos);
        self.activate(o);
        if let Some(k) = self.drag_owner.filter(|&k| k < self.overlays.len() && self.overlays[k].selection.dragging()) {
            let local = self.to_overlay(o, k, pos);
            let ov = &mut self.overlays[k];
            ov.selection.motion(&mut ov.annotations, local, mods);
            return;
        }
        let selecting = matches!(self.st.tool, Tool::Region | Tool::Record) && self.overlays[o].active.is_none();
        // Where the selection would start or end (the crosshair shows it).
        let snap = if selecting && self.st.snap_edges { Some(self.snapped(o, pos, mods)) } else { None };
        self.overlays[o].snap_point = snap;
        let bounds = self.drag_bounds(o);
        // (the text being typed can be another monitor's, reaching here)
        let text_at = self.st.text_edit.as_ref().filter(|(eo, _)| *eo < self.overlays.len()).map(|(eo, _)| self.to_overlay(o, *eo, pos));
        let ov = &mut self.overlays[o];
        if let Some(d) = &mut ov.sel_origin {
            d.extend(snap.unwrap_or(pos), Mods::from_keys(mods));
            ov.sel_rect = Some(clip_selection(d.rect(), bounds));
            ov.hover_window = None;
            if self.st.spans {
                self.selection_moved(o, Some(pos));
            }
        } else if ov.rec_drag.is_some() {
            self.drag_area(o, pos, snap, mods & keyseq::CTRL != 0);
        } else if ov.text_drag.is_some()
            && let Some(local) = text_at
        {
            // Dragging in the text selects (only the select tool moves text).
            if let Some((_, shape)) = &mut self.st.text_edit {
                let size = shape.size;
                if let Some(t) = shape.as_text_mut() {
                    let i = t.index_at(local, size);
                    t.edit.place(i, true);
                }
            }
        } else if let Some(active) = &mut ov.active {
            active.extend(pos, Mods::from_keys(mods));
        } else {
            self.update_hover(o);
        }
    }

    fn canvas_release(&mut self, o: usize, button: Button, pos: Point) {
        if button != Button::Left {
            return;
        }
        if self.overlays[o].text_drag.take().is_some() {
            return;
        }
        if let Some(k) = self.drag_owner.take().filter(|&k| k < self.overlays.len() && self.overlays[k].selection.dragging()) {
            let len = self.overlays[k].annotations.len();
            let ov = &mut self.overlays[k];
            ov.selection.release(&mut ov.annotations);
            return self.selection_changed(k, len);
        }
        if self.overlays[o].sel_origin.is_some() {
            let mut rect = self.overlays[o].sel_rect.take();
            self.overlays[o].sel_origin = None;
            let mut window = None;
            if rect.is_none_or(|r| r.w < 3.0 || r.h < 3.0) {
                // A click captures the window under the pointer, else the screen.
                self.overlays[o].cursor_pos = Some(pos);
                self.update_hover(o);
                match self.overlays[o].hover_window {
                    Some((r, w)) => {
                        rect = Some(r);
                        window = Some(w);
                    }
                    None => rect = None,
                }
            }
            let g = self.overlays[o].output.rect;
            let spans = self.st.spans;
            if self.st.tool == Tool::Record {
                if let Some(w) = window
                    && spans
                    && let Some(win) = self.st.windows.get(w)
                {
                    rect = Some(win.rect.translated(-g.x, -g.y).to_f()); // all of a window that crosses monitors
                }
                let r = rect.unwrap_or(self.overlays[o].rect());
                self.arm(o, r, window);
            } else if let Some(w) = window
                && spans
                && let Some(win) = self.st.windows.get(w)
                && !g.contains_rect(&win.rect)
            {
                let r = win.rect;
                self.capture_span(r, Some(w)); // all of a window that crosses monitors
            } else if let Some(r) = rect
                && spans
                && !self.overlays[o].rect().contains_rect(&r)
            {
                self.capture_span(r.translated(g.top_left().to_f()).aligned(), None);
            } else {
                self.capture(o, rect, window);
            }
        } else if self.overlays[o].rec_drag.is_some() {
            self.overlays[o].rec_drag = None;
            if self.overlays[o].rec_rect.is_some_and(|r| r.w < 8.0 || r.h < 8.0) {
                self.overlays[o].rec_rect = None;
            }
            self.rehome_area(o);
            self.refresh();
        } else if let Some(shape) = self.overlays[o].active.take()
            && shape.is_valid()
        {
            self.commit_shape(o, shape);
        }
    }
}

impl Drop for Session {
    fn drop(&mut self) {
        self.cancelled.store(true, Ordering::Relaxed);
    }
}

/// The toolbar's view of the state.
fn toolbar_view(st: &State, codes: CodeScan, can_undo: bool, can_redo: bool) -> View<'_> {
    View {
        tool: st.tool,
        can_record: st.can_record,
        color_index: st.color_index,
        custom_color: st.custom_color,
        size: st.size,
        codes_visible: st.codes_visible,
        codes,
        snap: st.snap_edges,
        pointer_available: st.pointer_available,
        show_pointer: st.show_pointer,
        can_undo,
        can_redo,
        pin_mode: st.pin_mode,
        beautify: st.cfg.beautify,
        keymap: &st.keymap,
    }
}

fn handle_cursor(h: Handle) -> CursorShape {
    match h {
        Handle::Move => CursorShape::Move,
        Handle::Edges { left, right, top, bottom } => match ((left || right), (top || bottom)) {
            (true, true) => {
                if (left && top) || (right && bottom) {
                    CursorShape::ResizeNWSE
                } else {
                    CursorShape::ResizeNESW
                }
            }
            (true, false) => CursorShape::ResizeEW,
            _ => CursorShape::ResizeNS,
        },
    }
}

fn is_modifier(k: i32) -> bool {
    matches!(k, key::SHIFT | key::CONTROL | key::ALT | key::ALT_GR | key::META)
}

/// The pixel of `img` under `pos` (logical, at `dpr`), clamped inside it.
fn pixel_at(img: &Image, pos: Point, dpr: f32) -> u32 {
    let x = ((pos.x * dpr) as i64).clamp(0, img.width() as i64 - 1);
    let y = ((pos.y * dpr) as i64).clamp(0, img.height() as i64 - 1);
    img.pixel(x as u32, y as u32)
}

fn poly_bounds(poly: &[Point]) -> Rect {
    let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in poly {
        l = l.min(p.x);
        t = t.min(p.y);
        r = r.max(p.x);
        b = b.max(p.y);
    }
    if poly.is_empty() { Rect::default() } else { Rect::from_ltrb(l, t, r, b) }
}

/// A selection kept within `bounds`; one with no width or height yet stays
/// where it is (rather than jumping to the corner, as an empty overlap would).
fn clip_selection(r: Rect, bounds: Rect) -> Rect {
    if r.w <= 0.0 || r.h <= 0.0 {
        let x = r.x.clamp(bounds.left(), bounds.right());
        let y = r.y.clamp(bounds.top(), bounds.bottom());
        let right = (r.x + r.w).clamp(bounds.left(), bounds.right());
        let bottom = (r.y + r.h).clamp(bounds.top(), bounds.bottom());
        return Rect::new(x, y, right - x, bottom - y);
    }
    r.intersected(&bounds)
}

/// The window the capture is of, as `Window`.
pub fn window(rect: IRect, title: &str, app: &str) -> Window {
    Window { rect, title: title.to_string(), app: app.to_string() }
}
