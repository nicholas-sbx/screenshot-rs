//! The annotation editor: a window for drawing on a screenshot already
//! taken (or any picture), with the capture overlay's tools, Save, Save as
//! and Copy. The picture moves and zooms as in a paint program, and any
//! number of editors can be open at once.
//!
//! This is the editor's state, input and painting, with nothing of the
//! window system: what it needs done outside (the clipboard, a file
//! dialog, closing) it asks for with [`Request`]s.

use std::cell::RefCell;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::config::Config;
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::{Canvas, Image};
use crate::keys::{KeyPress, Keymap, Where};
use crate::keyseq::{self, key};
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD, MEDIUM};
use crate::render::theme::{self, Color, SIZES, SWATCHES};
use crate::render::{Painter, icons};
use crate::ui::panels::{ColorPicker, PICKER_SIZE, PickerEvent};
use crate::ui::session::{Button, CursorShape};
use crate::ui::shapes::{Mods, Shape, TOOLS, Tool, copy_for_editing};
use crate::ui::textedit::KeyResult;
use crate::ui::widgets::{Item, Look, Pointer, layout_row, paint_item};

pub const BAR_H: f32 = 48.0;
pub const STATUS_H: f32 = 28.0;
const ZOOM: (f32, f32) = (0.05, 32.0);
/// How much of the picture always stays in view.
const KEEP: f32 = 48.0;
const CUSTOM: usize = SWATCHES.len();
const SAY_FOR: Duration = Duration::from_secs(4);
const COPIED_FOR: Duration = Duration::from_secs(2);

const SHAPE_HINT: &str = "Shift: square  ·  Ctrl: from the middle  ·  Alt: move it";
const TEXT_HINT: &str = "Click to place text, or on text to change it  ·  Drag text to move it  ·  Shift+Enter for a new line";
const HINT: &str = "Scroll to move around  ·  Ctrl+scroll to zoom  ·  Space+drag or middle-drag to pan";

/// What the editor needs done outside itself.
#[derive(Debug)]
pub enum Request {
    /// put this PNG on the clipboard
    CopyImage(Arc<Vec<u8>>),
    /// put this text on the clipboard
    CopyText(String),
    /// read the clipboard's text and hand it to `paste`
    Paste,
    /// ask where to save, then `save_to` there
    SaveAs,
    /// the window can close now
    Close,
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Answer {
    Discard,
    Cancel,
    Save,
}

/// Copy, Save as and Save: an icon and a word; `strong` fills it with the accent.
#[derive(Clone, Debug)]
struct ActionButton {
    id: &'static str,
    text: &'static str,
    icon: &'static str,
    strong: bool,
    hint: String,
    rect: Rect,
    /// as wide as its widest word, so changing words never moves anything
    width: f32,
}

fn action_font() -> text::FontSpec {
    text::font(13.0, DEMIBOLD)
}

impl ActionButton {
    fn new(id: &'static str, words: &[&'static str], icon: &'static str) -> ActionButton {
        let width = words.iter().map(|w| text::width(w, action_font())).fold(0.0f32, f32::max).round() + 46.0;
        ActionButton { id, text: words[0], icon, strong: false, hint: String::new(), rect: Rect::default(), width }
    }

    fn paint(&self, p: &mut Painter<'_>, hovered: bool) {
        let c = theme::c();
        let (fill, fg) = if self.strong {
            (if hovered { c.accent.lighter(108) } else { c.accent }, c.on_accent)
        } else {
            (if hovered { c.hover } else { c.raised }, if hovered { c.text } else { c.soft })
        };
        p.fill_rounded_rect(self.rect, 8.0, fill);
        let text_w = text::width(self.text, action_font());
        let x = self.rect.x + (self.rect.w - (18.0 + 7.0 + text_w)) / 2.0;
        icons::paint(p, self.icon, Rect::new(x, self.rect.y + (self.rect.h - 18.0) / 2.0, 18.0, 18.0), fg);
        p.draw_text(Rect::new(x + 25.0, self.rect.y, text_w + 2.0, self.rect.h), Align::Left, self.text, action_font(), fg);
    }
}

/// The picture and its drawings as last drawn, at the view's zoom.
type Layer = ((u32, u32, i64, i64, i64, u64), Image);

pub struct Editor {
    pub image: Arc<Image>,
    /// the picture's pixels per logical pixel (that of the screen it was
    /// taken on): drawings are as thick as on the capture overlay
    pub dpr: f32,
    pub path: PathBuf,
    /// where Save writes, its folder held since the file was opened
    target: Option<SaveTarget>,
    alpha: bool,
    cfg: Config,
    keymap: Keymap,
    shapes: Vec<(u64, Shape)>,
    undone: Vec<(u64, Shape)>,
    /// the drawings as last saved (or opened)
    saved: Vec<u64>,
    next_id: u64,
    active: Option<Shape>,
    text_edit: Option<Shape>,
    pub tool: Tool,
    colour_tool: Tool,
    color_index: usize,
    custom_color: Color,
    size: usize,
    picker: Option<ColorPicker>,
    eyedropper: bool,
    zoom: f32,
    /// where the picture's top left is in the canvas (logical)
    offset: Point,
    /// following the window's size, until moved or zoomed
    fitted: bool,
    /// the window's size, logical pixels
    view: (f32, f32),
    pub scale: f32,
    /// a pan: where the pointer and the picture were when it began
    pan: Option<(Point, Point)>,
    space: bool,
    /// a text being moved: where it was grabbed and where it was then
    text_drag: Option<(Point, Point)>,
    pointer: Option<Point>,
    mods: i32,
    bar: Vec<Item>,
    actions: Vec<ActionButton>,
    hovered: Option<&'static str>,
    pressed: Option<&'static str>,
    status: Option<(String, Instant)>,
    copied_until: Option<Instant>,
    /// "Save the changes?" is up, with the button under the pointer
    asking: Option<Option<Answer>>,
    closing: bool,
    dirty: bool,
    damaged: bool,
    layer: RefCell<Option<Layer>>,
    shapes_version: u64,
    requests: Vec<Request>,
}

fn picture_size(img: &Image, dpr: f32) -> (f32, f32) {
    (img.width() as f32 / dpr, img.height() as f32 / dpr)
}

/// The window's first size for a picture of `size` (logical) on a monitor
/// of `monitor` (logical).
pub fn initial_size(size: (f32, f32), monitor: (f32, f32)) -> (u32, u32) {
    let w = (monitor.0 * 0.85).min(size.0 + 48.0).max(720.0);
    let h = (monitor.1 * 0.85).min(size.1 + BAR_H + STATUS_H + 64.0).max(480.0);
    (w.round() as u32, h.round() as u32)
}

