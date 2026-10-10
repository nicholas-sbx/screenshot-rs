//! The desktop at capture time: the windows (for hover-and-click window
//! capture), the active one and where the pointer is (for the instant
//! "active window" and "current monitor" captures). Global logical pixels.
//!
//! Wayland gives programs no window list, so on KDE a small KWin script
//! (loaded over KWin's scripting D-Bus API) reads the stacking order and
//! calls back with every visible window. Sway and Hyprland answer through
//! their IPC sockets.

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
    var act = workspace.activeWindow || workspace.activeClient;
    var active = -1;
    for (var i = 0; i < list.length; i++) {
        var w = list[i];
        try {
            if (!(w.normalWindow || w.dialog) || w.minimized || w.deleted || w.hidden) continue;
            if (!onCurrent(w)) continue;
            var g = w.frameGeometry;
            if (!g || g.width < 8 || g.height < 8) continue;
            if (act && (w === act || String(w.internalId) === String(act.internalId))) active = out.length;
            out.push([Math.round(g.x), Math.round(g.y), Math.round(g.width), Math.round(g.height),
                      String(w.caption || ""), String(w.resourceClass || "")]);
        } catch (e) {}
    }
    var cursor = null;
    try { cursor = [Math.round(workspace.cursorPos.x), Math.round(workspace.cursorPos.y)]; } catch (e) {}
    callDBus("%SERVICE%", "%PATH%", "org.flatshot.Windows", "report",
             JSON.stringify({windows: out, active: active, cursor: cursor}));
})();
"#;

/// Ask whichever compositor this is; None if none can answer in `timeout`.
pub fn query(timeout: Duration) -> Option<Desktop> {
    if std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() {
        return hyprland();
    }
    if let Some(sock) = std::env::var_os("SWAYSOCK") {
        return sway(std::path::Path::new(&sock));
    }
    kwin(timeout)
}

/// Is there a compositor to ask (without asking)?
pub fn supported() -> bool {
    std::env::var_os("HYPRLAND_INSTANCE_SIGNATURE").is_some() || std::env::var_os("SWAYSOCK").is_some() || crate::dbus::has_owner(KWIN)
}

// -- KWin -----------------------------------------------------------------------------

/// Where the script's report arrives.
struct Report {
    tx: std::sync::Mutex<Option<mpsc::Sender<String>>>,
    kwin: String,
}

#[zbus::interface(name = "org.flatshot.Windows")]
impl Report {
    fn report(&self, #[zbus(header)] hdr: zbus::message::Header<'_>, json: String) {
        // Only KWin's word counts: anyone on the bus could call this.
        if hdr.sender().map(|s| s.as_str()) != Some(self.kwin.as_str()) {
            return;
        }
        if let Some(tx) = self.tx.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = tx.send(json);
        }
    }
}

fn kwin(timeout: Duration) -> Option<Desktop> {
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
    let (tx, rx) = mpsc::channel();
    let report = Report { tx: std::sync::Mutex::new(Some(tx)), kwin: owner };
    conn.object_server().at(path.as_str(), report).ok()?;
    // The script lives in our private folder: nobody else can change it
    // before KWin reads it.
    let dir = crate::ipc::runtime_dir().ok()?;
    let file = dir.join(format!("windows-{token}.js"));
    let script = SCRIPT.replace("%SERVICE%", &me).replace("%PATH%", &path);
    let name = format!("flatshot-windows-{token}");
    let result = (|| {
        write_private(&file, &script).ok()?;
        conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "loadScript", &(file.to_string_lossy().as_ref(), name.as_str())).ok()?;
        conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "start", &()).ok()?;
        rx.recv_timeout(timeout).ok()
    })();
    let _ = conn.call_method(Some(KWIN), "/Scripting", Some("org.kde.kwin.Scripting"), "unloadScript", &(name.as_str(),));
    let _ = std::fs::remove_file(&file);
    let _ = conn.object_server().remove::<Report, _>(path.as_str());
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
        let x = c.first()?.as_f64()?;
        let y = c.get(1)?.as_f64()?;
        (x.is_finite() && y.is_finite()).then(|| Point::new(x as f32, y as f32))
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
    Some(Window { rect: IRect::new(x, y, w, h), title: s(4), app: s(5) })
}

// -- sway ------------------------------------------------------------------------------

