//! Global shortcuts through the desktop's portal (org.freedesktop.portal.
//! GlobalShortcuts), for desktops without KDE's KGlobalAccel: GNOME (48 and
//! later) shows a dialog of its own to confirm or change the keys; on
//! Hyprland they're bound in its config, to the names the portal gives
//! them (`hyprctl globalshortcuts` lists them). Only asked for when
//! Settings → Shortcuts says so, so that dialog never comes as a surprise.
//!
//! A session lasts as long as the connection that made it, so it has a
//! connection of its own: saying who we are on it (the portal's host
//! registry) leaves every other portal call as it was.

use std::collections::HashMap;
use std::time::Duration;

use zbus::{OwnedObjectPath, OwnedVariant, Variant};

use super::portal::{self, Answer, DESKTOP, PATH};
use super::shortcuts::{ACTIONS, ShortcutEvent};

const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";
const REGISTRY: &str = "org.freedesktop.host.portal.Registry";

/// How long the desktop may take to answer: binding them can mean a
/// dialog, waiting on you.
const WAIT: Duration = Duration::from_secs(300);

/// Does the desktop's portal offer global shortcuts?
pub fn supported() -> bool {
    portal::has(IFACE)
}

/// Keys as Flatshot (and KDE) writes them, "Ctrl+Shift+Print", as the
/// shortcuts spec does: "CTRL+SHIFT+Print" (its modifiers' names, then the
/// key's keysym name, lower case for a letter).
pub fn trigger(keys: &str) -> String {
    keys.split('+')
        .filter(|p| !p.is_empty())
        .map(|part| match part.to_ascii_lowercase().as_str() {
            "ctrl" | "control" => "CTRL".to_string(),
            "alt" => "ALT".into(),
            "shift" => "SHIFT".into(),
            "meta" | "super" | "win" => "LOGO".into(),
            "num" => "NUM".into(),
            _ if part.chars().count() == 1 => part.to_lowercase(),
            _ => part.to_string(),
        })
        .collect::<Vec<_>>()
        .join("+")
}

/// Shortcuts the portal gave us, heard while this is kept.
pub struct Session {
    conn: Option<crate::dbus::Bus>,
}

impl Drop for Session {
    /// The connection closed for all who share it (the thread hearing the
    /// presses has its own handle): the portal ends the session with it,
    /// and that thread's done.
    fn drop(&mut self) {
        if let Some(conn) = self.conn.take() {
            let _ = conn.close();
        }
    }
}

/// Ask the portal for our actions' shortcuts (proposing the same keys as
/// on KDE), and hear them pressed: each press comes to `events`. Blocks
/// while the desktop answers (it may ask you first). Returns the session,
/// to keep for as long as they're wanted, and the keys each action got,
/// as the desktop describes them.
pub fn start(events: impl Fn(ShortcutEvent) + Send + 'static) -> Result<(Session, HashMap<String, String>), String> {
    let conn = crate::dbus::Bus::connect(crate::dbus::TIMEOUT)?;
    // Who we are, for its dialog and settings (xdg-desktop-portal 1.19 and
    // later; an older one works it out itself, or does without).
    let none: HashMap<&str, Variant> = HashMap::new();
    if let Err(e) = conn.call_method(Some(DESKTOP), PATH, Some(REGISTRY), "Register", &(crate::APP_ID, none)) {
        crate::timing::log(format!("portal: not registered as {} ({e})", crate::APP_ID));
    }
    let mut options: HashMap<&'static str, Variant<'static>> = HashMap::new();
    options.insert("session_handle_token", Variant::from(format!("flatshot{}", crate::util::random_hex(6))));
    let session = match portal::request_on(&conn, IFACE, "CreateSession", options, |o| (o,), WAIT, None)? {
        Answer::Done(r) => r.get("session_handle").and_then(handle).ok_or("the desktop gave no session")?,
        Answer::Cancelled => return Err("the desktop said no".into()),
    };
    // Heard from before they're bound, so no press can come first; only
    // the portal's own, for this session.
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(DESKTOP)
        .interface(IFACE)
        .member("Activated")
        .path(PATH)
        .build()
        .map_err(|e| e.to_string())?;
    let ours = session.clone();
    crate::dbus::listen_on(conn.inner().clone(), DESKTOP, rule, "flatshot-portal-keys", move |msg| {
        if let Ok((s, id, _, _)) = msg.body().deserialize::<(OwnedObjectPath, String, u64, HashMap<String, OwnedVariant>)>()
            && s == ours
            && let Some((action, ..)) = ACTIONS.iter().find(|a| a.0 == id)
        {
            events(ShortcutEvent::Pressed(action.to_string()));
        }
        true
    })?;
    let wanted: Vec<(String, HashMap<&str, Variant>)> = ACTIONS
        .iter()
        .map(|(action, label, keys)| {
            let mut p: HashMap<&str, Variant> = HashMap::new();
            p.insert("description", Variant::from(label.to_string()));
            if !keys.is_empty() {
                p.insert("preferred_trigger", Variant::from(trigger(keys)));
            }
            (action.to_string(), p)
        })
        .collect();
    let bound = match portal::request_on(&conn, IFACE, "BindShortcuts", HashMap::new(), |o| (session.clone(), wanted, "", o), WAIT, None)? {
        Answer::Done(r) => described(&r),
        Answer::Cancelled => return Err("they weren't given keys".into()),
    };
    Ok((Session { conn: Some(conn) }, bound))
}

