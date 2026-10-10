//! One fullscreen overlay per monitor, showing the frozen capture with the
//! drawings, the shading, the selection, the crosshair and magnifier and
//! the floating panels.
//!
//! Painting is incremental: each change works out which device pixels it
//! affects (a "scene" of elements, each with its bounds and a key for how
//! it looks, compared with the last one), and only those are repainted.
//! The picture underneath is copied from the capture with the shading
//! applied in the same pass.

use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};
use std::sync::Arc;

use crate::geom::{Damage, IPoint, IRect, Point, Rect};
use crate::image::{self, Canvas, Image};
use crate::render::Painter;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, BOLD, DEMIBOLD, MEDIUM};
use crate::render::theme::{self, Color};
use crate::ui::panels::{CodeChip, ColorPicker, RecordPanel};
use crate::ui::selection::Selection;
use crate::ui::shapes::{Shape, Tool};
use crate::ui::snapping::{EdgeMaps, py_round};
use crate::ui::state::State;
use crate::ui::toolbar::Toolbar;
use crate::util;

pub const HANDLE: f32 = 10.0;
/// The hint fades while the pointer is this close to it (logical px).
pub const HINT_NEAR: f32 = 60.0;
pub const RAINBOW_SECONDS: f32 = 4.0;
pub const LOUPE_ZOOM: (f32, f32) = (3.0, 40.0);
/// The thin dark edge of the magnifier's square and crosshair.
const LOUPE_EDGE: Color = Color::rgba(0, 0, 0, 120);

/// A monitor: its connector name, where it is (global logical pixels).
#[derive(Clone, Debug, PartialEq)]
pub struct Output {
    pub name: String,
    pub rect: IRect,
    pub scale: f32,
}

/// A window the compositor told us about (global logical pixels).
#[derive(Clone, Debug, PartialEq)]
pub struct Window {
    pub rect: IRect,
    pub title: String,
    pub app: String,
}

/// A code found on screen; corners in the pixels of the picture scanned.
#[derive(Clone, Debug, PartialEq)]
pub struct Code {
    pub text: String,
    pub kind: String,
    pub corners: Vec<Point>,
}

/// Payloads worth handing to the default handler with "Open".
pub const LINK_PREFIXES: &[&str] = &["http://", "https://", "mailto:", "tel:", "sms:", "geo:", "matrix:"];

impl Code {
    pub fn is_link(&self) -> bool {
        let t = self.text.trim().to_lowercase();
        LINK_PREFIXES.iter().any(|p| t.starts_with(p)) && !self.text.trim().chars().any(|c| c.is_whitespace() || c.is_control())
    }
}

/// A resize handle of the area chosen for recording, or the inside of it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub enum Handle {
    Edges { left: bool, right: bool, top: bool, bottom: bool },
    Move,
}

/// What the pointer is holding down on.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Grab {
    None,
    Canvas,
    Toolbar,
    Chip(usize),
    Panel,
    Picker,
}

pub struct Overlay {
    pub index: usize,
    pub output: Output,
    /// The picture shown (with or without the pointer).
    pub base: Arc<Image>,
    pub plain: Option<Arc<Image>>,
    pub with_pointer: Option<Arc<Image>>,
    /// The mouse pointer as captured: where its pixels go in the picture,
    /// and those pixels (only these are kept, not a second picture).
    pub pointer_patch: Option<(IPoint, Arc<Image>)>,
    /// picture pixels per logical pixel
    pub dpr: f32,
    /// top left of this screen in the whole desktop's picture, its pixels
    pub origin: IPoint,
    /// the whole desktop picture's pixels per logical pixel
    pub desktop_scale: Option<f32>,
    pub annotations: Vec<Shape>,
    pub undone: Vec<Shape>,
    pub active: Option<Shape>,
    /// the select tool's drawing, on this monitor
    pub selection: Selection,
    pub sel_origin: Option<Point>,
    pub sel_rect: Option<Rect>,
    pub cursor_pos: Option<Point>,
    /// (index in the session's codes, the code's corners here, logical)
    pub codes: Vec<(usize, Vec<Point>)>,
    /// (rect here, index in the session's windows), topmost first
    pub windows: Vec<(Rect, usize)>,
    pub hover_window: Option<(Rect, usize)>,
    pub chips: Vec<CodeChip>,
    pub code_hover: Option<usize>,
    pub toolbar: Option<Toolbar>,
    pub toolbar_visible: bool,
    pub rec_rect: Option<Rect>,
    pub rec_window: Option<usize>,
    pub rec_drag: Option<(Handle, Point, Rect)>,
    pub panel: Option<RecordPanel>,
    pub picker: Option<ColorPicker>,
    pub picker_visible: bool,
    pub span_rect: Option<Rect>,
    pub span_pointer: Option<Point>,
    pub span_hover: Option<(Rect, usize)>,
    pub edge_maps: Option<Arc<EdgeMaps>>,
    pub edges_requested: bool,
    pub snap_point: Option<Point>,
    pub text_drag: Option<(Point, Point)>,
    pub grab: Grab,
    pub pointer_touch: bool,
    last: Option<Scene>,
    pub damage: Damage,
    /// The device rectangle the magnifier was last drawn in.
    pub loupe_box: IRect,
}

impl Overlay {
    pub fn new(index: usize, output: Output, base: Arc<Image>, origin: IPoint) -> Overlay {
        let dpr = base.width() as f32 / output.rect.w.max(1) as f32;
        Overlay {
            index,
            output,
            base,
            plain: None,
            with_pointer: None,
            pointer_patch: None,
            dpr,
            origin,
            desktop_scale: None,
            annotations: Vec::new(),
            undone: Vec::new(),
            active: None,
            selection: Selection::default(),
            sel_origin: None,
            sel_rect: None,
            cursor_pos: None,
            codes: Vec::new(),
            windows: Vec::new(),
            hover_window: None,
            chips: Vec::new(),
            code_hover: None,
            toolbar: None,
            toolbar_visible: false,
            rec_rect: None,
            rec_window: None,
            rec_drag: None,
            panel: None,
            picker: None,
            picker_visible: false,
            span_rect: None,
            span_pointer: None,
            span_hover: None,
            edge_maps: None,
            edges_requested: false,
            snap_point: None,
            text_drag: None,
            grab: Grab::None,
            pointer_touch: false,
            last: None,
            damage: Damage::full(),
            loupe_box: IRect::default(),
        }
    }

    /// For a picture shown on a screen it doesn't fill (an image opened
    /// for annotating): scaled to fit, at `dpr`.
    pub fn with_dpr(mut self, dpr: f32) -> Overlay {
        self.dpr = dpr;
        self
    }

    /// Logical size: the monitor's.
    pub fn width(&self) -> f32 {
        self.output.rect.w as f32
    }

    pub fn height(&self) -> f32 {
        self.output.rect.h as f32
    }

    pub fn rect(&self) -> Rect {
        Rect::new(0.0, 0.0, self.width(), self.height())
    }

    /// The buffer's size in device pixels.
    pub fn device_size(&self) -> (u32, u32) {
        ((self.width() * self.dpr).round().max(1.0) as u32, (self.height() * self.dpr).round().max(1.0) as u32)
    }

    pub fn device_rect(&self) -> IRect {
        let (w, h) = self.device_size();
        IRect::new(0, 0, w as i32, h as i32)
    }

    /// A logical rectangle as the device pixels it touches (plus `pad`).
    pub fn dev(&self, r: Rect, pad: i32) -> IRect {
        r.scaled(self.dpr).aligned().adjusted(-pad, -pad, pad, pad).intersected(&self.device_rect())
    }

    pub fn picker_open(&self) -> bool {
        self.picker.is_some() && self.picker_visible
    }

