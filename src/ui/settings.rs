//! The settings window: a sidebar of pages, each a stack of quiet grouped
//! cards. Changes are saved as they're made and apply from the next
//! capture.
//!
//! The window's state, input and painting, with nothing of the window
//! system: what needs the desktop (file dialogs, KDE's shortcuts, the
//! clipboard) is asked for with [`Request`]s.

use serde_json::Value;
use std::collections::BTreeMap;

use crate::config::{self, Config};
use crate::desktop::{autostart, shortcuts, sound};
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::Canvas;
use crate::keys::{self, KeyPress};
use crate::keyseq::{self, key};
use crate::render::Painter;
use crate::render::paint::{Align, Pen};
use crate::render::text::{self, DEMIBOLD, MEDIUM, NORMAL};
use crate::render::theme::{self, Color, THEMES};
use crate::template::{self, Shot};
use crate::ui::panels::LineEdit;
use crate::ui::session::{Button, CursorShape};
use crate::ui::textedit::KeyResult;

const NAV_W: f32 = 184.0;
const MARGIN: f32 = 28.0;
const SEG_PAD: f32 = 3.0;
const SEG_GAP: f32 = 2.0;

const CODES_HINT: &str = "Date and time: %Y %m %d %H %M %S. Window: {app} {title}. Also {mode} (region, window, monitor, desktop), {monitor}, {w} {h} for the size and {n} for a counter ({n:4} pads it). For a literal %, { or }, write %%, {{ or }}.";

