//! The settings window: a sidebar of pages, each a stack of quiet grouped
//! cards. Changes are saved as they're made and apply from the next
//! capture.
//!
//! The window's state, input and painting, with nothing of the window
//! system: what needs the desktop (file dialogs, KDE's shortcuts, the
//! clipboard) is asked for with [`Request`]s.

use serde_json::Value;
use std::collections::{BTreeMap, HashMap};
use std::time::{Duration, Instant};

use crate::config::{self, Config};
use crate::desktop::{autostart, shortcuts, sound};
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::Canvas;
use crate::keys::{self, KeyPress};
use crate::keyseq::{self, key};
use crate::render::Painter;
use crate::render::paint::{Align, Pen, rounded_rect_path};
use crate::render::text::{self, DEMIBOLD, MEDIUM, NORMAL};
use crate::render::theme::{self, Color, THEMES};
use crate::template::{self, Shot};
use crate::ui::panels::{ColorPicker, LineEdit, PICKER_SIZE, PickerEvent};
use crate::ui::session::{Button, CursorShape};
use crate::ui::state::{CUSTOM, key_for_color};
use crate::ui::textedit::KeyResult;

const NAV_W: f32 = 184.0;
const MARGIN: f32 = 28.0;
const SEG_PAD: f32 = 3.0;
const SEG_GAP: f32 = 2.0;

const CODES_HINT: &str = "Available codes: %Y %m %d %H %M %S (date and time), {app}, {title}, {mode}, {monitor}, {w} and {h} (image size), {n} (counter; {n:4} pads it). A / creates a subfolder; %%, {{ and }} give a literal %, { or }.";

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
    /// the tray icon's look changed: the tray shows it at once
    TrayIconChanged(String),
    CopyText(String),
    Paste,
    /// Esc or Ctrl+W: done with the window
    Close,
    /// find out (in the background) what recording in this format needs
    CheckRecorder(String),
    /// global shortcuts from the desktop's portal: on, or off
    PortalShortcuts(bool),
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
    /// the toolbar's colours, a line each, and a button to add one
    Palette(Vec<Color>),
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
    /// a colour of the palette (it opens the picker), its ×, and the
    /// button that adds one
    Swatch(usize),
    Remove(usize),
    Add,
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
        Control::Palette(colors) => (0.0, colors.len() as f32 * PALETTE_LINE + 30.0),
    }
}

/// A palette's line: its height, gap included.
const PALETTE_LINE: f32 = 38.0;
/// How far the shadow of the palette line held reaches past it.
const SHADOW: f32 = 8.0;
const PALETTE_W: f32 = 340.0;
const ADD_COLOUR: &str = "Add colour";

/// Where a palette's parts go in its `rect`: each colour's line and its ×,
/// then the add button.
fn palette_parts(rect: Rect, n: usize) -> (Vec<(Rect, Rect)>, Rect) {
    let w = rect.w.min(PALETTE_W);
    let lines = (0..n)
        .map(|i| {
            let line = Rect::new(rect.x, rect.y + i as f32 * PALETTE_LINE, w, PALETTE_LINE - 6.0);
            (line, Rect::new(line.right() - 30.0, line.y + 3.0, 26.0, 26.0))
        })
        .collect();
    (lines, Rect::new(rect.x, rect.y + n as f32 * PALETTE_LINE, button_width(ADD_COLOUR), 30.0))
}

/// Where the top of the palette line held is drawn with the pointer at
/// `y`: under the pointer as it was held, but in the list (of `n` lines,
/// the first's top at `first`).
fn held_top(grab: f32, y: f32, first: f32, n: usize) -> f32 {
    (y - grab).clamp(first, first + n.saturating_sub(1) as f32 * PALETTE_LINE)
}

/// A colour to add to `colors`: a default one not there yet, else a new hue.
fn next_colour(colors: &[Color]) -> Color {
    theme::SWATCHES.iter().copied().find(|c| !colors.contains(c)).unwrap_or_else(|| Color::from_hsv((colors.len() as f32 * 0.618_034) % 1.0, 0.75, 1.0))
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

/// How near the window's right edge the scroll bar can be taken.
const BAR_REACH: f32 = 12.0;

/// How far a finger moves before it's scrolling rather than tapping.
const PAN_SLOP: f32 = 10.0;

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
    /// the pointer is a finger (as of its latest move)
    finger: bool,
    /// the scroll bar's thumb being dragged, held this far below its top
    bar_drag: Option<f32>,
    /// a finger down on the page: where, the scroll then, and whether it's
    /// moved far enough to be scrolling (not a tap)
    pan: Option<(Point, f32, bool)>,
    mods: i32,
    damaged: bool,
    /// parts to draw again when not all of it is (logical rectangles)
    touched: Vec<Rect>,
    requests: Vec<Request>,
    /// KDE's shortcut service is there (None: still finding out)
    global_shortcuts: Option<bool>,
    /// the desktop's portal offers global shortcuts (for where KDE's aren't)
    portal: bool,
    /// the picker open for a palette colour: which one, and whether a
    /// press on it is held (dragging)
    picker: Option<(usize, ColorPicker)>,
    picker_held: bool,
    /// a palette colour pressed, to be dragged up or down the list
    reorder: Option<Reorder>,
    /// the palette's lines on their way to their places
    glides: Option<Glides>,
    /// the time drawn at (the latest event's or tick's)
    clock: Instant,
}

/// A palette colour held: where it is in the list now, where the press
/// was and how far below its line's top, and whether it's moved far
/// enough to be dragged (not a click).
#[derive(Clone, Copy, Debug)]
struct Reorder {
    b: usize,
    r: usize,
    at: usize,
    start: Point,
    grab: f32,
    moving: bool,
}

/// How long a palette line takes to glide to its place.
const GLIDE: Duration = Duration::from_millis(180);
/// How often the lines are drawn while they glide (60 Hz).
const FRAME: Duration = Duration::from_millis(16);

/// Palette lines drawn away from their places, gliding there: for each (in
/// the palette's order), how far below its place it was when it set off
/// (logical pixels, negative above), and when; and the one let go, landing
/// (over the others).
#[derive(Clone, Debug)]
struct Glides {
    b: usize,
    r: usize,
    lines: Vec<(f32, Instant)>,
    landing: Option<usize>,
}

