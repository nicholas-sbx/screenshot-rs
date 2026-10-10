//! X11 sessions. Flatshot's surfaces are X windows, drawn as on Wayland:
//! into shared memory the X server reads straight from (MIT-SHM, with a
//! memfd of ours), only what changed. The capture overlay is a window per
//! monitor the window manager never sees (override-redirect), shown at its
//! first frame and holding the keyboard and pointer; pins stay above the
//! windows; the editor and settings are ordinary windows. Monitors come
//! from RandR, the scale from Xft.dpi, keys from XKB (see `keyboard`), the
//! clipboard from selections (see `clipboard`).

pub mod capture;
mod clipboard;
mod keyboard;
pub mod windows;

use std::collections::{HashMap, VecDeque};
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use x11rb::connection::{Connection, RequestConnection};
use x11rb::properties::{WmHints, WmSizeHints, WmSizeHintsSpecification};
use x11rb::protocol::Event;
use x11rb::protocol::randr::{self, ConnectionExt as _};
use x11rb::protocol::shape::{self, ConnectionExt as _};
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xkb::{self, ConnectionExt as _};
use x11rb::protocol::xproto::{
    self, AtomEnum, ChangeWindowAttributesAux, ClientMessageEvent, ConfigureWindowAux, ConnectionExt as _, CreateGCAux, CreateWindowAux, EventMask, GrabMode,
    GrabStatus, ImageFormat, PropMode, StackMode, WindowClass,
};
use x11rb::rust_connection::RustConnection;
use x11rb::wrapper::ConnectionExt as _;

use crate::app::{App, Input};
use crate::geom::{Damage, IRect, Rect};
use crate::image::Image;
use crate::platform::{Display, Monitor, Output, Owner, Painted, paint_damage};
use crate::ui::session::CursorShape;

x11rb::atom_manager! {
    pub Atoms: AtomsCookie {
        WM_PROTOCOLS,
        WM_DELETE_WINDOW,
        UTF8_STRING,
        _NET_WM_NAME,
        _NET_WM_PID,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_UTILITY,
        _NET_WM_WINDOW_TYPE_POPUP_MENU,
        _NET_WM_WINDOW_TYPE_NOTIFICATION,
        _NET_WM_STATE,
        _NET_WM_STATE_ABOVE,
        _NET_WM_STATE_SKIP_TASKBAR,
        _NET_WM_STATE_SKIP_PAGER,
        _NET_WM_MOVERESIZE,
        _MOTIF_WM_HINTS,
        _XKB_RULES_NAMES,
        CLIPBOARD,
        CLIPBOARD_MANAGER,
        SAVE_TARGETS,
        TARGETS,
        MULTIPLE,
        TIMESTAMP,
        INCR,
        ATOM_PAIR,
        FLATSHOT_PASTE,
        FLATSHOT_SAVE,
        MANAGER,
        _NET_SYSTEM_TRAY_OPCODE,
        _NET_SYSTEM_TRAY_VISUAL,
        _XEMBED_INFO,
    }
}

/// The pointer and keyboard events Flatshot's windows hear.
fn input_mask() -> EventMask {
    EventMask::KEY_PRESS
        | EventMask::KEY_RELEASE
        | EventMask::BUTTON_PRESS
        | EventMask::BUTTON_RELEASE
        | EventMask::POINTER_MOTION
        | EventMask::ENTER_WINDOW
        | EventMask::LEAVE_WINDOW
        | EventMask::FOCUS_CHANGE
        | EventMask::EXPOSURE
        | EventMask::STRUCTURE_NOTIFY
}
/// How long the overlay keeps trying to take the keyboard and pointer
/// (the shortcut's own program may still hold them).
const GRAB_FOR: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Kind {
    /// covering a monitor, unmanaged, holding the keyboard and pointer
    Overlay,
    /// a pin: managed, undecorated, above the windows
    Layer,
    /// a recording's controls: unmanaged, never focused
    Control,
    /// an ordinary window
    Window,
    /// a menu: unmanaged, holding the pointer until it's dismissed
    Popup,
    /// a tray icon, inside the panel (XEmbed)
    Embedded,
}

struct Buf {
    img: Image,
    /// its MIT-SHM segment (None: sent with each request)
    seg: Option<u32>,
    /// still being read by the server
    busy: bool,
    stale: Damage,
    drawn: u64,
}

struct Surf {
    owner: Owner,
    kind: Kind,
    win: xproto::Window,
    gc: xproto::Gcontext,
    depth: u8,
    /// see-through where nothing is drawn: an ARGB window (with a
    /// compositor), else a window shaped to what's drawn
    transparent: bool,
    argb: bool,
    logical: (u32, u32),
    /// where it is in the root window (device pixels; unmanaged windows)
    origin: (i32, i32),
    mapped: bool,
    configured: bool,
    bufs: Vec<Buf>,
    frames: u64,
    scratch: Vec<u8>,
    /// what the server lost (uncovered), to send again
    exposed: Damage,
    cursor: Option<CursorShape>,
}

/// The keyboard and pointer to take for a window.
struct Grab {
    window: xproto::Window,
    keyboard: bool,
    pointer: bool,
    /// events elsewhere go to it too (menus: a click outside dismisses)
    exclusive: bool,
    until: Instant,
}

pub struct X11 {
    pub conn: RustConnection,
    screen: usize,
    root: xproto::Window,
    root_depth: u8,
    root_visual: xproto::Visualid,
    /// a 32-bit visual and its colormap, for see-through windows
    argb: Option<(xproto::Visualid, xproto::Colormap)>,
    pub atoms: Atoms,
    pub mods: i32,
    scale: i32,
    shm: bool,
    surfaces: Vec<Surf>,
    pointer_on: Option<xproto::Window>,
    keyboard_on: Option<xproto::Window>,
    keyboard: keyboard::Keyboard,
    cursor_theme: Option<x11rb::cursor::Handle>,
    cursors: HashMap<&'static str, xproto::Cursor>,
    clip: clipboard::Clipboard,
    /// a hidden window: owns the clipboard, receives what's pasted
    helper: xproto::Window,
    /// the latest event's time (for the clipboard)
    time: xproto::Timestamp,
    grab: Option<Grab>,
    /// events read while waiting for a reply, handled next round
    queued: VecDeque<Event>,
    /// where the pointer was, in the root window, while a button is held
    drag: Option<(i16, i16)>,
    /// the overlay a button went down on: until it's let go, the pointer's
    /// events are its (as on Wayland), wherever the pointer is
    held: Option<xproto::Window>,
}

/// Is this an X11 session (no Wayland compositor to use)?
pub fn chosen() -> bool {
    match std::env::var("FLATSHOT_PLATFORM").as_deref() {
        Ok("x11") => return true,
        Ok("wayland") => return false,
        _ => {}
    }
    std::env::var_os("WAYLAND_DISPLAY").is_none_or(|d| d.is_empty()) && std::env::var_os("DISPLAY").is_some_and(|d| !d.is_empty())
}

