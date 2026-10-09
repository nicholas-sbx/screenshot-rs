//! The screenshot portal (org.freedesktop.portal.Screenshot): how GNOME
//! and sandboxed setups hand out the screen. The portal writes a PNG and
//! answers with its URI on a request object.

use std::collections::HashMap;
use std::sync::mpsc;
use std::time::Duration;

use zbus::zvariant::{OwnedValue, Value};

use crate::codec;
use crate::image::Image;
use crate::util;

const DESKTOP: &str = "org.freedesktop.portal.Desktop";
const PATH: &str = "/org/freedesktop/portal/desktop";

/// The whole desktop, without asking the user (where the portal allows it).
pub fn screenshot() -> Result<Image, String> {
    let conn = crate::dbus::session()?;
    let token = format!("flatshot{}", util::random_hex(6));
    // The request object is named after us and the token: listen on it
    // before asking, so the answer can't come first.
    let sender = conn.unique_name().ok_or("no name on the session bus")?.trim_start_matches(':').replace('.', "_");
    let request = format!("{PATH}/request/{sender}/{token}");
    let proxy = zbus::blocking::Proxy::new(conn, DESKTOP, request.as_str(), "org.freedesktop.portal.Request").map_err(|e| e.to_string())?;
    let mut signals = proxy.receive_signal("Response").map_err(|e| format!("portal: {e}"))?;
    let mut options: HashMap<&str, Value<'_>> = HashMap::new();
    options.insert("handle_token", Value::from(token.as_str()));
    options.insert("interactive", Value::Bool(false));
    conn.call_method(Some(DESKTOP), PATH, Some("org.freedesktop.portal.Screenshot"), "Screenshot", &("", options))
        .map_err(|e| format!("the screenshot portal: {e}"))?;
    // Wait for the answer, but not for ever (a dialog may be waiting on the user).
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        let _ = tx.send(signals.next());
    });
    let msg = rx.recv_timeout(Duration::from_secs(60)).map_err(|_| "the screenshot portal didn't answer".to_string())?.ok_or("the portal went away")?;
    let (response, results): (u32, HashMap<String, OwnedValue>) = msg.body().deserialize().map_err(|e| format!("portal reply: {e}"))?;
    match response {
        0 => {}
        1 => return Err("the screenshot was cancelled".into()),
        _ => return Err("the screenshot portal refused".into()),
    }
    let uri = results.get("uri").and_then(|v| <&str>::try_from(&**v).ok()).ok_or("the portal gave no picture")?.to_string();
    let path = util::uri_to_path(&uri).ok_or_else(|| format!("unexpected picture location {uri}"))?;
    let img = codec::decode_file(&path)?;
    // The portal's file is a copy made for us: gone once read, unless it's
    // somewhere the user keeps pictures.
    let temp = std::env::temp_dir();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    if path.starts_with(&temp) || runtime.is_some_and(|r| path.starts_with(r)) {
        let _ = std::fs::remove_file(&path);
    }
    Ok(img)
}
