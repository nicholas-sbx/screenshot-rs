//! The desktop at capture time: the windows (for hover-and-click window
//! capture), the active one and where the pointer is (for the instant
//! "active window" and "current monitor" captures). Global logical pixels.
//!
//! Wayland gives programs no window list, so on KDE a small KWin script
//! (loaded over KWin's scripting D-Bus API) reads the stacking order and
//! calls back with every visible window. Sway and Hyprland answer through
//! their IPC sockets (and say less about which window is over which).

use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::sync::mpsc;
use std::time::Duration;

use serde_json::Value;

use crate::geom::{IRect, Point};
use crate::ui::overlay::Window;
use crate::ui::state::Desktop;
use crate::util;

const KWIN: &str = "org.kde.KWin";
/// At most this many windows, titles at most this long (a report is untrusted).
const MAX_WINDOWS: usize = 512;
const MAX_TITLE: usize = 512;

/// The script KWin runs: windows on the current desktop, bottom to top,
/// the active one, and the pointer. Plasma 5 and 6 differ; both handled.
const SCRIPT: &str = r#"
(function () {
    var cur = workspace.currentDesktop;
    var list = workspace.stackingOrder;
    var out = [];
    function onCurrent(w) {
        if (w.onAllDesktops) return true;
        if (w.desktops !== undefined && w.desktops !== null && w.desktops.length !== undefined) {
            for (var j = 0; j < w.desktops.length; j++) {
                var d = w.desktops[j];
                if (d === cur || (d && cur && d.id !== undefined && d.id === cur.id)) return true;
            }
            return w.desktops.length === 0;
        }
        if (w.desktop !== undefined) return w.desktop === cur || w.desktop === -1;
        return true;
    }
    var all = %ALL%;  // panels, notifications, popups and the like too
    function wanted(w) {
        if (w.normalWindow || w.dialog) return true;
        if (!all || w.desktopWindow) return false;  // (not the wallpaper)
        return !!(w.dock || w.notification || w.criticalNotification || w.onScreenDisplay || w.utility || w.splash
                  || w.toolbar || w.menu || w.popupMenu || w.dropdownMenu || w.tooltip || w.comboBox || w.popupWindow
                  || w.appletPopup);
    }
    var act = workspace.activeWindow || workspace.activeClient;
    var active = -1;
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        try {
            if (!wanted(w) || w.minimized || w.deleted || w.hidden) continue;
            if (!onCurrent(w)) continue;
            var g = w.frameGeometry;
            if (!g || g.width < 8 || g.height < 8) continue;
            if (act && (w === act || String(w.internalId) === String(act.internalId))) active = out.length;
            out.push([Math.round(g.x), Math.round(g.y), Math.round(g.width), Math.round(g.height),
                      String(w.caption || ""), String(w.resourceClass || ""), w.pid === %PID%]);
        } catch (e) {}
    }
    var cursor = null;
    try { cursor = [Math.round(workspace.cursorPos.x), Math.round(workspace.cursorPos.y)]; } catch (e) {}
    callDBus("%SERVICE%", "%PATH%", "org.flatshot.Windows", "report",
             JSON.stringify({windows: out, active: active, cursor: cursor}));
})();
"#;

/// Ask whichever compositor this is; None if none can answer in `timeout`.
/// `all`: panels, notifications, popups and the like too, not just app
/// windows and dialogs.
pub fn query(timeout: Duration, all: bool) -> Option<Desktop> {
    if crate::x11::chosen() {
        return crate::x11::windows::query(all);
    }
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return hyprland();
    }
    if let Some(sock) = std::env::var_os("SWAYSOCK") {
        return sway(std::path::Path::new(&sock));
    }
    kwin(timeout, all)
}

/// Is there a compositor to ask (without asking)?
pub fn supported() -> bool {
    crate::x11::chosen() || std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() || std::env::var_os("SWAYSOCK").is_some() || crate::dbus::has_owner(KWIN)
}

// -- KWin -----------------------------------------------------------------------------

/// Where the script's report arrives.
struct Report {
    /// (who sent it, and the report: weighed by the thread asking)
    tx: std::sync::Mutex<mpsc::SyncSender<(String, String)>>,
}