/// What `flatshot --diagnose` says of the X server.
pub fn diagnose() -> Vec<(&'static str, String)> {
    let yes = |b: bool| if b { "yes" } else { "no" }.to_string();
    let (conn, screen) = match x11rb::connect(None) {
        Ok(c) => c,
        Err(e) => return vec![("X server", format!("not reached: {e}"))],
    };
    let has = |name: &'static str| conn.extension_information(name).ok().flatten().is_some();
    let owned = |name: String| {
        let atom = conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        atom.and_then(|a| conn.get_selection_owner(a).ok()?.reply().ok()).is_some_and(|r| r.owner != x11rb::NONE)
    };
    let mut out = vec![
        ("pixels", capture::check_format(&conn, screen).map(|()| "32-bit BGRX, as Flatshot draws".to_string()).unwrap_or_else(|e| e)),
        ("screen copy into our memory (MIT-SHM)", yes(capture::shm_usable(&conn))),
        ("pointer pictures (XFixes)", yes(has("XFIXES"))),
        ("monitors (RandR)", yes(has("RANDR"))),
        ("keymap (XKB)", yes(has("XKEYBOARD"))),
        ("compositing manager (see-through windows)", yes(owned(format!("_NET_WM_CM_S{screen}")))),
        ("clipboard manager (keeps a copy after exit)", yes(owned("CLIPBOARD_MANAGER".into()))),
        ("scale (Xft.dpi)", format!("{}", scale_of(&conn))),
    ];
    if !has("MIT-SHM") {
        out.push(("note", "without MIT-SHM (a remote X server) the screen is copied slowly".into()));
    }
    out
}

/// The desktop's scale: Xft.dpi over 96, in whole steps (as X11 desktops
/// scale their own), 1 without it.
pub fn scale_of(conn: &impl Connection) -> f32 {
    let dpi = x11rb::resource_manager::new_from_default(conn).ok().and_then(|db| db.get_value::<f32>("Xft.dpi", "").ok().flatten());
    match dpi {
        Some(d) if d.is_finite() && d > 0.0 => ((d / 96.0) + 0.25).floor().clamp(1.0, 4.0),
        _ => 1.0,
    }
}