impl Editor {
    pub fn new(image: Image, dpr: f32, path: PathBuf, cfg: Config) -> Editor {
        let alpha = image.has_alpha();
        let tool = Tool::from_id(&cfg.default_tool).filter(|t| t.uses_color()).unwrap_or(Tool::Pen);
        let keymap = Keymap::new(&cfg.keys);
        let custom_color = Color::parse(&cfg.custom_color).unwrap_or(SWATCHES[0]);
        let mut ed = Editor {
            image: Arc::new(image),
            dpr: dpr.max(0.25),
            target: SaveTarget::open(&path).ok(),
            path,
            alpha,
            color_index: (cfg.default_color.clamp(0, CUSTOM as i64)) as usize,
            size: (cfg.default_size.clamp(0, SIZES.len() as i64 - 1)) as usize,
            cfg,
            keymap,
            shapes: Vec::new(),
            undone: Vec::new(),
            saved: Vec::new(),
            next_id: 1,
            active: None,
            text_edit: None,
            tool,
            colour_tool: tool,
            custom_color,
            picker: None,
            eyedropper: false,
            zoom: 1.0,
            offset: Point::ZERO,
            fitted: true,
            view: (800.0, 600.0),
            scale: 1.0,
            pan: None,
            space: false,
            text_drag: None,
            pointer: None,
            mods: 0,
            bar: Vec::new(),
            actions: vec![
                ActionButton::new("copy", &["Copy", "Copied"], "copy"),
                ActionButton::new("save_as", &["Save as"], "save-as"),
                ActionButton::new("save", &["Save", "Saved"], "save"),
            ],
            hovered: None,
            pressed: None,
            status: None,
            copied_until: None,
            asking: None,
            closing: false,
            dirty: false,
            damaged: true,
            layer: RefCell::new(None),
            shapes_version: 0,
            requests: Vec::new(),
        };
        ed.refresh_bar();
        ed
    }

    // -- the window ------------------------------------------------------------------------

    /// The window's title: the file's name, marked while it has unsaved changes.
    pub fn title(&self) -> String {
        let name = self.path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        format!("{}{} — Annotate", if self.dirty { "● " } else { "" }, crate::util::displayable(&name))
    }

    pub fn set_view(&mut self, w: f32, h: f32) {
        if (w, h) == self.view {
            return;
        }
        self.view = (w.max(200.0), h.max(BAR_H + STATUS_H + 100.0));
        self.layout_bar();
        if self.fitted {
            self.fit();
        } else {
            self.keep_in_view();
        }
        self.damaged = true;
    }

    /// The colour theme changed: all of it is drawn again.
    pub fn restyle(&mut self) {
        self.damaged = true;
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

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// When it next needs a look (a message to clear).
    pub fn next_deadline(&self) -> Option<Instant> {
        [self.status.as_ref().map(|s| s.1 + SAY_FOR), self.copied_until].into_iter().flatten().min()
    }

    pub fn on_timer(&mut self, now: Instant) {
        if self.status.as_ref().is_some_and(|s| s.1 + SAY_FOR <= now) {
            self.status = None;
            self.damaged = true;
        }
        if self.copied_until.is_some_and(|t| t <= now) {
            self.copied_until = None;
            self.refresh_bar();
        }
    }

    pub fn damage(&mut self) -> Damage {
        if std::mem::take(&mut self.damaged) { Damage::full() } else { Damage::new() }
    }

    fn canvas_rect(&self) -> Rect {
        Rect::new(0.0, BAR_H, self.view.0, (self.view.1 - BAR_H - STATUS_H).max(1.0))
    }

    // -- the bar ---------------------------------------------------------------------------

    fn refresh_bar(&mut self) {
        let km = &self.keymap;
        let mut items = Vec::new();
        for (tool, id, label) in TOOLS.iter().filter(|t| !matches!(t.0, Tool::Region | Tool::Record)) {
            let mut b = Item::new(id, Look::Icon { icon: id, size: 36.0 }, km.hint(label, &format!("tool.{id}")));
            b.active = *tool == self.tool;
            items.push(b);
        }
        items.push(Item::new("divider.tools", Look::Divider, ""));
        const SWATCH_IDS: [&str; 8] = ["color.1", "color.2", "color.3", "color.4", "color.5", "color.6", "color.7", "color.8"];
        for (i, id) in SWATCH_IDS.iter().enumerate().take(CUSTOM) {
            let mut s = Item::new(id, Look::Swatch(i), km.hint("Colour", id));
            s.selected = self.color_index == i;
            items.push(s);
        }
        let mut custom = Item::new("color.8", Look::CustomSwatch(self.custom_color), km.hint("Your own colour", "color.8") + "  ·  click to choose it");
        custom.selected = self.color_index == CUSTOM;
        items.push(custom);
        items.push(Item::new("size", Look::Size(self.size), "Stroke size"));
        items.push(Item::new("divider.colors", Look::Divider, ""));
        let mut undo = Item::new("undo", Look::Icon { icon: "undo", size: 36.0 }, km.hint("Undo", "undo"));
        undo.enabled = self.can_undo();
        let mut redo = Item::new("redo", Look::Icon { icon: "redo", size: 36.0 }, km.hint("Redo", "redo"));
        redo.enabled = self.can_redo();
        items.push(undo);
        items.push(redo);
        items.push(Item::new("fit", Look::Icon { icon: "fit", size: 36.0 }, km.hint("Zoom to fit the picture in the window", "fit")));
        self.bar = items;
        let copied = self.copied_until.is_some();
        let (dirty, path) = (self.dirty, self.path.display().to_string());
        for b in &mut self.actions {
            match b.id {
                "copy" => {
                    (b.text, b.icon) = if copied { ("Copied", "check") } else { ("Copy", "copy") };
                    b.hint = km.hint("Copy the picture", "copy");
                }
                "save_as" => b.hint = km.hint("Save to another file", "save_as"),
                _ => {
                    // Save stands out only while there's something to save.
                    (b.text, b.icon, b.strong) = if dirty { ("Save", "save", true) } else { ("Saved", "check", false) };
                    b.hint = if dirty { km.hint(&format!("Save to {path}"), "save") } else { format!("Saved in {path}") };
                }
            }
        }
        self.layout_bar();
        self.damaged = true;
    }

    fn layout_bar(&mut self) {
        layout_row(&mut self.bar, 8.0, 6.0, 36.0, 2.0);
        let mut x = self.view.0 - 8.0;
        for b in self.actions.iter_mut().rev() {
            x -= b.width;
            b.rect = Rect::new(x, 7.0, b.width, 34.0);
            x -= 4.0;
        }
    }

    fn bar_item_at(&self, at: Point) -> Option<&'static str> {
        if at.y >= BAR_H {
            return None;
        }
        if let Some(b) = self.actions.iter().find(|b| b.rect.contains(at)) {
            return Some(b.id);
        }
        self.bar.iter().find(|it| it.rect.contains(at) && it.clickable() && it.enabled).map(|it| it.id)
    }

    fn bar_click(&mut self, id: &str) {
        match id {
            "undo" => self.undo(),
            "redo" => self.redo(),
            "fit" => self.fit(),
            "copy" => self.copy(),
            "save_as" => self.save_as(),
            "save" => {
                self.save();
            }
            "size" => self.set_size((self.size + 1) % SIZES.len()),
            "color.8" => self.toggle_picker(),
            _ => {
                if let Some(t) = Tool::from_id(id) {
                    self.set_tool(t);
                } else if let Some(i) = id.strip_prefix("color.").and_then(|n| n.parse::<usize>().ok()) {
                    self.set_color(i.saturating_sub(1));
                }
            }
        }
    }

    // -- tools ---------------------------------------------------------------------------