    /// The text drawn at `at` (topmost), if any.
    pub fn text_at(&self, at: Point) -> Option<usize> {
        self.annotations.iter().enumerate().rev().find_map(|(i, s)| {
            let t = s.as_text()?;
            (!s.hidden && t.contains(at, s.size)).then_some(i)
        })
    }

    pub fn commit(&mut self, shape: Shape) {
        self.annotations.push(shape);
        self.undone.clear();
    }

    pub fn undo(&mut self) -> bool {
        let Some(shape) = self.annotations.pop() else { return false };
        if let Some(orig) = shape.replaces.and_then(|i| self.annotations.get_mut(i)) {
            orig.hidden = false; // the drawing as it was before it was changed
        }
        self.undone.push(shape);
        true
    }

    pub fn redo(&mut self) -> bool {
        let Some(shape) = self.undone.pop() else { return false };
        if let Some(orig) = shape.replaces.and_then(|i| self.annotations.get_mut(i)) {
            orig.hidden = true;
        }
        self.annotations.push(shape);
        true
    }

    /// Which handle of the recording area is at `at`, or Move inside it.
    pub fn handle_at(&self, at: Point) -> Option<Handle> {
        let r = self.rec_rect?;
        let xs = [(Some(false), r.left()), (None, r.center().x), (Some(true), r.right())];
        let ys = [(Some(false), r.top()), (None, r.center().y), (Some(true), r.bottom())];
        let grab = HANDLE / 2.0 + 4.0;
        for (yk, y) in ys {
            for (xk, x) in xs {
                if (xk, yk) != (None, None) && (at.x - x).abs() <= grab && (at.y - y).abs() <= grab {
                    return Some(Handle::Edges { left: xk == Some(false), right: xk == Some(true), top: yk == Some(false), bottom: yk == Some(true) });
                }
            }
        }
        // Nothing to move when it's the whole screen: a drag picks a new area.
        (r.contains(at) && r != self.rect()).then_some(Handle::Move)
    }

    /// The capture plus drawings, cropped to `rect` (logical; None: all).
    pub fn render(&self, rect: Option<Rect>) -> Result<Image, String> {
        let phys = self.render_rect(rect);
        let mut out = self.base.crop(phys)?;
        self.draw_annotations(&mut out, phys);
        Ok(out)
    }

    /// The picture's pixels for `rect` (logical; None: all of it).
    fn render_rect(&self, rect: Option<Rect>) -> IRect {
        let dpr = self.dpr;
        match rect {
            None => self.base.rect(),
            Some(r) => IRect::new(py_round(r.x * dpr), py_round(r.y * dpr), py_round(r.w * dpr), py_round(r.h * dpr)).intersected(&self.base.rect()),
        }
    }

    fn draw_annotations(&self, out: &mut Image, phys: IRect) {
        if self.annotations.iter().all(|s| s.hidden) {
            return; // (the capture is opaque as it is)
        }
        {
            let mut p = Painter::for_region(out.pixmap_mut(), self.dpr, (phys.x, phys.y));
            for shape in &self.annotations {
                if !shape.hidden {
                    shape.paint(&mut p, &self.base, self.dpr);
                }
            }
        }
        out.force_opaque();
    }

    /// As `render`, giving up the overlay: the whole picture with nothing
    /// drawn on it is handed over as it is, without a copy.
    pub fn into_render(mut self, rect: Option<Rect>) -> Result<Image, String> {
        let phys = self.render_rect(rect);
        let whole = phys == self.base.rect();
        if whole && self.annotations.iter().all(|s| s.hidden) {
            self.plain = None;
            self.with_pointer = None;
            self.edge_maps = None;
            let base = std::mem::replace(&mut self.base, Arc::new(Image::new(1, 1)?));
            return Ok(Arc::try_unwrap(base).unwrap_or_else(|shared| (*shared).clone()));
        }
        self.render(rect)
    }

    /// Show the picture with the mouse pointer in it, or without. The one
    /// with it is made when first wanted, and let go when hidden again.
    pub fn show_pointer(&mut self, show: bool) {
        let plain = self.plain.get_or_insert_with(|| self.base.clone()).clone();
        if show {
            if self.with_pointer.is_none()
                && let Some((at, patch)) = &self.pointer_patch
            {
                let mut img = (*plain).clone();
                img.blit(patch, at.x, at.y);
                self.with_pointer = Some(Arc::new(img));
            }
            if let Some(p) = &self.with_pointer {
                self.base = p.clone();
            }
        } else {
            self.base = plain;
            self.with_pointer = None;
        }
        self.damage.set_full();
    }

    /// The code under the pointer (its box or its card) goes on top.
    pub fn update_code_hover(&mut self, codes_shown: bool) {
        let mut hovered = None;
        if let Some(pt) = self.cursor_pos
            && codes_shown
            && !self.chips.is_empty()
        {
            hovered = (0..self.chips.len()).rev().find(|&i| self.chips[i].rect().contains(pt));
            if hovered.is_none() {
                hovered = self.codes.iter().position(|(_, poly)| code_box(poly).contains(pt));
            }
        }
        self.code_hover = hovered;
    }

    /// Is the pointer over (within `margin` of) a floating panel?
    pub fn over_floating(&self, margin: f32, codes_shown: bool) -> bool {
        let Some(pt) = self.cursor_pos else { return false };
        let near = |r: Rect| r.adjusted(-margin, -margin, margin, margin).contains(pt);
        if self.toolbar_visible && self.toolbar.as_ref().is_some_and(|t| near(t.rect())) {
            return true;
        }
        if self.panel.as_ref().is_some_and(|p| near(p.rect())) && self.rec_rect.is_some() {
            return true;
        }
        if self.picker_open() && self.picker.as_ref().is_some_and(|p| near(p.rect())) {
            return true;
        }
        codes_shown && self.chips.iter().any(|c| near(c.rect()))
    }

    // -- painting ---------------------------------------------------------------------

    /// Work out what changed since the last frame and add it to `damage`.
    pub fn update_damage(&mut self, st: &State) {
        let scene = self.scene(st);
        match &self.last {
            None => self.damage.set_full(),
            Some(old) => {
                if old.bg_dim != scene.bg_dim {
                    self.damage.set_full();
                } else if old.hole != scene.hole && scene.bg_dim.is_some() {
                    match (old.hole, scene.hole) {
                        (Some(a), Some(b)) => {
                            for r in IRect::xor(&a, &b) {
                                self.damage.add(r);
                            }
                        }
                        (Some(a), None) | (None, Some(a)) => self.damage.add(a),
                        (None, None) => {}
                    }
                }
                for e in &scene.elements {
                    match old.elements.iter().find(|o| o.id == e.id) {
                        Some(o) if o.key == e.key && o.bounds == e.bounds => {}
                        Some(o) => {
                            self.damage.add(o.bounds);
                            self.damage.add(e.bounds);
                        }
                        None => self.damage.add(e.bounds),
                    }
                }
                for o in &old.elements {
                    if !scene.elements.iter().any(|e| e.id == o.id) {
                        self.damage.add(o.bounds);
                    }
                }
            }
        }
        self.last = Some(scene);
    }

    /// Forget what's on screen: the next frame is drawn in full.
    pub fn invalidate(&mut self) {
        self.last = None;
        self.damage.set_full();
    }