impl X11 {
    pub fn connect() -> Result<X11, String> {
        let (conn, screen) = x11rb::connect(None).map_err(|e| format!("cannot connect to the X server: {e}"))?;
        capture::check_format(&conn, screen)?;
        let atoms = Atoms::new(&conn).map_err(|e| e.to_string())?.reply().map_err(|e| format!("X11 atoms: {e}"))?;
        let s = &conn.setup().roots[screen];
        let (root, root_depth, root_visual) = (s.root, s.root_depth, s.root_visual);
        let visual32 = s
            .allowed_depths
            .iter()
            .filter(|d| d.depth == 32)
            .flat_map(|d| &d.visuals)
            .find(|v| v.class == xproto::VisualClass::TRUE_COLOR)
            .map(|v| v.visual_id);
        let argb = match visual32 {
            Some(v) => {
                let map = conn.generate_id().map_err(|e| e.to_string())?;
                conn.create_colormap(xproto::ColormapAlloc::NONE, map, root, v).map_err(|e| e.to_string())?;
                Some((v, map))
            }
            None => None,
        };
        let shm = capture::shm_usable(&conn);
        // (versions asked before use, as these extensions require)
        let _ = conn.xfixes_query_version(5, 0).map(|c| c.reply());
        let _ = conn.randr_query_version(1, 5).map(|c| c.reply());
        let _ = conn.randr_select_input(root, randr::NotifyMask::SCREEN_CHANGE);
        if conn.xkb_use_extension(1, 0).ok().and_then(|c| c.reply().ok()).is_some_and(|r| r.supported) {
            // Held keys repeat as presses alone (the server otherwise sends
            // a release before each).
            let auto = xkb::PerClientFlag::DETECTABLE_AUTO_REPEAT;
            let _ = conn.xkb_per_client_flags(
                xkb::ID::USE_CORE_KBD.into(),
                auto,
                auto,
                xkb::BoolCtrl::from(0u32),
                xkb::BoolCtrl::from(0u32),
                xkb::BoolCtrl::from(0u32),
            );
        }
        let names = keyboard::Keyboard::read_names(&conn, root, atoms._XKB_RULES_NAMES);
        let scale = scale_of(&conn) as i32;
        let db = x11rb::resource_manager::new_from_default(&conn).map_err(|e| e.to_string())?;
        let cursor_theme = x11rb::cursor::Handle::new(&conn, screen, &db).ok().and_then(|c| c.reply().ok());
        let helper = conn.generate_id().map_err(|e| e.to_string())?;
        conn.create_window(
            x11rb::COPY_DEPTH_FROM_PARENT,
            helper,
            root,
            -10,
            -10,
            1,
            1,
            0,
            WindowClass::INPUT_ONLY,
            x11rb::COPY_FROM_PARENT,
            &CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE),
        )
        .map_err(|e| e.to_string())?;
        let x = X11 {
            conn,
            screen,
            root,
            root_depth,
            root_visual,
            argb,
            atoms,
            mods: 0,
            scale,
            shm,
            surfaces: Vec::new(),
            pointer_on: None,
            keyboard_on: None,
            keyboard: keyboard::Keyboard::new(names),
            cursor_theme,
            cursors: HashMap::new(),
            clip: clipboard::Clipboard::default(),
            helper,
            time: x11rb::CURRENT_TIME,
            grab: None,
            queued: VecDeque::new(),
            drag: None,
            held: None,
        };
        x.name_window(helper, "Flatshot");
        let _ = x.conn.flush();
        Ok(x)
    }

    fn device(&self, n: i32) -> i32 {
        n * self.scale
    }

    fn logical(&self, x: i16, y: i16) -> (f64, f64) {
        (x as f64 / self.scale as f64, y as f64 / self.scale as f64)
    }

    /// Whose a pointer event on `window` at `root` (device pixels, in the
    /// root window) is, and where on it (logical): while a button is held
    /// on an overlay, that overlay's, even past its edge.
    fn pointer_target(&self, window: xproto::Window, at: (i16, i16), root: (i16, i16)) -> (Option<Owner>, (f64, f64)) {
        if let Some(h) = self.held
            && h != window
            && let Some(s) = self.surfaces.iter().find(|s| s.win == h)
        {
            let x = (root.0 as i32 - s.origin.0) as f64 / self.scale as f64;
            let y = (root.1 as i32 - s.origin.1) as f64 / self.scale as f64;
            return (Some(s.owner), (x, y));
        }
        (self.owner_of(window), self.logical(at.0, at.1))
    }

    // -- monitors ------------------------------------------------------------------------

    pub fn monitors(&self) -> Vec<Monitor> {
        let s = self.scale;
        let found = self.conn.randr_get_monitors(self.root, true).ok().and_then(|c| c.reply().ok());
        let mut out = Vec::new();
        if let Some(r) = found {
            for m in r.monitors {
                if m.width == 0 || m.height == 0 {
                    continue;
                }
                let name = self
                    .conn
                    .get_atom_name(m.name)
                    .ok()
                    .and_then(|c| c.reply().ok())
                    .map(|n| String::from_utf8_lossy(&n.name).into_owned())
                    .unwrap_or_else(|| format!("monitor-{}", out.len()));
                let rect = IRect::new(m.x as i32 / s, m.y as i32 / s, m.width as i32 / s, m.height as i32 / s);
                out.push(Monitor { output: Output::X11(m.name), name, rect, scale: s });
            }
        }
        // (a monitor holding others whole is an output's own, left over
        // when it's been split into monitors of its own: theirs count)
        let all: Vec<IRect> = out.iter().map(|m| m.rect).collect();
        out.retain(|m| !all.iter().any(|r| *r != m.rect && m.rect.contains_rect(r)));
        if out.is_empty() {
            // (no RandR: the whole screen)
            let r = &self.conn.setup().roots[self.screen];
            let rect = IRect::new(0, 0, r.width_in_pixels as i32 / s, r.height_in_pixels as i32 / s);
            out.push(Monitor { output: Output::X11(0), name: "screen".into(), rect, scale: s });
        }
        out
    }

    // -- surfaces ------------------------------------------------------------------------

    fn surf(&self, owner: Owner) -> Option<&Surf> {
        self.surfaces.iter().find(|s| s.owner == owner)
    }

    fn owner_of(&self, w: xproto::Window) -> Option<Owner> {
        self.surfaces.iter().find(|s| s.win == w).map(|s| s.owner)
    }

    /// Title, class and process for `w`.
    fn name_window(&self, w: xproto::Window, title: &str) {
        let c = &self.conn;
        let a = &self.atoms;
        let _ = c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes());
        let _ = c.change_property8(PropMode::REPLACE, w, a._NET_WM_NAME, a.UTF8_STRING, title.as_bytes());
        let _ = c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_CLASS, AtomEnum::STRING, b"flatshot\0Flatshot\0");
        let _ = c.change_property32(PropMode::REPLACE, w, a._NET_WM_PID, AtomEnum::CARDINAL, &[std::process::id()]);
        if let Ok(host) = rustix::system::uname().nodename().to_str() {
            let _ = c.change_property8(PropMode::REPLACE, w, AtomEnum::WM_CLIENT_MACHINE, AtomEnum::STRING, host.as_bytes());
        }
    }

    /// A window at `at` (device pixels, in the root window), `size` big
    /// (logical).
    fn create(&mut self, owner: Owner, kind: Kind, at: (i32, i32), size: (u32, u32), transparent: bool) -> Result<xproto::Window, String> {
        let (w, h) = ((size.0.max(1) as i32 * self.scale) as u16, (size.1.max(1) as i32 * self.scale) as u16);
        let argb = transparent && self.argb.is_some() && (kind == Kind::Embedded || self.compositing());
        let unmanaged = matches!(kind, Kind::Overlay | Kind::Control | Kind::Popup);
        let win = self.conn.generate_id().map_err(|e| e.to_string())?;
        let mut aux = CreateWindowAux::new().event_mask(input_mask()).override_redirect(u32::from(unmanaged)).border_pixel(0);
        let (depth, visual) = match (argb, self.argb) {
            (true, Some((v, map))) => {
                aux = aux.colormap(map).background_pixel(0);
                (32, v)
            }
            // (no background: nothing is painted before our first frame)
            _ => {
                aux = aux.background_pixmap(x11rb::NONE);
                (self.root_depth, self.root_visual)
            }
        };
        self.conn
            .create_window(depth, win, self.root, at.0 as i16, at.1 as i16, w, h, 0, WindowClass::INPUT_OUTPUT, visual, &aux)
            .map_err(|e| e.to_string())?;
        let gc = self.conn.generate_id().map_err(|e| e.to_string())?;
        self.conn.create_gc(gc, win, &CreateGCAux::new().graphics_exposures(0)).map_err(|e| e.to_string())?;
        self.surfaces.push(Surf {
            owner,
            kind,
            win,
            gc,
            depth,
            transparent,
            argb,
            logical: size,
            origin: at,
            mapped: false,
            // (unmanaged windows are as made; others once mapped)
            configured: unmanaged,
            bufs: Vec::new(),
            frames: 0,
            scratch: Vec::new(),
            exposed: Damage::new(),
            cursor: None,
        });
        Ok(win)
    }

    /// The panel's tray (the owner of _NET_SYSTEM_TRAY_Sn), if there's one.
    fn tray_manager(&self) -> Option<xproto::Window> {
        let name = format!("_NET_SYSTEM_TRAY_S{}", self.screen);
        let atom = self.conn.intern_atom(false, name.as_bytes()).ok()?.reply().ok()?.atom;
        let owner = self.conn.get_selection_owner(atom).ok()?.reply().ok()?.owner;
        (owner != x11rb::NONE).then_some(owner)
    }

    /// An icon for the panel's tray, the old way (XEmbed): for X11 panels
    /// without the D-Bus tray. The panel takes the window in, shows it, and
    /// says its size; clicks on it come as input for Owner::TrayIcon.
    pub fn add_tray_icon(&mut self) -> Result<(), String> {
        let manager = self.tray_manager().ok_or("no system tray")?;
        // (a new tray, when the panel restarts, is announced on the root window)
        let _ = self.conn.change_window_attributes(self.root, &ChangeWindowAttributesAux::new().event_mask(EventMask::STRUCTURE_NOTIFY));
        if self.surf(Owner::TrayIcon).is_none() {
            // See-through if the tray takes 32-bit icons, else shaped.
            let wants = self.conn.get_property(false, manager, self.atoms._NET_SYSTEM_TRAY_VISUAL, AtomEnum::VISUALID, 0, 1).ok().and_then(|c| c.reply().ok());
            let argb_ok = wants.and_then(|r| r.value32().and_then(|mut v| v.next())).is_some_and(|v| self.argb.is_some_and(|a| a.0 == v));
            let size = 22u32.div_ceil(self.scale as u32).max(1);
            let win = self.create(Owner::TrayIcon, Kind::Embedded, (0, 0), (size, size), true)?;
            if let Some(s) = self.surfaces.iter_mut().find(|s| s.win == win) {
                s.argb = s.argb && argb_ok;
            }
            self.name_window(win, "Flatshot");
            // (version 0, to be shown)
            let _ = self.conn.change_property32(PropMode::REPLACE, win, self.atoms._XEMBED_INFO, self.atoms._XEMBED_INFO, &[0, 1]);
        }
        self.dock(manager);
        Ok(())
    }

    /// Ask `manager` to take our tray icon in.
    fn dock(&mut self, manager: xproto::Window) {
        let Some(s) = self.surf(Owner::TrayIcon) else { return };
        // SYSTEM_TRAY_REQUEST_DOCK
        let ev = ClientMessageEvent::new(32, manager, self.atoms._NET_SYSTEM_TRAY_OPCODE, [self.time, 0, s.win, 0, 0]);
        let _ = self.conn.send_event(false, manager, EventMask::NO_EVENT, ev);
        let _ = self.conn.flush();
    }

    /// Is a compositing manager running (so see-through windows are)?
    fn compositing(&self) -> bool {
        let name = format!("_NET_WM_CM_S{}", self.screen);
        let atom = self.conn.intern_atom(false, name.as_bytes()).ok().and_then(|c| c.reply().ok()).map(|r| r.atom);
        atom.and_then(|a| self.conn.get_selection_owner(a).ok()?.reply().ok()).is_some_and(|r| r.owner != x11rb::NONE)
    }

    fn set_atoms(&self, w: xproto::Window, property: xproto::Atom, values: &[xproto::Atom]) {
        let _ = self.conn.change_property32(PropMode::REPLACE, w, property, AtomEnum::ATOM, values);
    }

    /// No title bar or frame (Motif hints, which window managers honour).
    fn undecorated(&self, w: xproto::Window) {
        let _ = self.conn.change_property32(PropMode::REPLACE, w, self.atoms._MOTIF_WM_HINTS, self.atoms._MOTIF_WM_HINTS, &[2, 0, 0, 0, 0]);
    }

    pub fn add_overlay(&mut self, owner: Owner, monitor: &Monitor, _namespace: &str) -> Result<(), String> {
        self.refresh_keymap();
        let at = (self.device(monitor.rect.x), self.device(monitor.rect.y));
        let win = self.create(owner, Kind::Overlay, at, (monitor.rect.w as u32, monitor.rect.h as u32), false)?;
        self.name_window(win, "Flatshot");
        let a = self.atoms;
        self.set_atoms(win, a._NET_WM_WINDOW_TYPE, &[a._NET_WM_WINDOW_TYPE_NOTIFICATION]);
        self.set_cursor_on(win, CursorShape::Crosshair);
        // (shown at its first frame: see redraw)
        Ok(())
    }

    pub fn add_layer(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), _namespace: &str) -> bool {
        let pos = (self.device(monitor.rect.x + at.0), self.device(monitor.rect.y + at.1));
        let Ok(win) = self.create(owner, Kind::Layer, pos, size, false) else { return false };
        self.name_window(win, "Flatshot pin");
        let a = self.atoms;
        self.set_atoms(win, a._NET_WM_WINDOW_TYPE, &[a._NET_WM_WINDOW_TYPE_UTILITY]);
        self.set_atoms(win, a._NET_WM_STATE, &[a._NET_WM_STATE_ABOVE, a._NET_WM_STATE_SKIP_TASKBAR, a._NET_WM_STATE_SKIP_PAGER]);
        self.undecorated(win);
        // Placed where it was captured, not where the window manager would.
        let device = (self.device(size.0 as i32), self.device(size.1 as i32));
        let hints = WmSizeHints {
            position: Some((WmSizeHintsSpecification::UserSpecified, pos.0, pos.1)),
            size: Some((WmSizeHintsSpecification::UserSpecified, device.0, device.1)),
            ..Default::default()
        };
        let _ = hints.set_normal_hints(&self.conn, win);
        let _ = WmHints { input: Some(true), ..Default::default() }.set(&self.conn, win);
        self.map(owner);
        true
    }

    pub fn add_control(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), _namespace: &str, click_through: bool) -> bool {
        let pos = (self.device(monitor.rect.x + at.0), self.device(monitor.rect.y + at.1));
        let Ok(win) = self.create(owner, Kind::Control, pos, size, true) else { return false };
        self.name_window(win, "Flatshot recording");
        if click_through && let Ok(region) = self.conn.generate_id() {
            // (an empty input shape: the pointer goes to what's under it)
            let _ = self.conn.xfixes_create_region(region, &[]);
            let _ = self.conn.xfixes_set_window_shape_region(win, shape::SK::INPUT, 0, 0, region);
            let _ = self.conn.xfixes_destroy_region(region);
        }
        self.map(owner);
        true
    }

    pub fn add_window(&mut self, owner: Owner, title: &str, size: (u32, u32), decorated: bool) -> Result<(), String> {
        self.refresh_keymap();
        let win = self.create(owner, Kind::Window, (0, 0), size, false)?;
        self.name_window(win, title);
        let a = self.atoms;
        self.set_atoms(win, a.WM_PROTOCOLS, &[a.WM_DELETE_WINDOW]);
        self.set_atoms(win, a._NET_WM_WINDOW_TYPE, &[a._NET_WM_WINDOW_TYPE_NORMAL]);
        if !decorated {
            self.undecorated(win);
        }
        let _ = WmHints { input: Some(true), ..Default::default() }.set(&self.conn, win);
        self.map(owner);
        Ok(())
    }

    pub fn add_popup(&mut self, owner: Owner, parent: Owner, anchor: IRect, size: (u32, u32)) -> Result<(), String> {
        let p = self.surf(parent).ok_or("no parent surface")?.win;
        let origin = self.conn.translate_coordinates(p, self.root, 0, 0).map_err(|e| e.to_string())?.reply().map_err(|e| e.to_string())?;
        // Below and right of the anchor, flipped or slid to stay on its monitor.
        let (w, h) = (self.device(size.0 as i32), self.device(size.1 as i32));
        let ax = origin.dst_x as i32 + self.device(anchor.x + anchor.w);
        let ay = origin.dst_y as i32 + self.device(anchor.y + anchor.h);
        let s = self.scale;
        let area = self
            .monitors()
            .into_iter()
            .map(|m| IRect::new(m.rect.x * s, m.rect.y * s, m.rect.w * s, m.rect.h * s))
            .find(|r| r.contains(crate::geom::IPoint::new(ax, ay)))
            .unwrap_or(IRect::new(0, 0, i32::MAX / 2, i32::MAX / 2));
        let x = if ax + w > area.x2() { (ax - w - self.device(anchor.w)).max(area.x) } else { ax };
        let y = if ay + h > area.y2() { (ay - h - self.device(anchor.h)).max(area.y) } else { ay };
        let win = self.create(owner, Kind::Popup, (x, y), size, true)?;
        self.name_window(win, "Flatshot menu");
        let a = self.atoms;
        self.set_atoms(win, a._NET_WM_WINDOW_TYPE, &[a._NET_WM_WINDOW_TYPE_POPUP_MENU]);
        self.map(owner);
        self.grab = Some(Grab { window: win, keyboard: false, pointer: false, exclusive: true, until: Instant::now() + GRAB_FOR });
        self.try_grab();
        Ok(())
    }

    fn map(&mut self, owner: Owner) {
        let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) else { return };
        if s.mapped {
            return;
        }
        s.mapped = true;
        let win = s.win;
        let _ = self.conn.map_window(win);
        let _ = self.conn.configure_window(win, &ConfigureWindowAux::new().stack_mode(StackMode::ABOVE));
    }

    pub fn remove(&mut self, owner: Owner) {
        let Some(i) = self.surfaces.iter().position(|s| s.owner == owner) else { return };
        let s = self.surfaces.remove(i);
        self.destroy(s);
        let _ = self.conn.flush();
    }

    fn destroy(&mut self, s: Surf) {
        for b in &s.bufs {
            if let Some(seg) = b.seg {
                let _ = self.conn.shm_detach(seg);
            }
        }
        if self.grab.as_ref().is_some_and(|g| g.window == s.win) {
            self.grab = None;
        }
        if self.pointer_on == Some(s.win) {
            self.pointer_on = None;
        }
        if self.keyboard_on == Some(s.win) {
            self.keyboard_on = None;
        }
        let _ = self.conn.free_gc(s.gc);
        let _ = self.conn.destroy_window(s.win);
    }

    pub fn remove_overlays(&mut self) {
        let (gone, kept): (Vec<Surf>, Vec<Surf>) = std::mem::take(&mut self.surfaces).into_iter().partition(|s| s.kind == Kind::Overlay);
        self.surfaces = kept;
        for s in gone {
            self.destroy(s);
        }
        let _ = self.conn.flush();
    }

    pub fn place_layer(&mut self, owner: Owner, at: (i32, i32), size: (u32, u32)) {
        let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) else { return };
        s.logical = size;
        let win = s.win;
        // (from the monitor it's on: the one under its centre now)
        let origin = self.conn.translate_coordinates(win, self.root, 0, 0).ok().and_then(|c| c.reply().ok());
        let Some(o) = origin else { return };
        let s_ = self.scale;
        let centre = crate::geom::IPoint::new(o.dst_x as i32 / s_, o.dst_y as i32 / s_);
        let monitors = self.monitors();
        let m = monitors.iter().find(|m| m.rect.contains(centre)).or(monitors.first()).map(|m| m.rect).unwrap_or_default();
        let aux = ConfigureWindowAux::new()
            .x(self.device(m.x + at.0))
            .y(self.device(m.y + at.1))
            .width(self.device(size.0.max(1) as i32) as u32)
            .height(self.device(size.1.max(1) as i32) as u32);
        let _ = self.conn.configure_window(win, &aux);
    }

    pub fn set_logical_size(&mut self, owner: Owner, size: (u32, u32)) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) {
            s.logical = size;
            let win = s.win;
            let (w, h) = (self.device(size.0.max(1) as i32) as u32, self.device(size.1.max(1) as i32) as u32);
            let _ = self.conn.configure_window(win, &ConfigureWindowAux::new().width(w).height(h));
        }
    }

    pub fn set_min_size(&mut self, owner: Owner, size: (u32, u32)) {
        let Some(s) = self.surf(owner) else { return };
        let hints = WmSizeHints { min_size: Some((self.device(size.0 as i32), self.device(size.1 as i32))), ..Default::default() };
        let _ = hints.set_normal_hints(&self.conn, s.win);
    }

    pub fn set_title(&mut self, owner: Owner, title: &str) {
        if let Some(s) = self.surf(owner) {
            let win = s.win;
            let a = self.atoms;
            let _ = self.conn.change_property8(PropMode::REPLACE, win, a._NET_WM_NAME, a.UTF8_STRING, title.as_bytes());
            let _ = self.conn.change_property8(PropMode::REPLACE, win, AtomEnum::WM_NAME, AtomEnum::STRING, title.as_bytes());
        }
    }

    pub fn set_transparent(&mut self, owner: Owner) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) {
            s.transparent = true;
        }
    }

    pub fn surface(&self, owner: Owner) -> Option<((u32, u32), f32)> {
        self.surf(owner).map(|s| (s.logical, self.scale as f32))
    }

    pub fn owners(&self) -> Vec<Owner> {
        self.surfaces.iter().map(|s| s.owner).collect()
    }

    // -- drawing -------------------------------------------------------------------------

    /// Draw `owner`'s window (part `part` of `ui`) if it can be now: it's
    /// shown, and a buffer is free. Only what changed is drawn, and only
    /// what changed (or was uncovered) is sent.
    pub fn redraw_owner(&mut self, owner: Owner, part: usize, ui: &mut dyn Painted) -> bool {
        let Some(i) = self.surfaces.iter().position(|s| s.owner == owner) else { return false };
        if !self.surfaces[i].configured {
            return false;
        }
        let (w, h) = ui.buffer_size(part);
        if w == 0 || h == 0 || w > u16::MAX as u32 || h > u16::MAX as u32 {
            return false;
        }
        // Buffers of another size are of no more use (once let go).
        let old: Vec<Option<u32>> = {
            let s = &mut self.surfaces[i];
            let (keep, gone): (Vec<Buf>, Vec<Buf>) = std::mem::take(&mut s.bufs).into_iter().partition(|b| b.busy || b.img.size() == (w, h));
            s.bufs = keep;
            gone.into_iter().map(|b| b.seg).collect()
        };
        for seg in old.into_iter().flatten() {
            let _ = self.conn.shm_detach(seg);
        }
        let free = self.surfaces[i].bufs.iter().position(|b| !b.busy && b.img.size() == (w, h));
        let idx = match free {
            Some(idx) => idx,
            None if self.surfaces[i].bufs.len() < 2 => match self.new_buffer(w, h) {
                Ok(mut b) => {
                    let s = &mut self.surfaces[i];
                    // (a second buffer starts as a copy of the latest frame)
                    if let Some(latest) = s.bufs.iter().filter(|o| o.img.size() == (w, h)).max_by_key(|o| o.drawn)
                        && latest.stale.is_empty()
                    {
                        b.img.data_mut().copy_from_slice(latest.img.data());
                        b.stale = Damage::new();
                    }
                    s.bufs.push(b);
                    s.bufs.len() - 1
                }
                Err(e) => {
                    crate::warn!("no buffer for a window: {e}");
                    return false;
                }
            },
            None => return false, // (both being read: drawn when one is free)
        };
        let damage = ui.damage(part);
        let s = &mut self.surfaces[i];
        let mut todo = std::mem::take(&mut s.bufs[idx].stale);
        todo.add_all(&damage);
        for (j, b) in s.bufs.iter_mut().enumerate() {
            if j != idx {
                b.stale.add_all(&damage);
            }
        }
        let full = IRect::new(0, 0, w as i32, h as i32);
        if todo.is_empty() && s.exposed.is_empty() && s.mapped {
            return false;
        }
        let started = Instant::now();
        let mut rects = paint_damage(&mut s.bufs[idx].img, &mut s.scratch, &todo, ui, part);
        if crate::timing::enabled() && todo.is_full() {
            crate::timing::log(format!("painted {w} × {h} in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
        }
        let first = !s.mapped;
        if first || s.exposed.is_full() {
            rects = vec![full];
        } else {
            rects.extend(s.exposed.rects_in(full));
        }
        s.exposed = Damage::new();
        s.frames += 1;
        s.bufs[idx].drawn = s.frames;
        let (win, gc, depth, shaped) = (s.win, s.gc, s.depth, s.transparent && !s.argb && todo.is_full());
        if shaped {
            let r = shape_of(&s.bufs[idx].img);
            let _ = self.conn.shape_rectangles(shape::SO::SET, shape::SK::BOUNDING, xproto::ClipOrdering::UNSORTED, win, 0, 0, &r);
        }
        if first {
            self.map(owner);
            if self.surfaces[i].kind == Kind::Overlay && self.grab.is_none() {
                self.grab = Some(Grab { window: win, keyboard: false, pointer: false, exclusive: false, until: Instant::now() + GRAB_FOR });
            }
        }
        let b = &mut self.surfaces[i].bufs[idx];
        match b.seg {
            Some(seg) => {
                // (the server says when it's done reading, after the last part)
                for (n, r) in rects.iter().enumerate() {
                    let last = n + 1 == rects.len();
                    let _ = self.conn.shm_put_image(
                        win,
                        gc,
                        w as u16,
                        h as u16,
                        r.x as u16,
                        r.y as u16,
                        r.w as u16,
                        r.h as u16,
                        r.x as i16,
                        r.y as i16,
                        depth,
                        ImageFormat::Z_PIXMAP.into(),
                        last,
                        seg,
                        0,
                    );
                }
                b.busy = true;
            }
            None => put_plain(&self.conn, win, gc, depth, &b.img, &rects),
        }
        if first && self.grab.is_some() {
            self.try_grab();
        }
        let _ = self.conn.flush();
        true
    }

    fn new_buffer(&self, w: u32, h: u32) -> Result<Buf, String> {
        let img = Image::shared(w, h)?;
        let seg = if self.shm {
            let fd = img.fd().ok_or("not shared memory")?.try_clone_to_owned().map_err(|e| e.to_string())?;
            let seg = self.conn.generate_id().map_err(|e| e.to_string())?;
            self.conn.shm_attach_fd(seg, fd, true).map_err(|e| e.to_string())?;
            Some(seg)
        } else {
            None
        };
        Ok(Buf { img, seg, busy: false, stale: Damage::full(), drawn: 0 })
    }

    /// The server has read buffer `seg`.
    fn shm_done(&mut self, seg: u32) {
        for s in &mut self.surfaces {
            for b in &mut s.bufs {
                if b.seg == Some(seg) {
                    b.busy = false;
                }
            }
        }
    }

    /// Take the keyboard and pointer for the grab asked for, as far as can
    /// be done now (retried until GRAB_FOR is over).
    fn try_grab(&mut self) {
        let Some(g) = &mut self.grab else { return };
        let owner_events = !g.exclusive;
        if !g.keyboard {
            let r = self.conn.grab_keyboard(owner_events, g.window, x11rb::CURRENT_TIME, GrabMode::ASYNC, GrabMode::ASYNC).ok().and_then(|c| c.reply().ok());
            g.keyboard = r.is_some_and(|r| r.status == GrabStatus::SUCCESS);
            if g.keyboard {
                self.keyboard_on = Some(g.window);
            }
        }
        if !g.pointer {
            let mask = EventMask::BUTTON_PRESS | EventMask::BUTTON_RELEASE | EventMask::POINTER_MOTION | EventMask::ENTER_WINDOW | EventMask::LEAVE_WINDOW;
            let r = self
                .conn
                .grab_pointer(owner_events, g.window, mask, GrabMode::ASYNC, GrabMode::ASYNC, x11rb::NONE, x11rb::NONE, x11rb::CURRENT_TIME)
                .ok()
                .and_then(|c| c.reply().ok());
            g.pointer = r.is_some_and(|r| r.status == GrabStatus::SUCCESS);
        }
        if (g.keyboard && g.pointer) || Instant::now() >= g.until {
            if !(g.keyboard && g.pointer) {
                crate::timing::log("couldn't take the keyboard or pointer: another program holds it");
            }
            self.grab = None;
        }
    }

    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    // -- input -----------------------------------------------------------------------------

    pub fn pointer_owner(&self) -> Option<Owner> {
        self.pointer_on.and_then(|w| self.owner_of(w))
    }

    pub fn keyboard_owner(&self) -> Option<Owner> {
        self.keyboard_on.and_then(|w| self.owner_of(w))
    }

    pub fn text_owner(&self) -> Option<Owner> {
        None
    }

    fn cursor(&mut self, shape: CursorShape) -> xproto::Cursor {
        let names: &[&'static str] = match shape {
            CursorShape::Crosshair => &["crosshair"],
            CursorShape::Text => &["text", "xterm"],
            CursorShape::Move => &["move", "fleur"],
            CursorShape::Hand => &["pointer", "hand2"],
            CursorShape::Arrow => &["default", "left_ptr"],
            CursorShape::ResizeNS => &["ns-resize", "sb_v_double_arrow"],
            CursorShape::ResizeEW => &["ew-resize", "sb_h_double_arrow"],
            CursorShape::ResizeNWSE => &["nwse-resize", "bd_double_arrow", "bottom_right_corner"],
            CursorShape::ResizeNESW => &["nesw-resize", "fd_double_arrow", "bottom_left_corner"],
        };
        if let Some(c) = self.cursors.get(names[0]) {
            return *c;
        }
        let Some(theme) = &self.cursor_theme else { return x11rb::NONE };
        let c = names.iter().map(|n| theme.load_cursor(&self.conn, n).unwrap_or(x11rb::NONE)).find(|c| *c != x11rb::NONE).unwrap_or(x11rb::NONE);
        self.cursors.insert(names[0], c);
        c
    }

    fn set_cursor_on(&mut self, win: xproto::Window, shape: CursorShape) {
        let c = self.cursor(shape);
        let _ = self.conn.change_window_attributes(win, &ChangeWindowAttributesAux::new().cursor(c));
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.win == win) {
            s.cursor = Some(shape);
        }
    }

    pub fn set_cursor(&mut self, owner: Owner, shape: CursorShape) {
        let Some(s) = self.surf(owner) else { return };
        if s.cursor != Some(shape) {
            let win = s.win;
            self.set_cursor_on(win, shape);
        }
    }

    pub fn set_text_input(&mut self, _caret: Option<Rect>) {}

    /// The window manager moves `owner`'s window with the pointer.
    pub fn start_move(&mut self, owner: Owner) -> bool {
        let Some(s) = self.surf(owner) else { return false };
        let win = s.win;
        let Some(p) = self.conn.query_pointer(self.root).ok().and_then(|c| c.reply().ok()) else { return false };
        let _ = self.conn.ungrab_pointer(x11rb::CURRENT_TIME);
        // _NET_WM_MOVERESIZE: where, "move", the left button, from an application
        let data = [p.root_x as u32, p.root_y as u32, 8, 1, 1];
        let ev = ClientMessageEvent::new(32, win, self.atoms._NET_WM_MOVERESIZE, data);
        let mask = EventMask::SUBSTRUCTURE_REDIRECT | EventMask::SUBSTRUCTURE_NOTIFY;
        self.conn.send_event(false, self.root, mask, ev).is_ok()
    }

    pub fn has_relative(&self) -> bool {
        true
    }

    pub fn can_place(&self) -> bool {
        true
    }

    pub fn without_focus(&self) -> bool {
        true
    }

    /// Read the keymap's names again (setxkbmap may have changed them).
    fn refresh_keymap(&mut self) {
        let names = keyboard::Keyboard::read_names(&self.conn, self.root, self.atoms._XKB_RULES_NAMES);
        self.keyboard.reset_if_changed(names);
    }

    // -- the loop ----------------------------------------------------------------------------

    pub fn tick(&mut self) {
        if self.grab.is_some() {
            self.try_grab();
        }
        self.clipboard_tick();
        let _ = self.conn.flush();
    }

    pub fn next_deadline(&self) -> Option<Instant> {
        let grab = self.grab.as_ref().map(|_| Instant::now() + Duration::from_millis(15));
        [grab, self.clipboard_deadline()].into_iter().flatten().min()
    }

    /// The next event: one read earlier, or one waiting now.
    fn next_event(&mut self) -> Result<Option<Event>, x11rb::errors::ConnectionError> {
        if let Some(e) = self.queued.pop_front() {
            return Ok(Some(e));
        }
        self.conn.poll_for_event()
    }

    /// Have events been read into the connection, which the loop won't hear
    /// of (they're no longer on the socket)?
    pub fn has_events(&mut self) -> bool {
        if !self.queued.is_empty() {
            return true;
        }
        match self.conn.poll_for_event() {
            Ok(Some(e)) => {
                self.queued.push_back(e);
                true
            }
            _ => false,
        }
    }
}