    fn color(&self) -> Color {
        if self.color_index >= CUSTOM { self.custom_color } else { SWATCHES[self.color_index] }
    }

    pub fn set_tool(&mut self, tool: Tool) {
        if matches!(tool, Tool::Region | Tool::Record) {
            return;
        }
        self.commit_text();
        self.tool = tool;
        if tool.uses_color() {
            self.colour_tool = tool;
        }
        self.refresh_bar();
    }

    /// A colour; from a tool that has none (pixelate, blur), back to the
    /// last drawing tool, so the colour gets used.
    pub fn set_color(&mut self, index: usize) {
        if !self.tool.uses_color() {
            self.tool = self.colour_tool;
        }
        self.color_index = index.min(CUSTOM);
        let color = self.color();
        if let Some(s) = &mut self.text_edit {
            s.color = color;
        }
        self.refresh_bar();
    }

    pub fn set_size(&mut self, size: usize) {
        self.size = size.min(SIZES.len() - 1);
        if let Some(s) = &mut self.text_edit {
            s.size = self.size;
        }
        self.refresh_bar();
    }

    fn toggle_picker(&mut self) {
        if self.picker.is_some() {
            self.close_picker();
            return;
        }
        self.set_color(CUSTOM);
        let mut picker = ColorPicker::new(self.custom_color);
        let swatch = self.bar.iter().find(|it| it.id == "color.8").map(|it| it.rect).unwrap_or_default();
        let x = (swatch.center().x - PICKER_SIZE.0 / 2.0).clamp(8.0, (self.view.0 - PICKER_SIZE.0 - 8.0).max(8.0));
        picker.pos = Point::new(x.round(), BAR_H + 6.0);
        self.picker = Some(picker);
        self.damaged = true;
    }

    /// Close the colour picker (and the eyedropper); keep the colour for
    /// next time. True if one was open.
    fn close_picker(&mut self) -> bool {
        let was_open = self.picker.is_some() || self.eyedropper;
        self.picker = None;
        self.eyedropper = false;
        let hex = self.custom_color.hex();
        if was_open && hex != self.cfg.custom_color {
            self.cfg.custom_color = hex.clone();
            crate::config::update(|c| c.custom_color = hex);
        }
        if was_open {
            self.damaged = true;
        }
        was_open
    }

    fn picker_event(&mut self, ev: PickerEvent) {
        match ev {
            PickerEvent::None => {}
            PickerEvent::Color(c) => {
                self.custom_color = c;
                self.set_color(CUSTOM);
            }
            PickerEvent::Eyedropper => {
                self.picker = None;
                self.eyedropper = true;
            }
            PickerEvent::Close => {
                self.close_picker();
            }
            PickerEvent::Copy(s) => self.requests.push(Request::CopyText(s)),
            PickerEvent::Paste => self.requests.push(Request::Paste),
        }
        self.damaged = true;
    }

    // -- drawing ---------------------------------------------------------------------------

    fn next_number(&self) -> u32 {
        1 + self.shapes.iter().filter(|(_, s)| s.is_counter() && !s.hidden).count() as u32
    }

    fn commit(&mut self, shape: Shape) {
        self.shapes.push((self.next_id, shape));
        self.next_id += 1;
        self.undone.clear();
        self.changed();
    }

    /// The drawings changed: unsaved while they differ from what was saved
    /// (undoing back to that is saved again).
    fn changed(&mut self) {
        self.dirty = self.shapes.iter().map(|(id, _)| *id).ne(self.saved.iter().copied());
        self.shapes_version += 1;
        self.refresh_bar();
    }

    fn begin_text(&mut self, shape: Shape) {
        self.commit_text();
        if let Some(t) = shape.as_text()
            && let Some((i, ..)) = &t.replaces
            && let Some((_, orig)) = self.shapes.get_mut(*i)
        {
            orig.hidden = true; // (the copy shows while it's edited)
            self.shapes_version += 1;
        }
        self.text_edit = Some(shape);
        self.refresh_bar();
    }

    /// Finish the text being typed; true if there was one.
    fn commit_text(&mut self) -> bool {
        let Some(mut shape) = self.text_edit.take() else { return false };
        let mut replaces = None;
        if let Some(t) = shape.as_text_mut() {
            t.editing = false;
            replaces = t.replaces.as_ref().map(|r| r.0);
        }
        if shape.is_valid() && shape.text_changed() {
            self.commit(shape);
        } else if let Some(i) = replaces
            && let Some((_, orig)) = self.shapes.get_mut(i)
        {
            orig.hidden = false; // edited back to as it was
            self.shapes_version += 1;
        }
        self.refresh_bar();
        true
    }

    fn text_at(&self, at: Point) -> Option<usize> {
        self.shapes.iter().enumerate().rev().find_map(|(i, (_, s))| (!s.hidden && s.as_text()?.contains(at, s.size)).then_some(i))
    }

    pub fn can_undo(&self) -> bool {
        !self.shapes.is_empty() || self.text_edit.is_some()
    }

    pub fn can_redo(&self) -> bool {
        !self.undone.is_empty()
    }

    pub fn undo(&mut self) {
        self.commit_text();
        if let Some((id, shape)) = self.shapes.pop() {
            if let Some(i) = shape.as_text().and_then(|t| t.replaces.as_ref().map(|r| r.0))
                && let Some((_, orig)) = self.shapes.get_mut(i)
            {
                orig.hidden = false; // the text as it was before it was edited
            }
            self.undone.push((id, shape));
            self.changed();
        }
    }

    pub fn redo(&mut self) {
        self.commit_text();
        if let Some((id, shape)) = self.undone.pop() {
            if let Some(i) = shape.as_text().and_then(|t| t.replaces.as_ref().map(|r| r.0))
                && let Some((_, orig)) = self.shapes.get_mut(i)
            {
                orig.hidden = true;
            }
            self.shapes.push((id, shape));
            self.changed();
        }
    }

    // -- the view --------------------------------------------------------------------------

    fn to_picture(&self, at: Point) -> Point {
        let c = self.canvas_rect();
        Point::new((at.x - c.x - self.offset.x) / self.zoom, (at.y - c.y - self.offset.y) / self.zoom)
    }

    pub fn zoom(&self) -> f32 {
        self.zoom
    }

    /// The whole picture in view, no bigger than it really is.
    pub fn fit(&mut self) {
        let (pw, ph) = picture_size(&self.image, self.dpr);
        let c = self.canvas_rect();
        self.zoom = 1.0f32.min((c.w - 32.0) / pw.max(1.0)).min((c.h - 32.0) / ph.max(1.0)).max(ZOOM.0);
        self.offset = Point::new(((c.w - pw * self.zoom) / 2.0).round(), ((c.h - ph * self.zoom) / 2.0).round());
        self.fitted = true;
        self.damaged = true;
    }

    /// Zoom, keeping the point under `around` (window coordinates; the
    /// canvas's middle by default) where it is.
    pub fn zoom_to(&mut self, zoom: f32, around: Option<Point>) {
        let c = self.canvas_rect();
        let around = around.unwrap_or(c.center());
        let at = self.to_picture(around);
        self.zoom = zoom.clamp(ZOOM.0, ZOOM.1);
        self.offset = Point::new(around.x - c.x - at.x * self.zoom, around.y - c.y - at.y * self.zoom);
        self.fitted = false;
        self.keep_in_view();
        self.damaged = true;
    }