    fn scene(&self, st: &State) -> Scene {
        let mut elements = Vec::new();
        let region = matches!(st.tool, Tool::Region | Tool::Record);
        let selection = self.sel_rect.or(self.span_rect);
        let hover = self.hover_window.or(if st.spans { self.span_hover } else { None });
        let bg_dim = (region && st.dim.a > 0).then_some(st.dim);
        let hole = if bg_dim.is_some() {
            let focus = selection.or(self.rec_rect).or(hover.map(|h| h.0));
            focus.map(|f| self.hole_rect(f)).filter(|h| !h.is_empty())
        } else {
            None
        };
        for (i, s) in self.annotations.iter().enumerate() {
            elements.push(Element { id: Id::Annotation(i), bounds: self.dev(s.bounds(), 2), key: shape_key(s, i) });
        }
        if let Some(s) = &self.active {
            elements.push(Element { id: Id::Active, bounds: self.dev(s.bounds(), 2), key: shape_key(s, usize::MAX) });
        }
        if let Some(s) = self.selection.moving() {
            elements.push(Element { id: Id::Moving, bounds: self.dev(s.bounds(), 2), key: shape_key(s, usize::MAX - 2) });
        }
        if st.tool == Tool::Select
            && let Some(r) = self.selection.frame_bounds(&self.annotations, 1.0)
        {
            let key = hash(&(rect_key(r), self.selection.current(&self.annotations), st.theme_key));
            elements.push(Element { id: Id::SelectionFrame, bounds: self.dev(r, 2), key });
        }
        if let Some((o, s)) = &st.text_edit
            && *o == self.index
        {
            elements.push(Element { id: Id::TextEdit, bounds: self.dev(s.bounds(), 2), key: shape_key(s, usize::MAX - 1) });
        }
        let picking = region && self.rec_rect.is_none();
        if region {
            if selection.is_none()
                && let Some((r, w)) = hover
            {
                let label = self.window_label(st, r, w);
                let bounds = self.dev(r, 2).united(&self.dev(pill_rect(&label, Point::new(r.x + 8.0, r.y + 8.0), PillStyle::small()), 2));
                elements.push(Element { id: Id::Window, bounds, key: hash(&(rect_key(r), &label)) });
            }
            if self.sel_rect.is_none() && st.codes_visible && st.tool == Tool::Region && !self.codes.is_empty() {
                let mut b = IRect::default();
                for (_, poly) in &self.codes {
                    b = b.united(&self.dev(code_box(poly), 3));
                }
                elements.push(Element { id: Id::Codes, bounds: b, key: hash(&(self.codes.len(), self.code_hover, st.theme_key)) });
            }
        }
        if let Some(sel) = selection {
            let label = self.sel_rect.map(|r| self.selection_label(st, r));
            let mut b = self.dev(sel.adjusted(-2.0, -2.0, 2.0, 2.0), 2);
            if let Some(label) = &label {
                b = b.united(&self.dev(self.selection_pill_rect(sel, label), 2));
            }
            elements.push(Element { id: Id::Selection, bounds: b, key: hash(&(rect_key(sel), &label, st.theme_key)) });
        } else if let Some(r) = self.rec_rect {
            elements.push(Element { id: Id::Selection, bounds: self.dev(r.adjusted(-2.0, -2.0, 2.0, 2.0), 2), key: hash(&(rect_key(r), st.theme_key)) });
            if let Some(n) = st.countdown {
                let size = 56f32.max(140f32.min(r.w * 0.6).min(r.h * 0.6));
                let bx = Rect::new(r.center().x - size / 2.0, r.center().y - size / 2.0, size, size);
                elements.push(Element { id: Id::Countdown, bounds: self.dev(bx.adjusted(-9.0, -9.0, 9.0, 9.0), 2), key: hash(&n) });
            } else if self.rec_drag.is_none() {
                elements.push(Element {
                    id: Id::Handles,
                    bounds: self.dev(r.adjusted(-HANDLE, -HANDLE, HANDLE, HANDLE), 2),
                    key: hash(&(rect_key(r), st.theme_key)),
                });
            }
        } else if picking
            && st.cfg.show_crosshair
            && let Some(pos) = self.cursor_pos
        {
            let at = self.snap_point.unwrap_or(pos);
            let (x, y) = (py_round(at.x) as f32 + 0.5, py_round(at.y) as f32 + 0.5);
            let color = self.mark_color(st);
            elements.push(Element { id: Id::CrossV, bounds: self.dev(Rect::new(x - 1.0, 0.0, 2.0, self.height()), 1), key: hash(&(color, (x * 2.0) as i32)) });
            elements.push(Element { id: Id::CrossH, bounds: self.dev(Rect::new(0.0, y - 1.0, self.width(), 2.0), 1), key: hash(&(color, (y * 2.0) as i32)) });
        }
        if let Some(at) = self.loupe_at(st) {
            let (bounds, key) = self.loupe_bounds_key(st, at);
            elements.push(Element { id: Id::Loupe, bounds, key });
        }
        if self.sel_rect.is_none() && st.pointer_overlay == Some(self.index) && st.cfg.show_hint {
            let (text, at, opacity) = self.hint(st);
            let r = pill_rect(&text, at, PillStyle::default());
            elements.push(Element { id: Id::Hint, bounds: self.dev(r, 2), key: hash(&(&text, (opacity * 10.0) as i32, st.theme_key)) });
        }
        let codes_shown = self.chips_shown(st);
        if codes_shown {
            for (i, chip) in self.chips.iter().enumerate() {
                elements.push(Element {
                    id: Id::Chip(i),
                    bounds: self.dev(chip.rect(), 2),
                    key: hash(&(rect_key(chip.rect()), &chip.ptr, self.code_hover == Some(i), st.theme_key)),
                });
            }
        }
        if self.toolbar_visible
            && let Some(t) = &self.toolbar
        {
            let key = hash(&(rect_key(t.rect()), &t.ptr, toolbar_key(t), st.theme_key));
            elements.push(Element { id: Id::Toolbar, bounds: self.dev(t.rect(), 2), key });
        }
        if self.rec_rect.is_some()
            && self.rec_drag.is_none()
            && st.countdown.is_none()
            && let Some(panel) = &self.panel
        {
            let key = hash(&(rect_key(panel.rect()), &panel.ptr, panel_key(panel), st.theme_key));
            elements.push(Element { id: Id::Panel, bounds: self.dev(panel.rect(), 2), key });
        }
        if self.picker_open()
            && let Some(picker) = &self.picker
        {
            let key = hash(&(
                rect_key(picker.rect()),
                (picker.h * 1e4) as i32,
                (picker.s * 1e4) as i32,
                (picker.v * 1e4) as i32,
                &picker.hex.edit.text,
                picker.hex.edit.cursor,
                picker.hex.edit.anchor,
                picker.hex.focused,
                &picker.ptr,
                st.theme_key,
            ));
            elements.push(Element { id: Id::Picker, bounds: self.dev(picker.rect(), 2), key });
        }
        Scene { bg_dim, hole, elements }
    }

    fn hole_rect(&self, focus: Rect) -> IRect {
        let f = focus.intersected(&self.rect());
        if f.is_empty() {
            return IRect::default();
        }
        let d = self.dpr;
        IRect::from_ltrb((f.x * d).round() as i32, (f.y * d).round() as i32, (f.right() * d).round() as i32, (f.bottom() * d).round() as i32)
    }

    pub fn chips_shown(&self, st: &State) -> bool {
        st.tool == Tool::Region && self.sel_rect.is_none() && st.codes_visible
    }

    /// Where the magnifier is drawn, if it is.
    pub fn loupe_at(&self, st: &State) -> Option<Point> {
        let picking = matches!(st.tool, Tool::Region | Tool::Record) && self.rec_rect.is_none();
        if self.span_rect.is_some()
            && let Some(sp) = self.span_pointer
            && st.cfg.show_loupe
        {
            return Some(sp); // a drag from another monitor is here now
        }
        let pos = self.cursor_pos?;
        let allowed = !self.over_floating(24.0, self.chips_shown(st));
        ((picking && st.cfg.show_loupe || st.eyedropper) && allowed && self.rect().contains(pos) && pos.x < self.width() && pos.y < self.height())
            .then_some(pos)
    }

