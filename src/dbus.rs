//! The session bus, shared by everything that talks to the desktop: KWin,
//! the portals, notifications, global shortcuts and the tray.

use std::sync::OnceLock;
use std::time::Duration;

use zbus::blocking::Connection;

static SESSION: OnceLock<Result<Connection, String>> = OnceLock::new();

/// No call waits on the desktop longer than this.
pub const TIMEOUT: Duration = Duration::from_secs(15);

/// The session bus (connected once, then kept).
pub fn session() -> Result<&'static Connection, String> {
    SESSION
        .get_or_init(|| {
            zbus::blocking::connection::Builder::session()
                .map_err(|e| e.to_string())?
                .method_timeout(TIMEOUT)
                .build()
                .map_err(|e| format!("no session bus: {e}"))
        })
        .as_ref()
        .map_err(|e| e.clone())
}

/// Connect in the background (startup: it's ready by the time it's needed).
pub fn connect_early() {
    let _ = std::thread::Builder::new().name("flatshot-dbus".into()).spawn(|| {
        let _ = session();
    });
}

/// Is `name` owned on the session bus?
pub fn has_owner(name: &str) -> bool {
    let Ok(conn) = session() else { return false };
    let Ok(reply) = conn.call_method(
        Some("org.freedesktop.DBus"),
        "/org/freedesktop/DBus",
        Some("org.freedesktop.DBus"),
        "NameHasOwner",
        &(name,),
    ) else {
        return false;
    };
    reply.body().deserialize::<bool>().unwrap_or(false)
}