    fn pan_by(&mut self, d: Point) {
        self.offset += d;
        self.fitted = false;
        self.keep_in_view();
        self.damaged = true;
    }

    /// At least a bit of the picture stays in the window.
    fn keep_in_view(&mut self) {
        let (pw, ph) = picture_size(&self.image, self.dpr);
        let c = self.canvas_rect();
        let (w, h) = (pw * self.zoom, ph * self.zoom);
        self.offset = Point::new(self.offset.x.clamp(KEEP - w, (c.w - KEEP).max(KEEP - w)), self.offset.y.clamp(KEEP - h, (c.h - KEEP).max(KEEP - h)));
    }

    // -- input -----------------------------------------------------------------------------

    pub fn cursor(&self) -> CursorShape {
        let at = self.pointer.unwrap_or(Point::ZERO);
        if self.asking.is_some() || at.y < BAR_H || at.y > self.view.1 - STATUS_H || self.picker.as_ref().is_some_and(|p| p.rect().contains(at)) {
            return CursorShape::Arrow;
        }
        if self.pan.is_some() || self.text_drag.is_some_and(|_| self.text_edit.is_some()) {
            CursorShape::Move
        } else if self.space {
            CursorShape::Hand
        } else if self.tool == Tool::Text && !self.eyedropper {
            CursorShape::Text
        } else {
            CursorShape::Crosshair
        }
    }

    pub fn pointer_motion(&mut self, at: Point, mods: i32) {
        self.pointer = Some(at);
        self.mods = mods;
        if let Some(answer) = &mut self.asking {
            let now = ask_button_at(self.view, at);
            if *answer != now {
                *answer = now;
                self.damaged = true;
            }
            return;
        }
        let hovered = self.bar_item_at(at);
        if hovered != self.hovered {
            self.hovered = hovered;
            self.damaged = true;
        }
        if let Some(p) = &mut self.picker
            && (self.pressed == Some("picker") || p.rect().contains(at))
        {
            let ev = p.motion(at, self.pressed == Some("picker"));
            self.picker_event(ev);
            return;
        }
        if let Some((start, offset)) = self.pan {
            self.offset = offset + (at - start);
            self.fitted = false;
            self.keep_in_view();
            self.damaged = true;
            return;
        }
        let pic = self.to_picture(at);
        if let Some((start, origin)) = self.text_drag
            && let Some(t) = self.text_edit.as_mut().and_then(|s| s.as_text_mut())
        {
            if (pic - start).manhattan() * self.zoom > 3.0 || t.pos != origin {
                t.pos = origin + (pic - start);
                self.damaged = true;
            }
            return;
        }
        if let Some(s) = &mut self.active {
            s.extend(pic, Mods::from_keys(mods));
            self.damaged = true;
        }
    }

    pub fn pointer_leave(&mut self) {
        self.pointer = None;
        if self.hovered.take().is_some() {
            self.damaged = true;
        }
    }

    pub fn button(&mut self, button: Button, pressed: bool, at: Point, mods: i32) {
        self.pointer = Some(at);
        self.mods = mods;
        if pressed { self.press(button, at, mods) } else { self.release(button, at) }
    }

    fn press(&mut self, button: Button, at: Point, mods: i32) {
        if self.asking.is_some() {
            if button == Button::Left
                && let Some(a) = ask_button_at(self.view, at)
            {
                self.answer(a);
            }
            return;
        }
        if let Some(p) = &mut self.picker {
            if p.rect().contains(at) {
                self.pressed = Some("picker");
                let ev = p.press(at, mods & keyseq::SHIFT != 0);
                self.picker_event(ev);
                return;
            }
            if self.bar_item_at(at) != Some("color.8") {
                self.close_picker();
            }
        }
        if at.y < BAR_H {
            if button == Button::Left {
                self.pressed = self.bar_item_at(at);
            }
            return;
        }
        if at.y > self.view.1 - STATUS_H {
            return;
        }
        if button == Button::Middle || (button == Button::Left && self.space) {
            self.pan = Some((at, self.offset));
            return;
        }
        if button != Button::Left {
            return;
        }
        let pic = self.to_picture(at);
        if self.eyedropper {
            self.take_color(pic);
            return;
        }
        let shift = mods & keyseq::SHIFT != 0;
        if let Some(t) = self.text_edit.as_mut().and_then(|s| {
            let size = s.size;
            s.as_text_mut().map(|t| (t, size))
        }) && t.0.contains(pic, t.1)
        {
            // In the text being typed: the caret goes there (Shift: selects
            // to there); a drag moves the text.
            let (t, size) = t;
            let i = t.index_at(pic, size);
            t.edit.place(i, shift);
            self.text_drag = Some((pic, t.pos));
            self.damaged = true;
            return;
        }
        if self.commit_text() && self.tool == Tool::Text && self.text_at(pic).is_none() {
            return; // (the first click finishes the text being typed)
        }
        match self.tool {
            Tool::Text => {
                let shape = match self.text_at(pic) {
                    Some(i) => {
                        // edit it again (a copy, so undo brings the original back)
                        let Some(mut s) = copy_for_editing(&self.shapes[i].1, i) else { return };
                        let size = s.size;
                        if let Some(t) = s.as_text_mut() {
                            let idx = t.index_at(pic, size);
                            t.edit.place(idx, false);
                            self.text_drag = Some((pic, t.pos));
                        }
                        s
                    }
                    None => Shape::text(pic, self.color(), self.size),
                };
                self.begin_text(shape);
            }
            Tool::Counter => {
                let n = self.next_number();
                self.commit(Shape::counter(pic, self.color(), self.size, n));
            }
            tool => self.active = Shape::create(tool, pic, self.color(), self.size),
        }
        self.damaged = true;
    }

    fn release(&mut self, button: Button, at: Point) {
        if self.pressed == Some("picker") {
            self.pressed = None;
            if let Some(p) = &mut self.picker {
                let ev = p.release(at);
                self.picker_event(ev);
            }
            return;
        }
        if let Some(id) = self.pressed.take() {
            if self.bar_item_at(at) == Some(id) {
                self.bar_click(id);
            }
            return;
        }
        if self.pan.is_some() && matches!(button, Button::Middle | Button::Left) {
            self.pan = None;
            return;
        }
        if button != Button::Left {
            return;
        }
        if self.text_drag.take().is_some() {
            return;
        }
        if let Some(shape) = self.active.take() {
            if shape.is_valid() {
                self.commit(shape);
            }
            self.damaged = true;
        }
    }

    /// Wheel notches (up positive): scroll the picture; Ctrl zooms around
    /// the pointer, Shift scrolls sideways.
    pub fn scroll(&mut self, steps: f32) {
        if self.asking.is_some() {
            return;
        }
        if self.mods & keyseq::CTRL != 0 {
            let around = self.pointer;
            self.zoom_to(self.zoom * 1.15f32.powf(steps), around);
        } else if self.mods & keyseq::SHIFT != 0 {
            self.pan_by(Point::new(steps * 48.0, 0.0));
        } else {
            self.pan_by(Point::new(0.0, steps * 48.0));
        }
    }