    fn mark_color(&self, st: &State) -> Color {
        if st.cfg.rainbow { Color::from_hsv((st.rainbow_t / RAINBOW_SECONDS).rem_euclid(1.0), 0.75, 1.0) } else { theme::c().accent }
    }

    fn window_label(&self, st: &State, r: Rect, w: usize) -> String {
        let size = format!("{} × {}", py_round(r.w * self.dpr), py_round(r.h * self.dpr));
        let title = st.windows.get(w).map(|w| util::displayable(&w.title)).unwrap_or_default();
        let title = util::elide(&title, 40);
        if title.is_empty() { size } else { format!("{title}  ·  {size}") }
    }

    fn selection_label(&self, st: &State, r: Rect) -> String {
        let label = format!("{} × {}", py_round(r.w * self.dpr), py_round(r.h * self.dpr));
        if st.tool == Tool::Record {
            format!("Record  ·  {label}")
        } else if st.pin_mode {
            format!("Pin  ·  {label}")
        } else {
            label
        }
    }

    fn selection_pill_rect(&self, sel: Rect, label: &str) -> Rect {
        let here = sel.intersected(&self.rect());
        let anchor = if here.is_empty() { sel } else { here };
        pill_rect(
            label,
            Point::new(anchor.left(), anchor.top() - 10.0),
            PillStyle { anchor_bottom: true, keep_inside: true, accent: true, ..Default::default() },
        )
    }

    /// The hint text, where it goes, and how opaque it is.
    pub fn hint(&self, st: &State) -> (String, Point, f32) {
        let text = hint_text(st, self);
        let w = text::width(&text, text::font(12.0, MEDIUM)) + 28.0;
        let at = Point::new((self.width() - w) / 2.0, (self.height() - 28.0) / 2.0);
        let near = self.cursor_pos.is_some_and(|c| Rect::new(at.x, at.y, w, 28.0).adjusted(-HINT_NEAR, -HINT_NEAR, HINT_NEAR, HINT_NEAR).contains(c));
        (text, at, if near { 0.3 } else { 1.0 })
    }

    fn loupe_geometry(&self, st: &State, pos: Point) -> Loupe {
        let size = st.cfg.loupe_size as f32;
        let cap = self.dpr;
        let dev = cap;
        let px = (pos.x * cap).floor() as i64;
        let py = (pos.y * cap).floor() as i64;
        let x = if pos.x + 24.0 + size < self.width() { pos.x + 24.0 } else { pos.x - 24.0 - size };
        let y = if pos.y + 24.0 + size + 34.0 < self.height() { pos.y + 24.0 } else { pos.y - 24.0 - size - 34.0 };
        let u = (py_round(dev) as i64).max(1);
        let bx = py_round(x * dev) as i64;
        let by = py_round(y * dev) as i64;
        let side = py_round(size * dev) as i64;
        let cell = (4 * u).max(2 * py_round(st.loupe_zoom * dev / 2.0) as i64);
        Loupe { size, px, py, x, y, u, bx, by, side, cell }
    }

    fn loupe_bounds_key(&self, st: &State, pos: Point) -> (IRect, u64) {
        let l = self.loupe_geometry(st, pos);
        let bx = IRect::new(l.bx as i32, l.by as i32, l.side as i32, l.side as i32).adjusted(-2, -2, 2, 2);
        let label = loupe_label(&self.base, l.px, l.py);
        let pill = self.dev(pill_rect(&label, Point::new(l.x, l.y + l.size + 6.0), PillStyle { mono: true, ..Default::default() }), 2);
        let snap = self.snap_point.map(|s| ((s.x * 64.0) as i32, (s.y * 64.0) as i32));
        let key = hash(&(
            l.px,
            l.py,
            l.cell,
            l.bx,
            l.by,
            snap,
            ((pos.x * 64.0) as i32, (pos.y * 64.0) as i32),
            self.mark_color(st),
            st.tool == Tool::Region || st.tool == Tool::Record,
            st.theme_key,
            Arc::as_ptr(&self.base) as usize,
        ));
        (bx.united(&pill).intersected(&self.device_rect()), key)
    }