/// The process on the other end of D-Bus connection `name`, as the bus says.
fn pid_of(conn: &crate::dbus::Bus, name: &str) -> Option<u32> {
    conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetConnectionUnixProcessID", &(name,))
        .ok()?
        .body()
        .deserialize::<u32>()
        .ok()
}

#[zbus::interface(name = "org.flatshot.Windows")]
impl Report {
    // (named as the script calls it: zbus would make it "Report")
    #[zbus(name = "report")]
    fn report(&self, #[zbus(header)] hdr: zbus::message::Header<'_>, json: String) {
        // (anyone on the bus could call this: who it was is checked by the
        // asking thread, not here, where asking the bus would hold it up)
        let Some(sender) = hdr.sender().map(|s| s.to_string()) else { return };
        let _ = self.tx.lock().unwrap_or_else(|e| e.into_inner()).try_send((sender, json));
    }
}

fn kwin(timeout: Duration, all: bool) -> Option<Desktop> {
    let conn = crate::dbus::session().ok()?;
    let owner = conn
        .call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetNameOwner", &(KWIN,))
        .ok()?
        .body()
        .deserialize::<String>()
        .ok()?;
    let me = conn.unique_name()?.to_string();
    let token = util::random_hex(6);
    let path = format!("/flatshot/windows/w{token}");
    let (tx, rx) = mpsc::sync_channel(8);
    let kwin_pid = pid_of(conn, &owner);
    let report = Report { tx: std::sync::Mutex::new(tx) };
    // (a fresh path: nothing's served there yet)
    zbus::block_on(conn.inner().object_server().at(path.as_str(), report)).ok()?;
    // The script lives in our private folder: nobody else can change it
    // before KWin reads it.
    let dir = crate::ipc::runtime_dir().ok()?;
    let file = dir.join(format!("windows-{token}.js"));
    let script = SCRIPT
        .replace("%ALL%", if all { "true" } else { "false" })
        .replace("%PID%", &std::process::id().to_string())
        .replace("%SERVICE%", &me)
        .replace("%PATH%", &path);
    let name = format!("flatshot-windows-{token}");
    let result = (|| {
        write_private(&file, &script).ok()?;
        conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(file.to_string_lossy().as_ref(), name.as_str())).ok()?;
        conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "start", &()).ok()?;
        // Only KWin's word counts: its own connection, or another of its
        // process's (its scripts may call from one), as the bus vouches.
        let until = std::time::Instant::now() + timeout;
        let mut got = None;
        while let Ok((sender, json)) = rx.recv_timeout(until.saturating_duration_since(std::time::Instant::now())) {
            if sender == owner || kwin_pid.is_some() && pid_of(conn, &sender) == kwin_pid {
                got = Some(json);
                break;
            }
        }
        if got.is_none() {
            crate::timing::log(format!("windows: KWin's script didn't report within {} ms", timeout.as_millis()));
        }
        got
    })();
    let _ = conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(name.as_str(),));
    let _ = std::fs::remove_file(&file);
    let _ = zbus::block_on(conn.inner().object_server().remove::<Report, _>(path.as_str()));
    result.map(|json| parse_kwin(&json))
}

/// A new file only we can read (no following links, no replacing).
fn write_private(path: &std::path::Path, text: &str) -> std::io::Result<()> {
    use std::os::unix::fs::OpenOptionsExt;
    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(path)?;
    f.write_all(text.as_bytes())
}

/// The script's report (bottom-to-top rows) as a Desktop (topmost first).
pub fn parse_kwin(report: &str) -> Desktop {
    let Ok(data) = serde_json::from_str::<Value>(report) else { return Desktop::default() };
    let rows = data.get("windows").and_then(|v| v.as_array()).cloned().unwrap_or_default();
    let active_index = data.get("active").and_then(|v| v.as_i64()).unwrap_or(-1);
    let mut windows = Vec::new();
    let mut active = None;
    for (i, row) in rows.iter().take(MAX_WINDOWS).enumerate() {
        let Some(w) = window_from_row(row) else { continue };
        if i as i64 == active_index {
            active = Some(w.clone());
        }
        windows.push(w);
    }
    windows.reverse();
    let cursor = data.get("cursor").and_then(|c| c.as_array()).and_then(|c| {
        let n = |v: &Value| v.as_f64().filter(|v| v.abs() < 1e6);
        Some(Point::new(n(c.first()?)? as f32, n(c.get(1)?)? as f32))
    });
    Desktop { windows, active, cursor }
}