    fn take_color(&mut self, pic: Point) {
        let (w, h) = self.image.size();
        let x = ((pic.x * self.dpr) as i64).clamp(0, w as i64 - 1);
        let y = ((pic.y * self.dpr) as i64).clamp(0, h as i64 - 1);
        let (r, g, b, _) = crate::image::unpack(self.image.pixel(x as u32, y as u32));
        self.eyedropper = false;
        self.custom_color = Color::rgba(r, g, b, 255);
        self.set_color(CUSTOM);
        self.toggle_picker();
    }

    pub fn modifiers(&mut self, mods: i32) {
        if mods == self.mods {
            return;
        }
        self.mods = mods;
        // Shift, Ctrl or Alt while drawing a shape: it changes at once.
        if let Some(s) = &mut self.active
            && let Some(at) = s.drag().map(|d| d.pointer)
        {
            let mut m = Mods::from_keys(mods);
            m.moving = false; // (moving needs the pointer to move)
            s.extend(at, m);
            self.damaged = true;
        }
    }

    pub fn key(&mut self, k: &KeyPress) {
        if self.asking.is_some() {
            match k.key {
                key::ESCAPE => self.answer(Answer::Cancel),
                key::RETURN | key::ENTER => self.answer(Answer::Save),
                x if x == 'D' as i32 || (x == 'W' as i32 && k.ctrl()) => self.answer(Answer::Discard),
                _ => {}
            }
            return;
        }
        if let Some(p) = &mut self.picker
            && p.hex.focused
        {
            let ev = p.key(k);
            self.picker_event(ev);
            return;
        }
        if let Some(shape) = &mut self.text_edit {
            let action = if k.ctrl() { self.keymap.action(k, Where::Editor) } else { None };
            let size = shape.size;
            let Some(t) = shape.as_text_mut() else { return };
            let mut used = matches!(action, Some("undo") | Some("redo")) && t.edit.undo_edit(action == Some("redo"));
            if !used {
                match t.key(k, size) {
                    KeyResult::Ignored => {}
                    KeyResult::Used => used = true,
                    KeyResult::Commit => {
                        self.commit_text();
                        used = true;
                    }
                    KeyResult::Copy(s) => {
                        self.requests.push(Request::CopyText(s));
                        used = true;
                    }
                    KeyResult::Paste => {
                        self.requests.push(Request::Paste);
                        used = true;
                    }
                }
            }
            if used {
                self.damaged = true;
                return;
            }
        }
        match k.key {
            key::SPACE if !k.repeat => self.space = true,
            key::ESCAPE => {
                if self.eyedropper {
                    self.close_picker();
                } else {
                    self.close_picker();
                    if self.active.take().is_some() {
                        self.damaged = true;
                    } else if !self.commit_text() {
                        self.close();
                    }
                }
            }
            _ => {
                if let Some(action) = self.keymap.action(k, Where::Editor) {
                    self.do_action(action);
                } else if k.ctrl() && k.key == 'Y' as i32 {
                    self.redo(); // (besides the Redo key)
                } else if k.ctrl() && k.key == '+' as i32 {
                    self.zoom_to(self.zoom * 1.25, None); // (Ctrl+= typed with Shift)
                }
            }
        }
    }

    pub fn key_release(&mut self, k: &KeyPress) {
        if k.key == key::SPACE && !k.repeat {
            self.space = false;
        }
    }

    fn do_action(&mut self, action: &str) {
        match action {
            "undo" => self.undo(),
            "redo" => self.redo(),
            "save" => {
                self.save();
            }
            "save_as" => self.save_as(),
            "copy" => self.copy(),
            "fit" => self.fit(),
            "actual_size" => self.zoom_to(1.0, None),
            "zoom_in" => self.zoom_to(self.zoom * 1.25, None),
            "zoom_out" => self.zoom_to(self.zoom / 1.25, None),
            "close" => self.close(),
            "size.down" => self.set_size(self.size.saturating_sub(1)),
            "size.up" => self.set_size(self.size + 1),
            _ => {
                if let Some(t) = action.strip_prefix("tool.").and_then(Tool::from_id) {
                    self.set_tool(t);
                } else if let Some(i) = action.strip_prefix("color.").and_then(|n| n.parse::<usize>().ok()) {
                    self.set_color(i.saturating_sub(1));
                }
            }
        }
    }

    /// Text from an input method (compose, dead keys, CJK, emoji).
    pub fn commit_string(&mut self, s: &str) {
        if let Some(t) = self.text_edit.as_mut().and_then(|s| s.as_text_mut()) {
            t.edit.insert(s);
            self.damaged = true;
        } else if let Some(p) = &mut self.picker
            && p.hex.focused
        {
            let ev = p.paste(s);
            self.picker_event(ev);
        }
    }

    /// The clipboard's text, asked for with Request::Paste.
    pub fn paste(&mut self, s: &str) {
        if let Some(t) = self.text_edit.as_mut().and_then(|s| s.as_text_mut()) {
            t.edit.paste(s);
            self.damaged = true;
        } else if let Some(p) = &mut self.picker {
            let ev = p.paste(s);
            self.picker_event(ev);
        }
    }

    /// Where the caret of the text being typed is (window logical), for
    /// the input method.
    pub fn text_input(&self) -> Option<Rect> {
        let s = self.text_edit.as_ref()?;
        let r = s.as_text()?.caret_rect(s.size);
        let c = self.canvas_rect();
        Some(Rect::new(c.x + self.offset.x + r.x * self.zoom, c.y + self.offset.y + r.y * self.zoom, r.w * self.zoom, r.h * self.zoom))
    }

    pub fn wants_text(&self) -> bool {
        self.text_edit.is_some() || self.picker.as_ref().is_some_and(|p| p.hex.focused)
    }

    // -- output ----------------------------------------------------------------------------

    /// The picture with the drawings on it, at full resolution.
    pub fn render(&mut self) -> Result<Image, String> {
        self.commit_text();
        let mut out = (*self.image).clone();
        if self.shapes.iter().any(|(_, s)| !s.hidden) {
            let mut p = Painter::new(out.pixmap_mut(), self.dpr);
            for (_, s) in self.shapes.iter().filter(|(_, s)| !s.hidden) {
                s.paint(&mut p, &self.image, self.dpr);
            }
        }
        if !self.alpha {
            out.force_opaque();
        }
        Ok(out)
    }

    fn say(&mut self, message: String) {
        self.status = Some((message, Instant::now()));
        self.damaged = true;
    }

    pub fn copy(&mut self) {
        let clock = crate::timing::Clock::new("editor copy");
        let png = self.render().and_then(|img| crate::codec::encode_png(&img, self.alpha));
        match png {
            Ok(png) => {
                let mut clock = clock;
                clock.step("encoded", format!("{} KiB", png.len() / 1024));
                self.requests.push(Request::CopyImage(Arc::new(png)));
                self.copied_until = Some(Instant::now() + COPIED_FOR);
                self.say("Copied to the clipboard".into());
                self.refresh_bar();
            }
            Err(e) => self.say(format!("Couldn't copy: {e}")),
        }
    }

