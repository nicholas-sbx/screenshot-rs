//! Desktop notifications (org.freedesktop.Notifications), with buttons in
//! the tray app (which stays running to hear the click); notify-send when
//! there's no session bus.
//!
//! Each notification is one of the events in flatshot.notifyrc, which KDE's
//! System Settings > Notifications > Flatshot lists to choose, per event,
//! whether it pops up, plays a sound or runs a command. KDE leaves doing
//! that to the app (KNotification does it in C++ apps), so the choices
//! (~/.config/flatshot.notifyrc) are read and done here.
//!
//! What a notification shows is escaped where the desktop reads markup: a
//! file's name or a code's text is shown as written, never as a link or an
//! image.

use std::collections::HashMap;
use std::ffi::OsStr;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, mpsc};
use std::time::Duration;

use zbus::Variant;

use super::sound;
use crate::ui::session::Waker;
use crate::util;

const SERVICE: &str = "org.freedesktop.Notifications";
const PATH: &str = "/org/freedesktop/Notifications";
/// Flatshot's events and what they do unless the user changes it.
pub const NOTIFYRC: &str = include_str!("../../assets/flatshot-rs.notifyrc");
/// Its file name, in knotifications6 (and the user's changes in ~/.config).
const NOTIFYRC_NAME: &str = "flatshot-rs.notifyrc";
/// The most text a notification's body shows.
const MAX_BODY: usize = 1000;
/// The most notifications whose buttons are remembered.
const MAX_REMEMBERED: usize = 64;

/// A notification.
#[derive(Clone, Debug, Default)]
pub struct Note {
    pub summary: String,
    pub body: String,
    /// shown as its thumbnail
    pub image: Option<PathBuf>,
    /// what Plasma offers to drag out (default: `image`)
    pub file: Option<PathBuf>,
    /// (key, label); "default" is a click on the bubble itself. Only shown
    /// where something stays to hear the click.
    pub actions: Vec<(String, String)>,
    pub urgent: bool,
    /// which of flatshot.notifyrc's events it is
    pub event: &'static str,
}

/// What KDE's settings say an event does.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct EventSettings {
    pub popup: bool,
    pub sound: Option<String>,
    pub command: Option<String>,
}

/// `event`'s settings: Flatshot's defaults, with the user's changes over them.
pub fn event_settings(event: &str) -> EventSettings {
    let user = util::read_limited(&crate::config::config_home().join(NOTIFYRC_NAME), 256 << 10).unwrap_or_default();
    settings_from(&[NOTIFYRC, &String::from_utf8_lossy(&user)], event)
}

fn settings_from(files: &[&str], event: &str) -> EventSettings {
    let section = format!("Event/{event}");
    let mut values: HashMap<String, String> = HashMap::new();
    for text in files {
        values.extend(ini_section(text, &section));
    }
    let action = values.get("Action").map(String::as_str).unwrap_or("Popup");
    let actions: Vec<&str> = action.split('|').map(str::trim).collect();
    let value = |key: &str| values.get(key).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    EventSettings {
        popup: actions.contains(&"Popup"),
        sound: if actions.contains(&"Sound") { value("Sound") } else { None },
        command: if actions.contains(&"Execute") { value("Execute") } else { None },
    }
}

/// The keys and values of one `[section]` of an INI file.
fn ini_section(text: &str, section: &str) -> Vec<(String, String)> {
    let mut inside = false;
    let mut out = Vec::new();
    for line in text.lines().map(str::trim) {
        if line.is_empty() || line.starts_with('#') || line.starts_with(';') {
            continue;
        }
        if let Some(name) = line.strip_prefix('[').and_then(|l| l.strip_suffix(']')) {
            inside = name == section;
        } else if inside && let Some((k, v)) = line.split_once('=') {
            out.push((k.trim().to_string(), v.trim().to_string()));
        }
    }
    out
}

/// Make the events known to KDE's notification settings: the packages
/// install flatshot-rs.notifyrc; anywhere else (cargo install, a copied
/// binary), a copy goes in ~/.local/share.
pub fn install_events() {
    for version in ["knotifications6", "knotifications5"] {
        let present = crate::config::data_dirs()
            .iter()
            .any(|d| util::read_limited(&d.join(version).join(NOTIFYRC_NAME), 64 << 10).is_ok_and(|t| t == NOTIFYRC.as_bytes()));
        if present {
            continue;
        }
        let target = crate::config::data_home().join(version).join(NOTIFYRC_NAME);
        let written = target.parent().map_or(Ok(()), std::fs::create_dir_all).and_then(|()| std::fs::write(&target, NOTIFYRC));
        if let Err(e) = written {
            crate::warn!("could not install the notification events: {e}");
        }
    }
}