fn window_from_row(row: &Value) -> Option<Window> {
    let r = row.as_array()?;
    let n = |i: usize| r.get(i)?.as_f64().filter(|v| v.is_finite() && v.abs() < 1e6).map(|v| v as i32);
    let (x, y, w, h) = (n(0)?, n(1)?, n(2)?, n(3)?);
    if w <= 0 || h <= 0 {
        return None;
    }
    let s = |i: usize| r.get(i).and_then(|v| v.as_str()).map(|t| t.chars().take(MAX_TITLE).collect::<String>()).unwrap_or_default();
    let own = r.get(6).and_then(|v| v.as_bool()).unwrap_or(false);
    Some(Window { rect: IRect::new(x, y, w, h), title: s(4), app: s(5), own })
}

// -- sway ------------------------------------------------------------------------------

/// Sway's tree over its IPC socket (i3's protocol).
fn sway(sock: &std::path::Path) -> Option<Desktop> {
    let tree = i3_ipc(sock, 4, b"")?;
    Some(parse_sway(&serde_json::from_slice(&tree).ok()?))
}

/// Sway's tree as a Desktop: the focused window, the visible ones (for the
/// overlay's hover), floating ones above the tiled, and the focused
/// output's middle for the pointer (sway can't be asked where it is).
pub fn parse_sway(tree: &Value) -> Desktop {
    let mut active = None;
    let mut cursor = None;
    fn walk(node: &Value, visible: bool, output: Option<&Value>, out: &mut Vec<(bool, Window)>, active: &mut Option<Window>, cursor: &mut Option<Point>) {
        let kind = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let output = if kind == "output" { Some(node) } else { output };
        // (windows say whether they're visible: on a shown workspace, not hidden in a stack)
        let visible = node.get("visible").and_then(|v| v.as_bool()).unwrap_or(visible);
        let r = node.get("rect");
        let rect = r.and_then(|r| {
            let g = |k: &str| r.get(k)?.as_i64().filter(|v| v.abs() < 1 << 20).map(|v| v as i32);
            Some(IRect::new(g("x")?, g("y")?, g("width")?, g("height")?))
        });
        let is_window = (kind == "con" || kind == "floating_con") && node.get("pid").is_some();
        if is_window && let Some(rect) = rect.filter(|r| r.w > 0 && r.h > 0) {
            let app = node.get("app_id").and_then(|v| v.as_str()).or_else(|| node.pointer("/window_properties/class").and_then(|v| v.as_str())).unwrap_or("");
            let title = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let w = Window { rect, title: title.chars().take(MAX_TITLE).collect(), app: app.chars().take(MAX_TITLE).collect(), own: false };
            if node.get("focused").and_then(|v| v.as_bool()).unwrap_or(false) {
                *active = Some(w.clone());
                if let Some(o) = output.and_then(|o| o.get("rect")) {
                    let g = |k: &str| o.get(k).and_then(|v| v.as_i64()).filter(|v| v.abs() < 1 << 20).unwrap_or(0) as f32;
                    *cursor = Some(Point::new(g("x") + g("width") / 2.0, g("y") + g("height") / 2.0));
                }
            }
            if visible && out.len() < MAX_WINDOWS {
                out.push((kind == "floating_con", w));
            }
        }
        for key in ["nodes", "floating_nodes"] {
            if let Some(children) = node.get(key).and_then(|c| c.as_array()) {
                for c in children {
                    walk(c, visible, output, out, active, cursor);
                }
            }
        }
    }
    let mut found = Vec::new();
    walk(tree, false, None, &mut found, &mut active, &mut cursor);
    // Topmost first: floating windows, then the tiled.
    found.sort_by_key(|(floating, _)| !*floating);
    Desktop { windows: found.into_iter().map(|(_, w)| w).collect(), active, cursor }
}

