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

use crate::beautify::{self, Style};
use crate::config::Config;
use crate::geom::IPoint;
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::{Canvas, Image};
use crate::keys::{KeyPress, Keymap, Where};
use crate::keyseq::{self, key};
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD, MEDIUM};
use crate::render::theme::{self, Color, SIZES, SWATCHES};
use crate::render::{Painter, icons};
use crate::ui::beautypanel::{self, Event as PanelEvent};
use crate::ui::cropper::{Cropper, Done};
use crate::ui::panels::{ColorPicker, LineEdit, PICKER_SIZE, PickerEvent};
use crate::ui::picture::{self, Moved};
use crate::ui::selection::{Drawings, DrawingsMut, Selection};
use crate::ui::session::{Button, CursorShape};
use crate::ui::shapes::{self, Mods, Shape, TOOLS, Tool, copy_for_editing};
use crate::ui::snapping::EdgeMaps;
use crate::ui::textedit::KeyResult;
use crate::ui::widgets::{Item, Look, Pointer, layout_row, paint_item};

pub const BAR_H: f32 = 48.0;
/// The bar under it while cropping: the picture's own changes.
pub const PICTURE_BAR_H: f32 = 44.0;
pub const STATUS_H: f32 = 34.0;
/// The zoom menu's, in %.
const ZOOM_STEPS: [u32; 10] = [10, 25, 50, 75, 100, 150, 200, 300, 400, 800];
/// The smallest the canvas gets.
const MIN_CANVAS: (f32, f32) = (200.0, 150.0);
const ZOOM: (f32, f32) = (0.05, 32.0);
/// How much of the picture always stays in view.
const KEEP: f32 = 48.0;
const CUSTOM: usize = SWATCHES.len();
const SAY_FOR: Duration = Duration::from_secs(4);
const COPIED_FOR: Duration = Duration::from_secs(2);
/// How long after the last change the style becomes the last used.
const REMEMBER_AFTER: Duration = Duration::from_millis(400);

const SHAPE_HINT: &str = "Shift: square  ·  Ctrl: from the middle  ·  Alt: move it";
const CROP_HINT: &str =
    "Drag the edges to crop, past the picture for a margin  ·  Shift keeps the shape  ·  Ctrl: from the middle, without snapping  ·  Enter crops";
const CUT_HINT: &str = "Drag across the {what} to cut out; the rest is joined  ·  Esc to stop";
const SELECT_HINT: &str = "Click a drawing to select it (Alt: the one under it)  ·  Drag to move  ·  Handles resize  ·  Delete removes it";
const SELECTED_HINT: &str =
    "Drag to move (Shift: straight)  ·  Handles: Shift keeps its shape, Ctrl from the middle  ·  Arrows nudge  ·  Colours and sizes change it";
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
    /// ask for a picture for the background, then `background_image` it
    ChooseImage(PathBuf),
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
    enabled: bool,
}

fn action_font() -> text::FontSpec {
    text::font(13.0, DEMIBOLD)
}

impl ActionButton {
    fn new(id: &'static str, words: &[&'static str], icon: &'static str) -> ActionButton {
        let width = words.iter().map(|w| text::width(w, action_font())).fold(0.0f32, f32::max).round() + 46.0;
        ActionButton { id, text: words[0], icon, strong: false, hint: String::new(), rect: Rect::default(), width, enabled: true }
    }

    fn paint(&self, p: &mut Painter<'_>, hovered: bool) {
        let c = theme::c();
        let hovered = hovered && self.enabled;
        let was = p.opacity();
        if !self.enabled {
            p.set_opacity(was * 0.4);
        }
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
        p.set_opacity(was);
    }
}

/// The editor's drawings, as the select tool works on them.
struct Shapes<'a> {
    list: &'a mut Vec<(u64, Shape)>,
    next_id: &'a mut u64,
}

impl Drawings for Shapes<'_> {
    fn count(&self) -> usize {
        self.list.len()
    }
    fn get(&self, i: usize) -> &Shape {
        &self.list[i].1
    }
}

/// ... and only looked at.
struct ShapesRef<'a>(&'a [(u64, Shape)]);

impl Drawings for ShapesRef<'_> {
    fn count(&self) -> usize {
        self.0.len()
    }
    fn get(&self, i: usize) -> &Shape {
        &self.0[i].1
    }
}

impl DrawingsMut for Shapes<'_> {
    fn get_mut(&mut self, i: usize) -> &mut Shape {
        &mut self.list[i].1
    }
    fn commit(&mut self, shape: Shape) {
        self.list.push((*self.next_id, shape));
        *self.next_id += 1;
    }
}

/// The picture and its drawings as last drawn, at the view's zoom.
type Layer = ((u32, u32, i64, i64, i64, u64, u64), Image);
/// Beautify's background and shadow, and where the picture goes on it.
type Backdrop = ((usize, u64), Arc<Image>, IPoint);

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
    /// the picture then (crop, rotate, ... change it)
    saved_image: Arc<Image>,
    /// the crop tool's frame
    cropper: Cropper,
    /// edge maps of the picture, for the crop's snapping
    edges: RefCell<Option<(usize, Arc<EdgeMaps>)>>,
    /// while cropping: the picture's own changes, and the size to resize to
    picture_bar: Vec<Item>,
    picture_actions: Vec<ActionButton>,
    width_field: LineEdit,
    height_field: LineEdit,
    /// width and height kept together when typing one
    linked: bool,
    /// the zoom's controls at the bottom right
    zoom_bar: Vec<Item>,
    zoom_slider: Rect,
    zoom_label: Rect,
    /// the zoom's menu, open (its rows' rectangles)
    zoom_menu: Option<Vec<(Rect, Option<u32>)>>,
    /// Beautify (the Background panel): shown round the picture, and in
    /// what's saved and copied
    beauty_on: bool,
    beauty: Style,
    /// what was saved (None: off)
    saved_beauty: Option<Style>,
    panel: beautypanel::Panel,
    /// the style becomes the last used once changes stop, then
    remember_at: Option<Instant>,
    /// (changes with the style)
    beauty_version: u64,
    backdrop: RefCell<Option<Backdrop>>,
    next_id: u64,
    active: Option<Shape>,
    text_edit: Option<Shape>,
    /// the select tool's drawing
    selection: Selection,
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

fn size_field(placeholder: &str) -> LineEdit {
    let mut f = LineEdit::new(Rect::default(), "", text::font(13.0, text::NORMAL), 6);
    f.placeholder = placeholder.into();
    f
}

/// A size typed: whole pixels or a percentage (50%), as `\d{1,5}%?`.
fn size_text(s: &str) -> String {
    let digits: String = s.chars().filter(char::is_ascii_digit).take(5).collect();
    if s.contains('%') && !digits.is_empty() { digits + "%" } else { digits }
}

/// A size typed, in pixels, from `now` pixels.
fn size_value(s: &str, now: u32) -> Option<u32> {
    let s = s.trim();
    let n: u64 = s.trim_end_matches('%').parse().ok()?;
    if s.ends_with('%') {
        return Some(((now as u64 * n + 50) / 100).clamp(1, crate::image::MAX_SIDE as u64) as u32);
    }
    (n > 0).then_some(n.min(crate::image::MAX_SIDE as u64) as u32)
}

fn picture_size(img: &Image, dpr: f32) -> (f32, f32) {
    (img.width() as f32 / dpr, img.height() as f32 / dpr)
}

/// The window's first size for a picture of `size` (logical) on a monitor
/// of `monitor` (logical).
pub fn initial_size(size: (f32, f32), monitor: (f32, f32), bar: f32) -> (u32, u32) {
    // (never so narrow the bar doesn't fit)
    let w = (monitor.0 * 0.85).min(size.0 + 48.0).max(720.0).max(bar);
    let h = (monitor.1 * 0.85).min(size.1 + BAR_H + STATUS_H + 64.0).max(480.0);
    (w.round() as u32, h.round() as u32)
}