    /// Save back to the file. False (and why, in the status line) if it couldn't.
    pub fn save(&mut self) -> bool {
        let target = match self.target.take() {
            Some(t) if t.is_for(&self.path) => Ok(t),
            _ => SaveTarget::open(&self.path),
        };
        self.save_in(target)
    }

    pub fn save_as(&mut self) {
        self.commit_text();
        self.requests.push(Request::SaveAs);
    }

    /// Save to `path` (chosen just now: Save as).
    pub fn save_to(&mut self, path: &Path) -> bool {
        self.save_in(SaveTarget::open(path))
    }

    fn save_in(&mut self, target: Result<SaveTarget, String>) -> bool {
        let mut clock = crate::timing::Clock::new("editor save");
        let written = target.and_then(|target| {
            let img = self.render()?;
            clock.step("drawn", format!("{} × {}, {} drawings", img.width(), img.height(), self.shapes.len()));
            let result = write_picture(&img, &target, &self.cfg, self.alpha);
            self.target = Some(target);
            result
        });
        match written {
            Ok(()) => {
                let path = self.target.as_ref().map(|t| t.path.clone()).unwrap_or_default();
                clock.step("saved", path.display().to_string());
                self.say(format!("Saved to {}", path.display()));
                self.path = path;
                self.saved = self.shapes.iter().map(|(id, _)| *id).collect();
                self.dirty = false;
                self.refresh_bar();
                true
            }
            Err(e) => {
                self.say(format!("Couldn't save: {e}"));
                false
            }
        }
    }

    /// Close the window (asking first, inside it, when there are unsaved
    /// changes).
    pub fn close(&mut self) {
        self.commit_text();
        self.close_picker();
        if self.dirty && !self.closing {
            self.asking = Some(None);
            self.damaged = true;
            return;
        }
        self.closing = true;
        self.requests.push(Request::Close);
    }

    pub fn is_asking(&self) -> bool {
        self.asking.is_some()
    }

    fn answer(&mut self, a: Answer) {
        self.asking = None;
        self.damaged = true;
        match a {
            Answer::Save => {
                if self.save() {
                    self.closing = true;
                    self.requests.push(Request::Close);
                }
            }
            Answer::Discard => {
                self.closing = true;
                self.requests.push(Request::Close);
            }
            Answer::Cancel => {}
        }
    }

    // -- painting --------------------------------------------------------------------------

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let c = theme::c();
        self.paint_canvas(target, origin, region);
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        // The bar.
        p.fill_rect(Rect::new(0.0, 0.0, self.view.0, BAR_H), c.base);
        p.fill_rect(Rect::new(0.0, BAR_H - 1.0, self.view.0, 1.0), c.line);
        let ptr = Pointer { hovered: self.hovered, pressed: self.pressed };
        for it in &self.bar {
            paint_item(&mut p, it, Point::ZERO, &ptr);
        }
        for b in &self.actions {
            b.paint(&mut p, self.hovered == Some(b.id));
        }
        // The status line: a hint (or what just happened), the size and zoom.
        let top = self.view.1 - STATUS_H;
        p.fill_rect(Rect::new(0.0, top, self.view.0, STATUS_H), c.base);
        p.fill_rect(Rect::new(0.0, top, self.view.0, 1.0), c.line);
        let f = text::font(12.0, text::NORMAL);
        let tip = self.hovered.and_then(|id| {
            self.bar.iter().find(|it| it.id == id).map(|it| it.hint.clone()).or_else(|| self.actions.iter().find(|b| b.id == id).map(|b| b.hint.clone()))
        });
        let (message, color) = match (&self.status, tip) {
            (Some((s, _)), _) => (s.clone(), c.soft),
            (None, Some(t)) => (t, c.muted),
            (None, None) => {
                let text = if self.eyedropper {
                    "Click a pixel to make it your colour  ·  Esc to stop"
                } else if self.active.as_ref().is_some_and(|s| s.drag().is_some()) {
                    SHAPE_HINT
                } else if self.tool == Tool::Text {
                    TEXT_HINT
                } else {
                    HINT
                };
                (text.to_string(), c.muted)
            }
        };
        let right = format!("{} × {}   {}%", self.image.width(), self.image.height(), (self.zoom * 100.0).round());
        let rw = text::width(&right, f);
        let left_w = (self.view.0 - rw - 36.0).max(10.0);
        p.draw_text(Rect::new(12.0, top, left_w, STATUS_H), Align::Left, &text::elide_end(&message, f, left_w), f, color);
        p.draw_text(Rect::new(self.view.0 - rw - 12.0, top, rw, STATUS_H), Align::Left, &right, f, c.muted);
        if let Some(picker) = &self.picker {
            picker.paint(&mut p);
        }
        if let Some(hovered) = self.asking {
            self.paint_ask(&mut p, hovered);
        }
    }

    /// The picture and its drawings, moved and zoomed.
    fn paint_canvas(&self, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let c = theme::c();
        let area = self.canvas_rect();
        let s = self.scale;
        let dev = IRect::new((area.x * s).round() as i32, (area.y * s).round() as i32, (area.w * s).round() as i32, (area.h * s).round() as i32);
        // The picture and what's committed: kept as drawn until they or the view change.
        let key = (
            (dev.w.max(1)) as u32,
            (dev.h.max(1)) as u32,
            (self.offset.x * 64.0) as i64,
            (self.offset.y * 64.0) as i64,
            (self.zoom * 4096.0) as i64,
            self.shapes_version,
        );
        {
            let mut layer = self.layer.borrow_mut();
            if layer.as_ref().is_none_or(|(k, _)| *k != key)
                && let Ok(mut img) = Image::filled(key.0, key.1, c.ink.premul())
            {
                self.draw_picture(&mut img);
                *layer = Some((key, img));
            }
        }
        if let Some((_, img)) = self.layer.borrow().as_ref() {
            let mut p = Painter::for_region(target.pixmap_mut(), 1.0, origin);
            p.blit_image(img, dev.x, dev.y);
        }
        let _ = region;
        // What's being drawn now.
        if self.active.is_some() || self.text_edit.is_some() {
            let mut p = Painter::for_region(target.pixmap_mut(), s, origin);
            p.clip_rect(area);
            p.translate(area.x + self.offset.x, area.y + self.offset.y);
            p.scale_by(self.zoom, self.zoom);
            let (pw, ph) = picture_size(&self.image, self.dpr);
            p.clip_rect(Rect::new(0.0, 0.0, pw, ph));
            for shape in self.active.iter().chain(self.text_edit.iter()) {
                shape.paint(&mut p, &self.image, self.dpr);
            }
        }
        if self.shapes.is_empty() && self.active.is_none() && self.text_edit.is_none() {
            // Until something is drawn: what to do, quietly, at the bottom.
            let mut p = Painter::for_region(target.pixmap_mut(), s, origin);
            let f = text::font(12.0, MEDIUM);
            let tip = "Pick a tool and draw  ·  Ctrl+S saves";
            let w = text::width(tip, f) + 28.0;
            let bx = Rect::new((area.w - w) / 2.0, area.y + area.h - 44.0, w, 28.0);
            p.draw_rounded_rect(bx, 8.0, Some(c.base), Some(Pen::new(c.line, 1.0)));
            p.draw_text(bx, Align::Center, tip, f, c.soft);
        }
    }

    /// The picture at the view's zoom, with its frame and the drawings, on `img` (the canvas, device pixels).
    fn draw_picture(&self, img: &mut Image) {
        let c = theme::c();
        let (pw, ph) = picture_size(&self.image, self.dpr);
        let mut p = Painter::new(img.pixmap_mut(), self.scale);
        let frame = Rect::new(self.offset.x, self.offset.y, pw * self.zoom, ph * self.zoom);
        p.stroke_rect(frame.adjusted(-0.5, -0.5, 0.5, 0.5), Pen::new(c.line, 1.0));
        // Close up, whole pixels (as a paint program shows them); further out, smooth.
        p.draw_image(&self.image, frame, self.image.rect().to_f(), self.zoom * self.scale < self.dpr);
        if self.shapes.iter().any(|(_, s)| !s.hidden) {
            p.translate(self.offset.x, self.offset.y);
            p.scale_by(self.zoom, self.zoom);
            // Only what's on the picture shows: nothing past its edges is saved.
            p.clip_rect(Rect::new(0.0, 0.0, pw, ph));
            for (_, s) in self.shapes.iter().filter(|(_, s)| !s.hidden) {
                s.paint(&mut p, &self.image, self.dpr);
            }
        }
    }

    fn paint_ask(&self, p: &mut Painter<'_>, hovered: Option<Answer>) {
        let c = theme::c();
        p.fill_rect(Rect::new(0.0, 0.0, self.view.0, self.view.1), c.dim.with_alpha(150));
        let card = ask_card(self.view);
        p.draw_rounded_rect(card.adjusted(0.5, 0.5, -0.5, -0.5), 14.0, Some(c.base), Some(Pen::new(c.line, 1.0)));
        let inner = card.adjusted(22.0, 20.0, -22.0, 0.0);
        let title_font = text::font(15.0, DEMIBOLD);
        let name = self.path.file_name().map(|n| crate::util::displayable(&n.to_string_lossy())).unwrap_or_default();
        let title = text::elide_middle(&format!("Save the changes to {name}?"), title_font, inner.w);
        p.draw_text(Rect::new(inner.x, inner.y, inner.w, 24.0), Align::Left, &title, title_font, c.text);
        let body = text::font(13.0, text::NORMAL);
        p.draw_text(Rect::new(inner.x, inner.y + 32.0, inner.w, 24.0), Align::Left, "Your drawings will be lost if you don't save them.", body, c.soft);
        for (a, r, label, icon) in ask_buttons(self.view) {
            let b = ActionButton { id: "", text: label, icon, strong: a == Answer::Save, hint: String::new(), rect: r, width: r.w };
            b.paint(p, hovered == Some(a));
        }
    }
}

