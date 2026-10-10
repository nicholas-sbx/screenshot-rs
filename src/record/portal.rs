//! The screen-cast portal (org.freedesktop.portal.ScreenCast): a PipeWire
//! stream of a monitor, or of the whole desktop, shared with the user's
//! consent. One session per recording, on a D-Bus connection of its own
//! (the session lasts as long as the connection that asked for it).

use std::collections::HashMap;
use std::os::fd::OwnedFd;
use std::process::Stdio;
use std::sync::Mutex;
use std::time::Duration;

use zbus::zvariant::{OwnedObjectPath, OwnedValue, Value};

use super::Target;
use crate::config;
use crate::desktop::portal::{self, Answer};
use crate::geom::IRect;
use crate::util;

const SCREENCAST: &str = "org.freedesktop.portal.ScreenCast";
// SelectSources: source types and cursor modes
const MONITOR: u32 = 1;
const CURSOR_HIDDEN: u32 = 1;
const CURSOR_EMBEDDED: u32 = 2;
const PERSIST_UNTIL_REVOKED: u32 = 2;
/// The restore token for sharing all the monitors (an area across them).
const WORKSPACE: &str = "workspace";
/// How long the desktop may take to ask the user which screen to share.
const START_WAIT: Duration = Duration::from_secs(600);
const CALL_WAIT: Duration = Duration::from_secs(10);

/// Why there is no stream.
#[derive(Debug)]
pub enum Failure {
    /// the user said no in the desktop's dialog
    Cancelled,
    Failed(String),
}

impl From<String> for Failure {
    fn from(s: String) -> Failure {
        Failure::Failed(s)
    }
}

/// A screen cast: what it shows, and how to read it.
pub struct ScreenCast {
    conn: zbus::blocking::Connection,
    session: Mutex<Option<OwnedObjectPath>>,
    /// the stream's PipeWire node
    pub node: u32,
    /// what it shows (global logical pixels), if known
    pub stream_rect: Option<IRect>,
    /// the portal's own position and size, for the log
    pub reported: String,
    /// the stream's pixels, as measured
    pub frame_size: Option<(u32, u32)>,
}

fn answer(a: Answer) -> Result<HashMap<String, OwnedValue>, Failure> {
    match a {
        Answer::Done(r) => Ok(r),
        Answer::Cancelled => Err(Failure::Cancelled),
    }
}

fn token() -> String {
    format!("flatshot{}", util::random_hex(6))
}

fn screens(target: &Target) -> Vec<IRect> {
    target.monitors.iter().map(|m| m.1).collect()
}

