//! What every overlay of a capture shares: the tool, colour and size, the
//! toggles, the hint, the windows on screen, and the text being typed.
//! The session changes it; the overlays read it to paint.

use crate::config::Config;
use crate::geom::{IRect, Point, Rect};
use crate::keys::Keymap;
use crate::render::theme::{self, Color, SWATCHES};
use crate::ui::overlay::Window;
use crate::ui::shapes::{Shape, Tool};

/// The colour index of your own colour, after the toolbar's others.
pub const CUSTOM: usize = SWATCHES.len();

/// What the compositor said about the desktop: its windows (topmost
/// first, global logical pixels), the active one, and where the pointer is.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Desktop {
    pub windows: Vec<Window>,
    pub active: Option<Window>,
    pub cursor: Option<Point>,
}

pub struct State {
    pub cfg: Config,
    pub keymap: Keymap,
    pub tool: Tool,
    /// the last tool used that draws in a colour
    pub colour_tool: Tool,
    pub color_index: usize,
    pub custom_color: Color,
    pub size: usize,
    /// the shading over what isn't chosen (its alpha from the settings)
    pub dim: Color,
    pub codes_visible: bool,
    pub snap_edges: bool,
    /// the picture shown with the mouse pointer in it
    pub show_pointer: bool,
    /// a picture with the pointer was taken, so it can be shown
    pub pointer_available: bool,
    pub can_record: bool,
    /// the capture is pinned to the screen instead of saved
    pub pin_mode: bool,
    /// the next click takes your own colour from the screen
    pub eyedropper: bool,
    /// magnifier screen pixels per captured pixel (the wheel changes it)
    pub loupe_zoom: f32,
    /// seconds left before recording starts
    pub countdown: Option<i64>,
    /// what the control under the pointer does, or a problem to show
    pub hint: Option<String>,
    /// why recording in the chosen format can't work here
    pub record_problem: Option<String>,
    /// The area chosen to record (global logical pixels): every monitor
    /// it reaches shows its part, handles and all, so it's moved and
    /// resized from any of them.
    pub rec_area: Option<Rect>,
    /// the monitor (an overlay) whose recording panel shows: the one with
    /// most of the area
    pub rec_home: usize,
    /// the area is being moved or resized (its handles and panel hide)
    pub area_moving: bool,
    /// each monitor (global logical) and its picture's pixels per logical
    /// pixel, for the size a capture across them comes out at
    pub monitors: Vec<(IRect, f32)>,
    /// a selection (or a window clicked) can cross monitors now
    pub spans: bool,
    /// the text being typed: (overlay, the text shape)
    pub text_edit: Option<(usize, Shape)>,
    /// where the pointer is, and where the toolbar is
    pub pointer_overlay: Option<usize>,
    pub toolbar_overlay: Option<usize>,
    pub windows: Vec<Window>,
    /// changes when the palette does (part of what each element looks like)
    pub theme_key: u64,
    /// seconds since the capture began, for the rainbow crosshair
    pub rainbow_t: f32,
}

impl State {
    pub fn new(cfg: Config) -> State {
        let keymap = Keymap::new(&cfg.keys);
        let tool = Tool::from_id(&cfg.default_tool).unwrap_or(Tool::Region);
        let mut dim = theme::c().dim;
        dim.a = ((cfg.dim_opacity.clamp(0, 100) as f32) * 2.55).round_ties_even() as u8;
        let custom = Color::parse(&cfg.custom_color).unwrap_or(Color::rgb(0xFF4FA3));
        State {
            keymap,
            tool,
            colour_tool: if tool.uses_color() { tool } else { Tool::Pen },
            color_index: (cfg.default_color.max(0) as usize).min(CUSTOM),
            custom_color: Color { a: 255, ..custom },
            size: (cfg.default_size.max(0) as usize).min(theme::SIZES.len() - 1),
            dim,
            codes_visible: cfg.show_codes,
            snap_edges: cfg.snap_edges,
            show_pointer: false,
            pointer_available: false,
            can_record: false,
            pin_mode: false,
            eyedropper: false,
            loupe_zoom: 8.0,
            countdown: None,
            hint: None,
            record_problem: None,
            rec_area: None,
            rec_home: 0,
            area_moving: false,
            monitors: Vec::new(),
            spans: false,
            text_edit: None,
            pointer_overlay: None,
            toolbar_overlay: None,
            windows: Vec::new(),
            theme_key: theme_key(&cfg.theme),
            rainbow_t: 0.0,
            cfg,
        }
    }

    /// All the monitors together (global logical).
    pub fn desktop(&self) -> Rect {
        self.monitors.iter().fold(Rect::default(), |acc, (m, _)| if acc.is_empty() { m.to_f() } else { acc.united(&m.to_f()) })
    }

    /// The pixels per logical pixel `area` (global logical) comes out at:
    /// its monitor's, else (across monitors) the highest of theirs.
    pub fn scale_for(&self, area: Rect) -> f32 {
        let touched = self.monitors.iter().filter(|(m, _)| m.to_f().intersects(&area));
        touched.map(|(_, s)| *s).fold(None, |a: Option<f32>, s| Some(a.map_or(s, |a| a.max(s)))).unwrap_or(1.0)
    }

    /// The colour drawn with now.
    pub fn color(&self) -> Color {
        if self.color_index >= CUSTOM { self.custom_color } else { SWATCHES[self.color_index] }
    }
}

fn theme_key(name: &str) -> u64 {
    use std::hash::{Hash, Hasher};
    let mut h = std::collections::hash_map::DefaultHasher::new();
    name.hash(&mut h);
    h.finish()
}
