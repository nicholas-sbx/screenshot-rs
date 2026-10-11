//! The display server, as the app sees it: Wayland or X11. Each shows
//! Flatshot's surfaces (the capture overlay, pins, menus, windows, a
//! recording's controls), draws them from what the UI paints, and reports
//! their input in logical pixels, by whose surface it is.

use std::sync::{Arc, mpsc};
use std::time::Instant;

use wayland_client::protocol::wl_output::WlOutput;

use crate::clipboard::Slot;
use crate::geom::{Damage, IRect, Rect};
use crate::image::{Canvas, Image};
use crate::ui::session::{CursorShape, Waker};
use crate::wayland::Wayland;
use crate::x11::X11;

/// A monitor.
#[derive(Clone, Debug)]
pub struct Monitor {
    pub output: Output,
    /// connector name: DP-1, HDMI-A-1, ...
    pub name: String,
    /// where it is in the desktop, in logical pixels
    pub rect: IRect,
    /// its integer scale (the real one may be fractional)
    pub scale: i32,
}

/// The display server's own handle on a monitor.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    Wayland(WlOutput),
    /// a RandR monitor, by its name's atom
    X11(u32),
}

impl Monitor {
    pub fn wl_output(&self) -> Option<&WlOutput> {
        match &self.output {
            Output::Wayland(o) => Some(o),
            Output::X11(_) => None,
        }
    }
}

/// Whose a surface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// the capture overlay on the session's monitor `.0`
    Overlay(usize),
    /// a pinned capture, by id
    Pin(u32),
    /// a popup menu, by id
    Menu(u32),
    /// an annotation editor's window, by id
    Editor(u32),
    /// the settings window
    Settings,
    /// the bar with a recording's controls
    RecBar,
    /// a corner mark round the area being recorded (0-3)
    RecMark(u8),
    /// the tray icon, where the panel takes it in itself (X11's XEmbed)
    TrayIcon,
}

/// The tray icon's menu (as a popup menu of ours: XEmbed).
pub const TRAY_MENU: u32 = u32::MAX;

/// Something drawn on surfaces: what changed, and how to draw a part of it.
pub trait Painted {
    /// What changed on part `part` since it was last drawn (device pixels).
    fn damage(&mut self, part: usize) -> Damage;
    /// Draw `region` (device pixels) of part `part` into `target`, whose
    /// top left is at `origin` (device pixels).
    fn paint(&self, part: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect);
    /// The size of part `part`'s buffer, in device pixels.
    fn buffer_size(&self, part: usize) -> (u32, u32);
}

/// Paint `todo` (device pixels) of part `part` of `ui` into `img`: whole,
/// or each changed part on its own (everything outside it clipped away,
/// in `scratch`) and copied into place. The rectangles painted.
pub fn paint_damage(img: &mut Image, scratch: &mut Vec<u8>, todo: &Damage, ui: &dyn Painted, part: usize) -> Vec<IRect> {
    let full = IRect::new(0, 0, img.width() as i32, img.height() as i32);
    let rects = todo.rects_in(full);
    for r in &rects {
        if *r == full {
            ui.paint(part, &mut img.canvas(), (0, 0), *r);
            continue;
        }
        let len = r.w as usize * r.h as usize * 4;
        if scratch.len() < len {
            scratch.resize(len, 0);
        }
        let Some(mut c) = Canvas::new(scratch, r.w as u32, r.h as u32) else { continue };
        ui.paint(part, &mut c, (r.x, r.y), *r);
        let (src, stride) = (c.data(), r.w as usize * 4);
        let dst_stride = img.stride();
        let data = img.data_mut();
        for row in 0..r.h as usize {
            let at = (r.y as usize + row) * dst_stride + r.x as usize * 4;
            data[at..at + stride].copy_from_slice(&src[row * stride..(row + 1) * stride]);
        }
    }
    rects
}

// (one, held for the program's life: its size doesn't matter)
#[allow(clippy::large_enum_variant)]
pub enum Display {
    Wayland(Wayland),
    X11(X11),
}

/// The same call on whichever display server it is.
macro_rules! each {
    ($self:expr, $d:ident => $e:expr) => {
        match $self {
            Display::Wayland($d) => $e,
            Display::X11($d) => $e,
        }
    };
}

impl Display {
    pub fn is_x11(&self) -> bool {
        matches!(self, Display::X11(_))
    }

    /// The monitors, as the display server describes them now.
    pub fn monitors(&self) -> Vec<Monitor> {
        each!(self, d => d.monitors())
    }