impl ScreenCast {
    /// Ask for a stream with `target`'s area in it. Blocks while the desktop
    /// asks the user (call it from a thread of its own).
    pub fn open(target: &Target, cursor: bool) -> Result<ScreenCast, Failure> {
        let conn = zbus::blocking::connection::Builder::session()
            .and_then(|b| b.method_timeout(CALL_WAIT).build())
            .map_err(|e| Failure::Failed(format!("no session bus: {e}")))?;
        let property = |name: &str| -> Option<u32> {
            let reply = conn.call_method(Some(portal::DESKTOP), portal::PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(SCREENCAST, name)).ok()?;
            let v: OwnedValue = reply.body().deserialize().ok()?;
            u32::try_from(&v).ok()
        };
        let version = property("version").ok_or_else(|| Failure::Failed("the desktop has no screen-cast portal".into()))?;
        let cursors = property("AvailableCursorModes").unwrap_or(CURSOR_HIDDEN);

        let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
        options.insert("session_handle_token", Value::from(token()));
        let results = answer(portal::request_on(&conn, SCREENCAST, "CreateSession", options, |o| (o,), CALL_WAIT)?)?;
        let session = results
            .get("session_handle")
            .and_then(|v| {
                let v = v.try_clone().ok()?;
                String::try_from(v.try_clone().ok()?).ok().or_else(|| OwnedObjectPath::try_from(v).ok().map(|p| p.to_string()))
            })
            .and_then(|s| OwnedObjectPath::try_from(s).ok())
            .ok_or_else(|| Failure::Failed("the screen-cast portal made no session".into()))?;
        let mut cast = ScreenCast { conn, session: Mutex::new(Some(session.clone())), node: 0, stream_rect: None, reported: String::new(), frame_size: None };

        // Which monitor: the restore token remembers what was shared last
        // time for this kind of area (its monitor, or all of them).
        let key = if target.spans() { WORKSPACE.to_string() } else { target.monitor_name.clone() };
        let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
        options.insert("types", Value::U32(MONITOR));
        options.insert("multiple", Value::Bool(false));
        let wanted = if cursor { CURSOR_EMBEDDED } else { CURSOR_HIDDEN };
        if cursors & wanted != 0 {
            options.insert("cursor_mode", Value::U32(wanted));
        }
        if version >= 4 {
            options.insert("persist_mode", Value::U32(PERSIST_UNTIL_REVOKED));
            let tokens = config::load_state().get("screencast_tokens").cloned();
            if let Some(t) = tokens.as_ref().and_then(|t| t.get(&key)).and_then(|t| t.as_str()).filter(|t| !t.is_empty()) {
                options.insert("restore_token", Value::from(t.to_string()));
            }
        }
        let path = session.clone();
        answer(portal::request_on(&cast.conn, SCREENCAST, "SelectSources", options, move |o| (path, o), CALL_WAIT)?)?;
        // This waits for the user when the desktop asks which screen to share.
        let path = session.clone();
        let results = answer(portal::request_on(&cast.conn, SCREENCAST, "Start", HashMap::new(), move |o| (path, "", o), START_WAIT)?)?;
        type Stream = (u32, HashMap<String, OwnedValue>);
        let streams: Vec<Stream> = results.get("streams").and_then(|v| v.try_clone().ok()).and_then(|v| Vec::<Stream>::try_from(v).ok()).unwrap_or_default();
        let (node, props) = streams.into_iter().next().ok_or_else(|| Failure::Failed("the screen-cast portal shared no screen".into()))?;
        cast.node = node;
        let pair = |name: &str| -> Option<(i32, i32)> { props.get(name).and_then(|v| v.try_clone().ok()).and_then(|v| <(i32, i32)>::try_from(v).ok()) };
        if let (Some(pos), Some(size)) = (pair("position"), pair("size")) {
            cast.reported = format!("position {},{} size {}x{}", pos.0, pos.1, size.0, size.1);
            cast.stream_rect = Some(super::stream_shows(pos, size, target.scale, &screens(target)));
        }
        // Whatever the desktop says (or doesn't), the stream's real size counts.
        cast.frame_size = cast.measure();
        if let Some(size) = cast.frame_size {
            cast.stream_rect = super::stream_measured(size, cast.stream_rect, target.rect, target.scale, &screens(target)).or(cast.stream_rect);
        }
        let whole = super::workspace(&target.monitors);
        let shared = cast.shared_monitor(target);
        let all = target.monitors.len() > 1 && cast.stream_rect == Some(whole);
        if let Some(t) = results.get("restore_token").and_then(|v| v.try_clone().ok()).and_then(|v| String::try_from(v).ok())
            && (shared.is_some() || all)
        {
            let mut tokens = config::load_state().get("screencast_tokens").and_then(|t| t.as_object().cloned()).unwrap_or_default();
            let at = if all { WORKSPACE.to_string() } else { shared.clone().unwrap_or_default() };
            tokens.insert(at, serde_json::Value::from(t));
            config::update_state(vec![("screencast_tokens", serde_json::Value::Object(tokens))]);
        }
        // A stream of the whole workspace is fine too, as long as the area is in it.
        let shows_area = match cast.stream_rect {
            Some(r) => r.contains_rect(&target.rect),
            None => shared.as_ref().is_none_or(|s| *s == target.monitor_name),
        };
        if !shows_area {
            let what = shared.unwrap_or_else(|| "a different part of the desktop".into());
            let name = &target.monitor_name;
            if target.spans() {
                let on: Vec<&str> = target.monitors.iter().filter(|m| m.1.intersects(&target.rect)).map(|m| m.0.as_str()).collect();
                return Err(Failure::Failed(format!(
                    "The shared screen is {what}, but the area crosses {}. Record again and share the full workspace.",
                    on.join(", ")
                )));
            }
            return Err(Failure::Failed(format!("The shared screen is {what}, but the area is on {name}. Record again and share {name}.")));
        }
        Ok(cast)
    }

