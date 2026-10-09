//! KDE global shortcuts through KGlobalAccel (org.kde.kglobalaccel).
//!
//! Flatshot registers as the component "flatshot", so its shortcuts also
//! show up (and can be changed) in System Settings > Shortcuts. Presses
//! arrive as the component's `globalShortcutPressed` signal, which a thread
//! of ours hears and hands to the event loop at once.

use std::time::Duration;

use crate::keyseq;

const SERVICE: &str = "org.kde.kglobalaccel";
const PATH: &str = "/kglobalaccel";
const IFACE: &str = "org.kde.KGlobalAccel";
const COMPONENT_IFACE: &str = "org.kde.kglobalaccel.Component";
const COMPONENT: &str = "flatshot";
const FRIENDLY: &str = "Flatshot";

/// (action, label, default keys)
pub const ACTIONS: &[(&str, &str, &str)] = &[
    ("region", "Capture region", "Ctrl+Print"),
    ("window", "Capture active window", "Ctrl+Alt+Print"),
    ("monitor", "Capture current monitor", ""),
    ("screen", "Capture all screens", "Ctrl+Shift+Print"),
    ("last", "Capture last region", ""),
    ("pin", "Pin region to screen", ""),
    ("record", "Record screen", ""),
];

// KGlobalAccel::SetShortcutFlag
const IS_DEFAULT: u32 = 1;
const SET_PRESENT: u32 = 2;
const NO_AUTOLOADING: u32 = 4;

/// What the shortcut service tells us.
#[derive(Clone, Debug, PartialEq)]
pub enum ShortcutEvent {
    /// an action's keys were pressed
    Pressed(String),
    /// assignments changed (here or in System Settings)
    Changed,
}

pub fn label(action: &str) -> &'static str {
    ACTIONS.iter().find(|a| a.0 == action).map(|a| a.1).unwrap_or("")
}

fn action_id(action: &str) -> Vec<String> {
    vec![COMPONENT.into(), action.into(), FRIENDLY.into(), label(action).into()]
}

/// Key sequences as KGlobalAccel's a(ai): always four ints per sequence
/// (the service reads four unconditionally).
fn wire(seqs: &[&str]) -> Vec<(Vec<i32>,)> {
    seqs.iter()
        .filter(|s| !s.is_empty())
        .map(|s| {
            let mut keys = keyseq::parse(s);
            keys.resize(4, 0);
            (keys,)
        })
        .collect()
}

fn unwire(value: Vec<(Vec<i32>,)>) -> Vec<String> {
    value
        .into_iter()
        .filter(|(keys,)| keys.iter().any(|k| *k != 0))
        .map(|(keys,)| keyseq::seq_to_text(&keys.into_iter().filter(|k| *k != 0).take(4).collect::<Vec<_>>()))
        .collect()
}

fn call<B>(method: &str, body: &B) -> Result<zbus::message::Message, String>
where
    B: serde::Serialize + zbus::zvariant::DynamicType,
{
    let conn = crate::dbus::session()?;
    conn.call_method(Some(SERVICE), PATH, Some(IFACE), method, body).map_err(|e| e.to_string())
}

fn set(action: &str, seqs: &[&str], flags: u32) -> Result<Vec<String>, String> {
    let aid = action_id(action);
    match call("setShortcutKeys", &(aid.clone(), wire(seqs), flags)) {
        Ok(reply) => reply.body().deserialize::<Vec<(Vec<i32>,)>>().map(unwire).map_err(|e| e.to_string()),
        Err(e) if e.contains("UnknownMethod") => {
            // An older kglobalaccel: one int per shortcut.
            let keys: Vec<i32> = seqs.iter().filter_map(|s| keyseq::parse(s).first().copied()).collect();
            let reply = call("setShortcut", &(aid, keys, flags))?;
            let keys: Vec<i32> = reply.body().deserialize().map_err(|e| e.to_string())?;
            Ok(keys.into_iter().filter(|k| *k != 0).map(|k| keyseq::seq_to_text(&[k])).collect())
        }
        Err(e) => Err(e),
    }
}

/// Is there a shortcut service here (running, or startable)?
pub fn supported() -> bool {
    crate::dbus::has_owner(SERVICE) || activatable()
}

fn activatable() -> bool {
    let Ok(conn) = crate::dbus::session() else { return false };
    let reply = conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "ListActivatableNames", &());
    reply.ok().and_then(|r| r.body().deserialize::<Vec<String>>().ok()).is_some_and(|names| names.iter().any(|n| n == SERVICE))
}

