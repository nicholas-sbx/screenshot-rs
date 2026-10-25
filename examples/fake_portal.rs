//! A stand-in for the desktop portal's global shortcuts (org.freedesktop.
//! portal.GlobalShortcuts, and its host registry), for testing the tray
//! app's without GNOME or Hyprland, for its file chooser's Save dialog,
//! and for the screen cast's: its Start asks which screen to share, and
//! never hears (until the request is closed). Prints what it's asked, one
//! line each, and then:
//!
//! FAKE_PORTAL_PRESS="screen@1000": presses that action's shortcut that
//! many milliseconds after the shortcuts are bound (Activated, for the
//! session they were bound in).
//! FAKE_PORTAL_SAVE=PATH: a Save dialog answers with PATH, as if it were
//! chosen (without it, the dialog is cancelled).

use std::collections::HashMap;
use std::time::Duration;

use zbus::message::Header;
use zbus::{ObjectPath, OwnedObjectPath, OwnedVariant, Variant};

const PATH: &str = "/org/freedesktop/portal/desktop";
const IFACE: &str = "org.freedesktop.portal.GlobalShortcuts";

/// The caller's unique name as the portal puts it in its objects' paths.
fn sender_part(header: &Header<'_>) -> String {
    header.sender().map(|s| s.as_str().trim_start_matches(':').replace('.', "_")).unwrap_or_default()
}

fn text(options: &HashMap<String, OwnedVariant>, key: &str) -> String {
    options.get(key).and_then(|v| v.try_clone().ok()).and_then(|v| String::try_from(v).ok()).unwrap_or_default()
}

/// Answer the request at `path`, a moment after the call has returned (as
/// a portal does, once its dialog is done).
fn respond(conn: &zbus::Connection, path: String, results: HashMap<String, OwnedVariant>) {
    respond_with(conn, path, 0, results);
}

/// The same, with the response's code (0 done, 1 cancelled).
fn respond_with(conn: &zbus::Connection, path: String, code: u32, results: HashMap<String, OwnedVariant>) {
    let conn = conn.clone();
    std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(50));
        let r = zbus::block_on(conn.emit_signal(None::<()>, path.as_str(), "org.freedesktop.portal.Request", "Response", &(code, results)));
        println!("portal: answered {path}: {}", r.is_ok());
    });
}

/// A session, made at once.
fn create_session(options: &HashMap<String, OwnedVariant>, header: &Header<'_>, conn: &zbus::Connection) -> OwnedObjectPath {
    let who = sender_part(header);
    let session = format!("{PATH}/session/{who}/{}", text(options, "session_handle_token"));
    let request = format!("{PATH}/request/{who}/{}", text(options, "handle_token"));
    println!("portal: session {session}");
    let handle = OwnedVariant::try_from(Variant::from(ObjectPath::try_from(session).unwrap())).unwrap();
    respond(conn, request.clone(), HashMap::from([("session_handle".to_string(), handle)]));
    OwnedObjectPath::try_from(request).unwrap()
}

struct Shortcuts;

#[zbus::interface(name = "org.freedesktop.portal.GlobalShortcuts")]
impl Shortcuts {
    #[zbus(name = "CreateSession")]
    fn create_session(
        &self,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        create_session(&options, &header, conn)
    }

    #[zbus(name = "BindShortcuts")]
    fn bind_shortcuts(
        &self,
        session: OwnedObjectPath,
        shortcuts: Vec<(String, HashMap<String, OwnedVariant>)>,
        _parent_window: String,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), text(&options, "handle_token"));
        let mut bound = Vec::new();
        for (id, props) in &shortcuts {
            let (description, trigger) = (text(props, "description"), text(props, "preferred_trigger"));
            println!("portal: bind {id} \"{description}\" {trigger}");
            // (as GNOME describes them: "Ctrl+Print")
            let shown = trigger
                .split('+')
                .map(|p| if p.len() > 1 && p.chars().all(|c| c.is_ascii_uppercase()) { p[..1].to_string() + &p[1..].to_lowercase() } else { p.to_string() });
            let shown = shown.collect::<Vec<_>>().join("+");
            let props: HashMap<String, OwnedVariant> = HashMap::from([
                ("description".to_string(), OwnedVariant::try_from(Variant::from(description)).unwrap()),
                ("trigger_description".to_string(), OwnedVariant::try_from(Variant::from(shown)).unwrap()),
            ]);
            bound.push((id.clone(), props));
        }
        respond(conn, request.clone(), HashMap::from([("shortcuts".to_string(), OwnedVariant::try_from(Variant::from(bound)).unwrap())]));
        if let Ok(spec) = std::env::var("FAKE_PORTAL_PRESS") {
            for (action, ms) in spec.split(',').filter_map(|s| s.split_once('@')) {
                let (conn, session, action, ms) = (conn.clone(), session.clone(), action.to_string(), ms.parse::<u64>().unwrap());
                std::thread::spawn(move || {
                    std::thread::sleep(Duration::from_millis(ms));
                    let options: HashMap<String, OwnedVariant> = HashMap::new();
                    let r = zbus::block_on(conn.emit_signal(None::<()>, PATH, IFACE, "Activated", &(session, action.as_str(), 0u64, options)));
                    println!("portal: pressed {action}: {}", r.is_ok());
                });
            }
        }
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        1
    }
}

