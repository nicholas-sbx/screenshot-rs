//! The desktop on X11, from what the window manager publishes (EWMH): the
//! windows on the current desktop, bottom to top, the active one, and
//! where the pointer is. Global logical pixels. What windows call
//! themselves is untrusted: titles and the list are capped.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, AtomEnum, ConnectionExt as _, MapState};
use x11rb::rust_connection::RustConnection;

use crate::geom::{IRect, Point};
use crate::ui::overlay::Window;
use crate::ui::state::Desktop;

const MAX_WINDOWS: usize = 512;
const MAX_TITLE: usize = 512;

x11rb::atom_manager! {
    Atoms: AtomsCookie {
        _NET_CLIENT_LIST_STACKING,
        _NET_CLIENT_LIST,
        _NET_ACTIVE_WINDOW,
        _NET_CURRENT_DESKTOP,
        _NET_WM_DESKTOP,
        _NET_WM_STATE,
        _NET_WM_STATE_HIDDEN,
        _NET_WM_WINDOW_TYPE,
        _NET_WM_WINDOW_TYPE_NORMAL,
        _NET_WM_WINDOW_TYPE_DIALOG,
        _NET_WM_WINDOW_TYPE_DESKTOP,
        _NET_WM_PID,
        _NET_FRAME_EXTENTS,
        _NET_WM_NAME,
        UTF8_STRING,
    }
}

/// `all`: panels, notifications, popups and the like too.
pub fn query(all: bool) -> Option<Desktop> {
    let (conn, screen) = x11rb::connect(None).ok()?;
    let root = conn.setup().roots[screen].root;
    let atoms = Atoms::new(&conn).ok()?.reply().ok()?;
    let scale = super::scale_of(&conn);
    let cursor = conn.query_pointer(root).ok()?.reply().ok().map(|p| Point::new(p.root_x as f32 / scale, p.root_y as f32 / scale));
    let clients = cardinals(&conn, root, atoms._NET_CLIENT_LIST_STACKING, AtomEnum::WINDOW.into())
        .filter(|l| !l.is_empty())
        .or_else(|| cardinals(&conn, root, atoms._NET_CLIENT_LIST, AtomEnum::WINDOW.into()))
        .unwrap_or_default();
    let current = cardinals(&conn, root, atoms._NET_CURRENT_DESKTOP, AtomEnum::CARDINAL.into()).and_then(|v| v.first().copied());
    let active = cardinals(&conn, root, atoms._NET_ACTIVE_WINDOW, AtomEnum::WINDOW.into()).and_then(|v| v.first().copied());
    let mut windows = Vec::new();
    let mut active_window = None;
    for &w in clients.iter().rev().take(MAX_WINDOWS).collect::<Vec<_>>().iter().rev() {
        let Some(found) = describe(&conn, &atoms, root, *w, current, scale, all) else { continue };
        if Some(*w) == active {
            active_window = Some(found.clone());
        }
        windows.push(found);
    }
    windows.reverse(); // (the stacking list is bottom to top; the topmost come first)
    Some(Desktop { windows, active: active_window, cursor })
}

/// A 32-bit list property.
fn cardinals(conn: &RustConnection, w: xproto::Window, property: xproto::Atom, kind: xproto::Atom) -> Option<Vec<u32>> {
    let r = conn.get_property(false, w, property, kind, 0, 4096).ok()?.reply().ok()?;
    Some(r.value32()?.collect())
}