/// Register our actions and hear their presses: each comes on `events`,
/// with `wake` called. Blocks while registering (call it from a thread of
/// its own); Err says why shortcuts can't be had.
pub fn start(events: impl Fn(ShortcutEvent) + Send + 'static) -> Result<(), String> {
    let conn = crate::dbus::session()?;
    if !crate::dbus::has_owner(SERVICE) {
        if !activatable() {
            return Err("KDE's global shortcut service isn't running".into());
        }
        // Start it and wait: activating it cold (right at login) can take
        // far longer than an ordinary call.
        let proxy = zbus::blocking::fdo::DBusProxy::new(conn).map_err(|e| e.to_string())?;
        proxy
            .start_service_by_name(SERVICE.try_into().map_err(|e: zbus::names::Error| e.to_string())?, 0)
            .map_err(|e| format!("could not start KDE's global shortcut service: {e}"))?;
    }
    for (action, _, default) in ACTIONS {
        call("doRegister", &(action_id(action),))?;
        let defaults: Vec<&str> = if default.is_empty() { vec![] } else { vec![*default] };
        set(action, &defaults, IS_DEFAULT)?;
        // Without NoAutoloading the service keeps the user's own keys.
        set(action, &defaults, SET_PRESENT)?;
    }
    let reply = call("getComponent", &(COMPONENT,))?;
    let component: zbus::zvariant::OwnedObjectPath = reply.body().deserialize().map_err(|e| e.to_string())?;
    let rule = zbus::MatchRule::builder().msg_type(zbus::message::Type::Signal).sender(SERVICE).map_err(|e| e.to_string())?.build();
    let iter = zbus::blocking::MessageIterator::for_match_rule(rule, conn, Some(32)).map_err(|e| e.to_string())?;
    std::thread::Builder::new()
        .name("flatshot-shortcuts".into())
        .spawn(move || {
            let mut service = owner(conn);
            for msg in iter {
                let Ok(msg) = msg else { continue };
                let header = msg.header();
                // Only the shortcut service's own signals count.
                let sender = header.sender().map(|s| s.to_string());
                if sender.is_none() || sender != service {
                    service = owner(conn);
                    if sender.is_none() || sender != service {
                        continue;
                    }
                }
                let (iface, member, path) =
                    (header.interface().map(|i| i.to_string()), header.member().map(|m| m.to_string()), header.path().map(|p| p.to_string()));
                match (iface.as_deref(), member.as_deref()) {
                    (Some(COMPONENT_IFACE), Some("globalShortcutPressed")) if path.as_deref() == Some(component.as_str()) => {
                        if let Ok((comp, action, _)) = msg.body().deserialize::<(String, String, i64)>()
                            && comp == COMPONENT
                            && ACTIONS.iter().any(|a| a.0 == action)
                        {
                            events(ShortcutEvent::Pressed(action));
                        }
                    }
                    (Some(IFACE), Some("yourShortcutsChanged")) => {
                        if let Ok((id, _)) = msg.body().deserialize::<(Vec<String>, Vec<(Vec<i32>,)>)>()
                            && id.first().map(String::as_str) == Some(COMPONENT)
                        {
                            events(ShortcutEvent::Changed);
                        }
                    }
                    _ => {}
                }
            }
        })
        .map_err(|e| e.to_string())?;
    Ok(())
}

fn owner(conn: &zbus::blocking::Connection) -> Option<String> {
    let reply = conn.call_method(Some("org.freedesktop.DBus"), "/org/freedesktop/DBus", Some("org.freedesktop.DBus"), "GetNameOwner", &(SERVICE,)).ok()?;
    reply.body().deserialize::<String>().ok()
}

/// The keys of `action` ("" if none).
pub fn get(action: &str) -> String {
    let keys = call("shortcutKeys", &(action_id(action),)).and_then(|r| r.body().deserialize::<Vec<(Vec<i32>,)>>().map_err(|e| e.to_string()));
    keys.ok().map(unwire).and_then(|k| k.into_iter().next()).unwrap_or_default()
}

/// Set (or clear, with "") an action's keys. Returns whether that worked,
/// and the keys it has now.
pub fn assign(action: &str, sequence: &str) -> (bool, String) {
    let seqs: Vec<&str> = if sequence.is_empty() { vec![] } else { vec![sequence] };
    match set(action, &seqs, SET_PRESENT | NO_AUTOLOADING) {
        Ok(now) => {
            let now = now.into_iter().next().unwrap_or_default();
            (now == sequence, now)
        }
        Err(e) => (false, e),
    }
}

/// What else already uses `sequence` ("Component › Action"), if anything.
pub fn owner_of(sequence: &str, action: &str) -> String {
    let Some(key) = keyseq::parse(sequence).first().copied() else { return String::new() };
    // KGlobalShortcutInfo: (uniqueName, friendlyName, componentUnique,
    // componentFriendly, contextUnique, contextFriendly, keys, defaultKeys)
    type Info = (String, String, String, String, String, String, Vec<i32>, Vec<i32>);
    let Ok(reply) = call("getGlobalShortcutsByKey", &(key,)) else { return String::new() };
    let Ok(hits) = reply.body().deserialize::<Vec<Info>>() else { return String::new() };
    hits.iter().find(|h| (h.2.as_str(), h.0.as_str()) != (COMPONENT, action)).map(|h| format!("{} › {}", h.3, h.1)).unwrap_or_default()
}

/// Suspend every global shortcut (while a new one is being typed).
pub fn block(blocked: bool) {
    let _ = call("blockGlobalShortcuts", &(blocked,));
}

/// How long registering may take before the tray goes on without it.
pub const START_TIMEOUT: Duration = Duration::from_secs(30);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_on_the_wire() {
        let w = wire(&["Ctrl+Print", ""]);
        assert_eq!(w.len(), 1);
        assert_eq!(w[0].0.len(), 4);
        assert_eq!(w[0].0[0], keyseq::CTRL | 0x0100_0009);
        assert_eq!(&w[0].0[1..], &[0, 0, 0]);
        assert_eq!(unwire(w), vec!["Ctrl+Print".to_string()]);
        assert!(unwire(vec![(vec![0, 0, 0, 0],)]).is_empty());
    }
}