    /// Paint the device rectangle `region` of the overlay into `target`,
    /// whose top left is at `origin` in device pixels (a damaged part drawn
    /// on its own, or the whole buffer).
    pub fn paint(&self, st: &State, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let c = theme::c();
        let region_local = region.translated(-origin.0, -origin.1);
        let region_rect = region.to_f();
        let region_logical = Rect::new(region_rect.x / self.dpr, region_rect.y / self.dpr, region_rect.w / self.dpr, region_rect.h / self.dpr);
        let rtool = matches!(st.tool, Tool::Region | Tool::Record);
        let selection = self.sel_rect.or(self.span_rect);
        let hover = self.hover_window.or(if st.spans { self.span_hover } else { None });
        let dim = (rtool && st.dim.a > 0).then_some(st.dim);
        let hole = dim.and_then(|_| selection.or(self.rec_rect).or(hover.map(|h| h.0)).map(|f| self.hole_rect(f)));
        let drawn: Vec<&Shape> = self
            .annotations
            .iter()
            .filter(|s| !s.hidden)
            .chain(self.active.iter())
            .chain(self.selection.moving())
            .chain(st.text_edit.iter().filter(|(o, _)| *o == self.index).map(|(_, s)| s))
            .filter(|s| self.dev(s.bounds(), 2).intersects(&region))
            .collect();
        // 1. The picture, shaded outside the focus unless drawings come first.
        let shade_now = if drawn.is_empty() { dim } else { None };
        copy_background_parallel(target, region_local, &self.base, region, shade_now, hole);
        if !drawn.is_empty() {
            let mut p = Painter::for_region(target.pixmap_mut(), self.dpr, origin);
            for s in &drawn {
                s.paint(&mut p, &self.base, self.dpr);
            }
            drop(p);
            if let Some(d) = dim {
                shade(target, region_local, region, d, hole);
            }
        }
        let mut p = Painter::for_region(target.pixmap_mut(), self.dpr, origin);
        if st.tool == Tool::Select {
            self.selection.paint_frame(&self.annotations, &mut p, 1.0);
        }
        let picking = rtool && self.rec_rect.is_none();
        if rtool {
            if selection.is_none()
                && let Some((r, w)) = hover
                && r.intersects(&region_logical.adjusted(-200.0, -40.0, 200.0, 40.0))
            {
                let edge = c.text.with_alpha(110);
                p.stroke_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), Pen::new(edge, 1.0));
                let label = self.window_label(st, r, w);
                pill(&mut p, &label, Point::new(r.x + 8.0, r.y + 8.0), PillStyle::small());
            }
            if self.sel_rect.is_none() && st.codes_visible && st.tool == Tool::Region {
                self.paint_codes(&mut p);
            }
        }
        if let Some(sel) = selection {
            p.stroke_rect(sel.adjusted(-1.0, -1.0, 1.0, 1.0), Pen::new(c.accent, 2.0));
            if self.sel_rect.is_some() {
                let label = self.selection_label(st, sel);
                let here = sel.intersected(&self.rect());
                let anchor = if here.is_empty() { sel } else { here };
                pill(
                    &mut p,
                    &label,
                    Point::new(anchor.left(), anchor.top() - 10.0),
                    PillStyle { anchor_bottom: true, keep_inside: true, accent: true, ..Default::default() },
                );
            }
        } else if let Some(r) = self.rec_rect {
            p.stroke_rect(r.adjusted(-1.0, -1.0, 1.0, 1.0), Pen::new(c.accent, 2.0));
            if let Some(n) = st.countdown {
                paint_countdown(&mut p, r, n, st.cfg.record_countdown);
            } else if self.rec_drag.is_none() {
                for x in [r.left(), r.center().x, r.right()] {
                    for y in [r.top(), r.center().y, r.bottom()] {
                        if (x, y) != (r.center().x, r.center().y) {
                            p.draw_rounded_rect(
                                Rect::new(x - HANDLE / 2.0, y - HANDLE / 2.0, HANDLE, HANDLE),
                                3.0,
                                Some(c.text),
                                Some(Pen::new(c.accent, 1.5)),
                            );
                        }
                    }
                }
            }
        } else if picking
            && st.cfg.show_crosshair
            && let Some(pos) = self.cursor_pos
        {
            let at = self.snap_point.unwrap_or(pos);
            let color = self.mark_color(st).with_alpha(110);
            let (x, y) = (py_round(at.x) as f32 + 0.5, py_round(at.y) as f32 + 0.5);
            p.draw_line(Point::new(x, 0.0), Point::new(x, self.height()), Pen::new(color, 1.0));
            p.draw_line(Point::new(0.0, y), Point::new(self.width(), y), Pen::new(color, 1.0));
        }
        if let Some(at) = self.loupe_at(st)
            && self.loupe_bounds_key(st, at).0.intersects(&region)
        {
            self.paint_loupe(&mut p, st, at);
        }
        if self.sel_rect.is_none() && st.pointer_overlay == Some(self.index) && st.cfg.show_hint {
            let (text, at, opacity) = self.hint(st);
            if self.dev(pill_rect(&text, at, PillStyle::default()), 2).intersects(&region) {
                pill(&mut p, &text, at, PillStyle { opacity, ..Default::default() });
            }
        }
        // The floating panels, bottom to top: cards (the hovered one last),
        // the toolbar, the recording panel, the picker.
        if self.chips_shown(st) {
            for (i, chip) in self.chips.iter().enumerate() {
                if Some(i) != self.code_hover && self.dev(chip.rect(), 2).intersects(&region) {
                    chip.paint(&mut p);
                }
            }
            if let Some(chip) = self.code_hover.and_then(|i| self.chips.get(i)) {
                chip.paint(&mut p);
            }
        }
        if self.toolbar_visible
            && let Some(t) = &self.toolbar
            && self.dev(t.rect(), 2).intersects(&region)
        {
            t.paint(&mut p);
        }
        if self.rec_rect.is_some()
            && self.rec_drag.is_none()
            && st.countdown.is_none()
            && let Some(panel) = &self.panel
            && self.dev(panel.rect(), 2).intersects(&region)
        {
            panel.paint(&mut p);
        }
        if self.picker_open()
            && let Some(picker) = &self.picker
            && self.dev(picker.rect(), 2).intersects(&region)
        {
            picker.paint(&mut p);
        }
    }

    fn paint_codes(&self, p: &mut Painter<'_>) {
        let c = theme::c();
        let pen = Pen::new(c.code, 2.0).with_join(crate::render::Join::Round);
        let mut hovered = None;
        for (i, (_, poly)) in self.codes.iter().enumerate() {
            if Some(i) == self.code_hover {
                hovered = Some(poly);
                continue;
            }
            let bx = code_box(poly);
            let r = (bx.h / 3.0).min(8.0);
            p.draw_rounded_rect(bx, r, Some(c.code_soft), Some(pen));
        }
        if let Some(poly) = hovered {
            // on top of the others, looking the same (it isn't a button)
            let bx = code_box(poly);
            p.draw_rounded_rect(bx, (bx.h / 3.0).min(8.0), Some(c.code_soft), Some(pen));
        }
    }

    fn paint_loupe(&self, p: &mut Painter<'_>, st: &State, pos: Point) {
        let c = theme::c();
        let l = self.loupe_geometry(st, pos);
        let (u, bx, by, side, cell) = (l.u, l.bx, l.by, l.side, l.cell);
        if side <= 0 || cell <= 0 {
            return;
        }
        let radius = 12.0 * self.dpr;
        let lead = (side - cell).div_euclid(2); // from the edge to the pointer's pixel, in the middle
        let before = -((-lead).div_euclid(cell)); // pixels shown before it (left and above), the first maybe in part
        let count = before + 1 + -((-(side - lead - cell)).div_euclid(cell));
        let (a, b) = (bx + lead, by + lead); // the pointer's pixel's top left
        // Drawn in device pixels: the zoomed pixels into a square of their
        // own, its corners rounded off, then put in place.
        let origin_dev = p.map(Point::ZERO);
        p.save();
        p.set_transform(tiny_skia::Transform::from_translate(origin_dev.x, origin_dev.y));
        if let Ok(mut sq) = Image::filled(side as u32, side as u32, c.ink.premul()) {
            zoom_into(&mut sq, &self.base, l.px - before, l.py - before, count, cell, a - before * cell - bx, b - before * cell - by);
            round_corners(&mut sq, radius);
            p.blit_image(&sq, bx as i32 + origin_dev.x as i32, by as i32 + origin_dev.y as i32);
        }
        p.antialias = false;
        if cell >= 6 * u
            && let Some(grid) = loupe_grid(side, cell, lead.rem_euclid(cell), radius, u)
        {
            p.blit_image(&grid, bx as i32 + origin_dev.x as i32, by as i32 + origin_dev.y as i32);
        }
        // The square around the pointer's pixel, on the grid lines round
        // it, and, while choosing an area, the crosshair: through the middle
        // of that pixel, or where it snaps, on the boundary between pixels.
        // In the crosshair's colour with a thin dark edge, which shows it on
        // any pixel. Rectangles as (x, y, w, h) that never overlap.
        let ring = (a - u, b - u, cell + 3 * u, cell + 3 * u);
        let hole = (a + u, b + u, cell - u, cell - u);
        let around = (a - 2 * u, b - 2 * u, cell + 5 * u, cell + 5 * u);
        let mut marks = cut(ring, hole);
        let mut edges = cut(around, hole);
        if matches!(st.tool, Tool::Region | Tool::Record) {
            let cap = self.dpr;
            let mut lx = None;
            let mut ly = None;
            if let Some(s) = self.snap_point {
                if s.x != pos.x {
                    lx = Some(a + (py_round(s.x * cap) as i64 - l.px) * cell);
                }
                if s.y != pos.y {
                    ly = Some(b + (py_round(s.y * cap) as i64 - l.py) * cell);
                }
            }
            let lx = lx.unwrap_or(a + cell / 2);
            let ly = ly.unwrap_or(b + cell / 2);
            let mut across = None;
            if bx + 2 * u <= lx && lx <= bx + side - 3 * u {
                let inset = corner_inset(lx - u - bx, side, radius, u).max(corner_inset(lx + 2 * u - 1 - bx, side, radius, u));
                marks.extend(cut((lx, by + inset, u, side - 2 * inset), ring));
                let ac = (lx - u, by + inset, 3 * u, side - 2 * inset);
                edges.extend(cut(ac, around));
                across = Some(ac);
            }
            if by + 2 * u <= ly && ly <= by + side - 3 * u {
                let inset = corner_inset(ly - u - by, side, radius, u).max(corner_inset(ly + 2 * u - 1 - by, side, radius, u));
                marks.extend(cut((bx + inset, ly, side - 2 * inset, u), ring));
                for part in cut((bx + inset, ly - u, side - 2 * inset, 3 * u), around) {
                    match across {
                        Some(ac) => edges.extend(cut(part, ac)),
                        None => edges.push(part),
                    }
                }
            }
        }
        for (x0, y0, w0, h0) in edges {
            p.fill_rect(Rect::new(x0 as f32, y0 as f32, w0 as f32, h0 as f32), LOUPE_EDGE);
        }
        let mark = self.mark_color(st);
        for (x0, y0, w0, h0) in marks {
            p.fill_rect(Rect::new(x0 as f32, y0 as f32, w0 as f32, h0 as f32), mark);
        }
        p.antialias = true;
        let uf = u as f32;
        p.stroke_rounded_rect(
            Rect::new(bx as f32, by as f32, side as f32, side as f32).adjusted(uf / 2.0, uf / 2.0, -uf / 2.0, -uf / 2.0),
            radius - uf / 2.0,
            Pen::new(c.line, uf),
        );
        p.restore();
        let label = loupe_label(&self.base, l.px, l.py);
        pill(p, &label, Point::new(l.x, l.y + l.size + 6.0), PillStyle { mono: true, ..Default::default() });
    }
}