/// Window `w`, if it's a normal window shown on the current desktop.
fn describe(conn: &RustConnection, atoms: &Atoms, root: xproto::Window, w: xproto::Window, current: Option<u32>, scale: f32, all: bool) -> Option<Window> {
    let attrs = conn.get_window_attributes(w).ok()?;
    let geometry = conn.get_geometry(w).ok()?;
    let origin = conn.translate_coordinates(w, root, 0, 0).ok()?;
    let state = conn.get_property(false, w, atoms._NET_WM_STATE, AtomEnum::ATOM, 0, 64).ok()?;
    let desktop = conn.get_property(false, w, atoms._NET_WM_DESKTOP, AtomEnum::CARDINAL, 0, 1).ok()?;
    let kind = conn.get_property(false, w, atoms._NET_WM_WINDOW_TYPE, AtomEnum::ATOM, 0, 16).ok()?;
    let frame = conn.get_property(false, w, atoms._NET_FRAME_EXTENTS, AtomEnum::CARDINAL, 0, 4).ok()?;
    let name = conn.get_property(false, w, atoms._NET_WM_NAME, atoms.UTF8_STRING, 0, (MAX_TITLE as u32) * 4).ok()?;
    let old_name = conn.get_property(false, w, AtomEnum::WM_NAME, AtomEnum::ANY, 0, (MAX_TITLE as u32) * 4).ok()?;
    let class = conn.get_property(false, w, AtomEnum::WM_CLASS, AtomEnum::STRING, 0, 64).ok()?;
    let pid = conn.get_property(false, w, atoms._NET_WM_PID, AtomEnum::CARDINAL, 0, 1).ok()?;
    if attrs.reply().ok()?.map_state != MapState::VIEWABLE {
        return None;
    }
    if state.reply().ok().and_then(|r| r.value32().map(|mut v| v.any(|a| a == atoms._NET_WM_STATE_HIDDEN))).unwrap_or(false) {
        return None;
    }
    let on = desktop.reply().ok().and_then(|r| r.value32().and_then(|mut v| v.next()));
    if let (Some(d), Some(c)) = (on, current)
        && d != c
        && d != 0xFFFF_FFFF
    {
        return None;
    }
    let types: Vec<u32> = kind.reply().ok().and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default();
    let app_window = types.is_empty() || types.iter().any(|t| *t == atoms._NET_WM_WINDOW_TYPE_NORMAL || *t == atoms._NET_WM_WINDOW_TYPE_DIALOG);
    let own = pid.reply().ok().and_then(|r| r.value32().and_then(|mut v| v.next())) == Some(std::process::id());
    // Panels, docks, notifications and the like, when asked for: not the wallpaper.
    if !app_window && (!all || types.contains(&atoms._NET_WM_WINDOW_TYPE_DESKTOP)) {
        return None;
    }
    let g = geometry.reply().ok()?;
    let o = origin.reply().ok()?;
    let ext: Vec<u32> = frame.reply().ok().and_then(|r| r.value32().map(|v| v.collect())).unwrap_or_default();
    let [left, right, top, bottom] = match ext[..] {
        [l, r, t, b] => [l, r, t, b].map(|n| n.min(512) as i32),
        _ => [0; 4],
    };
    let x = o.dst_x as i32 - left;
    let y = o.dst_y as i32 - top;
    let (w, h) = (g.width as i32 + left + right, g.height as i32 + top + bottom);
    if w < 8 || h < 8 {
        return None;
    }
    let s = |n: i32| (n as f32 / scale).round() as i32;
    let title = name
        .reply()
        .ok()
        .filter(|r| !r.value.is_empty())
        .map(|r| String::from_utf8_lossy(&r.value).into_owned())
        .or_else(|| old_name.reply().ok().map(|r| r.value.iter().map(|&b| b as char).collect()))
        .unwrap_or_default();
    let class = class.reply().ok().map(|r| r.value).unwrap_or_default();
    let mut parts = class.split(|&b| b == 0).map(|p| String::from_utf8_lossy(p).into_owned());
    let instance = parts.next().unwrap_or_default();
    let app = if instance.is_empty() { parts.next().unwrap_or_default().to_lowercase() } else { instance };
    Some(Window { rect: IRect::new(s(x), s(y), s(w), s(h)), title: title.chars().take(MAX_TITLE).collect(), app: app.chars().take(MAX_TITLE).collect(), own })
}