    /// The monitor the stream shows (None: all of them, or not known).
    fn shared_monitor(&self, target: &Target) -> Option<String> {
        match self.stream_rect {
            Some(r) => {
                if target.monitors.len() > 1 && r == super::workspace(&target.monitors) {
                    return None;
                }
                target.monitors.iter().find(|m| (m.1.x, m.1.y) == (r.x, r.y)).map(|m| m.0.clone())
            }
            None if target.monitors.len() == 1 => Some(target.monitors[0].0.clone()),
            None => Some(target.monitor_name.clone()),
        }
    }

    /// A new PipeWire connection for the stream (one per segment).
    pub fn pipewire_fd(&self) -> Result<OwnedFd, String> {
        let session = self.session.lock().map_err(|_| "the screen cast is gone")?.clone().ok_or("the screen cast is over")?;
        let options: HashMap<&str, Value<'_>> = HashMap::new();
        let reply = self
            .conn
            .call_method(Some(portal::DESKTOP), portal::PATH, Some(SCREENCAST), "OpenPipeWireRemote", &(session, options))
            .map_err(|e| format!("the screen cast's stream: {e}"))?;
        let fd: zbus::zvariant::OwnedFd = reply.body().deserialize().map_err(|e| format!("the screen cast's stream: {e}"))?;
        let fd: OwnedFd = fd.into();
        // (D-Bus hands descriptors over without close-on-exec: no program
        // started but the recorder may have this one)
        let _ = rustix::io::fcntl_setfd(&fd, rustix::io::FdFlags::CLOEXEC);
        Ok(fd)
    }

    /// The stream's size in pixels, from GStreamer's own negotiation (a
    /// moment's work: it connects, reads the format and stops). It asks for
    /// raw video, as the recorder does: offered anything, the stream may
    /// not be linked at all.
    fn measure(&self) -> Option<(u32, u32)> {
        if !util::have("gst-launch-1.0") {
            return None;
        }
        let fd = self.pipewire_fd().ok()?;
        let argv: Vec<String> =
            ["gst-launch-1.0", "-v", "pipewiresrc", "fd=3", &format!("path={}", self.node), "num-buffers=1", "!", "videoconvert", "!", "fakesink"]
                .iter()
                .map(|s| s.to_string())
                .collect();
        let mut child = super::spawn(&argv, Some(&fd), Stdio::piped(), Stdio::null()).ok()?;
        drop(fd);
        let mut out = child.stdout.take()?;
        let reader = std::thread::spawn(move || {
            use std::io::Read;
            let mut text = Vec::new();
            let _ = (&mut out).take(1 << 20).read_to_end(&mut text);
            text
        });
        super::wait_for(&mut child, Duration::from_secs(4));
        let text = String::from_utf8_lossy(&reader.join().ok()?).into_owned();
        caps_size(&text)
    }

    /// Done with the stream.
    pub fn close(&self) {
        let Ok(mut session) = self.session.lock() else { return };
        if let Some(path) = session.take() {
            let _ = self.conn.call_method(Some(portal::DESKTOP), path.as_str(), Some("org.freedesktop.portal.Session"), "Close", &());
        }
    }
}

impl Drop for ScreenCast {
    fn drop(&mut self) {
        self.close();
    }
}

/// The stream's size in gst-launch's verbose output: the caps of
/// pipewiresrc's source pad.
fn caps_size(text: &str) -> Option<(u32, u32)> {
    let number = |line: &str, key: &str| -> Option<u32> {
        let at = line.find(key)? + key.len();
        line[at..].chars().take_while(|c| c.is_ascii_digit()).collect::<String>().parse().ok()
    };
    text.lines()
        .filter(|l| l.contains("pipewiresrc") && l.contains(".GstPad:src: caps = video/x-raw"))
        .find_map(|l| Some((number(l, "width=(int)")?, number(l, "height=(int)")?)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stream_size_from_gst_output() {
        let out = "Setting pipeline to PAUSED ...\n/GstPipeline:pipeline0/GstPipeWireSrc:pipewiresrc0.GstPad:src: caps = video/x-raw, format=(string)BGRx, width=(int)2560, height=(int)1440, framerate=(fraction)0/1\n";
        assert_eq!(caps_size(out), Some((2560, 1440)));
        assert_eq!(caps_size("nothing"), None);
    }
}