struct Loupe {
    size: f32,
    px: i64,
    py: i64,
    x: f32,
    y: f32,
    u: i64,
    bx: i64,
    by: i64,
    side: i64,
    cell: i64,
}

fn loupe_label(base: &Image, px: i64, py: i64) -> String {
    let color = image::hex(base.pixel_clamped(px, py));
    format!("{px}, {py}   {color}")
}

/// Copy pixels (`sx`, `sy`) .. +`count` of `src`, each `cell` x `cell`, into
/// `dst` with the first at (`dx`, `dy`). Pixels outside `src` stay as they are.
#[allow(clippy::too_many_arguments)]
fn zoom_into(dst: &mut Image, src: &Image, sx: i64, sy: i64, count: i64, cell: i64, dx: i64, dy: i64) {
    let (dw, dh) = (dst.width() as i64, dst.height() as i64);
    for j in 0..count {
        let y = sy + j;
        if y < 0 || y >= src.height() as i64 {
            continue;
        }
        let y0 = (dy + j * cell).max(0);
        let y1 = (dy + (j + 1) * cell).min(dh);
        if y0 >= y1 {
            continue;
        }
        for i in 0..count {
            let x = sx + i;
            if x < 0 || x >= src.width() as i64 {
                continue;
            }
            let x0 = (dx + i * cell).max(0);
            let x1 = (dx + (i + 1) * cell).min(dw);
            if x0 >= x1 {
                continue;
            }
            let px = src.pixel(x as u32, y as u32).to_le_bytes();
            for row in y0..y1 {
                let r = dst.row_mut(row as u32);
                for col in x0..x1 {
                    r[col as usize * 4..col as usize * 4 + 4].copy_from_slice(&px);
                }
            }
        }
    }
}

/// Cut the corners of a square picture to a rounded square (anti-aliased).
fn round_corners(img: &mut Image, radius: f32) {
    let (w, h) = (img.width() as i64, img.height() as i64);
    let r = radius.min(w as f32 / 2.0).min(h as f32 / 2.0);
    let ri = r.ceil() as i64;
    for y in 0..h {
        for x in 0..w {
            let cx = if x < ri {
                r
            } else if x >= w - ri {
                w as f32 - r
            } else {
                continue;
            };
            let cy = if y < ri {
                r
            } else if y >= h - ri {
                h as f32 - r
            } else {
                continue;
            };
            // coverage of the pixel by the circle at (cx, cy)
            let (fx, fy) = (x as f32 + 0.5 - cx, y as f32 + 0.5 - cy);
            let inside_x = (x as f32 + 0.5 < r && fx < 0.0) || (x as f32 + 0.5 > w as f32 - r && fx > 0.0);
            let inside_y = (y as f32 + 0.5 < r && fy < 0.0) || (y as f32 + 0.5 > h as f32 - r && fy > 0.0);
            if !(inside_x && inside_y) {
                continue;
            }
            let d = (fx * fx + fy * fy).sqrt();
            let cov = (r - d + 0.5).clamp(0.0, 1.0);
            if cov >= 1.0 {
                continue;
            }
            let i = (y * w + x) as usize * 4;
            let data = img.data_mut();
            for k in 0..4 {
                data[i + k] = (data[i + k] as f32 * cov).round() as u8;
            }
        }
    }
}

/// The magnifier's pixel grid (device pixels), thin lines with its rounded
/// corners cut off, made once per size and zoom.
fn loupe_grid(side: i64, cell: i64, offset: i64, radius: f32, u: i64) -> Option<Arc<Image>> {
    use std::sync::Mutex;
    type Key = (i64, i64, i64, i32, i64);
    static GRIDS: Mutex<Vec<(Key, Arc<Image>)>> = Mutex::new(Vec::new());
    let key = (side, cell, offset, (radius * 100.0) as i32, u);
    let mut grids = GRIDS.lock().unwrap_or_else(|e| e.into_inner());
    if let Some((_, g)) = grids.iter().find(|(k, _)| *k == key) {
        return Some(g.clone());
    }
    let mut img = Image::new(side as u32, side as u32).ok()?;
    {
        let mut p = Painter::new(img.pixmap_mut(), 1.0);
        p.antialias = false;
        let color = Color::rgba(128, 128, 128, 70);
        let mut at = offset;
        while at < side {
            if at > 0 {
                p.fill_rect(Rect::new(at as f32, 0.0, u as f32, side as f32), color);
                p.fill_rect(Rect::new(0.0, at as f32, side as f32, u as f32), color);
            }
            at += cell;
        }
    }
    round_corners(&mut img, radius);
    let g = Arc::new(img);
    if grids.len() > 32 {
        grids.clear();
    }
    grids.push((key, g.clone()));
    Some(g)
}

/// Where a line across the magnifier, `at` pixels in from one side, starts
/// and ends (pixels in from each end): inside the border and its rounded
/// corners.
fn corner_inset(at: i64, size: i64, radius: f32, border: i64) -> i64 {
    let d = (at.min(size - 1 - at)).max(0) as f32 + 0.5;
    if d >= radius {
        return border;
    }
    (radius - (radius * radius - (radius - d) * (radius - d)).sqrt()).ceil() as i64 + border
}

type R4 = (i64, i64, i64, i64);

/// The parts of rectangle `r` outside `hole`, as (x, y, w, h).
fn cut(r: R4, hole: R4) -> Vec<R4> {
    let (x, y, w, h) = r;
    let (hx, hy, hw, hh) = hole;
    if x >= hx + hw || hx >= x + w || y >= hy + hh || hy >= y + h {
        return vec![r];
    }
    let mut out = Vec::new();
    if y < hy {
        out.push((x, y, w, hy - y));
    }
    if y + h > hy + hh {
        out.push((x, hy + hh, w, y + h - hy - hh));
    }
    let (top, bottom) = (y.max(hy), (y + h).min(hy + hh));
    if x < hx {
        out.push((x, top, hx - x, bottom - top));
    }
    if x + w > hx + hw {
        out.push((hx + hw, top, x + w - hx - hw, bottom - top));
    }
    out
}

fn paint_countdown(p: &mut Painter<'_>, r: Rect, n: i64, total: i64) {
    let c = theme::c();
    let size = 56f32.max(140f32.min(r.w * 0.6).min(r.h * 0.6));
    let bx = Rect::new(r.center().x - size / 2.0, r.center().y - size / 2.0, size, size);
    p.fill_ellipse(bx, c.base);
    let total = total.max(1) as f32;
    let mut pb = tiny_skia::PathBuilder::new();
    crate::render::icons::arc_to(&mut pb, bx.adjusted(-6.0, -6.0, 6.0, 6.0), 90.0, -360.0 * n as f32 / total, true);
    if let Some(path) = pb.finish() {
        p.stroke_path(&path, Pen::new(c.accent, 4.0));
    }
    p.draw_text(bx, Align::Center, &n.to_string(), text::font((size / 2.0).round(), BOLD), c.text);
}