struct Files;

#[zbus::interface(name = "org.freedesktop.portal.FileChooser")]
impl Files {
    #[zbus(name = "SaveFile")]
    fn save_file(
        &self,
        _parent_window: String,
        title: String,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), text(&options, "handle_token"));
        println!("portal: save file \"{title}\" as {}", text(&options, "current_name"));
        match std::env::var("FAKE_PORTAL_SAVE") {
            Ok(path) => {
                let uris = OwnedVariant::try_from(Variant::from(vec![format!("file://{path}")])).unwrap();
                respond(conn, request.clone(), HashMap::from([("uris".to_string(), uris)]));
            }
            Err(_) => respond_with(conn, request.clone(), 1, HashMap::new()),
        }
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }
}

struct ScreenCast;

#[zbus::interface(name = "org.freedesktop.portal.ScreenCast")]
impl ScreenCast {
    #[zbus(name = "CreateSession")]
    fn create_session(
        &self,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        create_session(&options, &header, conn)
    }

    #[zbus(name = "SelectSources")]
    fn select_sources(
        &self,
        _session: OwnedObjectPath,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), text(&options, "handle_token"));
        respond(conn, request.clone(), HashMap::new());
        OwnedObjectPath::try_from(request).unwrap()
    }

    /// Its dialog is up until the request is closed: no answer.
    #[zbus(name = "Start")]
    fn start(
        &self,
        _session: OwnedObjectPath,
        _parent_window: String,
        options: HashMap<String, OwnedVariant>,
        #[zbus(header)] header: Header<'_>,
        #[zbus(connection)] conn: &zbus::Connection,
    ) -> OwnedObjectPath {
        let request = format!("{PATH}/request/{}/{}", sender_part(&header), text(&options, "handle_token"));
        println!("portal: which screen to share? {request}");
        let (conn, path) = (conn.clone(), request.clone());
        std::thread::spawn(move || {
            let _ = zbus::block_on(conn.object_server().at(path.as_str(), Request { path: path.clone() }));
        });
        OwnedObjectPath::try_from(request).unwrap()
    }

    #[zbus(property, name = "version")]
    fn version(&self) -> u32 {
        4
    }

    #[zbus(property, name = "AvailableCursorModes")]
    fn cursor_modes(&self) -> u32 {
        3
    }
}

/// A request whose dialog is up, until the asker closes it.
struct Request {
    path: String,
}

#[zbus::interface(name = "org.freedesktop.portal.Request")]
impl Request {
    #[zbus(name = "Close")]
    fn close(&self) {
        println!("portal: closed {}", self.path);
    }
}

struct Registry;

#[zbus::interface(name = "org.freedesktop.host.portal.Registry")]
impl Registry {
    #[zbus(name = "Register")]
    fn register(&self, app_id: String, _options: HashMap<String, OwnedVariant>) {
        println!("portal: registered {app_id}");
    }
}

fn main() {
    let _conn = zbus::block_on(
        zbus::connection::Builder::session()
            .name("org.freedesktop.portal.Desktop")
            .serve_at(PATH, Shortcuts)
            .serve_at(PATH, Files)
            .serve_at(PATH, ScreenCast)
            .serve_at(PATH, Registry)
            .build(),
    )
    .unwrap();
    println!("fake portal ready");
    loop {
        std::thread::park();
    }
}