/// One i3-ipc request and its reply (at most 32 MiB).
fn i3_ipc(sock: &std::path::Path, kind: u32, payload: &[u8]) -> Option<Vec<u8>> {
    let mut s = UnixStream::connect(sock).ok()?;
    s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    s.set_write_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut msg = Vec::with_capacity(14 + payload.len());
    msg.extend_from_slice(b"i3-ipc");
    msg.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
    msg.extend_from_slice(&kind.to_ne_bytes());
    msg.extend_from_slice(payload);
    s.write_all(&msg).ok()?;
    let mut head = [0u8; 14];
    s.read_exact(&mut head).ok()?;
    if &head[..6] != b"i3-ipc" {
        return None;
    }
    let len = u32::from_ne_bytes(head[6..10].try_into().ok()?) as usize;
    if len > 32 << 20 {
        return None;
    }
    let mut body = vec![0u8; len];
    s.read_exact(&mut body).ok()?;
    Some(body)
}

// -- Hyprland ----------------------------------------------------------------------------

fn hyprland() -> Option<Desktop> {
    let sig = std::env::var("HYPRLAND_INSTANCE_SIGNATURE").ok()?;
    if sig.contains('/') || sig.contains("..") {
        return None;
    }
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from)?;
    let sock = [runtime.join("hypr").join(&sig).join(".socket.sock"), std::path::PathBuf::from(format!("/tmp/hypr/{sig}/.socket.sock"))]
        .into_iter()
        .find(|p| p.exists())?;
    let ask = |what: &str| -> Option<Value> {
        let mut s = UnixStream::connect(&sock).ok()?;
        // (the old place is in /tmp, open to all: only our own Hyprland counts)
        let ours = rustix::net::sockopt::socket_peercred(&s).is_ok_and(|c| c.uid == rustix::process::getuid());
        if !ours {
            return None;
        }
        s.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
        s.write_all(format!("j/{what}").as_bytes()).ok()?;
        let mut out = Vec::new();
        s.take(8 << 20).read_to_end(&mut out).ok()?;
        serde_json::from_slice(&out).ok()
    };
    let active = ask("activewindow").as_ref().and_then(hyprland_window);
    let cursor = ask("cursorpos").and_then(|p| {
        let n = |k: &str| p.get(k)?.as_f64().filter(|v| v.abs() < 1e6);
        Some(Point::new(n("x")? as f32, n("y")? as f32))
    });
    // Every window on the workspaces shown; the active one alone if that
    // can't be had.
    let mut windows = match (ask("clients"), ask("monitors")) {
        (Some(clients), Some(monitors)) => parse_hyprland(&clients, &monitors),
        _ => Vec::new(),
    };
    if let Some(a) = active.as_ref().filter(|a| !windows.iter().any(|w| w.rect == a.rect && w.title == a.title)) {
        windows.insert(0, a.clone());
    }
    Some(Desktop { windows, active, cursor })
}

/// Hyprland's clients (`j/clients`) on the workspaces its monitors show
/// (`j/monitors`), topmost first as near as it says: a special workspace's
/// (shown over the rest), then fullscreen, floating and tiled ones, each
/// the most recently focused first.
pub fn parse_hyprland(clients: &Value, monitors: &Value) -> Vec<Window> {
    let shown: Vec<i64> = monitors
        .as_array()
        .into_iter()
        .flatten()
        .flat_map(|m| [m.pointer("/activeWorkspace/id"), m.pointer("/specialWorkspace/id")])
        .filter_map(|id| id?.as_i64())
        .filter(|id| *id != 0)
        .collect();
    let mut found: Vec<(u8, i64, Window)> = Vec::new();
    for c in clients.as_array().into_iter().flatten().take(4 * MAX_WINDOWS) {
        let workspace = c.pointer("/workspace/id").and_then(Value::as_i64);
        let flag = |k: &str| c.get(k).and_then(Value::as_bool);
        if flag("mapped") == Some(false) || flag("hidden") == Some(true) || !workspace.is_some_and(|w| shown.contains(&w)) {
            continue;
        }
        let Some(mut w) = hyprland_window(c) else { continue };
        w.own = c.get("pid").and_then(Value::as_u64) == Some(std::process::id() as u64);
        // (older versions say true or false, newer how: 0 is not)
        let fullscreen = c.get("fullscreen").is_some_and(|f| f.as_bool().unwrap_or(false) || f.as_i64().is_some_and(|n| n > 0));
        let layer = if workspace.is_some_and(|w| w < 0) {
            0
        } else if fullscreen {
            1
        } else if flag("floating") == Some(true) {
            2
        } else {
            3
        };
        let recent = c.get("focusHistoryID").and_then(Value::as_i64).unwrap_or(i64::MAX);
        found.push((layer, recent, w));
    }
    found.sort_by_key(|(layer, recent, _)| (*layer, *recent));
    found.into_iter().take(MAX_WINDOWS).map(|(_, _, w)| w).collect()
}