/// What the window needs done outside itself.
#[derive(Debug, PartialEq)]
pub enum Request {
    /// pick a folder for the setting `key`, starting at the current one
    ChooseFolder(&'static str, String),
    /// pick a sound file
    ChooseSound(String),
    /// give a KDE global shortcut new keys ("" clears it)
    AssignShortcut(&'static str, String),
    /// hold KDE's shortcuts while one is being typed
    BlockShortcuts(bool),
    /// the colour theme changed: everything repaints
    ThemeChanged,
    CopyText(String),
    Paste,
    /// Esc or Ctrl+W: done with the window
    Close,
    /// find out (in the background) what recording in this format needs
    CheckRecorder(String),
}

#[derive(Clone, Debug)]
enum Control {
    None,
    Toggle(bool),
    Segmented {
        options: Vec<(String, String)>,
        dots: Vec<Option<(Color, Color)>>,
        value: String,
    },
    Slider {
        value: i64,
        min: i64,
        max: i64,
        step: i64,
        fmt: fn(i64) -> String,
    },
    Button(&'static str),
    /// a text field, and buttons after it
    Line {
        edit: Box<LineEdit>,
        buttons: Vec<&'static str>,
    },
    /// muted text the width of the row (the file name example)
    Text(String),
    Shortcut {
        seq: String,
        enabled: bool,
    },
    Combo {
        options: Vec<(String, String)>,
        value: String,
    },
}

#[derive(Clone, Debug)]
struct Row {
    id: &'static str,
    title: String,
    hint: String,
    control: Control,
    below: Control,
    error: Option<String>,
    visible: bool,
}

impl Row {
    fn new(id: &'static str, title: &str, hint: &str, control: Control) -> Row {
        Row { id, title: title.into(), hint: hint.into(), control, below: Control::None, error: None, visible: true }
    }

    fn below(mut self, c: Control) -> Row {
        self.below = c;
        self
    }
}

#[derive(Clone, Debug)]
enum Block {
    Title(String),
    Note(String),
    Card {
        heading: String,
        rows: Vec<Row>,
    },
    /// "Reset all to defaults" under the window's keys
    Reset,
}

struct Page {
    name: &'static str,
    blocks: Vec<Block>,
}

/// Where in the window something is.
#[derive(Clone, Copy, Debug, PartialEq)]
enum Hit {
    Nav(usize),
    /// a row's control (block, row), and which part of it
    Row(usize, usize, Part),
    Reset,
    /// an option of the open dropdown
    Option(usize),
}

#[derive(Clone, Copy, Debug, PartialEq)]
enum Part {
    Control,
    Segment(usize),
    /// the text field below
    Field,
    /// a button after the text field
    FieldButton(usize),
    /// a shortcut's ×
    Clear,
}

/// What's laid out where, this frame.
struct Layout {
    hits: Vec<(Hit, Rect)>,
    /// total height of the page's content
    height: f32,
}

fn title_font() -> text::FontSpec {
    text::font(13.0, NORMAL)
}
fn hint_font() -> text::FontSpec {
    text::font(12.0, NORMAL)
}
fn control_font() -> text::FontSpec {
    text::font(13.0, MEDIUM)
}
fn button_font() -> text::FontSpec {
    text::font(12.0, MEDIUM)
}

fn line_height(f: text::FontSpec) -> f32 {
    text::metrics(f).line_spacing.max(text::metrics(f).height)
}

fn segment_widths(options: &[(String, String)], dots: &[Option<(Color, Color)>]) -> Vec<f32> {
    let bold = text::font(13.0, DEMIBOLD);
    options
        .iter()
        .enumerate()
        .map(|(i, (_, label))| text::width(label, bold).ceil() + 32.0 + if dots.get(i).is_some_and(|d| d.is_some()) { 20.0 } else { 0.0 })
        .collect()
}

fn button_width(label: &str) -> f32 {
    text::width(label, button_font()).ceil() + 24.0
}

/// A control's size, logical pixels.
fn control_size(c: &Control) -> (f32, f32) {
    match c {
        Control::None => (0.0, 0.0),
        Control::Toggle(_) => (34.0, 20.0),
        Control::Segmented { options, dots, .. } => {
            let w = segment_widths(options, dots);
            (w.iter().sum::<f32>() + SEG_GAP * (w.len().max(1) - 1) as f32 + 2.0 * SEG_PAD, 34.0)
        }
        Control::Slider { .. } => (180.0 + 10.0 + 40.0, 24.0),
        Control::Button(label) => (button_width(label), 30.0),
        Control::Line { .. } => (0.0, 32.0),
        Control::Text(_) => (0.0, 0.0),
        Control::Shortcut { .. } => (200.0, 30.0),
        Control::Combo { .. } => (180.0, 32.0),
    }
}

fn fmt_percent_off(v: i64) -> String {
    if v == 0 { "Off".into() } else { format!("{v}%") }
}
fn fmt_px(v: i64) -> String {
    format!("{v}px")
}
fn fmt_plain(v: i64) -> String {
    v.to_string()
}
fn fmt_seconds_off(v: i64) -> String {
    if v == 0 { "Off".into() } else { format!("{v} s") }
}

fn seg(options: &[(&str, &str)], value: &str) -> Control {
    let options: Vec<(String, String)> = options.iter().map(|(k, l)| (k.to_string(), l.to_string())).collect();
    let dots = vec![None; options.len()];
    Control::Segmented { options, dots, value: value.to_string() }
}

fn line(value: &str, placeholder: &str, buttons: Vec<&'static str>) -> Control {
    let mut edit = LineEdit::new(Rect::default(), value, text::font(13.0, NORMAL), 4096);
    edit.placeholder = placeholder.into();
    Control::Line { edit: Box::new(edit), buttons }
}

pub struct Settings {
    cfg: Config,
    pages: Vec<Page>,
    page: usize,
    scroll: f32,
    view: (f32, f32),
    pub scale: f32,
    pointer: Option<Point>,
    hovered: Option<Hit>,
    pressed: Option<Hit>,
    /// the text field with the keyboard: (block, row)
    focus: Option<(usize, usize)>,
    /// the shortcut being typed: (block, row)
    recording: Option<(usize, usize)>,
    /// the dropdown open: (block, row)
    combo: Option<(usize, usize)>,
    /// a slider being dragged: (block, row)
    sliding: Option<(usize, usize)>,
    mods: i32,
    damaged: bool,
    /// parts to draw again when not all of it is (logical rectangles)
    touched: Vec<Rect>,
    requests: Vec<Request>,
    /// KDE's shortcut service is there (None: still finding out)
    global_shortcuts: Option<bool>,
}

impl Settings {
    /// KDE's shortcuts come later, with [`Settings::set_global`].
    pub fn new(cfg: Config) -> Settings {
        let mut s = Settings {
            pages: Vec::new(),
            page: 0,
            scroll: 0.0,
            view: (760.0, 560.0),
            scale: 1.0,
            pointer: None,
            hovered: None,
            pressed: None,
            focus: None,
            recording: None,
            combo: None,
            sliding: None,
            mods: 0,
            damaged: true,
            touched: Vec::new(),
            requests: Vec::new(),
            global_shortcuts: None,
            cfg,
        };
        s.pages = s.build();
        s.sync_visibility();
        s
    }

    fn build(&self) -> Vec<Page> {
        let c = &self.cfg;
        // General
        let theme_options: Vec<(String, String)> = THEMES.iter().map(|t| (t.0.to_string(), t.1.to_string())).collect();
        let dots = THEMES.iter().map(|t| Some((Color::rgb(t.2[1]), Color::rgb(t.2[8])))).collect();
        let theme_value = if THEMES.iter().any(|t| t.0 == c.theme) { c.theme.clone() } else { "ember".into() };
        let general = vec![Block::Card {
            heading: String::new(),
            rows: vec![
                Row::new("theme", "Theme", "For the toolbar, the settings and the other panels. Your drawing colours stay the same.", Control::None)
                    .below(Control::Segmented { options: theme_options, dots, value: theme_value }),
                Row::new("autostart", "Start at login", "Keeps Flatshot in the system tray so shortcuts work.", Control::Toggle(autostart::enabled())),
                Row::new(
                    "config_folder",
                    "Configuration folder",
                    &config::config_path().parent().map(|p| p.display().to_string()).unwrap_or_default(),
                    Control::Button("Open"),
                ),
            ],
        }];
        // Capture
        let backends: Vec<(String, String)> = config::BACKENDS
            .iter()
            .filter(|b| **b != "qt")
            .map(|b| {
                let label = match *b {
                    "auto" => "Automatic",
                    "kwin" => "KWin",
                    "screencopy" => "Screencopy",
                    "portal" => "Portal",
                    "spectacle" => "Spectacle",
                    "grim" => "grim",
                    "gnome-screenshot" => "GNOME Screenshot",
                    other => other,
                };
                (b.to_string(), label.to_string())
            })
            .collect();
        let capture = vec![
            Block::Card {
                heading: String::new(),
                rows: vec![
                    Row::new("dim_opacity", "Screen shading", "", Control::Slider { value: c.dim_opacity, min: 0, max: 90, step: 5, fmt: fmt_percent_off }),
                    Row::new(
                        "toolbar_follows_mouse",
                        "Toolbar follows the mouse",
                        "Show the toolbar on the monitor the pointer is on.",
                        Control::Toggle(c.toolbar_follows_mouse),
                    ),
                    Row::new(
                        "detect_windows",
                        "Detect windows",
                        "Hover a window and click to capture just that window. KDE Plasma, Sway and Hyprland.",
                        Control::Toggle(c.detect_windows),
                    ),
                    Row::new(
                        "span_monitors",
                        "Select across monitors",
                        "A drag, or a clicked window, may cross onto other monitors. The toolbar stays on one.",
                        Control::Toggle(c.span_monitors),
                    ),
                    Row::new(
                        "include_pointer",
                        "Include the mouse pointer",
                        "In instant captures: active window, monitor, all screens, last region.",
                        Control::Toggle(c.include_pointer),
                    ),
                    Row::new(
                        "region_pointer",
                        "Mouse pointer when selecting",
                        "Your choice adds a toolbar button (M), but takes a little longer to open.",
                        seg(&[("hidden", "Never"), ("shown", "Always"), ("toggle", "Your choice")], &c.region_pointer),
                    ),
                    Row::new(
                        "screenshot_method",
                        "Take the screenshot",
                        "Each screen keeps every monitor's own pixels when their scales differ, and is the fastest.",
                        seg(&[("screens", "Each screen"), ("desktop", "Whole desktop")], &c.screenshot_method),
                    ),
                    Row::new(
                        "snap_edges",
                        "Snap to edges",
                        "Selections snap to edges in the picture, boxes first. Also the toolbar's magnet button, or G; hold Ctrl to place freely.",
                        Control::Toggle(c.snap_edges),
                    ),
                    Row::new(
                        "snap_distance",
                        "Snap distance",
                        "How close the pointer must be to an edge.",
                        Control::Slider { value: c.snap_distance, min: 2, max: 40, step: 1, fmt: fmt_px },
                    ),
                    Row::new(
                        "snap_sensitivity",
                        "Snap sensitivity",
                        "Higher snaps to fainter and shorter edges too.",
                        Control::Slider { value: c.snap_sensitivity, min: 1, max: 10, step: 1, fmt: fmt_plain },
                    ),
                ],
            },
            Block::Card {
                heading: "Magnifier and crosshair".into(),
                rows: vec![
                    Row::new("show_loupe", "Magnifier", "Zoomed pixels, coordinates and colour beside the pointer.", Control::Toggle(c.show_loupe)),
                    Row::new(
                        "loupe_size",
                        "Magnifier size",
                        "",
                        Control::Slider { value: c.loupe_size, min: config::LOUPE_SIZES.0, max: config::LOUPE_SIZES.1, step: 20, fmt: fmt_px },
                    ),
                    Row::new("show_crosshair", "Crosshair lines", "Lines across the screen through the pointer.", Control::Toggle(c.show_crosshair)),
                    Row::new("rainbow", "Rainbow", "The crosshair and the magnifier's square cycle through the colours.", Control::Toggle(c.rainbow)),
                    Row::new(
                        "show_hint",
                        "Hints",
                        "What to do next, in the middle of the screen. It fades when the pointer comes near.",
                        Control::Toggle(c.show_hint),
                    ),
                ],
            },
            Block::Card {
                heading: "QR codes and barcodes".into(),
                rows: vec![
                    Row::new("scan_codes", "Scan the screen", "", Control::Toggle(c.scan_codes)),
                    Row::new("show_codes", "Show results", "Q or the toolbar's code button changes this too.", Control::Toggle(c.show_codes)),
                ],
            },
            Block::Card {
                heading: "Advanced".into(),
                rows: vec![Row::new(
                    "backend",
                    "Capture method",
                    "Automatic picks the fastest your desktop has.",
                    Control::Combo { options: backends, value: if c.backend == "qt" { "auto".into() } else { c.backend.clone() } },
                )],
            },
        ];
        // After capture
        let formats: Vec<(String, String)> = crate::codec::writable_formats().iter().map(|f| (f.0.to_string(), f.1.to_string())).collect();
        let format_value = if formats.iter().any(|f| f.0 == c.format) { c.format.clone() } else { "png".into() };
        let after = vec![
            Block::Card {
                heading: "Save".into(),
                rows: vec![
                    Row::new("save_to_disk", "Save to a folder", "", Control::Toggle(c.save_to_disk)),
                    Row::new("save_dir", "Folder", CODES_HINT, Control::None).below(line(&c.save_dir, "", vec!["Choose…"])),
                    Row::new("filename", "File name", "The same codes. A / makes subfolders; the extension is added for you.", Control::None).below(line(
                        &c.filename,
                        "",
                        vec![],
                    )),
                    Row::new("preview", "", "", Control::None).below(Control::Text(String::new())),
                    Row::new("format", "Format", "", Control::Segmented { dots: vec![None; formats.len()], options: formats, value: format_value }),
                    Row::new(
                        "quality",
                        "Quality",
                        "Higher is sharper and larger.",
                        Control::Slider { value: c.quality, min: 10, max: 100, step: 5, fmt: fmt_plain },
                    ),
                ],
            },
            Block::Card {
                heading: "Then".into(),
                rows: vec![
                    Row::new("clipboard", "Copy to clipboard", "", seg(&[("image", "Image"), ("path", "File path"), ("none", "Nothing")], &c.clipboard)),
                    Row::new("notify", "Show a notification", "With Open, Show in folder, Annotate and Pin.", Control::Toggle(c.notify)),
                    Row::new("open_after", "Open", "", seg(&[("none", "Nothing"), ("image", "Image"), ("folder", "Folder")], &c.open_after)),
                    Row::new("sound", "Play a sound", "", Control::Toggle(c.sound)),
                    Row::new("sound_file", "Sound file", "Leave empty for the desktop's own. OGG, WAV, FLAC or MP3.", Control::None).below(line(
                        &c.sound_file,
                        "The desktop's screenshot sound",
                        vec!["Choose…", "Play"],
                    )),
                    Row::new("run_command", "Run a command", "{path} stands for the image file (also $FLATSHOT_PATH).", Control::None).below(line(
                        &c.run_command,
                        "curl -F file=@{path} https://example.com/upload",
                        vec![],
                    )),
                ],
            },
        ];
        // Recording
        let recording = vec![
            Block::Card {
                heading: String::new(),
                rows: vec![
                    Row::new("recorder", "Recorder", crate::record::method().label(), Control::None),
                    Row::new("record_dir", "Folder", CODES_HINT, Control::None).below(line(&c.record_dir, "", vec!["Choose…"])),
                    Row::new("record_filename", "File name", "The same codes. The extension is added for you.", Control::None).below(line(&c.record_filename, "", vec![])),
                    Row::new("record_preview", "", "", Control::None).below(Control::Text(String::new())),
                    Row::new("record_countdown", "Countdown", "Before recording starts.", Control::Slider { value: c.record_countdown, min: 0, max: 10, step: 1, fmt: fmt_seconds_off }),
                ],
            },
            Block::Card {
                heading: "Defaults".into(),
                rows: vec![
                    Row::new("record_format", "Format", "GIFs have no sound.", seg(&[("mp4", "MP4"), ("webm", "WebM"), ("gif", "GIF")], &c.record_format)),
                    Row::new("record_fps", "Frame rate", "", seg(&[("24", "24 fps"), ("30", "30 fps"), ("60", "60 fps")], &c.record_fps.to_string())),
                    Row::new("record_mic", "Microphone", "", Control::Toggle(c.record_mic)),
                    Row::new("record_system_audio", "Computer sound", "", Control::Toggle(c.record_system_audio)),
                    Row::new("record_cursor", "Mouse pointer", "", Control::Toggle(c.record_cursor)),
                ],
            },
            Block::Note("You can also change these under the area before you start. After a recording, the clipboard, notification, open and command settings from After capture apply; Copy to clipboard copies the video file.".into()),
        ];
        // Shortcuts (KDE's keys come later)
        let global_rows = shortcuts::ACTIONS
            .iter()
            .map(|(action, label, _)| Row::new(sc_id(action), label, "", Control::Shortcut { seq: String::new(), enabled: false }))
            .collect();
        let mut shortcuts_page =
            vec![Block::Card { heading: String::new(), rows: global_rows }, Block::Note(global_note(None)), Block::Title("Keys in Flatshot".into())];
        shortcuts_page.push(Block::Note(
            "While capturing and in the annotation editor. Click one and press a new key; Backspace clears it. Esc, Enter, and Ctrl+S / Ctrl+C to capture, stay as they are.".into(),
        ));
        let keymap = keys::Keymap::new(&c.keys);
        for (heading, actions) in keys::groups() {
            let rows = actions
                .iter()
                .filter_map(|a| keys::binding(a))
                .map(|b| Row::new(key_id(b.0), b.1, "", Control::Shortcut { seq: keymap.keys.get(b.0).cloned().unwrap_or_default(), enabled: true }))
                .collect();
            shortcuts_page.push(Block::Card { heading: heading.into(), rows });
        }
        shortcuts_page.push(Block::Reset);
        vec![
            Page { name: "General", blocks: general },
            Page { name: "Capture", blocks: capture },
            Page { name: "After capture", blocks: after },
            Page { name: "Recording", blocks: recording },
            Page { name: "Shortcuts", blocks: shortcuts_page },
        ]
    }

    // -- the window ------------------------------------------------------------------------

    pub fn set_view(&mut self, w: f32, h: f32) {
        if (w, h) != self.view {
            self.view = (w, h);
            self.clamp_scroll();
            self.place_fields();
            self.damaged = true;
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
            let r = r.adjusted(-2.0, -2.0, 2.0, 2.0);
            let (x0, y0) = ((r.x * self.scale).floor() as i32, (r.y * self.scale).floor() as i32);
            let (x1, y1) = ((r.right() * self.scale).ceil() as i32, (r.bottom() * self.scale).ceil() as i32);
            d.add(IRect::new(x0, y0, x1 - x0, y1 - y0));
        }
        d
    }

    /// The pointer is over something else: only those two are drawn again.
    fn hover(&mut self, hovered: Option<Hit>) {
        if hovered == self.hovered {
            return;
        }
        let layout = self.layout();
        for h in [self.hovered, hovered].into_iter().flatten() {
            match layout.hits.iter().find(|(x, _)| *x == h) {
                Some((_, r)) => self.touched.push(*r),
                None => self.damaged = true,
            }
        }
        self.hovered = hovered;
    }

    pub fn take_requests(&mut self) -> Vec<Request> {
        std::mem::take(&mut self.requests)
    }

    /// KDE's shortcut keys, by action, when known or changed (None: no
    /// shortcut service).
    pub fn set_global(&mut self, global: Option<BTreeMap<String, String>>) {
        let enabled = global.is_some();
        self.global_shortcuts = Some(enabled);
        let keys = global.unwrap_or_default();
        let recording = self.recording.and_then(|(b, r)| self.row(b, r).map(|row| row.id));
        for (action, _, _) in shortcuts::ACTIONS {
            if recording == Some(sc_id(action)) {
                continue; // (being typed)
            }
            if let Some(row) = self.find_mut(sc_id(action)) {
                row.control = Control::Shortcut { seq: keys.get(*action).cloned().unwrap_or_default(), enabled };
            }
        }
        if let Some(Block::Note(n)) = self.pages.iter_mut().find(|p| p.name == "Shortcuts").and_then(|p| p.blocks.get_mut(1)) {
            *n = global_note(Some(enabled));
        }
        self.damaged = true;
    }

    /// Show page `index` (0 General ... 4 Shortcuts).
    pub fn show_page(&mut self, index: usize) {
        self.leave_field();
        self.stop_recording();
        self.page = index.min(self.pages.len() - 1);
        self.scroll = 0.0;
        self.combo = None;
        self.damaged = true;
    }

    // -- rows and settings -------------------------------------------------------------------

    #[cfg(test)]
    fn rows(&self) -> impl Iterator<Item = (usize, usize, &Row)> {
        self.pages[self.page].blocks.iter().enumerate().flat_map(|(b, block)| match block {
            Block::Card { rows, .. } => rows.iter().enumerate().map(move |(r, row)| (b, r, row)).collect::<Vec<_>>(),
            _ => Vec::new(),
        })
    }

    fn row_mut(&mut self, b: usize, r: usize) -> Option<&mut Row> {
        match self.pages[self.page].blocks.get_mut(b)? {
            Block::Card { rows, .. } => rows.get_mut(r),
            _ => None,
        }
    }

    fn row(&self, b: usize, r: usize) -> Option<&Row> {
        match self.pages[self.page].blocks.get(b)? {
            Block::Card { rows, .. } => rows.get(r),
            _ => None,
        }
    }

    /// The row `id` anywhere.
    fn find_mut(&mut self, id: &str) -> Option<&mut Row> {
        self.pages.iter_mut().flat_map(|p| p.blocks.iter_mut()).find_map(|b| match b {
            Block::Card { rows, .. } => rows.iter_mut().find(|r| r.id == id),
            _ => None,
        })
    }

    /// Save one setting (by its config name), keeping the rest on disk as it is.
    fn set(&mut self, key: &str, value: Value) {
        let mut m = self.cfg.to_map();
        m.insert(key.to_string(), value.clone());
        self.cfg = Config::from_map(&m);
        let key = key.to_string();
        config::update(move |c| {
            let mut m = c.to_map();
            m.insert(key, value);
            *c = Config::from_map(&m);
        });
        self.sync_visibility();
        self.damaged = true;
    }

    /// Rows shown only with another's setting.
    fn sync_visibility(&mut self) {
        let lossy: Vec<&str> = crate::codec::writable_formats().iter().filter(|f| f.2).map(|f| f.0).collect();
        let quality = lossy.contains(&self.cfg.format.as_str());
        let (loupe, sound) = (self.cfg.show_loupe, self.cfg.sound);
        let previews = [("preview", self.preview(false)), ("record_preview", self.preview(true))];
        for (id, shown) in [("quality", quality), ("loupe_size", loupe), ("sound_file", sound)] {
            if let Some(r) = self.find_mut(id) {
                r.visible = shown;
            }
        }
        for (id, text) in previews {
            if let Some(r) = self.find_mut(id) {
                r.below = Control::Text(text);
            }
        }
    }

    /// A live example of where a capture goes, from the folder and name being typed.
    fn preview(&self, recording: bool) -> String {
        let field = |id: &str| {
            self.pages.iter().flat_map(|p| p.blocks.iter()).find_map(|b| match b {
                Block::Card { rows, .. } => rows.iter().find(|r| r.id == id).and_then(|r| match &r.below {
                    Control::Line { edit, .. } => Some(edit.text().trim().to_string()),
                    _ => None,
                }),
                _ => None,
            })
        };
        let (dir, name, ext) = if recording {
            (
                field("record_dir").unwrap_or_else(|| self.cfg.record_dir.clone()),
                field("record_filename").unwrap_or_else(|| self.cfg.record_filename.clone()),
                self.cfg.record_format.clone(),
            )
        } else {
            (
                field("save_dir").unwrap_or_else(|| self.cfg.save_dir.clone()),
                field("filename").unwrap_or_else(|| self.cfg.filename.clone()),
                self.cfg.format.clone(),
            )
        };
        let shot = Shot { mode: "region".into(), app: "firefox".into(), title: "Example page".into(), monitor: "DP-1".into() };
        let path = template::target_path(&dir, &name, &shot, (1920, 1080), &ext, true);
        format!("For example: {}", path.display())
    }

    // -- layout ----------------------------------------------------------------------------

    fn content_x(&self) -> f32 {
        NAV_W + MARGIN
    }

    fn content_w(&self) -> f32 {
        (self.view.0 - NAV_W - 2.0 * MARGIN).max(200.0)
    }

    /// A row's height, and where its parts go (relative to its top left),
    /// for a row `w` wide.
    fn row_layout(row: &Row, w: f32) -> (f32, RowParts) {
        let (cw, ch) = control_size(&row.control);
        let text_w = (w - 28.0 - if cw > 0.0 { cw + 16.0 } else { 0.0 }).max(80.0);
        let title_h = if row.title.is_empty() { 0.0 } else { line_height(title_font()) };
        let hint_lines = if row.hint.is_empty() { Vec::new() } else { text::wrap(&row.hint, hint_font(), text_w) };
        let hint_h = hint_lines.len() as f32 * line_height(hint_font());
        let text_h = title_h + if hint_h > 0.0 { 1.0 + hint_h } else { 0.0 };
        let top_h = text_h.max(ch);
        let mut h = 10.0 + top_h;
        let control = Rect::new(w - 14.0 - cw, 10.0 + (top_h - ch) / 2.0, cw, ch);
        let mut below = None;
        let mut below_lines = Vec::new();
        match &row.below {
            Control::None => {}
            Control::Text(t) => {
                below_lines = text::wrap(t, hint_font(), w - 28.0);
                let bh = below_lines.len() as f32 * line_height(hint_font());
                if top_h > 0.0 {
                    h += 8.0;
                }
                below = Some(Rect::new(14.0, h, w - 28.0, bh));
                h += bh;
            }
            other => {
                let (bw, bh) = control_size(other);
                if top_h > 0.0 {
                    h += 8.0;
                }
                let bw = if bw == 0.0 { w - 28.0 } else { bw };
                below = Some(Rect::new(14.0, h, bw, bh));
                h += bh;
            }
        }
        let mut error_lines = Vec::new();
        if let Some(e) = &row.error {
            error_lines = text::wrap(e, hint_font(), w - 28.0);
            h += 6.0 + error_lines.len() as f32 * line_height(hint_font());
        }
        h += 10.0;
        let h = h.max(48.0);
        (h, RowParts { control, text_x: 14.0, text_w, title_y: 10.0 + (top_h - text_h) / 2.0, hint_lines, below, below_lines, error_lines })
    }

    fn layout(&self) -> Layout {
        let mut hits = Vec::new();
        for (i, _) in self.pages.iter().enumerate() {
            hits.push((Hit::Nav(i), Rect::new(12.0, 18.0 + i as f32 * 34.0, NAV_W - 24.0, 32.0)));
        }
        let (x, w) = (self.content_x(), self.content_w());
        let mut y = 22.0 - self.scroll;
        y += line_height(text::font(18.0, DEMIBOLD)) + 6.0 + 8.0;
        for (b, block) in self.pages[self.page].blocks.iter().enumerate() {
            match block {
                Block::Title(_) => y += 16.0 + line_height(text::font(18.0, DEMIBOLD)) + 8.0,
                Block::Note(t) => y += text::wrap(t, hint_font(), w).len() as f32 * line_height(hint_font()) + 8.0,
                Block::Reset => {
                    hits.push((Hit::Reset, Rect::new(x, y, button_width("Reset all to defaults"), 30.0)));
                    y += 30.0 + 8.0;
                }
                Block::Card { heading, rows } => {
                    if !heading.is_empty() {
                        y += 8.0 + line_height(hint_font()) + 4.0;
                    }
                    for (r, row) in rows.iter().enumerate() {
                        if !row.visible {
                            continue;
                        }
                        let (h, parts) = Self::row_layout(row, w);
                        let at = |rect: Rect| rect.translated(Point::new(x, y));
                        // The most particular parts first: the first hit found wins.
                        if let Control::Shortcut { seq, enabled: true } = &row.control
                            && !seq.is_empty()
                        {
                            let c = at(parts.control);
                            hits.push((Hit::Row(b, r, Part::Clear), Rect::new(c.right() - 26.0, c.y + 4.0, 22.0, 22.0)));
                        }
                        for (control, rect) in [(&row.control, Some(parts.control)), (&row.below, parts.below)] {
                            if let (Control::Segmented { options, dots, .. }, Some(rect)) = (control, rect) {
                                for (i, cell) in segment_cells(at(rect), options, dots).into_iter().enumerate() {
                                    hits.push((Hit::Row(b, r, Part::Segment(i)), cell));
                                }
                            }
                        }
                        if let (Control::Line { buttons, .. }, Some(bel)) = (&row.below, parts.below) {
                            let (field, button_rects) = line_parts(at(bel), buttons);
                            for (i, rect) in button_rects.into_iter().enumerate() {
                                hits.push((Hit::Row(b, r, Part::FieldButton(i)), rect));
                            }
                            hits.push((Hit::Row(b, r, Part::Field), field));
                        }
                        if !matches!(row.control, Control::None) {
                            hits.push((Hit::Row(b, r, Part::Control), at(parts.control)));
                        }
                        y += h;
                    }
                    y += 8.0;
                }
            }
        }
        // The open dropdown's options, over everything.
        if let Some((b, r)) = self.combo
            && let Some(Control::Combo { options, .. }) = self.row(b, r).map(|row| &row.control)
            && let Some(c) = hits.iter().find(|(h, _)| *h == Hit::Row(b, r, Part::Control)).map(|h| h.1)
        {
            let list_h = options.len() as f32 * 30.0 + 8.0;
            let top = if c.bottom() + 4.0 + list_h > self.view.1 { c.y - 4.0 - list_h } else { c.bottom() + 4.0 };
            for i in 0..options.len() {
                hits.insert(0, (Hit::Option(i), Rect::new(c.x + 4.0, top + 4.0 + i as f32 * 30.0, c.w - 8.0, 30.0)));
            }
        }
        Layout { hits, height: y + self.scroll + 24.0 }
    }

    /// Text fields learn where they are now (for clicks, and scrolling their text).
    fn place_fields(&mut self) {
        let fields: Vec<(usize, usize, Rect)> = self
            .layout()
            .hits
            .into_iter()
            .filter_map(|(h, rect)| match h {
                Hit::Row(b, r, Part::Field) => Some((b, r, rect)),
                _ => None,
            })
            .collect();
        for (b, r, rect) in fields {
            if let Some(Row { below: Control::Line { edit, .. }, .. }) = self.row_mut(b, r) {
                edit.rect = rect;
            }
        }
    }

    /// Where the caret of the field being typed in is (window logical),
    /// for the input method.
    pub fn text_input(&self) -> Option<Rect> {
        let (b, r) = self.focus?;
        let Control::Line { edit, .. } = &self.row(b, r)?.below else { return None };
        let m = text::metrics(edit.font);
        let x = edit.rect.x + 9.0 - edit.scroll + text::width(&edit.text()[..edit.edit.cursor], edit.font);
        Some(Rect::new(x, edit.rect.y + (edit.rect.h - m.height) / 2.0, 1.0, m.height))
    }

    fn hit_at(&self, at: Point) -> Option<Hit> {
        let layout = self.layout();
        // Content hidden under nothing but the page: rows can't be hit through the sidebar.
        layout.hits.into_iter().find(|(h, r)| r.contains(at) && (matches!(h, Hit::Nav(_)) || at.x >= NAV_W)).map(|(h, _)| h)
    }

    fn clamp_scroll(&mut self) {
        let h = self.layout().height;
        self.scroll = self.scroll.clamp(0.0, (h - self.view.1).max(0.0));
    }

    // -- input -----------------------------------------------------------------------------

    pub fn cursor(&self) -> CursorShape {
        match self.hovered {
            Some(Hit::Row(_, _, Part::Field)) if self.is_line_hovered() => CursorShape::Text,
            Some(_) => CursorShape::Hand,
            None => CursorShape::Arrow,
        }
    }

    fn is_line_hovered(&self) -> bool {
        matches!(self.hovered, Some(Hit::Row(b, r, Part::Field)) if matches!(self.row(b, r).map(|x| &x.below), Some(Control::Line { .. })))
    }

    pub fn pointer_motion(&mut self, at: Point) {
        self.pointer = Some(at);
        if let Some((b, r)) = self.sliding {
            self.slide_to(b, r, at.x, false);
            return;
        }
        if let Some((b, r)) = self.focus
            && self.pressed == Some(Hit::Row(b, r, Part::Field))
            && let Some(Row { below: Control::Line { edit, .. }, .. }) = self.row_mut(b, r)
        {
            // (the field was placed when pressed)
            edit.drag_to(at);
            self.damaged = true;
            return;
        }
        let hovered = self.hit_at(at);
        self.hover(hovered);
    }

    pub fn pointer_leave(&mut self) {
        self.pointer = None;
        self.hover(None);
    }

    pub fn scroll_by(&mut self, steps: f32) {
        self.combo = None;
        self.scroll -= steps * 48.0;
        self.clamp_scroll();
        self.place_fields();
        if let Some(p) = self.pointer {
            self.hovered = self.hit_at(p);
        }
        self.damaged = true;
    }

    pub fn button(&mut self, button: Button, pressed: bool, at: Point, mods: i32) {
        self.mods = mods;
        self.pointer = Some(at);
        if button != Button::Left {
            return;
        }
        if !pressed {
            if let Some((b, r)) = self.sliding.take() {
                self.slide_to(b, r, at.x, true);
            }
            let was = self.pressed.take();
            if let (Some(h), Some(now)) = (was, self.hit_at(at))
                && h == now
                && !matches!(h, Hit::Row(_, _, Part::Field))
            {
                self.click(h);
            }
            return;
        }
        self.place_fields();
        let hit = self.hit_at(at);
        // A press elsewhere: the dropdown closes, the field being typed in is done.
        if let Some(combo) = self.combo
            && !matches!(hit, Some(Hit::Option(_)))
        {
            self.combo = None;
            self.damaged = true;
            if hit == Some(Hit::Row(combo.0, combo.1, Part::Control)) {
                return;
            }
        }
        if let Some(rec) = self.recording
            && hit != Some(Hit::Row(rec.0, rec.1, Part::Control))
        {
            self.stop_recording();
        }
        let field = match hit {
            Some(Hit::Row(b, r, Part::Field)) if matches!(self.row(b, r).map(|x| &x.below), Some(Control::Line { .. })) => Some((b, r)),
            _ => None,
        };
        if field != self.focus {
            self.leave_field();
        }
        self.pressed = hit;
        match hit {
            Some(Hit::Row(b, r, Part::Field)) if field.is_some() => {
                self.focus = Some((b, r));
                let shift = mods & keyseq::SHIFT != 0;
                if let Some(Row { below: Control::Line { edit, .. }, .. }) = self.row_mut(b, r) {
                    edit.press(at, shift);
                }
                self.damaged = true;
            }
            Some(Hit::Row(b, r, Part::Control)) if matches!(self.row(b, r).map(|x| &x.control), Some(Control::Slider { .. })) => {
                self.sliding = Some((b, r));
                self.slide_to(b, r, at.x, false);
            }
            _ => {}
        }
    }

    /// A click (press and release) on `h`.
    fn click(&mut self, h: Hit) {
        self.damaged = true;
        match h {
            Hit::Nav(i) => self.show_page(i),
            Hit::Reset => {
                self.set("keys", Value::Object(Default::default()));
                for (action, _, default, _) in keys::BINDINGS {
                    if let Some(row) = self.find_mut(key_id(action)) {
                        row.control = Control::Shortcut { seq: default.to_string(), enabled: true };
                        row.error = None;
                    }
                }
            }
            Hit::Option(i) => {
                if let Some((b, r)) = self.combo.take()
                    && let Some(row) = self.row_mut(b, r)
                    && let Control::Combo { options, value } = &mut row.control
                    && let Some((v, _)) = options.get(i)
                {
                    *value = v.clone();
                    let (id, v) = (row.id, v.clone());
                    self.set(id, Value::from(v));
                }
            }
            Hit::Row(b, r, part) => self.click_row(b, r, part),
        }
    }

    fn click_row(&mut self, b: usize, r: usize, part: Part) {
        let Some(row) = self.row_mut(b, r) else { return };
        let id = row.id;
        match (part, &mut row.control, &mut row.below) {
            (Part::Control, Control::Toggle(on), _) => {
                *on = !*on;
                let on = *on;
                if id == "autostart" {
                    if let Err(e) = autostart::set_enabled(on) {
                        row.error = Some(format!("Couldn't change it: {e}"));
                    }
                } else {
                    self.set(id, Value::Bool(on));
                }
            }
            (Part::Segment(i), Control::Segmented { options, value, .. }, _) | (Part::Segment(i), _, Control::Segmented { options, value, .. }) => {
                let Some((v, _)) = options.get(i) else { return };
                if *value == *v {
                    return;
                }
                *value = v.clone();
                let v = v.clone();
                if id == "theme" {
                    theme::use_theme(&v);
                    self.requests.push(Request::ThemeChanged);
                }
                let json = if id == "record_fps" { Value::from(v.parse::<i64>().unwrap_or(30)) } else { Value::from(v.clone()) };
                self.set(id, json);
                if id == "record_format" {
                    self.requests.push(Request::CheckRecorder(v));
                }
            }
            (Part::Control, Control::Button(_), _) if id == "config_folder" => {
                if let Some(dir) = config::config_path().parent() {
                    let _ = std::fs::create_dir_all(dir);
                    crate::deliver::open_file(dir);
                }
            }
            (Part::Control, Control::Combo { .. }, _) => {
                self.combo = Some((b, r));
            }
            (Part::Control, Control::Shortcut { enabled: true, .. }, _) => self.start_recording(b, r),
            (Part::Clear, Control::Shortcut { .. }, _) => self.assign(b, r, String::new()),
            (Part::FieldButton(i), _, Control::Line { buttons, edit }) => {
                let label = buttons.get(i).copied().unwrap_or("");
                let current = edit.text().trim().to_string();
                match (id, label) {
                    ("save_dir" | "record_dir", _) => self.requests.push(Request::ChooseFolder(id, current)),
                    ("sound_file", "Choose…") => self.requests.push(Request::ChooseSound(current)),
                    ("sound_file", _) => {
                        self.set("sound_file", Value::from(current));
                        let ok = sound::play(&self.cfg.sound_file);
                        if let Some(row) = self.find_mut("sound_file") {
                            row.error = (!ok)
                                .then(|| "Couldn't play it: no such file, or no player (pw-play, paplay, ffplay or canberra-gtk-play) installed.".to_string());
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    fn slide_to(&mut self, b: usize, r: usize, x: f32, done: bool) {
        let layout = self.layout();
        let Some(rect) = layout.hits.iter().find(|(h, _)| *h == Hit::Row(b, r, Part::Control)).map(|h| h.1) else { return };
        let Some(row) = self.row_mut(b, r) else { return };
        let id = row.id;
        let Control::Slider { value, min, max, step, .. } = &mut row.control else { return };
        let track = slider_track(rect);
        let t = ((x - track.x) / track.w.max(1.0)).clamp(0.0, 1.0);
        let raw = *min as f32 + t * (*max - *min) as f32;
        let v = (((raw / *step as f32).round() as i64) * *step).clamp(*min, *max);
        let changed = v != *value;
        *value = v;
        if changed {
            self.touched.push(rect);
        }
        if done {
            self.set(id, Value::from(v));
        }
    }

    fn start_recording(&mut self, b: usize, r: usize) {
        self.leave_field();
        self.recording = Some((b, r));
        if self.row(b, r).is_some_and(|row| row.id.starts_with("sc.")) {
            self.requests.push(Request::BlockShortcuts(true));
        }
        self.damaged = true;
    }

    fn stop_recording(&mut self) {
        if let Some((b, r)) = self.recording.take() {
            if self.row(b, r).is_some_and(|row| row.id.starts_with("sc.")) {
                self.requests.push(Request::BlockShortcuts(false));
            }
            self.damaged = true;
        }
    }

    /// New keys for the shortcut or key in row (b, r).
    fn assign(&mut self, b: usize, r: usize, seq: String) {
        let Some(row) = self.row(b, r) else { return };
        let id = row.id;
        if let Some(action) = id.strip_prefix("sc.") {
            // KDE's: the app asks the shortcut service, then says how it went.
            if let Some(a) = shortcuts::ACTIONS.iter().find(|x| x.0 == action) {
                self.requests.push(Request::AssignShortcut(a.0, seq));
            }
            return;
        }
        let Some(action) = id.strip_prefix("key.") else { return };
        let Some(binding) = keys::binding(action) else { return };
        let label = keyseq::label(&seq);
        let error = if ["Esc", "Return", "Enter"].contains(&label.as_str()) {
            Some(format!("{label} is kept for cancelling and capturing."))
        } else {
            let current = keys::Keymap::new(&self.cfg.keys).keys;
            current
                .iter()
                .find(|(a, k)| a.as_str() != action && !k.is_empty() && !seq.is_empty() && keyseq::label(k) == label && keys::overlaps(a, action))
                .and_then(|(a, _)| keys::binding(a))
                .map(|clash| format!("{label} is already {}.", clash.1))
        };
        if let Some(e) = error {
            if let Some(row) = self.row_mut(b, r) {
                row.error = Some(e);
            }
            self.damaged = true;
            return;
        }
        let mut changed = self.cfg.keys.clone();
        if seq == binding.2 {
            changed.remove(action); // (back to the default)
        } else {
            changed.insert(action.to_string(), seq.clone());
        }
        let map: serde_json::Map<String, Value> = changed.into_iter().map(|(k, v)| (k, Value::from(v))).collect();
        self.set("keys", Value::Object(map));
        if let Some(row) = self.row_mut(b, r) {
            row.error = None;
            row.control = Control::Shortcut { seq, enabled: true };
        }
    }

    /// How a KDE shortcut change went: its keys now, or why not.
    pub fn shortcut_result(&mut self, action: &str, now: String, error: Option<String>) {
        let id = sc_id(action);
        if let Some(row) = self.find_mut(id) {
            row.control = Control::Shortcut { seq: now, enabled: true };
            row.error = error;
        }
        self.damaged = true;
    }

    /// A folder or file the desktop's dialog gave for row `id`.
    pub fn chosen(&mut self, id: &str, path: &str) {
        if let Some(row) = self.find_mut(id)
            && let Control::Line { edit, .. } = &mut row.below
        {
            edit.edit.set_text(path);
            row.error = None;
        }
        self.set(id, Value::from(path.to_string()));
    }

    /// What recording here lacks (None: nothing), under the recorder's name.
    pub fn set_recorder_problem(&mut self, problem: Option<String>) {
        if let Some(row) = self.find_mut("recorder") {
            row.error = problem.map(|p| format!("Not usable: {p}"));
        }
        self.damaged = true;
    }

    /// No dialog to choose with: why, under row `id`.
    pub fn dialog_failed(&mut self, id: &str, why: &str) {
        if let Some(row) = self.find_mut(id) {
            row.error = Some(why.to_string());
        }
        self.damaged = true;
    }

    /// Done with the text field being typed in: its setting is saved.
    fn leave_field(&mut self) {
        let Some((b, r)) = self.focus.take() else { return };
        let Some(row) = self.row_mut(b, r) else { return };
        let id = row.id;
        if let Control::Line { edit, .. } = &mut row.below {
            edit.focused = false;
            let text = edit.text().trim().to_string();
            self.set(id, Value::from(text));
        }
        self.damaged = true;
    }

    pub fn modifiers(&mut self, mods: i32) {
        self.mods = mods;
    }

    pub fn key(&mut self, k: &KeyPress) {
        if let Some((b, r)) = self.recording {
            const MODIFIER_KEYS: [i32; 6] = [0x0100_0020, 0x0100_0021, 0x0100_0022, 0x0100_0023, 0x0100_1103, 0x0100_0053];
            if MODIFIER_KEYS.contains(&k.key) {
                return; // (wait for the key they go with)
            }
            let plain = k.mods & keyseq::MODS == 0;
            if k.key == key::ESCAPE && plain {
                self.stop_recording();
                return;
            }
            let seq = if (k.key == key::BACKSPACE || k.key == key::DELETE) && plain { String::new() } else { keyseq::to_text(k.key | (k.mods & keyseq::MODS)) };
            self.stop_recording();
            self.assign(b, r, seq);
            return;
        }
        if let Some((b, r)) = self.focus {
            self.place_fields();
            let mut result = KeyResult::Ignored;
            if let Some(Row { below: Control::Line { edit, .. }, .. }) = self.row_mut(b, r) {
                result = edit.key(k);
            }
            match result {
                KeyResult::Commit => self.leave_field(),
                KeyResult::Copy(s) => self.requests.push(Request::CopyText(s)),
                KeyResult::Paste => self.requests.push(Request::Paste),
                _ => {}
            }
            self.sync_visibility();
            self.damaged = true;
            return;
        }
        let plain = k.mods & keyseq::MODS == 0;
        if k.key == key::ESCAPE && plain && self.combo.take().is_some() {
            self.damaged = true;
        } else if (k.key == key::ESCAPE && plain) || (k.key == 'W' as i32 && k.mods & keyseq::MODS == keyseq::CTRL) {
            self.closing();
            self.requests.push(Request::Close);
        }
    }

    pub fn commit_string(&mut self, s: &str) {
        self.paste(s);
    }

    pub fn paste(&mut self, s: &str) {
        self.place_fields();
        if let Some((b, r)) = self.focus
            && let Some(Row { below: Control::Line { edit, .. }, .. }) = self.row_mut(b, r)
        {
            edit.edit.paste(s);
            self.sync_visibility();
            self.damaged = true;
        }
    }

    pub fn wants_text(&self) -> bool {
        self.focus.is_some()
    }

    /// Before the window closes: the field being typed in is saved, and
    /// KDE's shortcuts work again.
    pub fn closing(&mut self) {
        self.leave_field();
        self.stop_recording();
    }

    // -- painting --------------------------------------------------------------------------

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), _region: IRect) {
        let c = theme::c();
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        p.fill_rect(Rect::new(0.0, 0.0, self.view.0, self.view.1), c.base);
        // The page.
        p.save();
        p.clip_rect(Rect::new(NAV_W, 0.0, self.view.0 - NAV_W, self.view.1));
        let layout = self.layout();
        let (x, w) = (self.content_x(), self.content_w());
        let mut y = 22.0 - self.scroll;
        let title = text::font(18.0, DEMIBOLD);
        p.draw_text(Rect::new(x, y, w, line_height(title)), Align::Left, self.pages[self.page].name, title, c.text);
        y += line_height(title) + 6.0 + 8.0;
        for (b, block) in self.pages[self.page].blocks.iter().enumerate() {
            match block {
                Block::Title(t) => {
                    y += 16.0;
                    p.draw_text(Rect::new(x, y, w, line_height(title)), Align::Left, t, title, c.text);
                    y += line_height(title) + 8.0;
                }
                Block::Note(t) => {
                    for l in text::wrap(t, hint_font(), w) {
                        p.draw_text(Rect::new(x, y, w, line_height(hint_font())), Align::Left, &l, hint_font(), c.muted);
                        y += line_height(hint_font());
                    }
                    y += 8.0;
                }
                Block::Reset => {
                    let r = Rect::new(x, y, button_width("Reset all to defaults"), 30.0);
                    paint_button(&mut p, r, "Reset all to defaults", self.hovered == Some(Hit::Reset), true);
                    y += 38.0;
                }
                Block::Card { heading, rows } => {
                    if !heading.is_empty() {
                        y += 8.0;
                        p.draw_text(Rect::new(x, y, w, line_height(hint_font())), Align::Left, heading, hint_font(), c.muted);
                        y += line_height(hint_font()) + 4.0;
                    }
                    let shown: Vec<(usize, &Row)> = rows.iter().enumerate().filter(|(_, r)| r.visible).collect();
                    let heights: Vec<(f32, RowParts)> = shown.iter().map(|(_, row)| Self::row_layout(row, w)).collect();
                    let total: f32 = heights.iter().map(|h| h.0).sum();
                    p.draw_rounded_rect(Rect::new(x + 0.5, y + 0.5, w - 1.0, total - 1.0), 8.0, Some(c.raised), Some(Pen::new(c.line, 1.0)));
                    for (n, ((r, row), (h, parts))) in shown.iter().zip(heights).enumerate() {
                        if n > 0 {
                            p.fill_rect(Rect::new(x + 14.0, y, w - 28.0, 1.0), c.line);
                        }
                        if y + h > 0.0 && y < self.view.1 {
                            self.paint_row(&mut p, b, *r, row, &parts, Point::new(x, y));
                        }
                        y += h;
                    }
                    y += 8.0;
                }
            }
        }
        p.restore();
        // A thin scroll bar, when the page is longer than the window.
        if layout.height > self.view.1 {
            let frac = self.view.1 / layout.height;
            let bar_h = (self.view.1 * frac).max(32.0);
            let top = (self.scroll / (layout.height - self.view.1)) * (self.view.1 - bar_h);
            p.fill_rounded_rect(Rect::new(self.view.0 - 6.0, top + 2.0, 4.0, bar_h - 4.0), 2.0, c.line);
        }
        // The sidebar.
        p.fill_rect(Rect::new(0.0, 0.0, NAV_W, self.view.1), c.ink);
        for (i, page) in self.pages.iter().enumerate() {
            let r = Rect::new(12.0, 18.0 + i as f32 * 34.0, NAV_W - 24.0, 32.0);
            let selected = i == self.page;
            if selected || self.hovered == Some(Hit::Nav(i)) {
                p.fill_rounded_rect(r, 6.0, if selected { c.raised } else { c.base });
            }
            p.draw_text(r.adjusted(12.0, 0.0, 0.0, 0.0), Align::Left, page.name, control_font(), if selected { c.text } else { c.muted });
        }
        p.draw_text(Rect::new(24.0, self.view.1 - 34.0, NAV_W - 36.0, 20.0), Align::Left, &format!("Flatshot {}", crate::cli::VERSION), hint_font(), c.muted);
        // The open dropdown, over everything.
        if let Some((b, r)) = self.combo
            && let Some(Control::Combo { options, value }) = self.row(b, r).map(|row| &row.control)
        {
            let opts: Vec<Rect> = (0..options.len()).filter_map(|i| layout.hits.iter().find(|(h, _)| *h == Hit::Option(i)).map(|h| h.1)).collect();
            if let (Some(first), Some(last)) = (opts.first(), opts.last()) {
                let bg = Rect::new(first.x - 4.0, first.y - 4.0, first.w + 8.0, last.bottom() - first.y + 8.0);
                p.draw_rounded_rect(bg, 8.0, Some(c.raised), Some(Pen::new(c.line, 1.0)));
                for (i, (rect, (v, label))) in opts.iter().zip(options).enumerate() {
                    if self.hovered == Some(Hit::Option(i)) || v == value {
                        p.fill_rounded_rect(*rect, 6.0, c.hover);
                    }
                    p.draw_text(rect.adjusted(10.0, 0.0, 0.0, 0.0), Align::Left, label, control_font(), c.text);
                }
            }
        }
    }

    fn paint_row(&self, p: &mut Painter<'_>, b: usize, r: usize, row: &Row, parts: &RowParts, at: Point) {
        let c = theme::c();
        let mut y = at.y + parts.title_y;
        if !row.title.is_empty() {
            p.draw_text(Rect::new(at.x + parts.text_x, y, parts.text_w, line_height(title_font())), Align::Left, &row.title, title_font(), c.text);
            y += line_height(title_font()) + 1.0;
        }
        for l in &parts.hint_lines {
            p.draw_text(Rect::new(at.x + parts.text_x, y, parts.text_w, line_height(hint_font())), Align::Left, l, hint_font(), c.muted);
            y += line_height(hint_font());
        }
        let hovered = |part: Part| self.hovered == Some(Hit::Row(b, r, part));
        let control = parts.control.translated(at);
        self.paint_control(p, &row.control, control, b, r, &hovered);
        if let Some(bel) = parts.below {
            let bel = bel.translated(at);
            match &row.below {
                Control::Text(_) => {
                    let mut ty = bel.y;
                    for l in &parts.below_lines {
                        p.draw_text(Rect::new(bel.x, ty, bel.w, line_height(hint_font())), Align::Left, l, hint_font(), c.muted);
                        ty += line_height(hint_font());
                    }
                }
                other => self.paint_control(p, other, bel, b, r, &hovered),
            }
        }
        if !parts.error_lines.is_empty() {
            let mut ey = at.y + Self::row_layout(row, self.content_w()).0 - 10.0 - parts.error_lines.len() as f32 * line_height(hint_font());
            for l in &parts.error_lines {
                p.draw_text(Rect::new(at.x + 14.0, ey, self.content_w() - 28.0, line_height(hint_font())), Align::Left, l, hint_font(), c.accent);
                ey += line_height(hint_font());
            }
        }
    }

    fn paint_control(&self, p: &mut Painter<'_>, control: &Control, rect: Rect, b: usize, r: usize, hovered: &dyn Fn(Part) -> bool) {
        let c = theme::c();
        match control {
            Control::None | Control::Text(_) => {}
            Control::Toggle(on) => {
                p.fill_rounded_rect(rect, 10.0, if *on { c.accent } else { c.line });
                let x = if *on { rect.right() - 18.0 } else { rect.x + 2.0 };
                p.fill_ellipse(Rect::new(x, rect.y + 2.0, 16.0, 16.0), if *on { c.knob } else { c.text });
            }
            Control::Segmented { options, dots, value } => {
                p.draw_rounded_rect(rect.adjusted(0.5, 0.5, -0.5, -0.5), 8.0, Some(c.ink), Some(Pen::new(c.line, 1.0)));
                for (i, (cell, (v, label))) in segment_cells(rect, options, dots).into_iter().zip(options).enumerate() {
                    let selected = v == value;
                    let hover = hovered(Part::Segment(i));
                    if selected || hover {
                        p.fill_rounded_rect(cell, 6.0, if selected { c.hover } else { c.raised });
                    }
                    let mut text_r = cell;
                    if let Some(Some((outer, inner))) = dots.get(i) {
                        let dot = Rect::new(cell.x + 13.0, cell.center().y - 6.0, 12.0, 12.0);
                        p.draw_ellipse(dot, Some(*outer), Some(Pen::new(c.line, 1.0)));
                        p.fill_ellipse(dot.adjusted(3.0, 3.0, -3.0, -3.0), *inner);
                        text_r = Rect::new(cell.x + 18.0, cell.y, cell.w - 18.0, cell.h);
                    }
                    let f = if selected { text::font(13.0, DEMIBOLD) } else { control_font() };
                    p.draw_text(
                        text_r,
                        Align::Center,
                        label,
                        f,
                        if selected {
                            c.text
                        } else if hover {
                            c.soft
                        } else {
                            c.muted
                        },
                    );
                }
            }
            Control::Slider { value, min, max, fmt, .. } => {
                let track = slider_track(rect);
                p.fill_rounded_rect(track, 2.0, c.line);
                let t = (*value - *min) as f32 / (*max - *min).max(1) as f32;
                let x = track.x + track.w * t;
                p.fill_rounded_rect(Rect::new(track.x, track.y, x - track.x, track.h), 2.0, c.soft);
                p.fill_ellipse(Rect::new(x - 7.0, rect.center().y - 7.0, 14.0, 14.0), c.text);
                p.draw_text(Rect::new(rect.x + 190.0, rect.y, 40.0, rect.h), Align::Right, &fmt(*value), hint_font(), c.muted);
            }
            Control::Button(label) => paint_button(p, rect, label, hovered(Part::Control), true),
            Control::Line { edit, buttons } => {
                let (field, button_rects) = line_parts(rect, buttons);
                let mut e = (**edit).clone();
                e.rect = field;
                e.focused = self.focus == Some((b, r));
                e.paint(p, true);
                for (i, (label, r)) in buttons.iter().zip(button_rects).enumerate() {
                    paint_button(p, r, label, hovered(Part::FieldButton(i)), true);
                }
            }
            Control::Shortcut { seq, enabled } => {
                let recording = self.recording == Some((b, r));
                p.draw_rounded_rect(rect.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(c.ink), Some(Pen::new(if recording { c.muted } else { c.line }, 1.0)));
                let text_r = Rect::new(rect.x + 10.0, rect.y, rect.w - 36.0, rect.h);
                if recording {
                    p.draw_text(text_r, Align::Left, "Press a shortcut…", control_font(), c.soft);
                } else if !seq.is_empty() {
                    p.draw_text(text_r, Align::Left, &keyseq::label(seq), control_font(), if *enabled { c.text } else { c.muted });
                    if *enabled {
                        let cx = Point::new(rect.right() - 15.0, rect.center().y);
                        let pen = Pen::new(if hovered(Part::Clear) { c.text } else { c.muted }, 1.4);
                        p.draw_line(Point::new(cx.x - 3.5, cx.y - 3.5), Point::new(cx.x + 3.5, cx.y + 3.5), pen);
                        p.draw_line(Point::new(cx.x + 3.5, cx.y - 3.5), Point::new(cx.x - 3.5, cx.y + 3.5), pen);
                    }
                } else {
                    p.draw_text(text_r, Align::Left, "None", control_font(), c.muted);
                }
            }
            Control::Combo { options, value } => {
                let open = self.combo == Some((b, r));
                p.draw_rounded_rect(rect.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(c.ink), Some(Pen::new(if open { c.muted } else { c.line }, 1.0)));
                let label = options.iter().find(|o| &o.0 == value).map(|o| o.1.as_str()).unwrap_or(value);
                p.draw_text(Rect::new(rect.x + 10.0, rect.y, rect.w - 36.0, rect.h), Align::Left, label, text::font(13.0, NORMAL), c.text);
                let cx = Point::new(rect.right() - 14.0, rect.center().y);
                let pen = Pen::new(c.muted, 1.4);
                p.draw_line(Point::new(cx.x - 3.5, cx.y - 1.5), Point::new(cx.x, cx.y + 2.0), pen);
                p.draw_line(Point::new(cx.x, cx.y + 2.0), Point::new(cx.x + 3.5, cx.y - 1.5), pen);
            }
        }
    }
}

/// Where a row's parts go, relative to its top left.
struct RowParts {
    control: Rect,
    text_x: f32,
    text_w: f32,
    title_y: f32,
    hint_lines: Vec<String>,
    below: Option<Rect>,
    below_lines: Vec<String>,
    error_lines: Vec<String>,
}

fn sc_id(action: &str) -> &'static str {
    match action {
        "region" => "sc.region",
        "window" => "sc.window",
        "monitor" => "sc.monitor",
        "screen" => "sc.screen",
        "last" => "sc.last",
        "pin" => "sc.pin",
        "record" => "sc.record",
        _ => "sc.unknown",
    }
}

/// The row id of a window key's action ("key." and the action, static).
fn key_id(action: &str) -> &'static str {
    // (every action is a &'static str in keys::BINDINGS: its row id is kept in a table)
    static IDS: std::sync::OnceLock<Vec<(&'static str, &'static str)>> = std::sync::OnceLock::new();
    let ids = IDS.get_or_init(|| keys::BINDINGS.iter().map(|b| (b.0, &*Box::leak(format!("key.{}", b.0).into_boxed_str()))).collect());
    ids.iter().find(|(a, _)| *a == action).map(|(_, id)| *id).unwrap_or("key.unknown")
}

/// A text field's own part and its buttons' (in order), in the row's `rect`.
fn line_parts(rect: Rect, buttons: &[&str]) -> (Rect, Vec<Rect>) {
    let mut right = rect.right();
    let mut rects: Vec<Rect> = buttons
        .iter()
        .rev()
        .map(|label| {
            let w = button_width(label);
            right -= w;
            let r = Rect::new(right, rect.y + 1.0, w, 30.0);
            right -= 8.0;
            r
        })
        .collect();
    rects.reverse();
    (Rect::new(rect.x, rect.y, (right - rect.x).max(40.0), 32.0), rects)
}

fn global_note(enabled: Option<bool>) -> String {
    match enabled {
        None => "Looking for KDE's shortcut service…".into(),
        Some(true) => "These are KDE global shortcuts; they also appear in System Settings → Shortcuts → Flatshot.".into(),
        Some(false) => "Global shortcuts need KDE Plasma. Elsewhere, bind the command  flatshot  in your desktop's keyboard settings.".into(),
    }
}

fn segment_cells(rect: Rect, options: &[(String, String)], dots: &[Option<(Color, Color)>]) -> Vec<Rect> {
    let mut x = rect.x + SEG_PAD;
    segment_widths(options, dots)
        .into_iter()
        .map(|w| {
            let cell = Rect::new(x, rect.y + SEG_PAD, w, rect.h - 2.0 * SEG_PAD);
            x += w + SEG_GAP;
            cell
        })
        .collect()
}

fn slider_track(rect: Rect) -> Rect {
    Rect::new(rect.x + 7.0, rect.center().y - 2.0, 180.0 - 14.0, 4.0)
}

fn paint_button(p: &mut Painter<'_>, r: Rect, label: &str, hovered: bool, enabled: bool) {
    let c = theme::c();
    p.draw_rounded_rect(r.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(if hovered && enabled { c.hover } else { c.raised }), Some(Pen::new(c.line, 1.0)));
    p.draw_text(r, Align::Center, label, button_font(), if enabled { c.text } else { c.muted });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn settings() -> Settings {
        let mut s = Settings::new(Config::default());
        s.set_view(760.0, 560.0);
        s
    }

    fn hit_point(s: &Settings, want: Hit) -> Point {
        let r = s.layout().hits.into_iter().find(|(h, _)| *h == want).map(|h| h.1).expect("laid out");
        r.center()
    }

    #[test]
    fn pages_and_rows_lay_out() {
        let mut s = settings();
        for i in 0..s.pages.len() {
            s.show_page(i);
            let l = s.layout();
            assert!(l.height > 100.0, "page {i} has content");
        }
        // Long pages scroll, within bounds.
        s.show_page(4);
        s.scroll_by(-100.0);
        assert!(s.scroll > 0.0 && s.scroll <= s.layout().height);
    }

    #[test]
    fn rows_follow_their_settings() {
        let mut s = settings();
        s.show_page(2);
        // JPEG shows the quality slider; PNG hides it.
        assert!(!s.find_mut("quality").unwrap().visible);
        let (b, r) = s.rows().find(|(_, _, row)| row.id == "format").map(|(b, r, _)| (b, r)).unwrap();
        let at = hit_point(&s, Hit::Row(b, r, Part::Segment(1)));
        s.button(Button::Left, true, at, 0);
        s.button(Button::Left, false, at, 0);
        assert_eq!(s.cfg.format, "jpg");
        assert!(s.find_mut("quality").unwrap().visible);
        // The file name example follows the field being typed.
        assert!(matches!(&s.find_mut("preview").unwrap().below, Control::Text(t) if t.starts_with("For example: ")));
    }

    fn row_at(s: &Settings, id: &str) -> (usize, usize) {
        s.rows().find(|(_, _, row)| row.id == id).map(|(b, r, _)| (b, r)).unwrap()
    }

    fn click_at(s: &mut Settings, at: Point) {
        s.button(Button::Left, true, at, 0);
        s.button(Button::Left, false, at, 0);
    }

    fn click(s: &mut Settings, h: Hit) {
        let at = hit_point(s, h);
        click_at(s, at);
    }

    #[test]
    fn text_fields_and_their_buttons() {
        let mut s = settings();
        s.show_page(2);
        let (b, r) = row_at(&s, "save_dir");
        // The button asks for a dialog; the field doesn't take the keyboard.
        click(&mut s, Hit::Row(b, r, Part::FieldButton(0)));
        assert!(matches!(s.take_requests().as_slice(), [Request::ChooseFolder("save_dir", _)]));
        assert!(!s.wants_text());
        // A click in the field puts the caret where it was clicked: at the end, here.
        let (fb, fr) = row_at(&s, "filename");
        let field = s.layout().hits.into_iter().find(|(h, _)| *h == Hit::Row(fb, fr, Part::Field)).unwrap().1;
        click_at(&mut s, Point::new(field.right() - 4.0, field.center().y));
        assert!(s.wants_text());
        assert!(s.text_input().is_some_and(|c| field.contains(c.center())));
        s.commit_string("_x");
        s.key(&KeyPress { key: key::RETURN, ..Default::default() });
        assert!(!s.wants_text());
        assert!(s.cfg.filename.ends_with("_x"), "{}", s.cfg.filename);
        // A chosen folder replaces the field's text, and clears what went wrong.
        s.dialog_failed("save_dir", "no dialog");
        s.chosen("save_dir", "/tmp/shots");
        assert_eq!(s.cfg.save_dir, "/tmp/shots");
        assert!(s.find_mut("save_dir").unwrap().error.is_none());
    }

    #[test]
    fn global_shortcuts_arrive_later() {
        let mut s = settings();
        s.show_page(4);
        let (b, r) = row_at(&s, "sc.region");
        assert!(matches!(s.row(b, r).unwrap().control, Control::Shortcut { enabled: false, .. }));
        let keys: BTreeMap<String, String> = [("region".to_string(), "Ctrl+Print".to_string())].into();
        s.set_global(Some(keys));
        assert!(matches!(&s.row(b, r).unwrap().control, Control::Shortcut { enabled: true, seq } if seq == "Ctrl+Print"));
        // Typing one: KDE's shortcuts are held meanwhile, and the keys go to KDE.
        click(&mut s, Hit::Row(b, r, Part::Control));
        s.key(&KeyPress { key: 'K' as i32, mods: keyseq::CTRL, ..Default::default() });
        assert_eq!(s.take_requests(), vec![Request::BlockShortcuts(true), Request::BlockShortcuts(false), Request::AssignShortcut("region", "Ctrl+K".into())]);
        // The × clears it.
        s.shortcut_result("region", "Ctrl+K".into(), None);
        click(&mut s, Hit::Row(b, r, Part::Clear));
        assert_eq!(s.take_requests(), vec![Request::AssignShortcut("region", String::new())]);
        // No shortcut service: the rows can't be used.
        s.set_global(None);
        assert!(matches!(s.row(b, r).unwrap().control, Control::Shortcut { enabled: false, .. }));
    }

    #[test]
    fn window_keys_check_for_clashes() {
        let mut s = settings();
        s.show_page(4);
        let (b, r) = s.rows().find(|(_, _, row)| row.id == "key.tool.pen").map(|(b, r, _)| (b, r)).unwrap();
        // L is the line tool's: refused.
        s.assign(b, r, "L".into());
        assert!(s.row(b, r).unwrap().error.as_deref().is_some_and(|e| e.contains("Line")));
        // Esc is kept.
        s.assign(b, r, "Esc".into());
        assert!(s.row(b, r).unwrap().error.as_deref().is_some_and(|e| e.contains("kept")));
        // Q... is the codes key, while capturing only: the editor's pen can't share it either (pen works in both).
        s.assign(b, r, "W".into());
        assert!(s.row(b, r).unwrap().error.is_none());
        assert_eq!(s.cfg.keys.get("tool.pen").map(String::as_str), Some("W"));
        // Recording a key: the next press assigns it.
        s.start_recording(b, r);
        s.key(&KeyPress { key: 'P' as i32, ..Default::default() });
        assert!(!s.cfg.keys.contains_key("tool.pen"), "back to the default");
    }
}