/// Does the notification server read markup in the body? (Asked once.)
fn reads_markup(conn: &crate::dbus::Bus) -> bool {
    static MARKUP: OnceLock<bool> = OnceLock::new();
    *MARKUP.get_or_init(|| {
        let caps = conn.call_method(Some(SERVICE), PATH, Some(SERVICE), "GetCapabilities", &());
        match caps.ok().and_then(|r| r.body().deserialize::<Vec<String>>().ok()) {
            Some(caps) => caps.iter().any(|c| c == "body-markup"),
            None => true, // (unknown: escape anyway)
        }
    })
}

/// Text shown as it is where markup is read.
pub fn escape_markup(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        match c {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&#39;"),
            _ => out.push(c),
        }
    }
    out
}

/// Do what `note`'s event is set to do (pop up, play a sound, run a
/// command), now. Returns the notification's id when it popped up over
/// D-Bus. `with_actions`: show its buttons (someone is listening).
pub fn send_now(note: &Note, with_actions: bool) -> Option<u32> {
    let chosen = event_settings(note.event);
    if let Some(name) = &chosen.sound {
        let file = sound::find(name).map(|p| p.to_string_lossy().into_owned()).unwrap_or_default();
        sound::play(&file);
    }
    if let Some(command) = &chosen.command {
        util::spawn_detached(&["/bin/sh".as_ref(), "-c".as_ref(), OsStr::new(command)], &[]);
    }
    if !chosen.popup {
        return None;
    }
    match notify(note, with_actions) {
        Ok(id) => Some(id),
        Err(e) => {
            crate::timing::log(format!("notification over D-Bus failed ({e}); trying notify-send"));
            notify_send(note);
            None
        }
    }
}

fn notify(note: &Note, with_actions: bool) -> Result<u32, String> {
    let conn = crate::dbus::session()?;
    let body = util::elide(&note.body, MAX_BODY);
    let body = if reads_markup(conn) { escape_markup(&body) } else { body };
    let mut hints: HashMap<&str, Variant<'_>> = HashMap::new();
    hints.insert("desktop-entry", Variant::from(crate::APP_ID));
    hints.insert("x-kde-eventId", Variant::from(note.event));
    if note.urgent {
        hints.insert("urgency", Variant::U8(2));
    }
    if let Some(image) = &note.image {
        hints.insert("image-path", Variant::from(util::file_uri(image)));
    }
    if let Some(file) = note.file.as_ref().or(note.image.as_ref()) {
        // Plasma: a thumbnail, and the file to drag into other programs.
        hints.insert("x-kde-urls", Variant::from(vec![util::file_uri(file)]));
    }
    let actions: Vec<&str> = if with_actions { note.actions.iter().flat_map(|(k, l)| [k.as_str(), l.as_str()]).collect() } else { Vec::new() };
    let reply = conn
        .call_method(
            Some(SERVICE),
            PATH,
            Some(SERVICE),
            "Notify",
            &(crate::APP_NAME, 0u32, crate::APP_ID, note.summary.as_str(), body.as_str(), actions, hints, -1i32),
        )
        .map_err(|e| e.to_string())?;
    reply.body().deserialize::<u32>().map_err(|e| e.to_string())
}

fn notify_send(note: &Note) {
    if !util::have("notify-send") {
        return;
    }
    // (notify-send's body is markup too, wherever the desktop reads it)
    let body = escape_markup(&util::elide(&note.body, MAX_BODY));
    let mut argv: Vec<String> = vec!["notify-send".into(), "-a".into(), crate::APP_NAME.into(), "-i".into(), crate::APP_ID.into()];
    if note.urgent {
        argv.extend(["-u".into(), "critical".into()]);
    }
    if let Some(image) = &note.image {
        argv.extend(["-h".into(), format!("string:image-path:{}", util::file_uri(image))]);
    }
    // ("--": a body starting with "-" is text, not an option)
    argv.extend(["--".into(), note.summary.clone(), body]);
    let argv: Vec<&str> = argv.iter().map(String::as_str).collect();
    util::spawn(&argv);
}

/// A click on a notification's button (or the notification going away).
#[derive(Clone, Debug, PartialEq)]
pub enum NoteEvent {
    /// (token, the button's key)
    Action(u64, String),
    Closed(u64),
}

/// What the notifications' thread is asked.
enum Job {
    Send(Note, Option<u64>),
    /// say when everything before this is sent
    Flush(mpsc::Sender<()>),
}

/// Notifications from the tray app: sent from a thread of their own (a
/// busy desktop never holds up the next capture), with clicks on their
/// buttons reported back.
pub struct Notifier {
    jobs: mpsc::Sender<Job>,
    /// notification id → token of the notifications with buttons
    ids: Arc<Mutex<HashMap<u32, u64>>>,
    listening: bool,
    next_token: u64,
}