impl Drop for X11 {
    fn drop(&mut self) {
        for s in std::mem::take(&mut self.surfaces) {
            self.destroy(s);
        }
        let _ = self.conn.flush();
    }
}

/// The opaque parts of `img` (alpha at least half), as rectangles, for
/// shaping a see-through window when no compositor blends it.
fn shape_of(img: &Image) -> Vec<xproto::Rectangle> {
    let (w, h) = img.size();
    let mut out: Vec<xproto::Rectangle> = Vec::new();
    let mut prev: Vec<(u16, u16)> = Vec::new();
    let mut start = 0;
    for y in 0..=h {
        let runs: Vec<(u16, u16)> = if y < h {
            let row = img.row(y);
            let mut runs = Vec::new();
            let mut x = 0;
            while x < w {
                while x < w && row[x as usize * 4 + 3] < 128 {
                    x += 1;
                }
                let from = x;
                while x < w && row[x as usize * 4 + 3] >= 128 {
                    x += 1;
                }
                if x > from {
                    runs.push((from as u16, (x - from) as u16));
                }
            }
            runs
        } else {
            Vec::new()
        };
        // (rows alike are one band)
        if runs != prev || y == h {
            for &(x, len) in &prev {
                out.push(xproto::Rectangle { x: x as i16, y: start as i16, width: len, height: (y - start) as u16 });
            }
            prev = runs;
            start = y;
        }
    }
    out
}