fn ask_card(view: (f32, f32)) -> Rect {
    let w = 380.0f32.max(ask_widths().iter().sum::<f32>() + 16.0 * 2.0 + 8.0 * 2.0).min(view.0 - 32.0);
    let h = 168.0;
    Rect::new(((view.0 - w) / 2.0).round(), ((view.1 - h) / 2.0).round(), w, h)
}

fn ask_widths() -> [f32; 3] {
    ["Don't save", "Cancel", "Save"].map(|w| text::width(w, action_font()).round() + 46.0)
}

fn ask_buttons(view: (f32, f32)) -> Vec<(Answer, Rect, &'static str, &'static str)> {
    let card = ask_card(view);
    let widths = ask_widths();
    let mut x = card.right() - 20.0;
    let mut out = Vec::new();
    for (i, (a, label, icon)) in
        [(Answer::Discard, "Don't save", "trash"), (Answer::Cancel, "Cancel", "close"), (Answer::Save, "Save", "save")].into_iter().enumerate().rev()
    {
        x -= widths[i];
        out.push((a, Rect::new(x, card.bottom() - 20.0 - 34.0, widths[i], 34.0), label, icon));
        x -= 8.0;
    }
    out
}

fn ask_button_at(view: (f32, f32), at: Point) -> Option<Answer> {
    ask_buttons(view).into_iter().find(|(_, r, ..)| r.contains(at)).map(|(a, ..)| a)
}

/// A file to save to, in a folder held open from the moment the file was
/// chosen: a link put in the file's place later is replaced, never
/// followed, and the folder can't be swapped for another.
pub struct SaveTarget {
    /// with the links it had when chosen resolved (one the user opened a
    /// picture through saves to that picture)
    pub path: PathBuf,
    /// as it was given
    given: PathBuf,
    dir: std::os::fd::OwnedFd,
    name: std::ffi::OsString,
}

impl SaveTarget {
    pub fn open(path: &Path) -> Result<SaveTarget, String> {
        use rustix::fs::{Mode, OFlags};
        let resolved = match std::fs::canonicalize(path) {
            Ok(p) => p,
            Err(_) => {
                // (a new file: its folder resolved)
                let parent = path.parent().filter(|d| !d.as_os_str().is_empty()).unwrap_or(Path::new("."));
                let name = path.file_name().ok_or_else(|| format!("{} names no file", path.display()))?;
                std::fs::canonicalize(parent).map_err(|e| format!("{}: {e}", parent.display()))?.join(name)
            }
        };
        let (Some(parent), Some(name)) = (resolved.parent(), resolved.file_name()) else {
            return Err(format!("{} names no file", path.display()));
        };
        let dir =
            rustix::fs::open(parent, OFlags::PATH | OFlags::DIRECTORY | OFlags::CLOEXEC, Mode::empty()).map_err(|e| format!("{}: {e}", parent.display()))?;
        let name = name.to_os_string();
        Ok(SaveTarget { path: resolved, given: path.to_path_buf(), dir, name })
    }

    /// Is this where `path` saves?
    pub fn is_for(&self, path: &Path) -> bool {
        self.path == path || self.given == path
    }

    /// Put `bytes` in the file: written to a new file beside it, then
    /// renamed into its place, so a failed write never leaves half a file.
    pub fn write(&self, bytes: &[u8]) -> Result<(), String> {
        use rustix::fs::{AtFlags, FileType, Mode, OFlags};
        use std::io::Write;
        let fail = |e: &dyn std::fmt::Display| format!("could not write {}: {e}", self.path.display());
        let temp = format!(".{}.{}.tmp", self.name.to_string_lossy(), crate::util::random_hex(4));
        let flags = OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC;
        let fd = rustix::fs::openat(&self.dir, temp.as_str(), flags, Mode::from_raw_mode(0o600)).map_err(|e| fail(&e))?;
        // (the file keeps who may read it)
        if let Ok(st) = rustix::fs::statat(&self.dir, self.name.as_os_str(), AtFlags::SYMLINK_NOFOLLOW)
            && FileType::from_raw_mode(st.st_mode) == FileType::RegularFile
        {
            let _ = rustix::fs::fchmod(&fd, Mode::from_raw_mode(st.st_mode & 0o777));
        }
        let written = std::fs::File::from(fd).write_all(bytes);
        let placed =
            written.map_err(|e| fail(&e)).and_then(|()| rustix::fs::renameat(&self.dir, temp.as_str(), &self.dir, self.name.as_os_str()).map_err(|e| fail(&e)));
        if placed.is_err() {
            let _ = rustix::fs::unlinkat(&self.dir, temp.as_str(), AtFlags::empty());
        }
        placed
    }
}