/// A Hyprland window (a client, or the active window) from its `at` and `size`.
fn hyprland_window(w: &Value) -> Option<Window> {
    let (at, size) = (w.get("at")?.as_array()?, w.get("size")?.as_array()?);
    let n = |v: Option<&Value>| v?.as_i64().filter(|v| v.abs() < 1 << 20).map(|v| v as i32);
    let rect = IRect::new(n(at.first())?, n(at.get(1))?, n(size.first())?, n(size.get(1))?);
    let s = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or("").chars().take(MAX_TITLE).collect::<String>();
    (rect.w > 0 && rect.h > 0).then(|| Window { rect, title: s("title"), app: s("class"), own: false })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kwin_report() {
        let d = parse_kwin(
            r#"{"windows": [[120, 140, 820, 560, "Notes — Kate", "kate"], [1060, 240, 225, 225, "QR card", "viewer", true], ["x"], [0, 0, -5, 10, "bad", ""]], "active": 0, "cursor": [300, 400]}"#,
        );
        assert_eq!(d.windows.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(), ["QR card", "Notes — Kate"]);
        assert_eq!(d.active.as_ref().map(|w| w.app.as_str()), Some("kate"));
        assert_eq!(d.windows.iter().map(|w| w.own).collect::<Vec<_>>(), [true, false], "whose windows are Flatshot's own");
        assert_eq!(d.cursor, Some(Point::new(300.0, 400.0)));
        assert_eq!(parse_kwin("not json"), Desktop::default());
        let long = format!(r#"{{"windows": [[0, 0, 10, 10, "{}", "a"]]}}"#, "x".repeat(5000));
        assert_eq!(parse_kwin(&long).windows[0].title.len(), MAX_TITLE);
    }

    #[test]
    fn sway_tree() {
        let tree: Value = serde_json::from_str(
            r#"{"type": "root", "rect": {"x": 0, "y": 0, "width": 3840, "height": 1080}, "nodes": [
                {"type": "output", "name": "DP-1", "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}, "nodes": [
                    {"type": "workspace", "name": "1", "visible": true, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                     "nodes": [{"type": "con", "pid": 10, "app_id": "foot", "name": "~", "visible": true, "focused": true, "rect": {"x": 0, "y": 0, "width": 960, "height": 1080}}],
                     "floating_nodes": [{"type": "floating_con", "pid": 11, "app_id": null, "window_properties": {"class": "Gimp"}, "name": "GIMP", "visible": true, "rect": {"x": 400, "y": 300, "width": 600, "height": 400}}]},
                    {"type": "workspace", "name": "2", "visible": false, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080},
                     "nodes": [{"type": "con", "pid": 12, "app_id": "code", "name": "elsewhere", "visible": false, "rect": {"x": 0, "y": 0, "width": 1920, "height": 1080}}]}]},
                {"type": "output", "name": "DP-2", "rect": {"x": 1920, "y": 0, "width": 1920, "height": 1080}, "nodes": [
                    {"type": "workspace", "name": "3", "visible": true, "rect": {"x": 1920, "y": 0, "width": 1920, "height": 1080},
                     "nodes": [{"type": "con", "name": "no pid: a split", "rect": {"x": 1920, "y": 0, "width": 1920, "height": 1080},
                                "nodes": [{"type": "con", "pid": 13, "app_id": "firefox", "name": "Web", "visible": true, "rect": {"x": 1920, "y": 0, "width": 1920, "height": 1080}}]}]}]}]}"#,
        )
        .unwrap();
        let d = parse_sway(&tree);
        // Floating first; hidden workspaces left out; containers aren't windows.
        assert_eq!(d.windows.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(), ["GIMP", "~", "Web"]);
        assert_eq!(d.windows[0].app, "Gimp");
        assert_eq!(d.active.as_ref().map(|w| w.app.as_str()), Some("foot"));
        assert_eq!(d.cursor, Some(Point::new(960.0, 540.0)), "the focused output's middle");
        assert_eq!(parse_sway(&Value::Null), Desktop::default());
    }

    #[test]
    fn hyprland_clients() {
        let clients: Value = serde_json::from_str(
            r#"[
            {"address": "0x1", "mapped": true, "hidden": false, "at": [10, 50], "size": [940, 1020], "workspace": {"id": 1, "name": "1"}, "floating": false, "class": "kitty", "title": "~", "pid": 1234, "fullscreen": 0, "focusHistoryID": 1},
            {"address": "0x2", "mapped": true, "hidden": false, "at": [970, 50], "size": [940, 1020], "workspace": {"id": 1, "name": "1"}, "floating": false, "class": "firefox", "title": "Web", "pid": 1300, "fullscreen": 0, "focusHistoryID": 0},
            {"address": "0x3", "mapped": true, "hidden": false, "at": [600, 300], "size": [500, 400], "workspace": {"id": 1, "name": "1"}, "floating": true, "class": "pavucontrol", "title": "Volume", "pid": 1400, "fullscreen": 0, "focusHistoryID": 2},
            {"address": "0x4", "mapped": true, "hidden": false, "at": [10, 50], "size": [1900, 1020], "workspace": {"id": 2, "name": "2"}, "floating": false, "class": "code", "title": "not shown", "pid": 1500, "fullscreen": 0, "focusHistoryID": 3},
            {"address": "0x5", "mapped": true, "hidden": false, "at": [2000, 100], "size": [800, 600], "workspace": {"id": -98, "name": "special:scratch"}, "floating": true, "class": "foot", "title": "scratch", "pid": 1600, "fullscreen": 0, "focusHistoryID": 4},
            {"address": "0x6", "mapped": true, "hidden": true, "at": [1930, 50], "size": [1880, 1020], "workspace": {"id": 5, "name": "5"}, "floating": false, "class": "tab", "title": "behind in its group", "pid": 1700, "fullscreen": 0, "focusHistoryID": 5},
            {"address": "0x7", "mapped": true, "hidden": false, "at": [1930, 50], "size": [1880, 1020], "workspace": {"id": 5, "name": "5"}, "floating": false, "class": "mpv", "title": "video", "pid": 1800, "fullscreen": 2, "focusHistoryID": 6},
            {"address": "0x8", "mapped": false, "hidden": false, "at": [0, 0], "size": [100, 100], "workspace": {"id": 1, "name": "1"}, "class": "x", "title": "unmapped", "focusHistoryID": 7},
            {"address": "0x9", "mapped": true, "hidden": false, "at": [0, 0], "size": [0, 100], "workspace": {"id": 1, "name": "1"}, "class": "x", "title": "empty"}
            ]"#,
        )
        .unwrap();
        let monitors: Value = serde_json::from_str(
            r#"[{"id": 0, "name": "DP-1", "x": 0, "y": 0, "activeWorkspace": {"id": 1, "name": "1"}, "specialWorkspace": {"id": 0, "name": ""}},
                {"id": 1, "name": "DP-2", "x": 1920, "y": 0, "activeWorkspace": {"id": 5, "name": "5"}, "specialWorkspace": {"id": -98, "name": "special:scratch"}}]"#,
        )
        .unwrap();
        let w = parse_hyprland(&clients, &monitors);
        // The special workspace over all, then fullscreen, floating, and the tiled most recently focused first.
        assert_eq!(w.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(), ["scratch", "video", "Volume", "Web", "~"]);
        assert_eq!((w[3].rect, w[3].app.as_str()), (IRect::new(970, 50, 940, 1020), "firefox"));
        // (an older version's fullscreen: true)
        let old: Value = serde_json::from_str(r#"[{"at": [0, 0], "size": [10, 10], "workspace": {"id": 1}, "title": "a", "focusHistoryID": 1},
                                                 {"at": [0, 0], "size": [10, 10], "workspace": {"id": 1}, "title": "b", "fullscreen": true, "focusHistoryID": 2}]"#).unwrap();
        assert_eq!(parse_hyprland(&old, &monitors).iter().map(|w| w.title.as_str()).collect::<Vec<_>>(), ["b", "a"]);
        assert!(parse_hyprland(&Value::Null, &monitors).is_empty() && parse_hyprland(&clients, &Value::Null).is_empty());
    }
}