impl Editor {
    pub fn new(image: Image, dpr: f32, path: PathBuf, cfg: Config) -> Editor {
        let alpha = image.has_alpha();
        let tool = Tool::from_id(&cfg.default_tool).filter(|t| t.uses_color()).unwrap_or(Tool::Pen);
        let keymap = Keymap::new(&cfg.keys);
        let custom_color = Color::parse(&cfg.custom_color).unwrap_or(SWATCHES[0]);
        let cfg_style = cfg.beautify_style.clone();
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
            saved_image: Arc::new(Image::new(1, 1).expect("tiny")),
            cropper: Cropper::default(),
            edges: RefCell::new(None),
            picture_bar: Vec::new(),
            picture_actions: vec![
                ActionButton::new("crop.ratio", &["Free", "1:1", "4:3", "16:9", "Original"], "ratio"),
                ActionButton::new("crop.margin", &["Clear", "Colour"], "margin"),
                ActionButton::new("crop.resize", &["Resize"], "resize"),
                ActionButton::new("crop.apply", &["Crop"], "crop"),
            ],
            width_field: size_field("Width"),
            height_field: size_field("Height"),
            linked: true,
            zoom_bar: Vec::new(),
            zoom_slider: Rect::default(),
            zoom_label: Rect::default(),
            zoom_menu: None,
            beauty_on: false,
            beauty: Style::from_map(&cfg_style),
            saved_beauty: None,
            panel: beautypanel::Panel::default(),
            remember_at: None,
            beauty_version: 0,
            backdrop: RefCell::new(None),
            next_id: 1,
            active: None,
            text_edit: None,
            selection: Selection::default(),
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
                ActionButton::new("save", &["Save"], "save"),
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
        ed.saved_image = ed.image.clone();
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
        [self.status.as_ref().map(|s| s.1 + SAY_FOR), self.copied_until, self.remember_at].into_iter().flatten().min()
    }