impl Notifier {
    /// Clicks come on the returned channel, with `wake` called after each.
    pub fn start(wake: Waker) -> (Notifier, mpsc::Receiver<NoteEvent>) {
        let (events_tx, events) = mpsc::channel();
        let ids: Arc<Mutex<HashMap<u32, u64>>> = Arc::default();
        let listening = listen(ids.clone(), events_tx, wake);
        let (jobs, rx) = mpsc::channel::<Job>();
        let worker_ids = ids.clone();
        let _ = std::thread::Builder::new().name("flatshot-notify".into()).spawn(move || {
            for job in rx {
                let (note, token) = match job {
                    Job::Send(note, token) => (note, token),
                    Job::Flush(done) => {
                        let _ = done.send(());
                        continue;
                    }
                };
                let id = send_now(&note, token.is_some());
                if let (Some(id), Some(token)) = (id, token)
                    && let Ok(mut ids) = worker_ids.lock()
                {
                    if ids.len() >= MAX_REMEMBERED
                        && let Some(oldest) = ids.iter().min_by_key(|(_, t)| **t).map(|(i, _)| *i)
                    {
                        ids.remove(&oldest);
                    }
                    ids.insert(id, token);
                }
            }
        });
        (Notifier { jobs, ids, listening, next_token: 1 }, events)
    }

    /// Can a notification's buttons be used?
    pub fn listening(&self) -> bool {
        self.listening
    }

    /// Send `note`. Returns the token its clicks will be reported with, if
    /// it has buttons (None: it's shown without).
    pub fn send(&mut self, mut note: Note) -> Option<u64> {
        let token = (self.listening && !note.actions.is_empty()).then(|| {
            self.next_token += 1;
            self.next_token
        });
        if token.is_none() {
            note.actions.clear();
        }
        let _ = self.jobs.send(Job::Send(note, token));
        token
    }

    /// Wait (up to `limit`) for the notifications sent so far to be shown:
    /// before the tray app quits.
    pub fn flush(&self, limit: Duration) {
        let (tx, rx) = mpsc::channel();
        if self.jobs.send(Job::Flush(tx)).is_ok() {
            let _ = rx.recv_timeout(limit);
        }
    }

    /// The tokens still remembered (the others' buttons do nothing now).
    pub fn live_tokens(&self) -> Vec<u64> {
        self.ids.lock().map(|ids| ids.values().copied().collect()).unwrap_or_default()
    }
}

/// Hear the notification server's ActionInvoked and NotificationClosed for
/// our notifications, on a thread of its own. False if that can't be done.
fn listen(ids: Arc<Mutex<HashMap<u32, u64>>>, events: mpsc::Sender<NoteEvent>, wake: Waker) -> bool {
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(SERVICE).interface(SERVICE).path(PATH).build();
    let Ok(rule) = rule else { return false };
    // (only the notification server's own signals come through)
    let listening = crate::dbus::listen(SERVICE, rule, "flatshot-notify-events", move |msg| {
        let event = match msg.header().member().map(|m| m.as_str()) {
            Some("ActionInvoked") => match msg.body().deserialize::<(u32, String)>() {
                Ok((id, key)) => ids.lock().ok().and_then(|ids| ids.get(&id).copied()).map(|t| NoteEvent::Action(t, key)),
                Err(_) => None,
            },
            Some("NotificationClosed") => match msg.body().deserialize::<(u32, u32)>() {
                Ok((id, _)) => ids.lock().ok().and_then(|mut ids| ids.remove(&id)).map(NoteEvent::Closed),
                Err(_) => None,
            },
            _ => None,
        };
        if let Some(e) = event {
            if events.send(e).is_err() {
                return false;
            }
            wake();
        }
        true
    });
    listening.is_ok()
}

/// A path for a notification's text: its file name.
pub fn file_name(path: &Path) -> String {
    path.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn events_follow_kde_settings() {
        let d = settings_from(&[NOTIFYRC], "screenshot");
        assert_eq!(d, EventSettings { popup: true, sound: None, command: None });
        let user = "[Event/screenshot]\nAction=Sound|Execute\nSound=camera-shutter\nExecute=touch /tmp/x\n[Event/failed]\nAction=None\n";
        let s = settings_from(&[NOTIFYRC, user], "screenshot");
        assert_eq!(s, EventSettings { popup: false, sound: Some("camera-shutter".into()), command: Some("touch /tmp/x".into()) });
        assert!(!settings_from(&[NOTIFYRC, user], "failed").popup);
        assert!(settings_from(&[NOTIFYRC, user], "copied").popup);
        assert!(settings_from(&[""], "unknown").popup);
    }

    #[test]
    fn markup_is_escaped() {
        assert_eq!(escape_markup("<a href='x'>&</a>"), "&lt;a href=&#39;x&#39;&gt;&amp;&lt;/a&gt;");
        assert_eq!(escape_markup("plain · text"), "plain · text");
    }
}