/// The green box round a code: padded in proportion to its size.
pub fn code_box(poly: &[Point]) -> Rect {
    let (mut l, mut t, mut r, mut b) = (f32::MAX, f32::MAX, f32::MIN, f32::MIN);
    for p in poly {
        l = l.min(p.x);
        t = t.min(p.y);
        r = r.max(p.x);
        b = b.max(p.y);
    }
    if poly.is_empty() {
        return Rect::default();
    }
    let rect = Rect::from_ltrb(l, t, r, b);
    let pad = (rect.w.min(rect.h) * 0.08).clamp(3.0, 16.0);
    rect.adjusted(-pad, -pad, pad, pad)
}

// -- pills ---------------------------------------------------------------------------

#[derive(Clone, Copy, Debug)]
pub struct PillStyle {
    pub anchor_bottom: bool,
    pub accent: bool,
    pub mono: bool,
    pub keep_inside: bool,
    pub small: bool,
    pub opacity: f32,
}

impl Default for PillStyle {
    fn default() -> Self {
        PillStyle { anchor_bottom: false, accent: false, mono: false, keep_inside: false, small: false, opacity: 1.0 }
    }
}

impl PillStyle {
    pub fn small() -> PillStyle {
        PillStyle { small: true, ..Default::default() }
    }

    fn font(&self) -> text::FontSpec {
        let px = if self.small { 11.0 } else { 12.0 };
        let weight = if self.accent { DEMIBOLD } else { MEDIUM };
        if self.mono { text::mono(px, weight) } else { text::font(px, weight) }
    }
}

/// Where a pill with `label` goes, anchored at `at`.
pub fn pill_rect(label: &str, at: Point, s: PillStyle) -> Rect {
    let w = text::width(label, s.font()) + if s.small { 18.0 } else { 28.0 };
    let h = if s.small { 22.0 } else { 28.0 };
    let mut r = Rect::new(at.x, if s.anchor_bottom { at.y - h } else { at.y }, w, h);
    if s.keep_inside && r.y < 4.0 {
        r.y = at.y + 20.0; // no room above the selection: tuck inside
    }
    r
}

pub fn pill(p: &mut Painter<'_>, label: &str, at: Point, s: PillStyle) {
    let c = theme::c();
    let r = pill_rect(label, at, s);
    p.save();
    p.set_opacity(s.opacity);
    let radius = if s.small { 6.0 } else { 8.0 };
    if s.accent {
        p.fill_rounded_rect(r, radius, c.accent);
    } else {
        p.draw_rounded_rect(r, radius, Some(c.base), Some(Pen::new(c.line, 1.0)));
    }
    p.draw_text(r, Align::Center, label, s.font(), if s.accent { c.on_accent } else { c.soft });
    p.restore();
}

// -- hints --------------------------------------------------------------------------

pub const HINT_REGION: &str = "Drag to capture  ·  Click for whole screen  ·  Esc to cancel";
pub const HINT_CODES: &str = "Drag to capture  ·  {codes} hides detected codes  ·  Esc to cancel";
pub const HINT_WINDOWS: &str = "Drag to capture  ·  Click a window to capture it  ·  Esc to cancel";
pub const HINT_TEXT: &str = "Click to place text, or on text to change it  ·  Drag text to move it  ·  Enter to finish";
pub const HINT_SHAPE: &str = "Shift: square  ·  Ctrl: from the middle  ·  Alt: move it";
pub const HINT_SELECT: &str = "Click a drawing to select it  ·  Drag to move  ·  Handles resize  ·  Delete removes it";
pub const HINT_SELECTED: &str = "Drag to move (Shift: straight)  ·  Handles: Shift keeps its shape, Ctrl from the middle  ·  Arrows nudge  ·  Delete";
pub const PIN_HINT: &str = "Drag to pin a region  ·  Click a window to pin it  ·  {pin} to save instead";
pub const EYEDROPPER_HINT: &str = "Click a pixel to make it your colour  ·  Esc to stop";
pub const DRAW_HINT: &str = "Draw on the screen  ·  {tool.region} then drag to capture  ·  Enter for whole screen";
pub const RECORD_PICK: &str = "Drag the area to record  ·  Click for whole screen  ·  Esc to cancel";
pub const RECORD_WINDOWS: &str = "Drag the area to record  ·  Click a window  ·  Enter for whole screen  ·  Esc to cancel";
pub const RECORD_ARMED: &str = "Drag the edges to adjust  ·  Enter to start  ·  Esc to go back";

/// A hint with its {action} keys filled in; a part whose key is unset goes.
pub fn with_keys(text: &str, keymap: &crate::keys::Keymap) -> String {
    let mut parts = Vec::new();
    for part in text.split("  ·  ") {
        if let (Some(a), Some(b)) = (part.find('{'), part.find('}')) {
            let action = &part[a + 1..b];
            let key = keymap.label(action);
            if key.is_empty() {
                continue;
            }
            parts.push(part.replace(&format!("{{{action}}}"), &key));
        } else {
            parts.push(part.to_string());
        }
    }
    parts.join("  ·  ")
}

fn hint_text(st: &State, o: &Overlay) -> String {
    if st.tool == Tool::Record {
        if let Some(n) = st.countdown {
            return format!("Recording starts in {n}  ·  Esc to cancel");
        }
        if let Some(h) = &st.hint {
            return h.clone();
        }
        if let Some(p) = &st.record_problem {
            return p.clone();
        }
        return if st.any_armed {
            RECORD_ARMED.into()
        } else if !o.windows.is_empty() {
            RECORD_WINDOWS.into()
        } else {
            RECORD_PICK.into()
        };
    }
    if st.eyedropper {
        return st.hint.clone().unwrap_or_else(|| EYEDROPPER_HINT.into());
    }
    if let Some(h) = &st.hint {
        return h.clone();
    }
    let shape_active = o.active.as_ref().is_some_and(|s| s.drag().is_some());
    let template = if shape_active {
        HINT_SHAPE
    } else if st.pin_mode && st.tool == Tool::Region {
        PIN_HINT
    } else {
        match st.tool {
            Tool::Region if !o.codes.is_empty() && st.codes_visible => HINT_CODES,
            Tool::Region if !o.windows.is_empty() => HINT_WINDOWS,
            Tool::Region => HINT_REGION,
            Tool::Text => HINT_TEXT,
            Tool::Select if o.selection.current(&o.annotations).is_some() => HINT_SELECTED,
            Tool::Select => HINT_SELECT,
            _ => DRAW_HINT,
        }
    };
    with_keys(template, &st.keymap)
}

// -- the background ---------------------------------------------------------------------

/// Above this many pixels the background is copied by several threads
/// (the first frame of a big monitor).
const PARALLEL_ABOVE: i64 = 1 << 20;

/// `copy_background` over bands of rows at once, for big regions.
fn copy_background_parallel(target: &mut Canvas<'_>, at: IRect, base: &Image, region: IRect, dim: Option<Color>, hole: Option<IRect>) {
    let threads = std::thread::available_parallelism().map_or(1, |n| n.get()).min(8) as i32;
    if region.area() < PARALLEL_ABOVE || threads < 2 {
        let stride = target.stride();
        copy_background(target.data_mut(), stride, at, base, region, dim, hole);
        return;
    }
    let stride = target.stride();
    let band = (region.h + threads - 1) / threads;
    let first_row = at.y as usize;
    let data = &mut target.data_mut()[first_row * stride..(first_row + region.h as usize) * stride];
    std::thread::scope(|s| {
        for (i, rows) in data.chunks_mut(band as usize * stride).enumerate() {
            let y0 = i as i32 * band;
            let h = (rows.len() / stride) as i32;
            let part = IRect::new(region.x, region.y + y0, region.w, h);
            let at_part = IRect::new(at.x, 0, at.w, h);
            s.spawn(move || copy_background(rows, stride, at_part, base, part, dim, hole));
        }
    });
}