    pub fn on_timer(&mut self, now: Instant) {
        if self.status.as_ref().is_some_and(|s| s.1 + SAY_FOR <= now) {
            self.status = None;
            self.damaged = true;
        }
        if self.remember_at.is_some_and(|t| t <= now) {
            self.remember_at = None;
            self.remember_beauty();
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
        let top = self.top_h();
        let panel = if self.beauty_on { beautypanel::WIDTH } else { 0.0 };
        Rect::new(0.0, top, (self.view.0 - panel).max(1.0), (self.view.1 - top - STATUS_H).max(1.0))
    }

    /// The bars above the canvas: the toolbar, and the picture bar while cropping.
    fn top_h(&self) -> f32 {
        BAR_H + if self.tool == Tool::Crop { PICTURE_BAR_H } else { 0.0 }
    }

    /// The smallest the window can be with nothing overlapping: the bars'
    /// widths (the picture bar's too while cropping) and the canvas's least.
    pub fn min_size(&self) -> (u32, u32) {
        let panel = if self.beauty_on { beautypanel::WIDTH } else { 0.0 };
        let mut w = self.bar_width().max(self.zoom_bar_width() + 160.0).max(MIN_CANVAS.0 + panel);
        if self.tool == Tool::Crop {
            w = w.max(self.picture_bar_width());
        }
        let h = self.top_h() + MIN_CANVAS.1 + STATUS_H;
        (w.ceil() as u32, h.ceil() as u32)
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
        let mut beauty = Item::new("beautify", Look::Icon { icon: "beautify", size: 36.0 }, km.hint("Background, padding and shadow", "beautify"));
        beauty.active = self.beauty_on;
        items.push(beauty);
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
                    // Save stands out while there's something to save, and is greyed
                    // out when there isn't; its word and icon stay the same.
                    (b.text, b.icon, b.strong, b.enabled) = ("Save", "save", dirty, dirty);
                    b.hint = if dirty { km.hint(&format!("Save to {path}"), "save") } else { format!("Saved in {path}") };
                }
            }
        }
        self.refresh_picture_bar();
        self.refresh_zoom_bar();
        self.layout_bar();
        self.damaged = true;
    }

    /// The narrowest the window can be with all of the bar showing.
    pub fn bar_width(&self) -> f32 {
        let tools: f32 = self.bar.iter().map(|it| it.size().0 + 2.0).sum();
        let actions: f32 = self.actions.iter().map(|b| b.width + 4.0).sum();
        8.0 + tools + 16.0 + actions + 8.0
    }

    fn layout_bar(&mut self) {
        layout_row(&mut self.bar, 8.0, 6.0, 36.0, 2.0);
        let mut x = self.view.0 - 8.0;
        for b in self.actions.iter_mut().rev() {
            x -= b.width;
            b.rect = Rect::new(x, 7.0, b.width, 34.0);
            x -= 4.0;
        }
        self.layout_picture_bar();
        self.layout_zoom_bar();
        self.place_panel();
    }

    fn place_panel(&mut self) {
        if self.beauty_on {
            let (rect, name) = (self.panel_rect(), self.image_name());
            self.panel.place(rect, &self.beauty, &name);
        }
    }

    /// The picture bar, while cropping: the crop's shape and margin, cutting
    /// out rows or columns, rotating, flipping, resizing, and Crop.
    fn refresh_picture_bar(&mut self) {
        let crop = &self.cropper;
        let icon = |id: &'static str, hint: &str| Item::new(id, Look::Icon { icon: id, size: 36.0 }, hint);
        let mut rows = icon("cut-rows", "Cut out rows: drag across them, and the rest is joined");
        rows.active = crop.cut == Some(true);
        let mut cols = icon("cut-cols", "Cut out columns: drag across them, and the rest is joined");
        cols.active = crop.cut == Some(false);
        let link = if self.linked { "link" } else { "unlink" };
        let mut lock = Item::new(
            "crop.link",
            Look::Icon { icon: link, size: 34.0 },
            if self.linked { "Width and height linked: the shape is kept" } else { "Width and height unlinked: each on its own" },
        );
        lock.toggled = Some(self.linked);
        self.picture_bar = vec![
            Item::new("divider.crop", Look::Divider, ""),
            rows,
            cols,
            Item::new("divider.cut", Look::Divider, ""),
            icon("rotate-left", "Rotate left"),
            icon("rotate-right", "Rotate right"),
            icon("flip-h", "Flip left to right"),
            icon("flip-v", "Flip upside down"),
            Item::new("divider.turn", Look::Divider, ""),
            lock,
        ];
        let changed = crop.changed();
        let (ratio, margin) = (crop.ratio_label(), if crop.margin_colour { "Colour" } else { "Clear" });
        for b in &mut self.picture_actions {
            match b.id {
                "crop.ratio" => {
                    b.text = ratio;
                    b.hint = "The crop's shape: click for the next. Shift keeps the shape while you drag.".into();
                }
                "crop.margin" => {
                    b.text = margin;
                    b.hint = "Past the picture's edges: a clear margin, or one in your colour".into();
                }
                "crop.resize" => b.hint = "Resize the picture to this size".into(),
                _ => {
                    b.strong = changed;
                    b.hint = "Crop to the frame  ·  Enter".into();
                }
            }
        }
        let (w, h) = self.image.size();
        for (field, now) in [(&mut self.width_field, w), (&mut self.height_field, h)] {
            if !field.focused {
                field.edit = crate::ui::textedit::TextEdit::single_line(&now.to_string(), 6);
            }
        }
    }

    /// Where the picture bar's things go (one row under the toolbar).
    fn layout_picture_bar(&mut self) {
        let (top, h) = (BAR_H + 5.0, 34.0);
        let mut x = 8.0;
        let place = |b: &mut ActionButton, x: &mut f32| {
            b.rect = Rect::new(*x, top, b.width, h);
            *x += b.width + 4.0;
        };
        let [ratio, margin, resize, apply] = &mut self.picture_actions[..] else { return };
        place(ratio, &mut x);
        place(margin, &mut x);
        // (dividers, cut, turn and flip; the link sits between the fields and Resize)
        let n = self.picture_bar.len().saturating_sub(1);
        let (items, link) = self.picture_bar.split_at_mut(n);
        x = layout_row(items, x, BAR_H + 4.0, 36.0, 4.0) + 4.0;
        self.width_field.rect = Rect::new(x, top, 64.0, h);
        x += 64.0 + 4.0 + 14.0 + 4.0;
        self.height_field.rect = Rect::new(x, top, 64.0, h);
        x += 64.0 + 4.0;
        x = layout_row(link, x, BAR_H + 5.0, 34.0, 4.0) + 4.0;
        place(resize, &mut x);
        apply.rect = Rect::new((self.view.0 - 8.0 - apply.width).max(x), top, apply.width, h);
    }

    /// The narrowest the picture bar fits in.
    fn picture_bar_width(&self) -> f32 {
        let actions: f32 = self.picture_actions.iter().map(|b| b.width + 4.0).sum();
        let items: f32 = self.picture_bar.iter().map(|it| it.size().0 + 4.0).sum();
        8.0 + actions + items + 2.0 * (64.0 + 4.0) + 14.0 + 4.0 + 16.0 + 8.0
    }

    /// The zoom's controls at the bottom right: out, a slider, in, the zoom
    /// (a menu of zooms) and fit.
    fn refresh_zoom_bar(&mut self) {
        let km = &self.keymap;
        let icon = |id: &'static str, hint: String| Item::new(id, Look::Icon { icon: id, size: 28.0 }, hint);
        let mut fit = icon("fit", km.hint("Fit the picture in the window", "fit"));
        fit.active = self.fitted;
        self.zoom_bar = vec![icon("zoom-out", km.hint("Zoom out", "zoom_out")), icon("zoom-in", km.hint("Zoom in", "zoom_in")), fit];
    }

    fn zoom_label_width() -> f32 {
        (text::width("8888%", text::font(12.0, DEMIBOLD)) + 18.0).round()
    }

    fn layout_zoom_bar(&mut self) {
        let top = self.view.1 - STATUS_H;
        let mid = top + STATUS_H / 2.0;
        let mut x = self.view.0 - 8.0;
        let [out, zoom_in, fit] = &mut self.zoom_bar[..] else { return };
        x -= 28.0;
        fit.rect = Rect::new(x, mid - 14.0, 28.0, 28.0);
        x -= 2.0 + Self::zoom_label_width();
        self.zoom_label = Rect::new(x, mid - 13.0, Self::zoom_label_width(), 26.0);
        x -= 2.0 + 28.0;
        zoom_in.rect = Rect::new(x, mid - 14.0, 28.0, 28.0);
        x -= 2.0 + 132.0;
        self.zoom_slider = Rect::new(x, mid - 12.0, 132.0, 24.0);
        x -= 2.0 + 28.0;
        out.rect = Rect::new(x, mid - 14.0, 28.0, 28.0);
    }

    fn zoom_bar_width(&self) -> f32 {
        28.0 * 3.0 + 132.0 + Self::zoom_label_width() + 4.0 * 2.0 + 8.0 + 14.0
    }

    /// Where the slider is for `zoom` (0 to 1, evenly spaced as ratios).
    fn slider_position(zoom: f32) -> f32 {
        let (lo, hi) = ZOOM;
        (zoom.clamp(lo, hi) / lo).ln() / (hi / lo).ln()
    }

    fn slider_zoom(t: f32) -> f32 {
        let (lo, hi) = ZOOM;
        lo * (hi / lo).powf(t.clamp(0.0, 1.0))
    }

    fn bar_item_at(&self, at: Point) -> Option<&'static str> {
        if let Some(menu) = &self.zoom_menu {
            return menu.iter().any(|(r, _)| r.contains(at)).then_some("zoom.menu-row");
        }
        if at.y >= self.view.1 - STATUS_H {
            if self.zoom_slider.contains(at) {
                return Some("zoom.slider");
            }
            if self.zoom_label.contains(at) {
                return Some("zoom.label");
            }
            return self.zoom_bar.iter().find(|it| it.rect.contains(at)).map(|it| it.id);
        }
        if self.tool == Tool::Crop && (BAR_H..BAR_H + PICTURE_BAR_H).contains(&at.y) {
            if let Some(b) = self.picture_actions.iter().find(|b| b.rect.contains(at) && b.enabled) {
                return Some(b.id);
            }
            if self.width_field.rect.contains(at) {
                return Some("crop.width");
            }
            if self.height_field.rect.contains(at) {
                return Some("crop.height");
            }
            return self.picture_bar.iter().find(|it| it.rect.contains(at) && it.clickable() && it.enabled).map(|it| it.id);
        }
        if at.y >= BAR_H {
            return None;
        }
        if let Some(b) = self.actions.iter().find(|b| b.rect.contains(at) && b.enabled) {
            return Some(b.id);
        }
        self.bar.iter().find(|it| it.rect.contains(at) && it.clickable() && it.enabled).map(|it| it.id)
    }

    /// Open the zoom's menu over its label: fit, then the zooms.
    fn open_zoom_menu(&mut self) {
        let rows: Vec<Option<u32>> = std::iter::once(None).chain(ZOOM_STEPS.iter().map(|z| Some(*z))).collect();
        let (w, h) = (170.0, 26.0);
        let x = (self.zoom_label.right() - w).max(4.0);
        let mut y = self.zoom_label.y - 6.0 - h * rows.len() as f32 - 8.0;
        y = y.max(4.0);
        self.zoom_menu = Some(rows.into_iter().enumerate().map(|(i, z)| (Rect::new(x + 4.0, y + 4.0 + i as f32 * h, w - 8.0, h), z)).collect());
        self.damaged = true;
    }

    fn bar_click(&mut self, id: &str) {
        match id {
            "undo" => self.undo(),
            "redo" => self.redo(),
            "fit" => self.fit(),
            "zoom-in" => self.zoom_to(self.zoom * 1.25, None),
            "zoom-out" => self.zoom_to(self.zoom / 1.25, None),
            "zoom.label" => self.open_zoom_menu(),
            "beautify" => self.toggle_beauty(),
            "crop.ratio" => {
                self.cropper.next_ratio();
                self.refresh_bar();
            }
            "crop.margin" => {
                self.cropper.margin_colour = !self.cropper.margin_colour;
                self.refresh_bar();
            }
            "crop.link" => {
                self.linked = !self.linked;
                self.refresh_bar();
            }
            "crop.resize" => self.resize_picture(),
            "crop.apply" => self.crop(),
            "cut-rows" | "cut-cols" => {
                self.cropper.set_cut(id == "cut-rows");
                self.refresh_bar();
            }
            "rotate-left" | "rotate-right" => {
                let r = picture::rotate(&self.image, self.dpr, id == "rotate-right");
                self.change_picture("Rotated", r);
            }
            "flip-h" | "flip-v" => {
                let r = picture::flip(&self.image, self.dpr, id == "flip-h");
                self.change_picture("Flipped", r);
            }
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
        if tool != Tool::Select {
            self.with_selection(|sel, d| sel.clear(d));
        }
        let was = self.tool;
        if tool == Tool::Crop && was != Tool::Crop {
            self.cropper.start(picture_size(&self.image, self.dpr));
        } else if tool != Tool::Crop {
            self.cropper.stop();
            self.width_field.focused = false;
            self.height_field.focused = false;
        }
        self.tool = tool;
        if tool.uses_color() {
            self.colour_tool = tool;
        }
        self.refresh_bar();
        if (was == Tool::Crop) != (tool == Tool::Crop) {
            // (the picture bar came or went: the canvas moved)
            self.relayout();
        }
    }

    /// The bars or the canvas changed size.
    fn relayout(&mut self) {
        self.layout_bar();
        if self.fitted {
            self.fit();
        } else {
            self.keep_in_view();
        }
        self.damaged = true;
    }

    // -- the picture itself (crop, cut out, rotate, flip, resize) --------------

    /// Put in the picture a picture:: function gave: (new picture, how a
    /// point moves onto it). The drawings move with it. One undo step.
    fn change_picture(&mut self, what: &str, changed: Result<(Image, Moved), String>) {
        self.commit_text();
        let (image, moved) = match changed {
            Ok(c) => c,
            Err(e) => return self.say(format!("Couldn't change the picture: {e}")),
        };
        let drawings: Vec<Shape> = picture::move_drawings(&self.shapes.iter().map(|(_, s)| s.clone()).collect::<Vec<_>>(), moved);
        let mut after: Vec<(u64, Shape)> = Vec::with_capacity(drawings.len() + 1);
        for s in drawings {
            after.push((self.next_id, s));
            self.next_id += 1;
        }
        let image = Arc::new(image);
        let (w, h) = image.size();
        let step = picture::Step { what: what.to_string(), before: (self.image.clone(), self.shapes.clone()), after: (image.clone(), after.clone()) };
        after.push((self.next_id, Shape::step(std::rc::Rc::new(step))));
        self.next_id += 1;
        self.image = image;
        self.shapes = after;
        self.undone.clear();
        self.picture_changed();
        self.changed();
        self.say(format!("{what}: {w} × {h}"));
    }

    fn picture_changed(&mut self) {
        self.with_selection(|sel, d| sel.clear(d));
        if self.tool == Tool::Crop {
            self.cropper.start(picture_size(&self.image, self.dpr));
        }
        self.width_field.focused = false;
        self.height_field.focused = false;
        self.refresh_bar();
        self.fit();
    }

    fn crop(&mut self) {
        let Some(rect) = self.cropper.rect.filter(|_| self.cropper.changed()) else { return };
        let margin = self.cropper.margin_colour.then(|| self.color());
        let what = if self.cropper.whole().contains_rect(&rect) { "Cropped" } else { "Margin added" };
        let r = picture::crop(&self.image, self.dpr, rect, margin);
        self.change_picture(what, r);
    }

    fn resize_picture(&mut self) {
        let (w, h) = self.image.size();
        let (Some(tw), Some(th)) = (size_value(self.width_field.text(), w), size_value(self.height_field.text(), h)) else { return };
        if (tw, th) == (w, h) {
            return;
        }
        let r = picture::resize(&self.image, tw, th);
        self.change_picture("Resized", r);
    }

    /// With the shape kept, the other side follows the one typed.
    fn follow_size(&mut self, from_width: bool) {
        let (field, other) = if from_width { (&self.width_field, &mut self.height_field) } else { (&self.height_field, &mut self.width_field) };
        let text = field.text().to_string();
        if !self.linked || text.is_empty() {
            return;
        }
        let (w, h) = self.image.size();
        let follow = if text.ends_with('%') {
            text
        } else if let Ok(n) = text.parse::<u64>() {
            let (a, b) = if from_width { (h as u64, w as u64) } else { (w as u64, h as u64) };
            ((n * a + b / 2) / b.max(1)).max(1).to_string()
        } else {
            return;
        };
        other.edit = crate::ui::textedit::TextEdit::single_line(&follow, 6);
    }

    /// The edge maps of the picture (made once per picture), for snapping the crop.
    fn crop_snap(&self, at: Point, mods: i32) -> Point {
        if !self.cfg.snap_edges || mods & keyseq::CTRL != 0 {
            return at;
        }
        let key = Arc::as_ptr(&self.image) as usize;
        let maps = {
            let mut edges = self.edges.borrow_mut();
            if edges.as_ref().is_none_or(|(k, _)| *k != key) {
                *edges = Some((key, Arc::new(EdgeMaps::build(&self.image))));
            }
            edges.as_ref().map(|(_, m)| m.clone())
        };
        let Some(maps) = maps else { return at };
        let (x, y) = crate::ui::snapping::snap(&maps, at.x, at.y, self.dpr, self.cfg.snap_distance, self.cfg.snap_sensitivity);
        Point::new(x, y)
    }

    /// Work on the drawings with the select tool: a changed copy it adds is
    /// an undo step.
    fn with_selection<R>(&mut self, f: impl FnOnce(&mut Selection, &mut dyn DrawingsMut) -> R) -> R {
        let before = self.shapes.len();
        let mut sel = std::mem::take(&mut self.selection);
        let r = f(&mut sel, &mut Shapes { list: &mut self.shapes, next_id: &mut self.next_id });
        self.selection = sel;
        if self.shapes.len() > before {
            self.undone.clear();
            self.changed();
        }
        // (a drawing hidden or shown while it's dragged)
        self.shapes_version += 1;
        self.damaged = true;
        r
    }

    fn has_selection(&mut self) -> bool {
        self.tool == Tool::Select && self.with_selection(|sel, d| sel.index(d).is_some())
    }

    /// A colour; from a tool that has none (pixelate, blur), back to the
    /// last drawing tool, so the colour gets used.
    pub fn set_color(&mut self, index: usize) {
        if self.tool == Tool::Crop {
            // (the crop tool stays: the colour is the margin's)
            self.color_index = index.min(CUSTOM);
            return self.refresh_bar();
        }
        if self.has_selection() {
            // (it changes the selected drawing)
            self.color_index = index.min(CUSTOM);
            let color = self.color();
            self.with_selection(|sel, d| sel.restyle(d, Some(color), None));
            return self.refresh_bar();
        }
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
        if self.has_selection() {
            let size = self.size;
            self.with_selection(|sel, d| sel.restyle(d, None, Some(size)));
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
        // (one more than the highest shown: a moved counter keeps its number)
        shapes::next_number(self.shapes.iter().map(|(_, s)| s))
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
        self.dirty = self.shapes.iter().map(|(id, _)| *id).ne(self.saved.iter().copied())
            || !Arc::ptr_eq(&self.image, &self.saved_image)
            || self.beauty_state() != self.saved_beauty;
        self.shapes_version += 1;
        self.refresh_bar();
    }

    fn begin_text(&mut self, shape: Shape) {
        self.commit_text();
        if let Some((_, orig)) = shape.replaces.and_then(|i| self.shapes.get_mut(i)) {
            orig.hidden = true; // (the copy shows while it's edited)
            self.shapes_version += 1;
        }
        self.text_edit = Some(shape);
        self.refresh_bar();
    }

    /// Finish the text being typed; true if there was one.
    fn commit_text(&mut self) -> bool {
        let Some(mut shape) = self.text_edit.take() else { return false };
        let replaces = shape.replaces;
        if let Some(t) = shape.as_text_mut() {
            t.editing = false;
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
        self.with_selection(|sel, d| sel.clear(d));
        if let Some((id, shape)) = self.shapes.pop() {
            if let shapes::Kind::Step(step) = &shape.kind {
                // the picture and its drawings as they were
                self.image = step.before.0.clone();
                self.shapes = step.before.1.clone();
                self.picture_changed();
            } else if let Some((_, orig)) = shape.replaces.and_then(|i| self.shapes.get_mut(i)) {
                orig.hidden = false; // the drawing as it was before it was changed
            }
            self.undone.push((id, shape));
            self.changed();
        }
    }

    pub fn redo(&mut self) {
        self.commit_text();
        self.with_selection(|sel, d| sel.clear(d));
        if let Some((id, shape)) = self.undone.pop() {
            if let shapes::Kind::Step(step) = &shape.kind {
                self.image = step.after.0.clone();
                self.shapes = step.after.1.clone();
                self.picture_changed();
            } else if let Some((_, orig)) = shape.replaces.and_then(|i| self.shapes.get_mut(i)) {
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
        let b = self.view_box();
        let c = self.canvas_rect();
        self.zoom = 1.0f32.min((c.w - 32.0) / b.w.max(1.0)).min((c.h - 32.0) / b.h.max(1.0)).max(ZOOM.0);
        self.offset = Point::new(((c.w - b.w * self.zoom) / 2.0 - b.x * self.zoom).round(), ((c.h - b.h * self.zoom) / 2.0 - b.y * self.zoom).round());
        self.fitted = true;
        self.damaged = true;
        self.refresh_zoom_bar();
        self.layout_zoom_bar();
    }

    /// Zoom, keeping the point under `around` (window coordinates; the
    /// canvas's middle by default) where it is.
    pub fn zoom_to(&mut self, zoom: f32, around: Option<Point>) {
        let c = self.canvas_rect();
        let around = around.unwrap_or(c.center());
        let at = self.to_picture(around);
        self.zoom = zoom.clamp(ZOOM.0, ZOOM.1);
        self.offset = Point::new(around.x - c.x - at.x * self.zoom, around.y - c.y - at.y * self.zoom);
        self.unfit();
        self.keep_in_view();
        self.damaged = true;
    }

    fn pan_by(&mut self, d: Point) {
        self.offset += d;
        self.unfit();
        self.keep_in_view();
        self.damaged = true;
    }

    /// Moved or zoomed by hand: no longer following the window's size.
    fn unfit(&mut self) {
        if self.fitted {
            self.fitted = false;
            self.refresh_zoom_bar();
            self.layout_zoom_bar();
        }
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
        if self.zoom_menu.is_some() {
            return CursorShape::Arrow;
        }
        if self.width_field.rect.contains(at) || self.height_field.rect.contains(at) {
            return CursorShape::Text;
        }
        if self.beauty_on && (self.panel.is_open() || self.panel.rect.contains(at)) {
            return if self.panel.name.rect.contains(at) { CursorShape::Text } else { CursorShape::Arrow };
        }
        if self.asking.is_some() || at.y < self.top_h() || at.y > self.view.1 - STATUS_H || self.picker.as_ref().is_some_and(|p| p.rect().contains(at)) {
            return CursorShape::Arrow;
        }
        if self.pan.is_some() || self.text_drag.is_some_and(|_| self.text_edit.is_some()) {
            CursorShape::Move
        } else if self.space {
            CursorShape::Hand
        } else if self.tool == Tool::Text && !self.eyedropper {
            CursorShape::Text
        } else if self.tool == Tool::Select && !self.eyedropper {
            let pic = self.to_picture(at);
            self.selection.cursor(&ShapesRef(&self.shapes), pic, self.zoom)
        } else if self.tool == Tool::Crop && !self.eyedropper {
            self.cropper.cursor(self.to_picture(at), self.zoom)
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
        if hovered != self.hovered || self.zoom_menu.is_some() {
            self.hovered = hovered;
            self.damaged = true;
        }
        if self.beauty_on && (self.panel.dragging() || self.panel.is_open() || self.panel.rect.contains(at)) {
            let (name, presets) = (self.image_name(), self.preset_names());
            let was = self.panel.hovered;
            let ev = self.panel.motion(at, &mut self.beauty, &name, &presets);
            if ev != PanelEvent::None || was != self.panel.hovered {
                self.panel_event(ev);
            }
            return;
        }
        if self.panel.hovered.take().is_some() {
            self.damaged = true;
        }
        match self.pressed {
            Some("zoom.slider") => return self.slide_zoom(at.x),
            Some("crop.width") => {
                self.width_field.drag_to(at);
                self.damaged = true;
                return;
            }
            Some("crop.height") => {
                self.height_field.drag_to(at);
                self.damaged = true;
                return;
            }
            _ => {}
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
            self.unfit();
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
        if self.selection.dragging() {
            self.with_selection(|sel, d| sel.motion(d, pic, mods));
            return;
        }
        if self.cropper.busy() {
            let mut c = std::mem::take(&mut self.cropper);
            c.motion(pic, mods, &|p, m| self.crop_snap(p, m));
            self.cropper = c;
            self.damaged = true;
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
        if let Some(menu) = self.zoom_menu.take() {
            // A zoom picked, or a click elsewhere closes it.
            self.damaged = true;
            match menu.iter().find(|(r, _)| r.contains(at)) {
                Some((_, Some(z))) => self.zoom_to(*z as f32 / 100.0, None),
                Some((_, None)) => self.fit(),
                None => {}
            }
            return;
        }
        if self.beauty_on && button == Button::Left && (self.panel.is_open() || self.panel.rect.contains(at)) {
            let (name, presets) = (self.image_name(), self.preset_names());
            let ev = self.panel.press(at, &mut self.beauty, &name, &presets);
            self.panel_event(ev);
            return;
        }
        if self.panel.name.focused {
            self.panel.name.focused = false;
            self.damaged = true;
        }
        let in_field = |f: &LineEdit| f.rect.contains(at);
        if button == Button::Left && self.tool == Tool::Crop && (in_field(&self.width_field) || in_field(&self.height_field)) {
            let width = in_field(&self.width_field);
            let shift = mods & keyseq::SHIFT != 0;
            let (field, other) = if width { (&mut self.width_field, &mut self.height_field) } else { (&mut self.height_field, &mut self.width_field) };
            other.focused = false;
            field.press(at, shift);
            self.pressed = Some(if width { "crop.width" } else { "crop.height" });
            self.damaged = true;
            return;
        }
        if self.width_field.focused || self.height_field.focused {
            self.width_field.focused = false;
            self.height_field.focused = false;
            self.refresh_bar();
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
        if at.y < self.top_h() {
            if button == Button::Left {
                self.pressed = self.bar_item_at(at);
            }
            return;
        }
        if at.y > self.view.1 - STATUS_H {
            if button == Button::Left {
                self.pressed = self.bar_item_at(at);
                if self.pressed == Some("zoom.slider") {
                    self.slide_zoom(at.x);
                }
            }
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
            Tool::Select => {
                let zoom = self.zoom;
                self.with_selection(|sel, d| sel.press(d, pic, mods, zoom));
            }
            Tool::Crop => {
                let mut c = std::mem::take(&mut self.cropper);
                c.press(pic, mods, self.zoom, &|p, m| self.crop_snap(p, m));
                self.cropper = c;
            }
            tool => self.active = Shape::create(tool, pic, self.color(), self.size),
        }
        self.damaged = true;
    }

    fn release(&mut self, button: Button, at: Point) {
        if self.panel.dragging() {
            self.panel.release();
            return;
        }
        if self.pressed == Some("picker") {
            self.pressed = None;
            if let Some(p) = &mut self.picker {
                let ev = p.release(at);
                self.picker_event(ev);
            }
            return;
        }
        if let Some(id) = self.pressed.take() {
            if matches!(id, "zoom.slider" | "crop.width" | "crop.height") {
                return;
            }
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
        if self.selection.dragging() {
            self.with_selection(|sel, d| sel.release(d));
            return self.refresh_bar();
        }
        if self.cropper.busy() {
            if let Done::Cut { a, b, rows } = self.cropper.release(self.dpr) {
                let r = picture::cut(&self.image, self.dpr, a, b, rows);
                self.change_picture("Cut out", r);
            }
            return self.refresh_bar();
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
        if self.beauty_on && self.pointer.is_some_and(|p| self.panel.rect.contains(p)) {
            let name = self.image_name();
            self.panel.scroll_by(steps * 48.0, &self.beauty, &name);
            self.damaged = true;
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
        if self.width_field.focused || self.height_field.focused {
            return self.size_key(k);
        }
        if self.panel.wants_keys() {
            let ev = self.panel.key(k);
            return self.panel_event(ev);
        }
        if k.key == key::ESCAPE && self.panel.close() {
            self.damaged = true;
            return;
        }
        if self.has_selection() && self.with_selection(|sel, d| sel.key(d, k)) {
            return self.refresh_bar(); // (Delete, or an arrow key nudging it)
        }
        if self.tool == Tool::Crop && matches!(k.key, key::RETURN | key::ENTER) {
            return self.crop();
        }
        match k.key {
            key::SPACE if !k.repeat => self.space = true,
            key::ESCAPE => {
                if self.eyedropper {
                    self.close_picker();
                } else {
                    self.close_picker();
                    if self.zoom_menu.take().is_some() {
                        self.damaged = true;
                    } else if self.tool == Tool::Crop && self.cropper.cancel() {
                        self.refresh_bar();
                    } else if self.active.take().is_some() {
                        self.damaged = true;
                    } else if self.with_selection(|sel, d| sel.clear(d)) {
                        // (deselected first)
                    } else if self.commit_text() {
                    } else if self.tool == Tool::Crop {
                        self.set_tool(self.colour_tool); // (leaves the crop tool, not the editor)
                    } else {
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

    /// A key in the width or height field: digits (and %), Enter resizes,
    /// Esc leaves the field.
    fn size_key(&mut self, k: &KeyPress) {
        let width = self.width_field.focused;
        match k.key {
            key::RETURN | key::ENTER => {
                self.width_field.focused = false;
                self.height_field.focused = false;
                self.resize_picture();
                return self.refresh_bar();
            }
            key::ESCAPE => {
                self.width_field.focused = false;
                self.height_field.focused = false;
                return self.refresh_bar();
            }
            key::TAB => {
                self.width_field.focused = !width;
                self.height_field.focused = width;
                self.damaged = true;
                return;
            }
            _ => {}
        }
        let field = if width { &mut self.width_field } else { &mut self.height_field };
        let before = field.text().to_string();
        if let KeyResult::Paste = field.key(k) {
            self.requests.push(Request::Paste);
        }
        self.after_size_edit(width, &before);
    }

    /// Keep only what a size can be, and let the other side follow.
    fn after_size_edit(&mut self, width: bool, before: &str) {
        let field = if width { &mut self.width_field } else { &mut self.height_field };
        let clean = size_text(field.text());
        if clean != field.text() {
            let cursor = field.edit.cursor.min(clean.len());
            field.edit = crate::ui::textedit::TextEdit::single_line(&clean, 6);
            field.edit.place(cursor, false);
        }
        if field.text() != before {
            self.follow_size(width);
        }
        self.damaged = true;
    }

    /// The slider dragged to `x`.
    fn slide_zoom(&mut self, x: f32) {
        let t = (x - self.zoom_slider.x - 8.0) / (self.zoom_slider.w - 16.0).max(1.0);
        self.zoom_to(Self::slider_zoom(t), None);
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
            "beautify" => self.toggle_beauty(),
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
        if self.panel.wants_keys() {
            self.panel.insert(s);
            self.damaged = true;
            return;
        }
        if self.width_field.focused || self.height_field.focused {
            let width = self.width_field.focused;
            let field = if width { &mut self.width_field } else { &mut self.height_field };
            let before = field.text().to_string();
            field.edit.insert(s);
            return self.after_size_edit(width, &before);
        }
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
        if self.panel.wants_keys() {
            return self.commit_string(s.lines().next().unwrap_or(""));
        }
        if self.width_field.focused || self.height_field.focused {
            return self.commit_string(s.lines().next().unwrap_or(""));
        }
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
        self.text_edit.is_some()
            || self.picker.as_ref().is_some_and(|p| p.hex.focused)
            || self.width_field.focused
            || self.height_field.focused
            || self.panel.wants_keys()
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
        if !self.has_alpha() {
            out.force_opaque();
        }
        if self.beauty_on {
            out = beautify::render(&out, &self.beauty, self.dpr)?;
        }
        Ok(out)
    }

    /// Transparency to keep: the picture opened had it, or a clear margin was added.
    fn has_alpha(&self) -> bool {
        self.alpha || self.image.has_alpha()
    }

    fn say(&mut self, message: String) {
        self.status = Some((message, Instant::now()));
        self.damaged = true;
    }

    pub fn copy(&mut self) {
        let clock = crate::timing::Clock::new("editor copy");
        let png = self.render().and_then(|img| crate::codec::encode_png(&img, img.has_alpha()));
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
            let result = write_picture(&img, &target, &self.cfg, img.has_alpha());
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
                self.saved_image = self.image.clone();
                self.saved_beauty = self.beauty_state();
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
        if self.tool == Tool::Crop {
            self.paint_picture_bar(&mut p, &ptr);
        }
        if self.beauty_on {
            self.panel.paint(&mut p, &self.beauty, &self.image_name(), &self.preset_names());
        }
        // The status line: a hint (or what just happened), the size, and the zoom's controls.
        let top = self.view.1 - STATUS_H;
        p.fill_rect(Rect::new(0.0, top, self.view.0, STATUS_H), c.base);
        p.fill_rect(Rect::new(0.0, top, self.view.0, 1.0), c.line);
        let f = text::font(12.0, text::NORMAL);
        let tip = self.hovered.and_then(|id| {
            self.bar
                .iter()
                .chain(self.picture_bar.iter())
                .chain(self.zoom_bar.iter())
                .find(|it| it.id == id)
                .map(|it| it.hint.clone())
                .or_else(|| self.actions.iter().chain(self.picture_actions.iter()).find(|b| b.id == id).map(|b| b.hint.clone()))
                .or_else(|| match id {
                    "zoom.label" => Some("Choose a zoom".to_string()),
                    "zoom.slider" => Some("Zoom".to_string()),
                    "crop.width" | "crop.height" => Some(format!("{} in pixels, or a percentage (50%)", if id == "crop.width" { "Width" } else { "Height" })),
                    _ => None,
                })
        });
        let (message, color) = match (&self.status, tip) {
            (Some((s, _)), _) => (s.clone(), c.soft),
            (None, Some(t)) => (t, c.muted),
            (None, None) => (self.hint_text(), c.muted),
        };
        let right = format!("{} × {}", self.image.width(), self.image.height());
        let rw = text::width(&right, f);
        let end = self.zoom_bar.first().map_or(self.view.0, |it| it.rect.x) - 14.0;
        let left_w = (end - rw - 36.0 - 12.0).max(0.0);
        if left_w > 10.0 {
            p.draw_text(Rect::new(12.0, top, left_w, STATUS_H), Align::Left, &text::elide_end(&message, f, left_w), f, color);
        }
        p.draw_text(Rect::new(end - rw, top, rw, STATUS_H), Align::Left, &right, f, c.muted);
        for it in &self.zoom_bar {
            paint_item(&mut p, it, Point::ZERO, &ptr);
        }
        self.paint_zoom_slider(&mut p);
        let label_hover = self.hovered == Some("zoom.label");
        if label_hover {
            p.fill_rounded_rect(self.zoom_label.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, c.hover);
        }
        p.draw_text(
            self.zoom_label,
            Align::Center,
            &format!("{}%", (self.zoom * 100.0).round()),
            text::font(12.0, DEMIBOLD),
            if label_hover { c.text } else { c.soft },
        );
        if let Some(menu) = &self.zoom_menu {
            self.paint_zoom_menu(&mut p, menu);
        }
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
            if self.beauty_on { self.beauty_version + 1 } else { 0 },
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
        if self.active.is_some() || self.text_edit.is_some() || self.selection.moving().is_some() {
            let mut p = Painter::for_region(target.pixmap_mut(), s, origin);
            p.clip_rect(area);
            p.translate(area.x + self.offset.x, area.y + self.offset.y);
            p.scale_by(self.zoom, self.zoom);
            let (pw, ph) = picture_size(&self.image, self.dpr);
            p.clip_rect(Rect::new(0.0, 0.0, pw, ph));
            for shape in self.active.iter().chain(self.selection.moving()).chain(self.text_edit.iter()) {
                shape.paint(&mut p, &self.image, self.dpr);
            }
        }
        if self.tool == Tool::Select {
            // The selected drawing's frame and handles (past the picture's edges too).
            let mut p = Painter::for_region(target.pixmap_mut(), s, origin);
            p.clip_rect(area);
            p.translate(area.x + self.offset.x, area.y + self.offset.y);
            p.scale_by(self.zoom, self.zoom);
            self.selection.paint_frame(&ShapesRef(&self.shapes), &mut p, self.zoom);
        }
        if self.tool == Tool::Crop {
            let mut p = Painter::for_region(target.pixmap_mut(), s, origin);
            p.clip_rect(area);
            self.cropper.paint(&mut p, area, self.offset, self.zoom, self.dpr, self.color());
        }
        if self.shapes.is_empty() && self.active.is_none() && self.text_edit.is_none() && self.tool != Tool::Crop {
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
        let smooth = self.zoom * self.scale < self.dpr;
        if self.beauty_on {
            // The background and shadow round it, and its corners rounded, as it will be saved.
            if let Some((bg, at)) = self.backdrop() {
                let k = self.zoom / self.dpr;
                let r = Rect::new(frame.x - at.x as f32 * k, frame.y - at.y as f32 * k, bg.width() as f32 * k, bg.height() as f32 * k);
                p.draw_image(&bg, r, bg.rect().to_f(), true);
            }
            p.save();
            let radius = beautify::corners(&self.beauty, &self.image, self.dpr) / self.dpr * self.zoom;
            if radius > 0.0 {
                p.clip_rounded(frame, radius);
            }
            p.draw_image(&self.image, frame, self.image.rect().to_f(), smooth);
            p.restore();
        } else {
            p.stroke_rect(frame.adjusted(-0.5, -0.5, 0.5, 0.5), Pen::new(c.line, 1.0));
            // Close up, whole pixels (as a paint program shows them); further out, smooth.
            p.draw_image(&self.image, frame, self.image.rect().to_f(), smooth);
        }
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

    /// Beautify's background and shadow for the picture (kept until the
    /// picture or the style changes), and where the picture is on it (px).
    fn backdrop(&self) -> Option<(Arc<Image>, IPoint)> {
        let key = (Arc::as_ptr(&self.image) as usize, self.beauty_version);
        let mut cache = self.backdrop.borrow_mut();
        if cache.as_ref().is_none_or(|(k, ..)| *k != key) {
            let (img, at) = beautify::backdrop(&self.image, &self.beauty, self.dpr).ok()?;
            *cache = Some((key, Arc::new(img), at));
        }
        cache.as_ref().map(|(_, img, at)| (img.clone(), *at))
    }

    /// What's shown, in the picture's coordinates: the picture, and its
    /// background round it while beautifying.
    fn view_box(&self) -> Rect {
        let (pw, ph) = picture_size(&self.image, self.dpr);
        if !self.beauty_on {
            return Rect::new(0.0, 0.0, pw, ph);
        }
        match self.backdrop() {
            Some((bg, at)) => Rect::new(-at.x as f32 / self.dpr, -at.y as f32 / self.dpr, bg.width() as f32 / self.dpr, bg.height() as f32 / self.dpr),
            None => Rect::new(0.0, 0.0, pw, ph),
        }
    }

    fn beauty_state(&self) -> Option<Style> {
        self.beauty_on.then(|| self.beauty.clone())
    }

    /// Show or hide the Background panel; the background goes with it.
    pub fn toggle_beauty(&mut self) {
        self.beauty_on = !self.beauty_on;
        self.panel.close();
        self.refresh_bar();
        self.layout_bar();
        self.fit();
        self.changed();
    }

    /// The style changed (the panel's controls).
    fn beauty_changed(&mut self) {
        self.beauty.clean();
        self.place_panel();
        self.beauty_version += 1;
        self.remember_at = Some(Instant::now() + REMEMBER_AFTER);
        if self.fitted {
            self.fit();
        }
        self.changed();
    }

    /// The style becomes the last used: what Beautify after capture uses.
    fn remember_beauty(&mut self) {
        let style = self.beauty.to_map();
        self.cfg.beautify_style = style.clone();
        crate::config::update(move |c| c.beautify_style = style);
    }

    fn save_preset(&mut self, name: String) {
        let style = self.beauty.to_map();
        self.cfg.beautify_presets.insert(name.clone(), serde_json::Value::Object(style.clone()));
        let (n, st) = (name.clone(), style);
        crate::config::update(move |c| {
            c.beautify_presets.insert(n, serde_json::Value::Object(st));
        });
        self.say(format!("Saved the preset {name}"));
    }

    fn use_preset(&mut self, name: &str) {
        if let Some(serde_json::Value::Object(m)) = self.cfg.beautify_presets.get(name) {
            self.beauty = Style::from_map(m);
            self.beauty_changed();
        }
    }

    /// A picture chosen for the background.
    pub fn background_image(&mut self, path: &Path) {
        self.beauty.image = path.to_string_lossy().into_owned();
        self.beauty.background = "image".into();
        self.beauty_changed();
    }

    fn preset_names(&self) -> Vec<String> {
        self.cfg.beautify_presets.keys().cloned().collect()
    }

    fn image_name(&self) -> String {
        if self.beauty.image.is_empty() {
            "No image selected".into()
        } else {
            Path::new(&self.beauty.image).file_name().map(|n| crate::util::displayable(&n.to_string_lossy())).unwrap_or_default()
        }
    }

    fn panel_rect(&self) -> Rect {
        let top = self.top_h();
        Rect::new(self.view.0 - beautypanel::WIDTH, top, beautypanel::WIDTH, (self.view.1 - top - STATUS_H).max(1.0))
    }

    fn panel_event(&mut self, ev: PanelEvent) {
        match ev {
            PanelEvent::None => {}
            PanelEvent::Changed => self.beauty_changed(),
            PanelEvent::UseColour => {
                self.beauty.color = self.color().hex();
                self.beauty_changed();
            }
            PanelEvent::ChooseImage => {
                let start =
                    Path::new(&self.beauty.image).parent().filter(|p| !p.as_os_str().is_empty()).map(Path::to_path_buf).unwrap_or_else(crate::config::home);
                self.requests.push(Request::ChooseImage(start));
            }
            PanelEvent::UsePreset(name) => self.use_preset(&name),
            PanelEvent::SavePreset(name) => self.save_preset(name),
            PanelEvent::Paste => self.requests.push(Request::Paste),
        }
        self.damaged = true;
    }

    /// The status line's text when nothing else is said.
    fn hint_text(&self) -> String {
        let text = if self.eyedropper {
            "Click a pixel to make it your colour  ·  Esc to stop"
        } else if self.active.as_ref().is_some_and(|s| s.drag().is_some()) {
            SHAPE_HINT
        } else if self.tool == Tool::Text {
            TEXT_HINT
        } else if self.tool == Tool::Crop {
            return match self.cropper.cut {
                Some(rows) => CUT_HINT.replace("{what}", if rows { "rows" } else { "columns" }),
                None => CROP_HINT.into(),
            };
        } else if self.tool == Tool::Select && self.selection.current(&ShapesRef(&self.shapes)).is_some() {
            SELECTED_HINT
        } else if self.tool == Tool::Select {
            SELECT_HINT
        } else {
            HINT
        };
        text.to_string()
    }

    fn paint_picture_bar(&self, p: &mut Painter<'_>, ptr: &Pointer) {
        let c = theme::c();
        p.fill_rect(Rect::new(0.0, BAR_H, self.view.0, PICTURE_BAR_H), c.base);
        p.fill_rect(Rect::new(0.0, BAR_H + PICTURE_BAR_H - 1.0, self.view.0, 1.0), c.line);
        for b in &self.picture_actions {
            b.paint(p, self.hovered == Some(b.id));
        }
        for it in &self.picture_bar {
            paint_item(p, it, Point::ZERO, ptr);
        }
        self.width_field.paint(p, true);
        self.height_field.paint(p, true);
        let times = Rect::new(self.width_field.rect.right() + 4.0, self.width_field.rect.y, 14.0, self.width_field.rect.h);
        p.draw_text(times, Align::Center, "×", text::font(13.0, text::NORMAL), c.muted);
    }

    fn paint_zoom_slider(&self, p: &mut Painter<'_>) {
        let c = theme::c();
        let r = self.zoom_slider;
        let track = Rect::new(r.x + 8.0, r.center().y - 2.0, r.w - 16.0, 4.0);
        p.fill_rounded_rect(track, 2.0, c.line);
        let x = track.x + track.w * Self::slider_position(self.zoom);
        p.fill_rounded_rect(Rect::new(track.x, track.y, x - track.x, track.h), 2.0, c.accent);
        let hot = matches!(self.hovered, Some("zoom.slider")) || self.pressed == Some("zoom.slider");
        let k = if hot { 7.0 } else { 6.0 };
        p.draw_rounded_rect(Rect::new(x - k, r.center().y - k, 2.0 * k, 2.0 * k), k, Some(c.knob), Some(Pen::new(c.line, 1.0)));
    }

    fn paint_zoom_menu(&self, p: &mut Painter<'_>, rows: &[(Rect, Option<u32>)]) {
        let c = theme::c();
        let (Some(first), Some(last)) = (rows.first(), rows.last()) else { return };
        let panel = Rect::from_ltrb(first.0.x - 4.0, first.0.y - 4.0, first.0.right() + 4.0, last.0.bottom() + 4.0);
        p.draw_rounded_rect(panel.adjusted(0.5, 0.5, -0.5, -0.5), 10.0, Some(c.base), Some(Pen::new(c.line, 1.0)));
        let f = text::font(13.0, text::NORMAL);
        let at = self.pointer.unwrap_or(Point::new(-1.0, -1.0));
        for (i, (r, z)) in rows.iter().enumerate() {
            if r.contains(at) {
                p.fill_rounded_rect(*r, 6.0, c.hover);
            }
            let label = match z {
                Some(z) => format!("{z}%"),
                None => "Fit in the window".to_string(),
            };
            let current = z.is_some_and(|z| (self.zoom * 100.0).round() as u32 == z) || (z.is_none() && self.fitted);
            p.draw_text(r.adjusted(12.0, 0.0, -8.0, 0.0), Align::Left, &label, f, if current { c.accent } else { c.text });
            if i == 0 {
                p.fill_rect(Rect::new(r.x + 6.0, r.bottom() - 0.5, r.w - 12.0, 1.0), c.line);
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
            let b = ActionButton { id: "", text: label, icon, strong: a == Answer::Save, hint: String::new(), rect: r, width: r.w, enabled: true };
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
    fn select_tool() {
        let mut ed = editor();
        ed.set_tool(Tool::Rect);
        let (a, b) = (at(&ed, 50.0, 50.0), at(&ed, 150.0, 100.0));
        ed.button(Button::Left, true, a, 0);
        ed.pointer_motion(b, 0);
        ed.button(Button::Left, false, b, 0);
        ed.set_tool(Tool::Select);
        // On its line: selected, then dragged (a copy in its place).
        let (grab, to) = (at(&ed, 50.0, 60.0), at(&ed, 70.0, 80.0));
        ed.button(Button::Left, true, grab, 0);
        ed.pointer_motion(to, 0);
        ed.button(Button::Left, false, to, 0);
        assert_eq!(ed.shapes.len(), 2);
        assert!(ed.shapes[0].1.hidden);
        assert_eq!(ed.shapes[1].1.geometry(), Rect::new(70.0, 70.0, 100.0, 50.0));
        // The size buttons restyle it; Delete removes it; undo brings it back.
        ed.set_size(0);
        assert_eq!(ed.shapes.len(), 3);
        assert_eq!(ed.shapes[2].1.size, 0);
        ed.key(&KeyPress { key: key::DELETE, ..Default::default() });
        assert!(ed.shapes[3].1.is_removed() && ed.shapes[2].1.hidden);
        ed.undo();
        assert!(!ed.shapes[2].1.hidden);
        // Esc deselects before it would close.
        ed.button(Button::Left, true, at(&ed, 70.0, 90.0), 0);
        ed.button(Button::Left, false, at(&ed, 70.0, 90.0), 0);
        ed.key(&KeyPress { key: key::ESCAPE, ..Default::default() });
        assert!(!ed.closing && ed.selection.current(&ShapesRef(&ed.shapes)).is_none());
    }

    #[test]
    fn changing_the_picture() {
        let mut ed = editor();
        ed.set_tool(Tool::Rect);
        let (a, b) = (at(&ed, 300.0, 200.0), at(&ed, 350.0, 250.0));
        ed.button(Button::Left, true, a, 0);
        ed.pointer_motion(b, 0);
        ed.button(Button::Left, false, b, 0);
        let narrow = ed.min_size();
        ed.set_tool(Tool::Crop);
        assert!(ed.min_size().1 > narrow.1, "the picture bar adds to the least height");
        assert!(ed.min_size().0 as f32 >= ed.picture_bar_width());
        // The left edge dragged in by its handle, then Enter crops.
        let (from, to) = (at(&ed, 0.0, 150.0), at(&ed, 100.0, 150.0));
        ed.button(Button::Left, true, from, 0);
        ed.pointer_motion(to, keyseq::CTRL); // (Ctrl: no snapping, from the middle)
        ed.pointer_motion(to, 0);
        ed.button(Button::Left, false, to, 0);
        assert!(ed.cropper.changed());
        ed.key(&KeyPress { key: key::RETURN, ..Default::default() });
        assert_eq!(ed.image.size(), (300, 300));
        assert!(ed.dirty);
        // The drawing moved with the picture.
        let drawn = ed.shapes.iter().find(|(_, s)| s.drag().is_some()).map(|(_, s)| s.geometry()).unwrap();
        assert_eq!(drawn, Rect::new(200.0, 200.0, 50.0, 50.0));
        // Rotated: one more step; undo twice is the picture as it was.
        ed.bar_click("rotate-right");
        assert_eq!(ed.image.size(), (300, 300));
        let drawn = ed.shapes.iter().find(|(_, s)| s.drag().is_some()).map(|(_, s)| s.geometry()).unwrap();
        assert_eq!(drawn, Rect::new(50.0, 200.0, 50.0, 50.0));
        ed.undo();
        ed.undo();
        assert_eq!(ed.image.size(), (400, 300));
        let drawn = ed.shapes.iter().find(|(_, s)| s.drag().is_some()).map(|(_, s)| s.geometry()).unwrap();
        assert_eq!(drawn, Rect::new(300.0, 200.0, 50.0, 50.0), "the drawings as they were");
        ed.redo();
        assert_eq!(ed.image.size(), (300, 300));
        // Resize: a percentage typed in the width, the height follows.
        ed.press(Button::Left, ed.width_field.rect.center(), 0);
        ed.release(Button::Left, ed.width_field.rect.center());
        assert!(ed.wants_text());
        for _ in 0..4 {
            ed.key(&KeyPress { key: key::BACKSPACE, ..Default::default() });
        }
        ed.commit_string("5x0%");
        assert_eq!((ed.width_field.text(), ed.height_field.text()), ("50%", "50%"));
        ed.key(&KeyPress { key: key::RETURN, ..Default::default() });
        assert_eq!(ed.image.size(), (150, 150));
        // Cut out rows: drag across them.
        ed.bar_click("cut-rows");
        let (from, to) = (at(&ed, 10.0, 50.0), at(&ed, 10.0, 100.0));
        ed.button(Button::Left, true, from, 0);
        ed.pointer_motion(to, 0);
        ed.button(Button::Left, false, to, 0);
        assert_eq!(ed.image.size(), (150, 100));
        // Esc leaves the crop tool, not the editor.
        ed.key(&KeyPress { key: key::ESCAPE, ..Default::default() });
        assert!(ed.tool != Tool::Crop && !ed.closing);
        let img = ed.render().unwrap();
        assert_eq!(img.size(), (150, 100));
    }

    #[test]
    fn zoom_controls() {
        let mut ed = editor();
        ed.bar_click("zoom-in");
        assert!((ed.zoom - 1.25).abs() < 1e-4 && !ed.fitted);
        // The slider: its left end is the least zoom.
        let left = Point::new(ed.zoom_slider.x + 8.0, ed.zoom_slider.center().y);
        ed.button(Button::Left, true, left, 0);
        ed.button(Button::Left, false, left, 0);
        assert!((ed.zoom - ZOOM.0).abs() < 1e-3, "{}", ed.zoom);
        // The menu: a zoom picked.
        ed.bar_click("zoom.label");
        let row = ed.zoom_menu.as_ref().unwrap().iter().find(|(_, z)| *z == Some(200)).unwrap().0;
        ed.button(Button::Left, true, row.center(), 0);
        ed.button(Button::Left, false, row.center(), 0);
        assert!((ed.zoom - 2.0).abs() < 1e-4 && ed.zoom_menu.is_none());
        ed.bar_click("fit");
        assert!(ed.fitted);
        // Nothing in the status line overlaps at the least width.
        let (w, h) = ed.min_size();
        ed.set_view(w as f32, h as f32);
        assert!(ed.zoom_bar[0].rect.x >= 160.0, "room for the size and some hint");
        assert!(ed.actions[0].rect.x >= ed.bar.last().unwrap().rect.right(), "the toolbar's buttons apart");
    }

    #[test]
    fn beautify() {
        let mut ed = editor();
        let narrow = ed.min_size().0;
        ed.bar_click("beautify");
        assert!(ed.beauty_on && ed.dirty);
        assert!(ed.min_size().0 >= narrow, "room for the panel too");
        assert!(ed.canvas_rect().w <= ed.view.0 - beautypanel::WIDTH);
        let style = ed.beauty.clone();
        let (size, _) = beautify::layout((400, 300), &style, 1.0);
        let img = ed.render().unwrap();
        assert_eq!(img.size(), size);
        // A change from the panel: the style, remembered once changes stop.
        ed.beauty.padding = 8;
        ed.panel_event(PanelEvent::Changed);
        assert!(ed.next_deadline().is_some());
        ed.on_timer(Instant::now() + Duration::from_secs(1));
        assert_eq!(Style::from_map(&crate::config::load().beautify_style).padding, 8);
        assert_eq!(ed.render().unwrap().size(), (416, 316));
        // A preset saved and used again.
        ed.panel_event(PanelEvent::SavePreset("tight".into()));
        ed.beauty.padding = 100;
        ed.panel_event(PanelEvent::UsePreset("tight".into()));
        assert_eq!(ed.beauty.padding, 8);
        // Off again: as saved.
        ed.toggle_beauty();
        assert!(!ed.dirty);
        assert_eq!(ed.render().unwrap().size(), (400, 300));
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