/// Sway's tree over its IPC socket (i3's protocol): the focused window, the
/// visible ones (for the overlay's hover), and the focused output's middle
/// for the pointer (sway can't be asked where the pointer is).
fn sway(sock: &std::path::Path) -> Option<Desktop> {
    let tree = i3_ipc(sock, 4, b"")?;
    let tree: Value = serde_json::from_slice(&tree).ok()?;
    let mut windows = Vec::new();
    let mut active = None;
    let mut cursor = None;
    // Visible workspaces' windows: floating ones above the tiled.
    fn walk(node: &Value, visible: bool, output: Option<&Value>, out: &mut Vec<(bool, Window)>, active: &mut Option<Window>, cursor: &mut Option<Point>) {
        let kind = node.get("type").and_then(|t| t.as_str()).unwrap_or("");
        let output = if kind == "output" { Some(node) } else { output };
        // (windows say whether they're visible: on a shown workspace, not hidden in a stack)
        let visible = node.get("visible").and_then(|v| v.as_bool()).unwrap_or(visible);
        let r = node.get("rect");
        let rect = r.and_then(|r| {
            let g = |k: &str| r.get(k)?.as_i64().map(|v| v as i32);
            Some(IRect::new(g("x")?, g("y")?, g("width")?, g("height")?))
        });
        let is_window = (kind == "con" || kind == "floating_con") && node.get("pid").is_some();
        if is_window && let Some(rect) = rect.filter(|r| r.w > 0 && r.h > 0) {
            let app = node.get("app_id").and_then(|v| v.as_str()).or_else(|| node.pointer("/window_properties/class").and_then(|v| v.as_str())).unwrap_or("");
            let title = node.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let w = Window { rect, title: title.chars().take(MAX_TITLE).collect(), app: app.chars().take(MAX_TITLE).collect() };
            if node.get("focused").and_then(|v| v.as_bool()).unwrap_or(false) {
                *active = Some(w.clone());
                if let Some(o) = output.and_then(|o| o.get("rect")) {
                    let g = |k: &str| o.get(k).and_then(|v| v.as_i64()).unwrap_or(0) as f32;
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
    walk(&tree, false, None, &mut found, &mut active, &mut cursor);
    let _ = &mut windows;
    // Topmost first: floating windows, then the tiled.
    found.sort_by_key(|(floating, _)| !*floating);
    windows.extend(found.into_iter().map(|(_, w)| w));
    Some(Desktop { windows, active, cursor })
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
    let win = ask("activewindow");
    let active = win.as_ref().and_then(|w| {
        let at = w.get("at")?.as_array()?;
        let size = w.get("size")?.as_array()?;
        let rect = IRect::new(at.first()?.as_i64()? as i32, at.get(1)?.as_i64()? as i32, size.first()?.as_i64()? as i32, size.get(1)?.as_i64()? as i32);
        let s = |k: &str| w.get(k).and_then(|v| v.as_str()).unwrap_or("").chars().take(MAX_TITLE).collect::<String>();
        (rect.w > 0 && rect.h > 0).then(|| Window { rect, title: s("title"), app: s("class") })
    });
    let cursor = ask("cursorpos").and_then(|p| Some(Point::new(p.get("x")?.as_f64()? as f32, p.get("y")?.as_f64()? as f32)));
    Some(Desktop { windows: active.iter().cloned().collect(), active, cursor })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kwin_report() {
        let d = parse_kwin(
            r#"{"windows": [[120, 140, 820, 560, "Notes — Kate", "kate"], [1060, 240, 225, 225, "QR card", "viewer"], ["x"], [0, 0, -5, 10, "bad", ""]], "active": 0, "cursor": [300, 400]}"#,
        );
        assert_eq!(d.windows.iter().map(|w| w.title.as_str()).collect::<Vec<_>>(), ["QR card", "Notes — Kate"]);
        assert_eq!(d.active.as_ref().map(|w| w.app.as_str()), Some("kate"));
        assert_eq!(d.cursor, Some(Point::new(300.0, 400.0)));
        assert_eq!(parse_kwin("not json"), Desktop::default());
        let long = format!(r#"{{"windows": [[0, 0, 10, 10, "{}", "a"]]}}"#, "x".repeat(5000));
        assert_eq!(parse_kwin(&long).windows[0].title.len(), MAX_TITLE);
    }
}