/// Copy `region` (device pixels) of `base` into the rows `data` (`stride`
/// bytes each) at `at`, shaded with `dim` outside `hole` when given.
/// Pixels past the picture are ink.
fn copy_background(data: &mut [u8], stride: usize, at: IRect, base: &Image, region: IRect, dim: Option<Color>, hole: Option<IRect>) {
    let ink = theme::c().ink.premul().to_le_bytes();
    let src_rect = region.intersected(&base.rect());
    for row in 0..region.h {
        let y = region.y + row;
        let line = (at.y + row) as usize * stride;
        let dst = &mut data[line + at.x as usize * 4..line + (at.x + region.w) as usize * 4];
        if y < src_rect.y || y >= src_rect.y2() || src_rect.is_empty() {
            for px in dst.as_chunks_mut::<4>().0 {
                px.copy_from_slice(&ink);
            }
            continue;
        }
        // Columns inside the picture are copied, the rest are ink.
        let src_row = base.row(y as u32);
        for (i, px) in dst.as_chunks_mut::<4>().0.iter_mut().enumerate() {
            let x = region.x + i as i32;
            if x < src_rect.x || x >= src_rect.x2() {
                px.copy_from_slice(&ink);
            }
        }
        let (x0, x1) = (src_rect.x.max(region.x), src_rect.x2().min(region.x2()));
        if x0 < x1 {
            let off = (x0 - region.x) as usize * 4;
            let len = (x1 - x0) as usize * 4;
            dst[off..off + len].copy_from_slice(&src_row[x0 as usize * 4..x0 as usize * 4 + len]);
        }
        if let Some(d) = dim {
            shade_row(dst, region.x, y, d, hole);
        }
    }
}

/// Shade `region` of `target` (already drawn) outside `hole`.
fn shade(target: &mut Canvas<'_>, at: IRect, region: IRect, dim: Color, hole: Option<IRect>) {
    for row in 0..region.h {
        let y = region.y + row;
        let dst = &mut target.row_mut((at.y + row) as u32)[at.x as usize * 4..(at.x + region.w) as usize * 4];
        shade_row(dst, region.x, y, dim, hole);
    }
}

/// Blend the shading colour over a row of pixels starting at device x
/// `x0`, row `y`, except inside `hole`.
#[inline]
fn shade_row(row: &mut [u8], x0: i32, y: i32, dim: Color, hole: Option<IRect>) {
    let w = (row.len() / 4) as i32;
    let (skip0, skip1) = match hole {
        Some(h) if y >= h.y && y < h.y2() => ((h.x - x0).clamp(0, w), (h.x2() - x0).clamp(0, w)),
        _ => (0, 0),
    };
    let a = dim.a as u32;
    let inv = 255 - a;
    // dim colour premultiplied, BGRA order, scaled by 255 for one rounding
    let pre = [dim.b as u32 * a, dim.g as u32 * a, dim.r as u32 * a, 255 * a];
    let blend = |px: &mut [u8]| {
        for k in 0..4 {
            let v = px[k] as u32 * inv + pre[k];
            px[k] = ((v + 1 + (v >> 8)) >> 8) as u8;
        }
    };
    for px in row[..skip0 as usize * 4].as_chunks_mut::<4>().0 {
        blend(px);
    }
    for px in row[skip1 as usize * 4..].as_chunks_mut::<4>().0 {
        blend(px);
    }
}

// -- the scene ----------------------------------------------------------------------------

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Id {
    Annotation(usize),
    Active,
    Moving,
    SelectionFrame,
    TextEdit,
    Window,
    Codes,
    Selection,
    Handles,
    Countdown,
    CrossV,
    CrossH,
    Loupe,
    Hint,
    Chip(usize),
    Toolbar,
    Panel,
    Picker,
}

#[derive(Clone, Debug, PartialEq)]
struct Element {
    id: Id,
    bounds: IRect,
    key: u64,
}

#[derive(Clone, Debug, PartialEq)]
struct Scene {
    bg_dim: Option<Color>,
    hole: Option<IRect>,
    elements: Vec<Element>,
}

fn hash<T: Hash>(v: &T) -> u64 {
    let mut h = DefaultHasher::new();
    v.hash(&mut h);
    h.finish()
}

fn rect_key(r: Rect) -> (i32, i32, i32, i32) {
    ((r.x * 64.0) as i32, (r.y * 64.0) as i32, (r.w * 64.0) as i32, (r.h * 64.0) as i32)
}

/// A key for how a shape looks: its kind, geometry, colour, size, state.
fn shape_key(s: &Shape, index: usize) -> u64 {
    use crate::ui::shapes::Kind;
    let mut h = DefaultHasher::new();
    index.hash(&mut h);
    s.color.hash(&mut h);
    s.size.hash(&mut h);
    s.hidden.hash(&mut h);
    rect_key(s.bounds()).hash(&mut h);
    match &s.kind {
        Kind::Stroke(st) => {
            st.points.len().hash(&mut h);
            if let Some(p) = st.points.last() {
                ((p.x * 64.0) as i32, (p.y * 64.0) as i32).hash(&mut h);
            }
        }
        Kind::Text(t) => {
            t.edit.text.hash(&mut h);
            t.edit.cursor.hash(&mut h);
            t.edit.anchor.hash(&mut h);
            t.editing.hash(&mut h);
            ((t.pos.x * 64.0) as i32, (t.pos.y * 64.0) as i32).hash(&mut h);
        }
        Kind::Counter { number, .. } => number.hash(&mut h),
        _ => {
            if let Some(d) = s.drag() {
                ((d.start.x * 64.0) as i32, (d.start.y * 64.0) as i32, (d.end.x * 64.0) as i32, (d.end.y * 64.0) as i32).hash(&mut h);
            }
        }
    }
    h.finish()
}

fn toolbar_key(t: &Toolbar) -> u64 {
    let mut h = DefaultHasher::new();
    for it in &t.items {
        it.id.hash(&mut h);
        it.enabled.hash(&mut h);
        it.active.hash(&mut h);
        it.dimmed.hash(&mut h);
        it.toggled.hash(&mut h);
        it.selected.hash(&mut h);
        format!("{:?}", it.look).hash(&mut h);
    }
    h.finish()
}

fn panel_key(p: &RecordPanel) -> u64 {
    let mut h = DefaultHasher::new();
    for it in &p.items {
        it.id.hash(&mut h);
        it.enabled.hash(&mut h);
        format!("{:?}", it.look).hash(&mut h);
    }
    p.choice_hover.hash(&mut h);
    h.finish()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cutting() {
        let parts = cut((0, 0, 10, 10), (2, 2, 6, 6));
        let area: i64 = parts.iter().map(|r| r.2 * r.3).sum();
        assert_eq!(area, 100 - 36);
        assert_eq!(cut((0, 0, 5, 5), (10, 10, 2, 2)), vec![(0, 0, 5, 5)]);
    }

    #[test]
    fn shading_skips_the_hole() {
        let mut row = vec![200u8; 10 * 4];
        shade_row(&mut row, 0, 5, Color::rgba(0, 0, 0, 128), Some(IRect::new(3, 0, 4, 10)));
        assert!(row[0] < 120);
        assert_eq!(row[3 * 4], 200);
        assert_eq!(row[6 * 4], 200);
        assert!(row[7 * 4] < 120);
    }

    #[test]
    fn link_codes() {
        let c = |t: &str| Code { text: t.into(), kind: "QR".into(), corners: vec![] };
        assert!(c("https://example.com").is_link());
        assert!(c("MAILTO:a@b.c").is_link());
        assert!(!c("javascript:alert(1)").is_link());
        assert!(!c("file:///etc/passwd").is_link());
        assert!(!c("https://a.b/ x").is_link());
    }
}