/// Write `img` to `target` in the format its extension names (PNG if none
/// can be written here).
pub fn write_picture(img: &Image, target: &SaveTarget, cfg: &Config, alpha: bool) -> Result<(), String> {
    let ext = target.path.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = if ext == "jpeg" { "jpg".to_string() } else { ext };
    let format = crate::codec::writable_formats().iter().find(|f| f.0 == ext).map(|f| f.0).unwrap_or("png");
    let bytes = if format == "png" { crate::codec::encode_png(img, alpha)? } else { crate::codec::encode(img, format, cfg.quality.clamp(1, 100) as u8)? };
    target.write(&bytes)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn editor() -> Editor {
        let img = Image::filled(400, 300, 0xFF20_4060).unwrap();
        let mut ed = Editor::new(img, 1.0, PathBuf::from("/tmp/flatshot-editor-test.png"), Config::from_map(&Default::default()));
        ed.set_view(900.0, 600.0);
        ed
    }

    fn at(ed: &Editor, x: f32, y: f32) -> Point {
        // picture coordinates to window coordinates
        let c = ed.canvas_rect();
        Point::new(c.x + ed.offset.x + x * ed.zoom, c.y + ed.offset.y + y * ed.zoom)
    }

    #[test]
    fn saving_follows_no_link_put_in_later() {
        let base = std::env::temp_dir().join(format!("flatshot-save-{}", crate::util::random_hex(4)));
        let (dir, elsewhere) = (base.join("shared"), base.join("home"));
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::create_dir_all(&elsewhere).unwrap();
        let secret = elsewhere.join("secret");
        std::fs::write(&secret, "keep me").unwrap();
        let pic = dir.join("pic.png");
        std::fs::write(&pic, "old").unwrap();
        let target = SaveTarget::open(&pic).unwrap();
        // The file swapped for a link: the link is replaced, not followed.
        std::fs::remove_file(&pic).unwrap();
        std::os::unix::fs::symlink(&secret, &pic).unwrap();
        target.write(b"new").unwrap();
        assert_eq!(std::fs::read_to_string(&secret).unwrap(), "keep me");
        assert!(std::fs::symlink_metadata(&pic).unwrap().is_file());
        assert_eq!(std::fs::read(&pic).unwrap(), b"new");
        // The folder swapped for a link: the picture still goes where it was.
        std::fs::rename(&dir, base.join("moved")).unwrap();
        std::os::unix::fs::symlink(&elsewhere, &dir).unwrap();
        std::fs::write(elsewhere.join("pic.png"), "theirs").unwrap();
        target.write(b"newer").unwrap();
        assert_eq!(std::fs::read(base.join("moved/pic.png")).unwrap(), b"newer");
        assert_eq!(std::fs::read_to_string(elsewhere.join("pic.png")).unwrap(), "theirs");
        let left: Vec<_> = std::fs::read_dir(base.join("moved")).unwrap().flatten().map(|e| e.file_name()).collect();
        assert_eq!(left.len(), 1, "no temporary file left: {left:?}");
        let _ = std::fs::remove_dir_all(&base);
    }

    #[test]
    fn drawing_undo_and_dirty() {
        let mut ed = editor();
        assert!((ed.zoom - 1.0).abs() < 1e-6, "a small picture shows at its size");
        let (a, b) = (at(&ed, 10.0, 10.0), at(&ed, 100.0, 80.0));
        ed.set_tool(Tool::Rect);
        ed.button(Button::Left, true, a, 0);
        ed.pointer_motion(b, 0);
        ed.button(Button::Left, false, b, 0);
        assert_eq!(ed.shapes.len(), 1);
        assert!(ed.dirty && ed.title().starts_with("● "));
        ed.undo();
        assert!(!ed.dirty, "undone back to as saved");
        ed.redo();
        assert!(ed.dirty);
        // A counter is a click; text is typed.
        ed.set_tool(Tool::Counter);
        ed.button(Button::Left, true, at(&ed, 50.0, 50.0), 0);
        ed.button(Button::Left, false, at(&ed, 50.0, 50.0), 0);
        assert_eq!(ed.shapes.len(), 2);
        ed.set_tool(Tool::Text);
        ed.button(Button::Left, true, at(&ed, 200.0, 200.0), 0);
        ed.button(Button::Left, false, at(&ed, 200.0, 200.0), 0);
        ed.commit_string("hi");
        assert!(ed.wants_text());
        ed.key(&KeyPress { key: key::ESCAPE, ..Default::default() });
        assert_eq!(ed.shapes.len(), 3);
        let img = ed.render().unwrap();
        assert_eq!(img.size(), (400, 300));
        assert_ne!(img.pixel(10, 40), 0xFF20_4060, "the rectangle's edge is drawn");
    }

    #[test]
    fn zoom_and_pan() {
        let mut ed = editor();
        let before = ed.to_picture(Point::new(300.0, 300.0));
        ed.zoom_to(2.0, Some(Point::new(300.0, 300.0)));
        let after = ed.to_picture(Point::new(300.0, 300.0));
        assert!((before.x - after.x).abs() < 0.01 && (before.y - after.y).abs() < 0.01, "the point under the pointer stays");
        ed.pan_by(Point::new(-100000.0, 0.0));
        assert!(ed.offset.x >= KEEP - 400.0 * ed.zoom - 0.01, "some of the picture stays in view");
        ed.fit();
        assert!(ed.fitted);
    }

    #[test]
    fn asking_before_closing() {
        let mut ed = editor();
        ed.close();
        assert!(matches!(ed.take_requests()[..], [Request::Close]), "nothing to save: it just closes");
        let mut ed = editor();
        ed.set_tool(Tool::Counter);
        ed.button(Button::Left, true, at(&ed, 5.0, 5.0), 0);
        ed.close();
        assert!(ed.is_asking() && ed.take_requests().is_empty());
        ed.key(&KeyPress { key: key::ESCAPE, ..Default::default() });
        assert!(!ed.is_asking(), "Esc: cancel");
        ed.close();
        ed.key(&KeyPress { key: 'D' as i32, ..Default::default() });
        assert!(matches!(ed.take_requests()[..], [Request::Close]), "D: don't save");
    }

    #[test]
    fn saving_writes_in_place() {
        let dir = std::env::temp_dir().join(format!("flatshot-editor-{}", crate::util::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("shot.png");
        std::fs::write(&path, b"old").unwrap();
        let mut ed = editor();
        ed.path = path.clone();
        ed.set_tool(Tool::Counter);
        ed.button(Button::Left, true, at(&ed, 30.0, 30.0), 0);
        assert!(ed.save());
        assert!(!ed.dirty);
        let back = crate::codec::decode_file(&path).unwrap();
        assert_eq!(back.size(), (400, 300));
        assert_eq!(std::fs::read_dir(&dir).unwrap().count(), 1, "no temporary file left");
        let _ = std::fs::remove_dir_all(&dir);
    }
}