/// A session's handle: an object path (some portals give it as a string).
fn handle(v: &OwnedVariant) -> Option<OwnedObjectPath> {
    match &**v {
        Variant::ObjectPath(p) => Some(p.clone().into()),
        Variant::Str(s) => OwnedObjectPath::try_from(s.as_str()).ok(),
        _ => None,
    }
}

/// The keys each action got ("shortcuts": a(sa{sv}), each with its
/// "trigger_description"), by action.
fn described(results: &HashMap<String, OwnedVariant>) -> HashMap<String, String> {
    let Some(list) = results.get("shortcuts").and_then(|v| v.try_clone().ok()) else { return HashMap::new() };
    let Ok(list) = Vec::<(String, HashMap<String, OwnedVariant>)>::try_from(list) else { return HashMap::new() };
    list.into_iter()
        .filter(|(id, _)| ACTIONS.iter().any(|a| a.0 == id))
        .filter_map(|(id, props)| {
            let keys = props.get("trigger_description").and_then(|v| v.try_clone().ok()).and_then(|v| String::try_from(v).ok())?;
            (!keys.is_empty()).then_some((id, keys))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_as_the_shortcuts_spec_writes_them() {
        assert_eq!(trigger("Ctrl+Print"), "CTRL+Print");
        assert_eq!(trigger("Ctrl+Shift+Print"), "CTRL+SHIFT+Print");
        assert_eq!(trigger("Meta+Alt+S"), "LOGO+ALT+s");
        assert_eq!(trigger("F12"), "F12");
        assert_eq!(trigger(""), "");
    }

    #[test]
    fn what_the_portal_answers() {
        let path = OwnedVariant::try_from(Variant::from(zbus::ObjectPath::try_from("/org/freedesktop/portal/desktop/session/1_2/flatshot").unwrap())).unwrap();
        assert_eq!(handle(&path).unwrap().as_str(), "/org/freedesktop/portal/desktop/session/1_2/flatshot");
        assert!(handle(&OwnedVariant::from(zbus::Str::from("/a/b"))).is_some());
        assert!(handle(&OwnedVariant::from(7u32)).is_none());
        let props = |keys: &str| HashMap::from([("trigger_description".to_string(), OwnedVariant::from(zbus::Str::from(keys.to_string())))]);
        let list = vec![("region".to_string(), props("Ctrl+Print")), ("nonsense".to_string(), props("X")), ("last".to_string(), props(""))];
        let results = HashMap::from([("shortcuts".to_string(), OwnedVariant::try_from(Variant::from(list)).unwrap())]);
        assert_eq!(described(&results), HashMap::from([("region".to_string(), "Ctrl+Print".to_string())]));
        assert!(described(&HashMap::new()).is_empty());
    }
}