/// `rects` of `img` sent with the requests themselves (no shared memory),
/// in pieces the server takes.
fn put_plain(conn: &RustConnection, win: xproto::Window, gc: xproto::Gcontext, depth: u8, img: &Image, rects: &[IRect]) {
    let max = conn.maximum_request_bytes().saturating_sub(64).max(4096);
    for r in rects {
        let row = r.w as usize * 4;
        let rows_per = (max / row.max(1)).max(1);
        let mut y = r.y;
        while y < r.y + r.h {
            let n = (rows_per as i32).min(r.y + r.h - y);
            let mut data = Vec::with_capacity(row * n as usize);
            for yy in y..y + n {
                let line = img.row(yy as u32);
                data.extend_from_slice(&line[r.x as usize * 4..r.x as usize * 4 + row]);
            }
            let _ = conn.put_image(ImageFormat::Z_PIXMAP, win, gc, r.w as u16, n as u16, r.x as i16, y as i16, 0, depth, &data);
            y += n;
        }
    }
}

// -- events ------------------------------------------------------------------------------

/// The X server's events, in the event loop.
pub fn insert_source(handle: &calloop::LoopHandle<'static, App>, app: &App) -> Result<(), String> {
    let Display::X11(x) = &app.dpy else { return Err("not an X11 session".into()) };
    let fd = x.conn.stream().as_fd().try_clone_to_owned().map_err(|e| e.to_string())?;
    let source = calloop::generic::Generic::new(fd, calloop::Interest::READ, calloop::Mode::Level);
    handle
        .insert_source(source, |_, _, app: &mut App| {
            app.x11_dispatch();
            Ok(calloop::PostAction::Continue)
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

impl App {
    fn x11(&mut self) -> &mut X11 {
        match &mut self.dpy {
            Display::X11(x) => x,
            Display::Wayland(_) => unreachable!("an X11 event on Wayland"),
        }
    }

    /// Every event the X server has sent.
    pub fn x11_dispatch(&mut self) {
        loop {
            match self.x11().next_event() {
                Ok(Some(e)) => self.x11_event(e),
                Ok(None) => break,
                Err(e) => {
                    crate::warn!("lost the X server: {e}");
                    self.exit = Some(2);
                    break;
                }
            }
        }
    }

    fn x11_event(&mut self, ev: Event) {
        let x = self.x11();
        if let Some(t) = time_of(&ev) {
            x.time = t;
        }
        match ev {
            Event::KeyPress(e) => self.x11_key(e.event, e.detail, e.state.into(), true),
            Event::KeyRelease(e) => self.x11_key(e.event, e.detail, e.state.into(), false),
            Event::ButtonPress(e) | Event::ButtonRelease(e) => {
                let pressed = e.response_type & 0x7f == xproto::BUTTON_PRESS_EVENT;
                let x = self.x11();
                x.mods = keyboard::mods_of(e.state.into());
                let (mut owner, at) = x.pointer_target(e.event, (e.event_x, e.event_y), (e.root_x, e.root_y));
                // A menu holds the pointer: a click outside it dismisses it.
                if let Some(s) = x.surfaces.iter().find(|s| s.win == e.event && s.kind == Kind::Popup)
                    && pressed
                    && (!(0.0..s.logical.0 as f64).contains(&at.0) || !(0.0..s.logical.1 as f64).contains(&at.1))
                {
                    let o = s.owner;
                    return self.surface_closed(o);
                }
                let code = match e.detail {
                    1 => 0x110,
                    2 => 0x112,
                    3 => 0x111,
                    4 | 5 => {
                        if pressed {
                            self.input(owner, Input::Scroll(if e.detail == 4 { 1.0 } else { -1.0 }));
                        }
                        return;
                    }
                    6 | 7 => return,
                    n => 0x10b + n as u32, // (back, forward: 0x113, 0x114)
                };
                let x = self.x11();
                x.drag = if pressed { Some((e.root_x, e.root_y)) } else { None };
                if owner.is_none() {
                    owner = x.pointer_owner();
                }
                let overlay = x.surfaces.iter().any(|s| s.win == e.event && s.kind == Kind::Overlay);
                let crossed = if pressed {
                    if overlay {
                        x.held = Some(e.event);
                    }
                    None
                } else {
                    // Let go: if the pointer is now on another overlay, it
                    // leaves the one held and enters that one.
                    let held = x.held.take();
                    match (held, x.pointer_on) {
                        (Some(h), Some(on)) if h != on => {
                            let on_at = x.surfaces.iter().find(|s| s.win == on).map(|s| {
                                let sc = x.scale as f64;
                                ((e.root_x as i32 - s.origin.0) as f64 / sc, (e.root_y as i32 - s.origin.1) as f64 / sc)
                            });
                            Some((x.owner_of(h), x.owner_of(on), on_at.unwrap_or_default()))
                        }
                        _ => None,
                    }
                };
                // A window clicked takes the keyboard, as window managers do
                // (and as there's none to do it, on a bare X server).
                if pressed && x.keyboard_on != Some(e.event) && x.surfaces.iter().any(|s| s.win == e.event && matches!(s.kind, Kind::Window | Kind::Layer)) {
                    let _ = x.conn.set_input_focus(xproto::InputFocus::PARENT, e.event, e.time);
                    x.keyboard_on = Some(e.event);
                }
                self.input(owner, Input::Button(code, pressed, at));
                if let Some((left, entered, at)) = crossed {
                    self.input(left, Input::Leave);
                    self.input(entered, Input::Enter(at));
                }
            }
            Event::MotionNotify(e) => {
                let x = self.x11();
                let (owner, at) = x.pointer_target(e.event, (e.event_x, e.event_y), (e.root_x, e.root_y));
                let moved = match x.drag {
                    Some(from) => {
                        x.drag = Some((e.root_x, e.root_y));
                        let s = x.scale as f64;
                        Some(((e.root_x - from.0) as f64 / s, (e.root_y - from.1) as f64 / s))
                    }
                    None => None,
                };
                if let Some((dx, dy)) = moved
                    && (dx, dy) != (0.0, 0.0)
                {
                    self.relative_motion(dx, dy);
                }
                self.input(owner, Input::Motion(at, false));
            }
            Event::EnterNotify(e) => {
                let x = self.x11();
                let at = x.logical(e.event_x, e.event_y);
                let owner = x.owner_of(e.event);
                if owner.is_some() {
                    x.pointer_on = Some(e.event);
                    if let Some(s) = x.surfaces.iter_mut().find(|s| s.win == e.event) {
                        s.cursor = None; // (set again for this entry)
                    }
                }
                // (while a button is held on an overlay, it keeps the pointer)
                if x.held.is_none_or(|h| h == e.event) {
                    self.input(owner, Input::Enter(at));
                }
            }
            Event::LeaveNotify(e) => {
                let x = self.x11();
                let owner = x.owner_of(e.event);
                if x.pointer_on == Some(e.event) {
                    x.pointer_on = None;
                }
                if x.held.is_none() {
                    self.input(owner, Input::Leave);
                }
            }
            Event::FocusIn(e) => {
                let x = self.x11();
                if x.owner_of(e.event).is_some() {
                    x.keyboard_on = Some(e.event);
                }
            }
            Event::FocusOut(e) => {
                let x = self.x11();
                // (an overlay keeps the keyboard it grabbed)
                if x.keyboard_on == Some(e.event) && e.mode != xproto::NotifyMode::WHILE_GRABBED {
                    x.keyboard_on = None;
                }
            }
            Event::Expose(e) => {
                let x = self.x11();
                if let Some(s) = x.surfaces.iter_mut().find(|s| s.win == e.window) {
                    s.exposed.add(IRect::new(e.x as i32, e.y as i32, e.width as i32, e.height as i32));
                }
            }
            Event::ConfigureNotify(e) => {
                let x = self.x11();
                let scale = x.scale as u32;
                if let Some(s) = x.surfaces.iter_mut().find(|s| s.win == e.window)
                    && matches!(s.kind, Kind::Window | Kind::Layer | Kind::Embedded)
                {
                    s.logical = ((e.width as u32 / scale).max(1), (e.height as u32 / scale).max(1));
                }
            }
            Event::MapNotify(e) => {
                let x = self.x11();
                if let Some(s) = x.surfaces.iter_mut().find(|s| s.win == e.window) {
                    s.configured = true;
                    s.exposed = Damage::full();
                }
            }
            Event::UnmapNotify(e) => {
                let x = self.x11();
                if let Some(s) = x.surfaces.iter_mut().find(|s| s.win == e.window)
                    && matches!(s.kind, Kind::Window | Kind::Layer | Kind::Embedded)
                {
                    s.configured = false;
                }
            }
            Event::ClientMessage(e) => {
                let x = self.x11();
                // A new panel's tray: the icon goes there.
                if e.type_ == x.atoms.MANAGER
                    && e.window == x.root
                    && x.surf(Owner::TrayIcon).is_some()
                    && let Some(m) = x.tray_manager()
                {
                    x.dock(m);
                    return;
                }
                if e.type_ == x.atoms.WM_PROTOCOLS
                    && e.format == 32
                    && e.data.as_data32()[0] == x.atoms.WM_DELETE_WINDOW
                    && let Some(owner) = x.owner_of(e.window)
                {
                    self.surface_closed(owner);
                }
            }
            Event::SelectionRequest(e) => {
                let x = self.x11();
                if crate::timing::enabled() {
                    let what = x.describe(e.target);
                    crate::timing::log(format!("clipboard asked for {what}"));
                }
                x.selection_request(e);
            }
            Event::SelectionClear(e) => self.x11().selection_clear(e),
            Event::SelectionNotify(e) => self.x11().selection_notify(e),
            Event::PropertyNotify(e) => self.x11().property_notify(e),
            Event::ShmCompletion(e) => self.x11().shm_done(e.shmseg),
            Event::RandrScreenChangeNotify(_) => {
                // A monitor gone takes its part of a capture with it.
                let now: Vec<Output> = self.x11().monitors().into_iter().map(|m| m.output).collect();
                let gone: Vec<Output> = self.monitors_in_use().into_iter().filter(|o| !now.contains(o)).collect();
                for o in gone {
                    self.monitor_gone(&o);
                }
            }
            Event::Error(e) => crate::timing::log(format!("X11 error: {:?} in request {}", e.error_kind, e.major_opcode)),
            _ => {}
        }
    }

    fn x11_key(&mut self, window: xproto::Window, keycode: u8, state: u16, pressed: bool) {
        let x = self.x11();
        let owner = x.owner_of(window).or(x.keyboard_owner());
        let press = x.keyboard.key(keycode, state, pressed);
        let mods = keyboard::mods_after(keyboard::mods_of(state), press.keysym, pressed);
        let changed = mods != x.mods;
        x.mods = mods;
        self.input(owner, if pressed { Input::Key(press) } else { Input::KeyUp(press) });
        if changed {
            self.input(owner, Input::Modifiers(mods));
        }
    }
}

/// When an event happened, for those that say.
fn time_of(ev: &Event) -> Option<xproto::Timestamp> {
    Some(match ev {
        Event::KeyPress(e) | Event::KeyRelease(e) => e.time,
        Event::ButtonPress(e) | Event::ButtonRelease(e) => e.time,
        Event::MotionNotify(e) => e.time,
        Event::EnterNotify(e) | Event::LeaveNotify(e) => e.time,
        Event::PropertyNotify(e) => e.time,
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn shapes_follow_what_is_drawn() {
        // A 6x4 picture: opaque but its corner pixels, which are clear.
        let mut img = Image::filled(6, 4, 0xFF336699).unwrap();
        for (x, y) in [(0, 0), (5, 0), (0, 3), (5, 3)] {
            img.set_pixel(x, y, 0);
        }
        let r = shape_of(&img);
        let area: u32 = r.iter().map(|r| r.width as u32 * r.height as u32).sum();
        assert_eq!(area, 6 * 4 - 4);
        assert!(r.iter().any(|r| (r.x, r.y, r.width, r.height) == (0, 1, 6, 2)), "{r:?}");
    }
}