    /// A surface covering `monitor` for the capture overlay, taking the
    /// keyboard and pointer.
    pub fn add_overlay(&mut self, owner: Owner, monitor: &Monitor, namespace: &str) -> Result<(), String> {
        each!(self, d => d.add_overlay(owner, monitor, namespace))
    }

    /// A surface above the windows on `monitor`, its top left at `at`
    /// (logical, from the monitor's), `size` big: a pin. False if the
    /// display server can't place one (it's then a window).
    pub fn add_layer(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), namespace: &str) -> bool {
        each!(self, d => d.add_layer(owner, monitor, at, size, namespace))
    }

    /// A small see-through surface above everything on `monitor` that never
    /// takes the keyboard: a recording's controls. `click_through`: the
    /// pointer goes to what's under it. False if it can't be placed.
    pub fn add_control(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), namespace: &str, click_through: bool) -> bool {
        each!(self, d => d.add_control(owner, monitor, at, size, namespace, click_through))
    }

    /// A window of `size` (logical).
    pub fn add_window(&mut self, owner: Owner, title: &str, size: (u32, u32), decorated: bool) -> Result<(), String> {
        each!(self, d => d.add_window(owner, title, size, decorated))
    }

    /// A popup (a menu) of `size` (logical) next to `anchor` (logical, on
    /// `parent`'s surface), taking the keyboard and pointer until it's
    /// dismissed.
    pub fn add_popup(&mut self, owner: Owner, parent: Owner, anchor: IRect, size: (u32, u32)) -> Result<(), String> {
        each!(self, d => d.add_popup(owner, parent, anchor, size))
    }

    /// A tray icon the panel embeds (X11 panels without the D-Bus tray).
    pub fn add_tray_icon(&mut self) -> Result<(), String> {
        match self {
            Display::Wayland(_) => Err("no tray to embed an icon in on Wayland".into()),
            Display::X11(x) => x.add_tray_icon(),
        }
    }

    /// Take down `owner`'s surface.
    pub fn remove(&mut self, owner: Owner) {
        each!(self, d => d.remove(owner))
    }

    /// Take down every surface of the capture overlay.
    pub fn remove_overlays(&mut self) {
        each!(self, d => d.remove_overlays())
    }

    /// Move a placed surface (logical, from its monitor's top left) and size it.
    pub fn place_layer(&mut self, owner: Owner, at: (i32, i32), size: (u32, u32)) {
        each!(self, d => d.place_layer(owner, at, size))
    }

    /// The size `owner`'s surface is shown at (logical), ahead of the
    /// display server saying so.
    pub fn set_logical_size(&mut self, owner: Owner, size: (u32, u32)) {
        each!(self, d => d.set_logical_size(owner, size))
    }

    pub fn set_min_size(&mut self, owner: Owner, size: (u32, u32)) {
        each!(self, d => d.set_min_size(owner, size))
    }

    pub fn set_title(&mut self, owner: Owner, title: &str) {
        each!(self, d => d.set_title(owner, title))
    }

    /// See-through where nothing is drawn (rounded corners).
    pub fn set_transparent(&mut self, owner: Owner) {
        each!(self, d => d.set_transparent(owner))
    }

    /// `owner`'s surface: its size (logical) and the scale to draw it at.
    pub fn surface(&self, owner: Owner) -> Option<((u32, u32), f32)> {
        each!(self, d => d.surface(owner))
    }

    /// Whose the surfaces up are.
    pub fn owners(&self) -> Vec<Owner> {
        each!(self, d => d.owners())
    }

    /// Draw `owner`'s surface (part `part` of `ui`) if it can be drawn now:
    /// only what changed. Whether something was shown.
    pub fn redraw(&mut self, owner: Owner, part: usize, ui: &mut dyn Painted) -> bool {
        each!(self, d => d.redraw_owner(owner, part, ui))
    }

    /// Send what's waiting to the display server.
    pub fn flush(&self) {
        each!(self, d => d.flush())
    }

    /// keyseq modifier bits held now.
    pub fn mods(&self) -> i32 {
        each!(self, d => d.mods)
    }

    /// The surface the pointer is on.
    pub fn pointer_owner(&self) -> Option<Owner> {
        each!(self, d => d.pointer_owner())
    }

    /// The surface the keyboard is on.
    /// Bring `owner`'s window to the front, focused; `token`: the
    /// desktop's activation token for the click that asked, if it gave one.
    pub fn activate(&mut self, owner: Owner, token: Option<String>) {
        each!(self, d => d.activate(owner, token))
    }

    pub fn keyboard_owner(&self) -> Option<Owner> {
        each!(self, d => d.keyboard_owner())
    }

    /// The surface an input method types into.
    pub fn text_owner(&self) -> Option<Owner> {
        each!(self, d => d.text_owner())
    }

    /// The pointer's look over `owner`'s surface.
    pub fn set_cursor(&mut self, owner: Owner, shape: CursorShape) {
        each!(self, d => d.set_cursor(owner, shape))
    }

    /// Turn the input method on for the text being typed (with the caret
    /// at `caret`, logical, on the focused surface), or off.
    pub fn set_text_input(&mut self, caret: Option<Rect>) {
        each!(self, d => d.set_text_input(caret))
    }

    /// Let the desktop move `owner`'s window with the pointer (its button
    /// is down). False if it can't.
    pub fn start_move(&mut self, owner: Owner) -> bool {
        each!(self, d => d.start_move(owner))
    }

    /// Is the pointer's motion reported as it moves, wherever it is
    /// (App::relative_motion), for dragging placed surfaces?
    pub fn has_relative(&self) -> bool {
        each!(self, d => d.has_relative())
    }

    /// Can surfaces be placed exactly, above everything (add_layer,
    /// add_control)?
    pub fn can_place(&self) -> bool {
        each!(self, d => d.can_place())
    }

    // -- the clipboard -----------------------------------------------------------------

    /// Own the clipboard with what comes in `slot`, as each of `mimes`.
    pub fn offer(&mut self, mimes: Vec<String>, slot: Arc<Slot>) -> bool {
        each!(self, d => d.offer(mimes, slot))
    }

    /// Own the clipboard with each type's own data (the first's is the default).
    pub fn offer_each(&mut self, parts: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        each!(self, d => d.offer_each(parts))
    }

    /// Do we own the clipboard?
    pub fn holds(&self) -> bool {
        each!(self, d => d.holds())
    }

    /// Give up the clipboard (what was to be offered never came).
    pub fn withdraw(&mut self) {
        each!(self, d => d.withdraw())
    }

    /// Let the desktop keep what's on the clipboard, so a one-off capture
    /// can end (X11's clipboard manager). False if it can't.
    pub fn hand_off_clipboard(&mut self) -> bool {
        match self {
            Display::Wayland(_) => false,
            Display::X11(x) => x.hand_off_clipboard(),
        }
    }

    /// The program a one-off capture hands what it copies to, so it can end
    /// at once with the clipboard still served: wl-copy (Wayland, with
    /// data-control), xclip (X11, when no clipboard manager would keep it).
    pub fn copy_helper(&self) -> Option<&'static str> {
        match self {
            Display::Wayland(w) => (w.without_focus() && crate::util::have("wl-copy")).then_some("wl-copy"),
            Display::X11(x) => (crate::util::have("xclip") && !x.has_clipboard_manager()).then_some("xclip"),
        }
    }

    /// Can the clipboard be set without a focused surface?
    pub fn without_focus(&self) -> bool {
        each!(self, d => d.without_focus())
    }

    /// The clipboard's text, read in the background (None if there's none).
    pub fn paste_text(&mut self, wake: Waker) -> mpsc::Receiver<Option<String>> {
        each!(self, d => d.paste_text(wake))
    }

    // -- taking the screen through the connection --------------------------------------

    pub fn has_screencopy(&self) -> bool {
        match self {
            Display::Wayland(w) => w.screencopy.is_some(),
            Display::X11(_) => false,
        }
    }

    pub fn start_screencopy(&mut self, monitors: &[Monitor], cursor: bool, both: bool) -> Result<(), String> {
        match self {
            Display::Wayland(w) => w.start_screencopy(monitors, cursor, both),
            Display::X11(_) => Err("not on X11".into()),
        }
    }

    pub fn screencopy_done(&self) -> bool {
        match self {
            Display::Wayland(w) => w.screencopy_done(),
            Display::X11(_) => true,
        }
    }

    pub fn take_screencopy(&mut self) -> Vec<(usize, bool, Result<Image, String>)> {
        match self {
            Display::Wayland(w) => w.take_screencopy(),
            Display::X11(_) => Vec::new(),
        }
    }

    // -- the loop ----------------------------------------------------------------------

    /// After each round of events.
    pub fn tick(&mut self) {
        each!(self, d => d.tick())
    }

    /// When the display server's side wants the loop back.
    pub fn next_deadline(&self) -> Option<Instant> {
        each!(self, d => d.next_deadline())
    }
}