/// How far below its place a line that set off `from` below it at `since`
/// is at `now`: quickly at first, slowing as it arrives.
fn glided(from: f32, since: Instant, now: Instant) -> f32 {
    let t = (now.saturating_duration_since(since).as_secs_f32() / GLIDE.as_secs_f32()).min(1.0);
    from * (1.0 - t).powi(3)
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
            finger: false,
            bar_drag: None,
            pan: None,
            mods: 0,
            damaged: true,
            touched: Vec::new(),
            requests: Vec::new(),
            global_shortcuts: None,
            portal: false,
            picker: None,
            picker_held: false,
            reorder: None,
            glides: None,
            clock: Instant::now(),
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
                Row::new("theme", "Theme", "Colours of the toolbar, panels and windows. Drawing colours are not affected.", Control::None)
                    .below(Control::Segmented { options: theme_options, dots, value: theme_value }),
                Row::new("tray_icon", "Tray icon", "Auto uses white or black to match the desktop's light or dark colour scheme.", Control::None)
                    .below(seg(&[("color", "Colour"), ("theme", "Theme"), ("white", "White"), ("black", "Black"), ("auto", "Auto")], &c.tray_icon)),
                Row::new(
                    "autostart",
                    "Start at login",
                    "Starts Flatshot RS in the system tray, so global shortcuts work.",
                    Control::Toggle(autostart::enabled()),
                ),
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
                        "Shows the toolbar on the monitor under the pointer.",
                        Control::Toggle(c.toolbar_follows_mouse),
                    ),
                    Row::new(
                        "span_monitors",
                        "Select across monitors",
                        "Selections, clicked windows and drawings can span several monitors.",
                        Control::Toggle(c.span_monitors),
                    ),
                ],
            },
            Block::Card {
                heading: "Toolbar colours".into(),
                rows: vec![
                    Row::new(
                        "palette",
                        "Drawing colours",
                        "On the toolbar and in the editor, in this order, with your own colour after them. Click one to change it, drag it to move it.",
                        Control::Button("Reset to default"),
                    )
                    .below(Control::Palette(crate::ui::state::palette(&c.palette))),
                ],
            },
            Block::Card {
                heading: "Window capture".into(),
                rows: vec![
                    Row::new(
                        "detect_windows",
                        "Window selection",
                        "Highlights the window under the pointer; clicking captures it. KDE Plasma, Sway, Hyprland and X11.",
                        Control::Toggle(c.detect_windows),
                    ),
                    Row::new(
                        "detect_all_windows",
                        "Include panels and popups",
                        "Panels, docks, notifications, menus and tooltips can also be selected. KDE Plasma and X11.",
                        Control::Toggle(c.detect_all_windows),
                    ),
                    Row::new("window_frame", "Title bar and borders", "Included in active window captures. KDE Plasma only.", Control::Toggle(c.window_frame)),
                    Row::new(
                        "window_shadow",
                        "Window shadow",
                        "Included in active window captures, with a transparent background. JPEG files use white instead.",
                        Control::Toggle(c.window_shadow),
                    ),
                ],
            },
            Block::Card {
                heading: "Mouse pointer".into(),
                rows: vec![
                    Row::new(
                        "include_pointer",
                        "Show in instant captures",
                        "Applies to active window, monitor, all screens and last region captures.",
                        Control::Toggle(c.include_pointer),
                    ),
                    Row::new(
                        "region_pointer",
                        "Show when selecting",
                        "Toggle adds a toolbar button (M) and takes slightly longer to open.",
                        seg(&[("hidden", "Never"), ("shown", "Always"), ("toggle", "Toggle")], &c.region_pointer),
                    ),
                ],
            },
            Block::Card {
                heading: "Snap to edges".into(),
                rows: vec![
                    Row::new(
                        "snap_edges",
                        "Snap selections to edges",
                        "Moves selection corners onto nearby edges in the image. Also toggled with G or the magnet button. Hold Ctrl to place freely.",
                        Control::Toggle(c.snap_edges),
                    ),
                    Row::new(
                        "snap_distance",
                        "Distance",
                        "Maximum distance between the pointer and an edge.",
                        Control::Slider { value: c.snap_distance, min: 2, max: 40, step: 1, fmt: fmt_px },
                    ),
                    Row::new(
                        "snap_sensitivity",
                        "Sensitivity",
                        "Higher values also snap to faint and short edges.",
                        Control::Slider { value: c.snap_sensitivity, min: 1, max: 10, step: 1, fmt: fmt_plain },
                    ),
                ],
            },
            Block::Card {
                heading: "Magnifier and crosshair".into(),
                rows: vec![
                    Row::new(
                        "show_loupe",
                        "Magnifier",
                        "Shows enlarged pixels, coordinates and the colour next to the pointer.",
                        Control::Toggle(c.show_loupe),
                    ),
                    Row::new(
                        "loupe_size",
                        "Magnifier size",
                        "",
                        Control::Slider { value: c.loupe_size, min: config::LOUPE_SIZES.0, max: config::LOUPE_SIZES.1, step: 20, fmt: fmt_px },
                    ),
                    Row::new("show_crosshair", "Crosshair lines", "Lines across the screen through the pointer.", Control::Toggle(c.show_crosshair)),
                    Row::new("rainbow", "Rainbow", "The crosshair and magnifier cycle through colours.", Control::Toggle(c.rainbow)),
                    Row::new("show_hint", "Hints", "Usage tips in the middle of the screen.", Control::Toggle(c.show_hint)),
                ],
            },
            Block::Card {
                heading: "QR codes and barcodes".into(),
                rows: vec![Row::new("show_codes", "Detect and show", "Also toggled with Q or the code button on the toolbar.", Control::Toggle(c.show_codes))],
            },
            Block::Card {
                heading: "Copying text".into(),
                rows: vec![{
                    let mut row = Row::new(
                        "ocr_languages",
                        "Languages to read",
                        "The text tool on the toolbar (O) copies the text in an area, read by Tesseract. Its codes for the languages, such as eng or eng+deu; each needs its language data installed.",
                        Control::None,
                    )
                    .below(line(&c.ocr_languages, "eng", vec![]));
                    row.error = crate::ocr::problem();
                    row
                }],
            },
            Block::Card {
                heading: "Advanced".into(),
                rows: vec![
                    Row::new(
                        "screenshot_method",
                        "Screenshot",
                        "Each screen keeps every monitor at its own resolution when scales differ, and is the fastest.",
                        seg(&[("desktop", "Whole desktop"), ("screens", "Each screen")], &c.screenshot_method),
                    ),
                    Row::new(
                        "backend",
                        "Capture backend",
                        "Automatic uses the fastest one the desktop has.",
                        Control::Combo { options: backends, value: if c.backend == "qt" { "auto".into() } else { c.backend.clone() } },
                    ),
                ],
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
                    Row::new("save_dir", "Folder", "", Control::None).below(line(&c.save_dir, "", vec!["Choose…"])),
                    Row::new("filename", "File name", CODES_HINT, Control::None).below(line(&c.filename, "", vec![])),
                    Row::new("preview", "", "", Control::None).below(Control::Text(String::new())),
                    Row::new("format", "Format", "", Control::Segmented { dots: vec![None; formats.len()], options: formats, value: format_value }),
                    Row::new(
                        "quality",
                        "Quality",
                        "Higher values give sharper images and larger files.",
                        Control::Slider { value: c.quality, min: 10, max: 100, step: 5, fmt: fmt_plain },
                    ),
                ],
            },
            Block::Card {
                heading: "Beautify".into(),
                rows: vec![Row::new(
                    "beautify",
                    "Add a background",
                    "Uses the background, padding, corners and shadow last set in the annotation editor. Also toggled with Ctrl+B or the toolbar button.",
                    Control::Toggle(c.beautify),
                )],
            },
            Block::Card {
                heading: "Then".into(),
                rows: vec![
                    Row::new("clipboard", "Copy to clipboard", "", seg(&[("image", "Image"), ("path", "File path"), ("none", "Nothing")], &c.clipboard)),
                    Row::new("notify", "Show a notification", "Includes Open, Show in folder, Annotate and Pin buttons.", Control::Toggle(c.notify)),
                    Row::new("open_after", "Open afterwards", "", seg(&[("none", "Nothing"), ("image", "Image"), ("folder", "Folder")], &c.open_after)),
                    Row::new("sound", "Play a sound", "", Control::Toggle(c.sound)),
                    Row::new("sound_file", "Sound file", "Empty uses the desktop's screenshot sound. OGG, WAV, FLAC and MP3 are supported.", Control::None)
                        .below(line(&c.sound_file, "The desktop's screenshot sound", vec!["Choose…", "Play"])),
                    Row::new("run_command", "Run a command", "{path} is replaced with the path of the image (also in $FLATSHOT_PATH).", Control::None)
                        .below(line(&c.run_command, "curl -F file=@{path} https://example.com/upload", vec![])),
                ],
            },
        ];
        // Recording
        let recording = vec![
            Block::Card {
                heading: String::new(),
                rows: vec![
                    Row::new("recorder", "Recorder", crate::record::method().label(), Control::None),
                    Row::new("record_dir", "Folder", "", Control::None).below(line(&c.record_dir, "", vec!["Choose…"])),
                    Row::new("record_filename", "File name", CODES_HINT, Control::None).below(line(&c.record_filename, "", vec![])),
                    Row::new("record_preview", "", "", Control::None).below(Control::Text(String::new())),
                    Row::new("record_countdown", "Countdown", "Delay before recording starts.", Control::Slider { value: c.record_countdown, min: 0, max: 10, step: 1, fmt: fmt_seconds_off }),
                ],
            },
            Block::Card {
                heading: "Defaults".into(),
                rows: vec![
                    Row::new("record_format", "Format", "GIF files have no audio.", seg(&[("mp4", "MP4"), ("webm", "WebM"), ("gif", "GIF")], &c.record_format)),
                    Row::new("record_fps", "Frame rate", "", seg(&[("24", "24 fps"), ("30", "30 fps"), ("60", "60 fps"), ("0", "Native")], &c.record_fps.to_string())),
                    Row::new("record_mic", "Microphone", "", Control::Toggle(c.record_mic)),
                    Row::new("record_system_audio", "Computer sound", "", Control::Toggle(c.record_system_audio)),
                    Row::new("record_cursor", "Mouse pointer", "", Control::Toggle(c.record_cursor)),
                ],
            },
            Block::Note("These defaults can also be changed before each recording. The clipboard, notification, open and command settings under After capture also apply to recordings; the clipboard gets the video file.".into()),
        ];
        // Shortcuts (KDE's keys come later)
        let mut global_rows: Vec<Row> = shortcuts::ACTIONS
            .iter()
            .map(|(action, label, _)| Row::new(sc_id(action), label, "", Control::Shortcut { seq: String::new(), enabled: false }))
            .collect();
        // (without KDE's service: the desktop's portal, if it has them; shown once that's known)
        let mut portal = Row::new(
            "portal_shortcuts",
            "Ask the desktop for these",
            "Through its portal. GNOME asks you to confirm the keys; on Hyprland, bind keys to them in its config (hyprctl globalshortcuts lists them).",
            Control::Toggle(c.portal_shortcuts),
        );
        portal.visible = false;
        global_rows.push(portal);
        let mut shortcuts_page =
            vec![Block::Card { heading: String::new(), rows: global_rows }, Block::Note(global_note(None, false)), Block::Title("Keys in Flatshot RS".into())];
        shortcuts_page.push(Block::Note(
            "Keys used in the capture screen and the annotation editor. Click a field and press a key; Backspace clears it. Esc, Enter, and Ctrl+S / Ctrl+C to capture, stay as they are.".into(),
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
            d.add_logical(r, self.scale);
        }
        d
    }

    /// When it next needs drawing on its own: while palette lines glide.
    pub fn next_deadline(&self) -> Option<Instant> {
        self.glides.as_ref().map(|_| self.clock + FRAME)
    }

    pub fn on_timer(&mut self, now: Instant) {
        self.clock = self.clock.max(now);
        let Some(g) = &self.glides else { return };
        let (b, r) = (g.b, g.r);
        let arrived = g.lines.iter().all(|&(from, since)| from == 0.0 || self.clock >= since + GLIDE);
        self.touch_palette(b, r);
        if arrived {
            self.glides = None;
        }
    }

    /// The time now, for what starts moving now.
    fn now(&mut self) -> Instant {
        self.clock = self.clock.max(Instant::now());
        self.clock
    }

    /// The palette at (b, r)'s lines are drawn again (where they glide,
    /// and the shadow of the one held).
    fn touch_palette(&mut self, b: usize, r: usize) {
        if let Some((first, n)) = self.palette_lines(b, r) {
            self.touch_lines(first, n);
        }
    }

    /// The `n` lines from `first` on are drawn again (and their shadows).
    fn touch_lines(&mut self, first: Rect, n: usize) {
        let all = Rect::new(first.x, first.y, first.w, n.saturating_sub(1) as f32 * PALETTE_LINE + first.h);
        self.touched.push(all.adjusted(-SHADOW, -SHADOW, SHADOW, SHADOW));
    }

    /// How far below its place line `i` of the palette at (b, r) is drawn.
    fn glide_offset(&self, b: usize, r: usize, i: usize) -> f32 {
        match &self.glides {
            Some(g) if (g.b, g.r) == (b, r) => g.lines.get(i).map_or(0.0, |&(from, since)| glided(from, since, self.clock)),
            _ => 0.0,
        }
    }

    /// The palette at (b, r)'s glides, `n` lines of them.
    fn glides_mut(&mut self, b: usize, r: usize, n: usize) -> &mut Glides {
        let now = self.clock;
        let g = self.glides.get_or_insert_with(|| Glides { b, r, lines: Vec::new(), landing: None });
        if (g.b, g.r) != (b, r) || g.lines.len() != n {
            *g = Glides { b, r, lines: vec![(0.0, now); n], landing: None };
        }
        g
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
        self.sync_global_note();
        self.sync_visibility();
        self.damaged = true;
    }

    /// Whether the desktop's portal offers global shortcuts (asked in the
    /// background, for where KDE's service isn't).
    pub fn set_portal(&mut self, offered: bool) {
        self.portal = offered;
        self.sync_global_note();
        self.sync_visibility();
        self.damaged = true;
    }

    /// The keys the desktop's portal gave the actions: shown, but changed
    /// in the desktop's own settings, not here.
    pub fn portal_keys(&mut self, keys: &HashMap<String, String>) {
        for (action, _, _) in shortcuts::ACTIONS {
            if let Some(row) = self.find_mut(sc_id(action)) {
                row.control = Control::Shortcut { seq: keys.get(*action).cloned().unwrap_or_default(), enabled: false };
            }
        }
        self.damaged = true;
    }

    /// How asking the desktop's portal for the shortcuts went: why not, if
    /// it didn't.
    pub fn portal_answer(&mut self, error: Option<String>) {
        if let Some(row) = self.find_mut("portal_shortcuts") {
            row.error = error.map(|e| format!("The desktop didn't give them: {e}."));
            self.damaged = true;
        }
    }

    fn sync_global_note(&mut self) {
        let note = global_note(self.global_shortcuts, self.portal);
        if let Some(Block::Note(n)) = self.pages.iter_mut().find(|p| p.name == "Shortcuts").and_then(|p| p.blocks.get_mut(1)) {
            *n = note;
        }
    }

    /// Show page `index` (0 General ... 4 Shortcuts).
    pub fn show_page(&mut self, index: usize) {
        self.close_picker();
        self.leave_field();
        self.stop_recording();
        self.page = index.min(self.pages.len() - 1);
        self.scroll = 0.0;
        self.combo = None;
        self.reorder = None;
        self.glides = None;
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
        let (loupe, sound, snap) = (self.cfg.show_loupe, self.cfg.sound, self.cfg.snap_edges);
        let previews = [("preview", self.preview(false)), ("record_preview", self.preview(true))];
        let portal = self.global_shortcuts == Some(false) && self.portal;
        for (id, shown) in [
            ("quality", quality),
            ("loupe_size", loupe),
            ("sound_file", sound),
            ("snap_distance", snap),
            ("snap_sensitivity", snap),
            ("portal_shortcuts", portal),
        ] {
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
        let shot = Shot { mode: "region".into(), app: "firefox".into(), title: "Example page".into(), monitor: "DP-1".into(), ..Default::default() };
        let path = template::target_path(&dir, &name, &shot, (1920, 1080), &ext, true);
        format!("Example: {}", path.display())
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
                        if let (Control::Palette(colors), Some(bel)) = (&row.below, parts.below) {
                            let (lines, add) = palette_parts(at(bel), colors.len());
                            for (i, (line, remove)) in lines.into_iter().enumerate() {
                                hits.push((Hit::Row(b, r, Part::Remove(i)), remove));
                                hits.push((Hit::Row(b, r, Part::Swatch(i)), line));
                            }
                            hits.push((Hit::Row(b, r, Part::Add), add));
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
        if let Some((_, picker)) = &self.picker {
            return picker.caret_rect();
        }
        let (b, r) = self.focus?;
        let Control::Line { edit, .. } = &self.row(b, r)?.below else { return None };
        Some(edit.caret_rect())
    }

    /// Repaint `r` (window logical).
    pub fn touch(&mut self, r: Rect) {
        self.touched.push(r);
    }

    fn hit_at(&self, at: Point) -> Option<Hit> {
        let layout = self.layout();
        // Content hidden under nothing but the page: rows can't be hit through the sidebar.
        layout.hits.into_iter().find(|(h, r)| r.contains(at) && (matches!(h, Hit::Nav(_)) || at.x >= NAV_W)).map(|(h, _)| h)
    }

    /// The scroll bar's thumb: its top and height, when the page is
    /// longer than the window.
    fn thumb(&self, page_h: f32) -> Option<(f32, f32)> {
        if page_h <= self.view.1 {
            return None;
        }
        let bar_h = (self.view.1 * self.view.1 / page_h).max(32.0);
        Some(((self.scroll / (page_h - self.view.1)) * (self.view.1 - bar_h), bar_h))
    }

    /// Scroll so the thumb's top is at `top`.
    fn drag_thumb(&mut self, top: f32) {
        let h = self.layout().height;
        let Some((_, bar_h)) = self.thumb(h) else { return };
        let room = (self.view.1 - bar_h).max(1.0);
        self.set_scroll(top / room * (h - self.view.1));
    }

    fn set_scroll(&mut self, to: f32) {
        self.combo = None;
        self.close_picker();
        self.scroll = to;
        self.clamp_scroll();
        self.place_fields();
        self.damaged = true;
    }

    fn clamp_scroll(&mut self) {
        let h = self.layout().height;
        self.scroll = self.scroll.clamp(0.0, (h - self.view.1).max(0.0));
    }

    // -- input -----------------------------------------------------------------------------

    pub fn cursor(&self) -> CursorShape {
        if let (Some((_, picker)), Some(at)) = (&self.picker, self.pointer)
            && picker.rect().contains(at)
        {
            return CursorShape::Arrow;
        }
        match self.hovered {
            _ if self.reorder.is_some_and(|d| d.moving) => CursorShape::Move,
            Some(Hit::Row(_, _, Part::Field)) if self.is_line_hovered() => CursorShape::Text,
            Some(_) => CursorShape::Hand,
            None => CursorShape::Arrow,
        }
    }

    fn is_line_hovered(&self) -> bool {
        matches!(self.hovered, Some(Hit::Row(b, r, Part::Field)) if matches!(self.row(b, r).map(|x| &x.below), Some(Control::Line { .. })))
    }

    /// The pointer moved to `at`; `finger`: it's a finger on a touchscreen.
    pub fn pointer_motion(&mut self, at: Point, finger: bool) {
        let near_bar = |p: Option<Point>| p.is_some_and(|p| p.x >= self.view.0 - BAR_REACH);
        if near_bar(self.pointer) != near_bar(Some(at)) {
            self.damaged = true; // (the bar widens under the pointer)
        }
        self.pointer = Some(at);
        self.finger = finger;
        if let Some(grab) = self.bar_drag {
            self.drag_thumb(at.y - grab);
            return;
        }
        if let Some((_, picker)) = &mut self.picker {
            let over = picker.rect().contains(at);
            if self.picker_held || over {
                let ev = picker.motion(at, self.picker_held);
                self.picker_event(ev);
                self.hover(None);
                return;
            }
        }
        if let Some(mut d) = self.reorder {
            if !d.moving && (at.y - d.start.y).abs() > 4.0 {
                d.moving = true;
                self.pressed = None;
                self.hover(None);
                self.touch_palette(d.b, d.r);
            }
            if d.moving {
                self.drag_colour(&mut d, at);
            }
            self.reorder = Some(d);
            if d.moving {
                return;
            }
        }
        if let Some((start, scroll0, panning)) = self.pan {
            let d = at - start;
            if !panning && d.y.abs() > PAN_SLOP && d.y.abs() > d.x.abs() {
                // A finger moving up or down scrolls: it's not a tap any more.
                self.pan = Some((start, scroll0, true));
                self.pressed = None;
                self.sliding = None;
                self.hover(None);
            } else if !panning
                && d.x.abs() > PAN_SLOP
                && let Some(Hit::Row(b, r, Part::Control)) = self.pressed
                && matches!(self.row(b, r).map(|x| &x.control), Some(Control::Slider { .. }))
            {
                // Across a slider: it slides.
                self.pan = None;
                self.sliding = Some((b, r));
            }
            if let Some((_, _, true)) = self.pan {
                self.set_scroll(scroll0 - d.y);
                return;
            }
        }
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
        self.close_picker();
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
            if std::mem::take(&mut self.picker_held) {
                if let Some((_, picker)) = &mut self.picker {
                    let ev = picker.release(at);
                    self.picker_event(ev);
                }
                return;
            }
            if self.bar_drag.take().is_some() {
                self.damaged = true;
                return;
            }
            if let Some(d) = self.reorder.take()
                && d.moving
            {
                self.pressed = None;
                self.drop_colour(&d, at.y);
                // (only its lines changed, and they're drawn as they glide)
                let damaged = self.damaged;
                self.save_palette();
                self.damaged = damaged;
                return;
            }
            if let Some((_, _, panning)) = self.pan.take() {
                if panning {
                    return;
                }
                // A tap on a slider puts it there.
                if let Some(Hit::Row(b, r, Part::Control)) = self.pressed
                    && self.sliding.is_none()
                    && matches!(self.row(b, r).map(|x| &x.control), Some(Control::Slider { .. }))
                {
                    self.pressed = None;
                    self.slide_to(b, r, at.x, true);
                    return;
                }
            }
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
        // The scroll bar: its thumb is dragged; elsewhere on it, the thumb
        // jumps there (centred on the pointer) and is dragged from there.
        if at.x >= self.view.0 - BAR_REACH
            && let Some((top, bar_h)) = self.thumb(self.layout().height)
        {
            let grab = if (top..top + bar_h).contains(&at.y) { at.y - top } else { bar_h / 2.0 };
            self.combo = None;
            self.bar_drag = Some(grab);
            self.drag_thumb(at.y - grab);
            return;
        }
        // The picker takes presses on it; one elsewhere closes it (one on
        // its colour's line only that).
        if let Some((i, picker)) = &mut self.picker {
            if picker.rect().contains(at) {
                self.picker_held = true;
                let ev = picker.press(at, mods & keyseq::SHIFT != 0);
                self.picker_event(ev);
                return;
            }
            let i = *i;
            self.close_picker();
            if matches!(self.hit_at(at), Some(Hit::Row(_, _, Part::Swatch(j))) if j == i) {
                return;
            }
        }
        let hit = self.hit_at(at);
        if let Some(Hit::Row(b, r, Part::Swatch(i))) = hit {
            // (held: dragged up or down it moves, else it's a click)
            let top = self.layout().hits.into_iter().find(|(h, _)| *h == Hit::Row(b, r, Part::Swatch(i))).map_or(at.y, |h| h.1.y);
            self.reorder = Some(Reorder { b, r, at: i, start: at, grab: at.y - top, moving: false });
        } else if self.finger {
            self.pan = Some((at, self.scroll, false));
        }
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
            // (a finger on a slider: sliding or scrolling, as it moves)
            Some(Hit::Row(b, r, Part::Control)) if !self.finger && matches!(self.row(b, r).map(|x| &x.control), Some(Control::Slider { .. })) => {
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
                    if id == "portal_shortcuts" {
                        self.requests.push(Request::PortalShortcuts(on));
                    }
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
                if id == "tray_icon" {
                    self.requests.push(Request::TrayIconChanged(v.clone()));
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
            (Part::Control, Control::Button(_), Control::Palette(colors)) => {
                *colors = theme::SWATCHES.to_vec();
                self.glides = None;
                self.save_palette();
            }
            (Part::Swatch(i), _, Control::Palette(_)) => self.open_picker(b, r, i),
            (Part::Remove(i), _, Control::Palette(colors)) => {
                if colors.len() > 1 && i < colors.len() {
                    colors.remove(i);
                    self.glides = None;
                    self.save_palette();
                }
            }
            (Part::Add, _, Control::Palette(colors)) => {
                if colors.len() < config::MAX_PALETTE {
                    let n = colors.len();
                    colors.push(next_colour(colors));
                    self.glides = None;
                    self.save_palette();
                    self.open_picker(b, r, n);
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
                            row.error = (!ok).then(|| {
                                "Cannot play the sound: the file does not exist, or no player (pw-play, paplay, ffplay or canberra-gtk-play) installed."
                                    .to_string()
                            });
                        }
                    }
                    _ => {}
                }
            }
            _ => {}
        }
    }

    /// The palette's colours, as the settings window has them.
    fn palette_mut(&mut self) -> Option<&mut Vec<Color>> {
        match &mut self.find_mut("palette")?.below {
            Control::Palette(colors) => Some(colors),
            _ => None,
        }
    }

    fn save_palette(&mut self) {
        let Some(colors) = self.palette_mut() else { return };
        let hex: Vec<Value> = colors.iter().map(|c| Value::from(c.hex())).collect();
        self.set("palette", Value::Array(hex));
    }

    /// The colour held is dragged to `at`: drawn there, over the others,
    /// it takes the place its middle is nearest, and the lines it passes
    /// glide out of its way.
    fn drag_colour(&mut self, d: &mut Reorder, at: Point) {
        let now = self.now();
        let Some((line, n)) = self.palette_lines(d.b, d.r) else { return };
        let (first, top) = (line.y, held_top(d.grab, at.y, line.y, n));
        self.touch_lines(line, n);
        let Some(Row { below: Control::Palette(colors), .. }) = self.row_mut(d.b, d.r) else { return };
        let to = (((top - first) / PALETTE_LINE).round().max(0.0) as usize).min(n.saturating_sub(1));
        if to == d.at || d.at >= n {
            return;
        }
        let c = colors.remove(d.at);
        colors.insert(to, c);
        let g = self.glides_mut(d.b, d.r, n);
        g.landing = None;
        let held = g.lines.remove(d.at);
        g.lines.insert(to, held);
        // (each passed is drawn where it was, a line up or down, and sets off from there)
        let (passed, from) = if to > d.at { (d.at..to, PALETTE_LINE) } else { (to + 1..d.at + 1, -PALETTE_LINE) };
        for line in &mut g.lines[passed] {
            *line = (glided(line.0, line.1, now) + from, now);
        }
        d.at = to;
    }

    /// The palette at (b, r): where its first line is, and how many.
    fn palette_lines(&self, b: usize, r: usize) -> Option<(Rect, usize)> {
        let n = match self.row(b, r).map(|row| &row.below) {
            Some(Control::Palette(colors)) => colors.len(),
            _ => return None,
        };
        Some((self.layout().hits.into_iter().find(|(h, _)| *h == Hit::Row(b, r, Part::Swatch(0)))?.1, n))
    }

    /// The colour held is let go at `y`: it glides from there to its place.
    fn drop_colour(&mut self, d: &Reorder, y: f32) {
        let now = self.now();
        let Some((line, n)) = self.palette_lines(d.b, d.r) else { return };
        let (top, place) = (held_top(d.grab, y, line.y, n), line.y + d.at as f32 * PALETTE_LINE);
        let g = self.glides_mut(d.b, d.r, n);
        if let Some(glide) = g.lines.get_mut(d.at) {
            *glide = (top - place, now);
            g.landing = Some(d.at);
        }
        self.touch_lines(line, n);
    }

    /// The palette line at (b, r) landing after it was let go, and how much
    /// of its way it has left (1 to 0).
    fn landing(&self, b: usize, r: usize) -> Option<(usize, f32)> {
        let g = self.glides.as_ref().filter(|g| (g.b, g.r) == (b, r))?;
        let i = g.landing?;
        let (from, since) = *g.lines.get(i)?;
        let t = (self.clock.saturating_duration_since(since).as_secs_f32() / GLIDE.as_secs_f32()).min(1.0);
        (from != 0.0).then_some((i, (1.0 - t).powi(3)))
    }

    /// The picker for palette colour `i` (row (b, r)), under its line, or
    /// over it where there's no room below.
    fn open_picker(&mut self, b: usize, r: usize, i: usize) {
        let Some(line) = self.layout().hits.into_iter().find(|(h, _)| *h == Hit::Row(b, r, Part::Swatch(i))).map(|h| h.1) else { return };
        let Some(color) = self.palette_mut().and_then(|c| c.get(i).copied()) else { return };
        let mut picker = ColorPicker::new(color).without_dropper();
        let y = if line.bottom() + 4.0 + PICKER_SIZE.1 <= self.view.1 - 8.0 { line.bottom() + 4.0 } else { line.y - 4.0 - PICKER_SIZE.1 };
        let x = line.x.clamp(NAV_W + 8.0, (self.view.0 - PICKER_SIZE.0 - 8.0).max(NAV_W + 8.0));
        picker.pos = Point::new(x.round(), y.max(8.0).round());
        self.leave_field();
        self.picker = Some((i, picker));
        self.damaged = true;
    }

    /// Done with the picker: the colour it chose is saved.
    fn close_picker(&mut self) {
        self.picker_held = false;
        if self.picker.take().is_some() {
            self.save_palette();
            self.damaged = true;
        }
    }

    fn picker_event(&mut self, ev: PickerEvent) {
        self.damaged = true;
        match ev {
            PickerEvent::Color(color) => {
                let Some(i) = self.picker.as_ref().map(|p| p.0) else { return };
                if let Some(c) = self.palette_mut().and_then(|c| c.get_mut(i)) {
                    *c = Color { a: 255, ..color };
                }
            }
            PickerEvent::Close => self.close_picker(),
            PickerEvent::Copy(s) => self.requests.push(Request::CopyText(s)),
            PickerEvent::Paste => self.requests.push(Request::Paste),
            PickerEvent::None | PickerEvent::Eyedropper => {}
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
            Some(format!("{label} is reserved for cancelling and capturing."))
        } else {
            let current = keys::Keymap::new(&self.cfg.keys).keys;
            current
                .iter()
                .find(|(a, k)| a.as_str() != action && !k.is_empty() && !seq.is_empty() && keyseq::label(k) == label && keys::overlaps(a, action))
                .and_then(|(a, _)| keys::binding(a))
                .map(|clash| format!("{label} is already assigned to {}.", clash.1))
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
        if let Some((_, picker)) = &mut self.picker {
            if picker.hex.focused {
                let ev = picker.key(k);
                self.picker_event(ev);
            } else if k.key == key::ESCAPE || k.key == key::RETURN || k.key == key::ENTER {
                self.close_picker();
            }
            return;
        }
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
        if let Some((_, picker)) = &mut self.picker {
            let ev = picker.paste(s);
            self.picker_event(ev);
            return;
        }
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
        self.focus.is_some() || self.picker.as_ref().is_some_and(|(_, p)| p.hex.focused)
    }

    /// Before the window closes: the field being typed in is saved, and
    /// KDE's shortcuts work again.
    pub fn closing(&mut self) {
        self.close_picker();
        self.leave_field();
        self.stop_recording();
    }

    // -- painting --------------------------------------------------------------------------

    pub fn paint(&self, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        let c = theme::c();
        let mut p = Painter::for_region(target.pixmap_mut(), self.scale, origin);
        p.fill_rect(Rect::new(0.0, 0.0, self.view.0, self.view.1), c.base);
        // (what's drawn, window logical: the rest isn't)
        let k = self.scale;
        let region = Rect::new(region.x as f32 / k, region.y as f32 / k, region.w as f32 / k, region.h as f32 / k).adjusted(-2.0, -2.0, 2.0, 2.0);
        let shows = |y: f32, h: f32| y + h > region.y && y < region.bottom();
        // The page.
        p.save();
        p.clip_rect(Rect::new(NAV_W, 0.0, self.view.0 - NAV_W, self.view.1));
        let layout = self.layout();
        let (x, w) = (self.content_x(), self.content_w());
        let mut y = 22.0 - self.scroll;
        let title = text::font(18.0, DEMIBOLD);
        if shows(y, line_height(title)) {
            p.draw_text(Rect::new(x, y, w, line_height(title)), Align::Left, self.pages[self.page].name, title, c.text);
        }
        y += line_height(title) + 6.0 + 8.0;
        for (b, block) in self.pages[self.page].blocks.iter().enumerate() {
            match block {
                Block::Title(t) => {
                    y += 16.0;
                    if shows(y, line_height(title)) {
                        p.draw_text(Rect::new(x, y, w, line_height(title)), Align::Left, t, title, c.text);
                    }
                    y += line_height(title) + 8.0;
                }
                Block::Note(t) => {
                    for l in text::wrap(t, hint_font(), w) {
                        if shows(y, line_height(hint_font())) {
                            p.draw_text(Rect::new(x, y, w, line_height(hint_font())), Align::Left, &l, hint_font(), c.muted);
                        }
                        y += line_height(hint_font());
                    }
                    y += 8.0;
                }
                Block::Reset => {
                    let r = Rect::new(x, y, button_width("Reset all to defaults"), 30.0);
                    if shows(y, 30.0) {
                        paint_button(&mut p, r, "Reset all to defaults", self.hovered == Some(Hit::Reset), true);
                    }
                    y += 38.0;
                }
                Block::Card { heading, rows } => {
                    if !heading.is_empty() {
                        y += 8.0;
                        if shows(y, line_height(hint_font())) {
                            p.draw_text(Rect::new(x, y, w, line_height(hint_font())), Align::Left, heading, hint_font(), c.muted);
                        }
                        y += line_height(hint_font()) + 4.0;
                    }
                    let shown: Vec<(usize, &Row)> = rows.iter().enumerate().filter(|(_, r)| r.visible).collect();
                    let heights: Vec<(f32, RowParts)> = shown.iter().map(|(_, row)| Self::row_layout(row, w)).collect();
                    let total: f32 = heights.iter().map(|h| h.0).sum();
                    if shows(y, total) {
                        p.draw_rounded_rect(Rect::new(x + 0.5, y + 0.5, w - 1.0, total - 1.0), 8.0, Some(c.raised), Some(Pen::new(c.line, 1.0)));
                    }
                    for (n, ((r, row), (h, parts))) in shown.iter().zip(heights).enumerate() {
                        if n > 0 {
                            p.fill_rect(Rect::new(x + 14.0, y, w - 28.0, 1.0), c.line);
                        }
                        if shows(y, h) {
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
        if let Some((top, bar_h)) = self.thumb(layout.height) {
            let wide = self.bar_drag.is_some() || self.pointer.is_some_and(|p| p.x >= self.view.0 - BAR_REACH);
            let (x, w) = if wide { (self.view.0 - 8.0, 6.0) } else { (self.view.0 - 6.0, 4.0) };
            p.fill_rounded_rect(Rect::new(x, top + 2.0, w, bar_h - 4.0), w / 2.0, if self.bar_drag.is_some() { c.muted } else { c.line });
        }
        // The sidebar.
        p.fill_rect(Rect::new(0.0, 0.0, NAV_W, self.view.1), c.ink);
        for (i, page) in self.pages.iter().enumerate().filter(|_| region.x < NAV_W) {
            let r = Rect::new(12.0, 18.0 + i as f32 * 34.0, NAV_W - 24.0, 32.0);
            let selected = i == self.page;
            if selected || self.hovered == Some(Hit::Nav(i)) {
                p.fill_rounded_rect(r, 6.0, if selected { c.raised } else { c.base });
            }
            p.draw_text(r.adjusted(12.0, 0.0, 0.0, 0.0), Align::Left, page.name, control_font(), if selected { c.text } else { c.muted });
        }
        if region.x < NAV_W {
            p.draw_text(
                Rect::new(24.0, self.view.1 - 34.0, NAV_W - 36.0, 20.0),
                Align::Left,
                &format!("{} {}", crate::APP_NAME, crate::cli::VERSION),
                hint_font(),
                c.muted,
            );
        }
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
        // The colour picker, over everything.
        if let Some((_, picker)) = &self.picker {
            picker.paint(&mut p);
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
                    // (keys as the desktop describes them, if they're not in our form)
                    let label = Some(keyseq::label(seq)).filter(|l| !l.is_empty()).unwrap_or_else(|| seq.clone());
                    p.draw_text(text_r, Align::Left, &label, control_font(), if *enabled { c.text } else { c.muted });
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
            Control::Palette(colors) => {
                let (lines, add) = palette_parts(rect, colors.len());
                let keymap = keys::Keymap::new(&self.cfg.keys);
                let held = self.reorder.filter(|d| d.moving && (d.b, d.r) == (b, r));
                let landing = if held.is_some() { None } else { self.landing(b, r) };
                let picked = self.picker.as_ref().map(|p| p.0);
                // (each where it glides; the one held, or landing, last: over the others)
                let top = held.map(|d| d.at).or(landing.map(|l| l.0));
                let order = (0..colors.len()).filter(|&i| top != Some(i)).chain(top);
                for i in order {
                    let (Some(&(line, remove)), Some(color)) = (lines.get(i), colors.get(i)) else { continue };
                    let lifted = top == Some(i);
                    let dy = match (held, self.pointer) {
                        (Some(d), Some(at)) if lifted => held_top(d.grab, at.y, rect.y, colors.len()) - line.y,
                        _ => self.glide_offset(b, r, i),
                    };
                    let (line, remove) = (line.translated(Point::new(0.0, dy)), remove.translated(Point::new(0.0, dy)));
                    if lifted {
                        // (a soft shadow: lifted off the list, fading as it lands; only
                        // round the line, which hides the rest)
                        let alpha = (20.0 * landing.map_or(1.0, |l| l.1)).round() as u8;
                        let inside = rounded_rect_path(line, 6.0);
                        for k in 1..=4 {
                            let k = k as f32 * 1.5;
                            let mut ring = tiny_skia::PathBuilder::new();
                            for path in [rounded_rect_path(line.adjusted(-k, 2.0 - k, k, 2.0 + k), 6.0 + k), inside.clone()].iter().flatten() {
                                ring.push_path(path);
                            }
                            if let Some(ring) = ring.finish() {
                                p.fill_path_rule(&ring, Color::rgba(0, 0, 0, alpha), true);
                            }
                        }
                    }
                    let edge = if lifted || picked == Some(i) || (held.is_none() && hovered(Part::Swatch(i))) { c.muted } else { c.line };
                    p.draw_rounded_rect(line.adjusted(0.5, 0.5, -0.5, -0.5), 6.0, Some(c.ink), Some(Pen::new(edge, 1.0)));
                    // (a grip: the line is dragged up or down by it, or anywhere)
                    for col in 0..2 {
                        for row in 0..3 {
                            p.fill_ellipse(Rect::new(line.x + 8.0 + col as f32 * 5.0, line.center().y - 6.5 + row as f32 * 5.0, 2.4, 2.4), c.muted);
                        }
                    }
                    let swatch = Rect::new(line.x + 24.0, line.center().y - 10.0, 20.0, 20.0);
                    p.draw_rounded_rect(swatch, 5.0, Some(*color), Some(Pen::new(c.line, 1.0)));
                    p.draw_text(Rect::new(swatch.right() + 10.0, line.y, 90.0, line.h), Align::Left, &color.hex(), text::mono(13.0, NORMAL), c.text);
                    let key = keymap.label(&key_for_color(i, colors.len()));
                    if !key.is_empty() {
                        p.draw_text(Rect::new(remove.x - 110.0, line.y, 100.0, line.h), Align::Right, &format!("Key {key}"), hint_font(), c.muted);
                    }
                    if colors.len() > 1 {
                        let hover = hovered(Part::Remove(i));
                        if hover {
                            p.fill_rounded_rect(remove, 5.0, c.hover);
                        }
                        let cx = remove.center();
                        let pen = Pen::new(if hover { c.text } else { c.muted }, 1.4);
                        p.draw_line(Point::new(cx.x - 3.5, cx.y - 3.5), Point::new(cx.x + 3.5, cx.y + 3.5), pen);
                        p.draw_line(Point::new(cx.x + 3.5, cx.y - 3.5), Point::new(cx.x - 3.5, cx.y + 3.5), pen);
                    }
                }
                paint_button(p, add, ADD_COLOUR, hovered(Part::Add), colors.len() < config::MAX_PALETTE);
                let own = keymap.label(&key_for_color(CUSTOM, colors.len()));
                if !own.is_empty() {
                    // (right-aligned under the colours' keys)
                    let right = rect.x + rect.w.min(PALETTE_W) - 40.0;
                    let at = Rect::new(add.right() + 14.0, add.y, right - add.right() - 14.0, add.h);
                    p.draw_text(at, Align::Right, &format!("Your own colour: key {own}"), hint_font(), c.muted);
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
        "ocr" => "sc.ocr",
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

fn global_note(enabled: Option<bool>, portal: bool) -> String {
    match enabled {
        None => "Looking for KDE's shortcut service…".into(),
        Some(true) => "Global shortcuts are managed by KDE and also appear in System Settings → Shortcuts → Flatshot RS.".into(),
        Some(false) if portal => {
            "KDE's shortcut service isn't here, but the desktop can give these keys through its portal (the switch above), or bind the command flatshot-rs in the desktop's keyboard settings.".into()
        }
        Some(false) => "Global shortcuts require KDE Plasma. On other desktops, bind the command flatshot-rs in the desktop's keyboard settings.".into(),
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
    fn scrolling_by_finger_and_by_the_bar() {
        let mut s = settings();
        let long = (0..s.pages.len()).max_by(|&a, &b| {
            s.show_page(a);
            let ha = s.layout().height;
            s.show_page(b);
            ha.total_cmp(&s.layout().height)
        });
        s.show_page(long.unwrap());
        let h = s.layout().height;
        assert!(h > s.view.1, "a page longer than the window");
        let cfg = s.cfg.clone();
        // A finger dragged up the page scrolls it, and clicks nothing.
        let at = Point::new(400.0, 400.0);
        s.pointer_motion(at, true);
        s.button(Button::Left, true, at, 0);
        for y in [395.0, 380.0, 300.0] {
            s.pointer_motion(Point::new(402.0, y), true);
        }
        s.button(Button::Left, false, Point::new(402.0, 300.0), 0);
        assert_eq!(s.scroll, 100.0);
        assert!(s.cfg == cfg && s.requests.is_empty(), "the drag clicked something");
        // The thumb dragged down the bar: to the end at the bottom.
        let (top, _) = s.thumb(h).unwrap();
        let x = s.view.0 - 4.0;
        s.pointer_motion(Point::new(x, top + 5.0), false);
        s.button(Button::Left, true, Point::new(x, top + 5.0), 0);
        s.pointer_motion(Point::new(x, s.view.1 + 50.0), false);
        s.button(Button::Left, false, Point::new(x, s.view.1 + 50.0), 0);
        assert_eq!(s.scroll, h - s.view.1);
        // A click above the thumb: it jumps there (to the top here).
        s.pointer_motion(Point::new(x, 2.0), false);
        s.button(Button::Left, true, Point::new(x, 2.0), 0);
        s.button(Button::Left, false, Point::new(x, 2.0), 0);
        assert_eq!(s.scroll, 0.0);
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
        assert!(matches!(&s.find_mut("preview").unwrap().below, Control::Text(t) if t.starts_with("Example: ")));
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
    fn the_desktop_portal_where_kde_has_no_shortcuts() {
        let mut s = settings();
        s.show_page(4);
        let shown = |s: &mut Settings| s.find_mut("portal_shortcuts").is_some_and(|r| r.visible);
        // Offered only once KDE's service is known to be missing, and the portal has them.
        s.set_portal(true);
        assert!(!shown(&mut s));
        s.set_global(None);
        assert!(shown(&mut s));
        // Turned on: saved, and asked of the tray app; why not, if the desktop says no.
        let (b, r) = row_at(&s, "portal_shortcuts");
        click(&mut s, Hit::Row(b, r, Part::Control));
        assert!(s.cfg.portal_shortcuts && crate::config::load().portal_shortcuts);
        assert_eq!(s.take_requests(), vec![Request::PortalShortcuts(true)]);
        s.portal_answer(Some("you said no".into()));
        assert!(s.find_mut("portal_shortcuts").unwrap().error.as_deref().is_some_and(|e| e.contains("you said no")));
        s.portal_answer(None);
        assert!(s.find_mut("portal_shortcuts").unwrap().error.is_none());
        // Where KDE's service is, or the portal has none: not offered.
        s.set_portal(false);
        assert!(!shown(&mut s));
        s.set_portal(true);
        s.set_global(Some(BTreeMap::new()));
        assert!(!shown(&mut s));
    }

    #[test]
    fn toolbar_colours_are_added_changed_removed_and_reset() {
        let mut s = settings();
        s.show_page(1);
        let (b, r) = row_at(&s, "palette");
        let palette = |s: &Settings| s.cfg.palette.clone();
        assert_eq!(palette(&s), config::default_palette());
        // Removing the second.
        click(&mut s, Hit::Row(b, r, Part::Remove(1)));
        assert_eq!(palette(&s), ["#FF6A3D", "#3DDC97", "#3DB2FF", "#9B7BFF", "#F2EEE6", "#121218"]);
        // Adding one: the default missing comes back, its picker opens.
        click(&mut s, Hit::Row(b, r, Part::Add));
        assert_eq!(palette(&s).last().map(String::as_str), Some("#FFC53D"));
        assert_eq!(s.picker.as_ref().map(|p| p.0), Some(6));
        // Typing a colour in the picker changes it; Enter saves it.
        let hex = s.picker.as_ref().unwrap().1.hex.rect.translated(s.picker.as_ref().unwrap().1.pos).center();
        click_at(&mut s, hex);
        assert!(s.wants_text());
        for _ in 0..7 {
            s.key(&KeyPress { key: key::BACKSPACE, ..Default::default() });
        }
        s.commit_string("#123456");
        s.key(&KeyPress { key: key::RETURN, ..Default::default() });
        assert!(s.picker.is_none() && !s.wants_text());
        assert_eq!(palette(&s).last().map(String::as_str), Some("#123456"));
        // A click on a colour opens its picker; one elsewhere closes it.
        click(&mut s, Hit::Row(b, r, Part::Swatch(0)));
        assert_eq!(s.picker.as_ref().map(|p| p.0), Some(0));
        click_at(&mut s, Point::new(NAV_W + 4.0, 4.0));
        assert!(s.picker.is_none());
        // Dragged down two lines: it moves there, and is saved on release (not a click).
        let first = hit_point(&s, Hit::Row(b, r, Part::Swatch(0)));
        s.pointer_motion(first, false);
        s.button(Button::Left, true, first, 0);
        for dy in [10.0, 40.0, 2.0 * PALETTE_LINE] {
            s.pointer_motion(Point::new(first.x, first.y + dy), false);
        }
        s.button(Button::Left, false, Point::new(first.x, first.y + 2.0 * PALETTE_LINE), 0);
        assert!(s.picker.is_none());
        assert_eq!(&palette(&s)[..3], ["#3DDC97", "#3DB2FF", "#FF6A3D"]);
        // Up to the most the toolbar holds, and never none.
        for _ in 0..20 {
            click(&mut s, Hit::Row(b, r, Part::Add));
            s.close_picker();
        }
        assert_eq!(palette(&s).len(), config::MAX_PALETTE);
        for _ in 0..20 {
            let last = palette(&s).len() - 1;
            click(&mut s, Hit::Row(b, r, Part::Remove(last)));
        }
        assert_eq!(palette(&s), ["#3DDC97"]);
        // Reset.
        click(&mut s, Hit::Row(b, r, Part::Control));
        assert_eq!(palette(&s), config::default_palette());
    }

    #[test]
    fn dragged_colours_glide_to_their_places() {
        let mut s = settings();
        s.show_page(1);
        let (b, r) = row_at(&s, "palette");
        let first = hit_point(&s, Hit::Row(b, r, Part::Swatch(0)));
        s.pointer_motion(first, false);
        s.button(Button::Left, true, first, 0);
        s.pointer_motion(Point::new(first.x, first.y + 10.0), false);
        assert!(s.next_deadline().is_none(), "nothing glides yet");
        // Past the middle of the next line: they swap, and the one passed is
        // drawn where it was, setting off up to its place.
        let past = Point::new(first.x, first.y + 0.6 * PALETTE_LINE);
        s.pointer_motion(past, false);
        assert_eq!(s.glide_offset(b, r, 0), PALETTE_LINE);
        assert!(s.next_deadline().is_some_and(|t| t <= s.clock + FRAME));
        // Halfway through its glide it's most of the way there; then there.
        s.on_timer(s.clock + GLIDE / 2);
        let half = s.glide_offset(b, r, 0);
        assert!(half > 0.0 && half < PALETTE_LINE / 4.0, "{half}");
        s.on_timer(s.clock + GLIDE);
        assert_eq!(s.glide_offset(b, r, 0), 0.0);
        assert!(s.next_deadline().is_none() && s.glides.is_none());
        // Let go between two places: saved, and it lands, from where it was
        // let go (0.4 lines above its place), over the others.
        s.button(Button::Left, false, past, 0);
        let defaults = config::default_palette();
        assert_eq!(&s.cfg.palette[..2], [defaults[1].clone(), defaults[0].clone()]);
        assert_eq!(s.landing(b, r).map(|l| l.0), Some(1));
        assert!((s.glide_offset(b, r, 1) + 0.4 * PALETTE_LINE).abs() < 0.01, "{}", s.glide_offset(b, r, 1));
        s.on_timer(s.clock + GLIDE);
        assert!(s.landing(b, r).is_none() && s.next_deadline().is_none());
        // Dragged back up past both: each passed glides down from its old place.
        let second = hit_point(&s, Hit::Row(b, r, Part::Swatch(1)));
        s.pointer_motion(second, false);
        s.button(Button::Left, true, second, 0);
        s.pointer_motion(Point::new(second.x, second.y - 10.0), false);
        s.pointer_motion(Point::new(second.x, second.y - 3.0 * PALETTE_LINE), false);
        assert_eq!(s.glide_offset(b, r, 1), -PALETTE_LINE);
        s.button(Button::Left, false, Point::new(second.x, second.y - 3.0 * PALETTE_LINE), 0);
        assert_eq!(&s.cfg.palette[..2], [defaults[0].clone(), defaults[1].clone()]);
        // Removing a colour meanwhile: no glides for lines that aren't there.
        click(&mut s, Hit::Row(b, r, Part::Remove(3)));
        assert!(s.glides.is_none());
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
        assert!(s.row(b, r).unwrap().error.as_deref().is_some_and(|e| e.contains("reserved")));
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
