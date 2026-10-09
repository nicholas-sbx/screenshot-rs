//! Stand-ins for KDE's shortcut service (org.kde.kglobalaccel) and tray
//! (org.kde.StatusNotifierWatcher), for testing the tray app without
//! Plasma. Prints what it's told, one line each, and then:
//!
//! FAKE_KDE_PRESS="region@800,screen@2500": presses those actions'
//! shortcuts after that many milliseconds (globalShortcutPressed).
//! FAKE_KDE_MENU="Capture all screens@1500": clicks the tray menu's entry
//! with that label then (the menu is printed when the icon registers).
//! FAKE_KDE_ACTIVATE=MS: clicks the tray icon then.

use std::collections::HashMap;
use std::sync::Mutex;
use std::time::Duration;

use zbus::blocking::Connection;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, Structure};

struct Accel {
    keys: Mutex<HashMap<String, Vec<(Vec<i32>,)>>>,
}

#[zbus::interface(name = "org.kde.KGlobalAccel")]
impl Accel {
    #[zbus(name = "doRegister")]
    fn do_register(&self, id: Vec<String>) {
        println!("kglobalaccel: register {id:?}");
    }

    #[zbus(name = "setShortcutKeys")]
    fn set_shortcut_keys(&self, id: Vec<String>, keys: Vec<(Vec<i32>,)>, flags: u32) -> Vec<(Vec<i32>,)> {
        println!("kglobalaccel: set {} {keys:?} flags {flags}", id.get(1).cloned().unwrap_or_default());
        let mut all = self.keys.lock().unwrap();
        let action = id.get(1).cloned().unwrap_or_default();
        // As KDE does: the defaults (flag 1) don't change what's assigned.
        if flags & 1 == 0 || !all.contains_key(&action) {
            all.insert(action.clone(), keys);
        }
        all[&action].clone()
    }

    #[zbus(name = "shortcutKeys")]
    fn shortcut_keys(&self, id: Vec<String>) -> Vec<(Vec<i32>,)> {
        self.keys.lock().unwrap().get(id.get(1).map(String::as_str).unwrap_or("")).cloned().unwrap_or_default()
    }

    #[zbus(name = "getComponent")]
    fn get_component(&self, name: String) -> OwnedObjectPath {
        OwnedObjectPath::try_from(format!("/component/{name}")).unwrap()
    }

    #[zbus(name = "blockGlobalShortcuts")]
    fn block_global_shortcuts(&self, blocked: bool) {
        println!("kglobalaccel: blocked {blocked}");
    }
}

struct Watcher;

#[zbus::interface(name = "org.kde.StatusNotifierWatcher")]
impl Watcher {
    #[zbus(name = "RegisterStatusNotifierItem")]
    fn register(&self, service: String, #[zbus(header)] header: zbus::message::Header<'_>) {
        let sender = header.sender().map(|s| s.to_string()).unwrap_or_default();
        println!("tray: registered {service} (from {sender})");
        std::thread::spawn(move || inspect(&service));
    }

    #[zbus(property, name = "IsStatusNotifierHostRegistered")]
    fn host_registered(&self) -> bool {
        true
    }
}

/// Look at the tray icon like a panel would, and click what we're told to.
fn inspect(service: &str) {
    let conn = Connection::session().unwrap();
    let props =
        zbus::blocking::fdo::PropertiesProxy::builder(&conn).destination(service.to_string()).unwrap().path("/StatusNotifierItem").unwrap().build().unwrap();
    let all = props.get_all("org.kde.StatusNotifierItem".try_into().unwrap()).unwrap();
    let pixmaps: Vec<(i32, i32, Vec<u8>)> = all.get("IconPixmap").cloned().map(|v| v.try_into().unwrap()).unwrap_or_default();
    let menu: OwnedObjectPath = all["Menu"].clone().try_into().unwrap();
    println!(
        "tray: icon {:?} with {} pixmaps ({:?}), menu at {}",
        String::try_from(all["IconName"].clone()).unwrap(),
        pixmaps.len(),
        pixmaps.iter().map(|p| p.0).collect::<Vec<_>>(),
        menu.as_str()
    );
    let reply = conn.call_method(Some(service), menu.as_str(), Some("com.canonical.dbusmenu"), "GetLayout", &(0i32, -1i32, Vec::<String>::new())).unwrap();
    type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);
    let (revision, (_, _, children)): (u32, Layout) = reply.body().deserialize().unwrap();
    let mut labels = Vec::new();
    for child in children {
        let s: Structure = child.try_into().unwrap();
        let fields = s.into_fields();
        let id: i32 = fields[0].clone().try_into().unwrap();
        let props: HashMap<String, OwnedValue> = fields[1].try_clone().unwrap().try_into().unwrap();
        let label = props.get("label").map(|l| String::try_from(l.clone()).unwrap()).unwrap_or_else(|| "----".into());
        let keys = props.get("shortcut").map(|k| format!("{:?}", Vec::<Vec<String>>::try_from(k.clone()).unwrap())).unwrap_or_default();
        println!("tray: menu {id} {label} {keys}");
        labels.push((id, label));
    }
    println!("tray: menu revision {revision}");
    if let Ok(spec) = std::env::var("FAKE_KDE_MENU") {
        for (label, ms) in spec.split(',').filter_map(|s| s.rsplit_once('@')) {
            let label = label.to_string();
            let ms: u64 = ms.parse().unwrap();
            let id = labels.iter().find(|(_, l)| *l == label).map(|(i, _)| *i).expect("no such menu entry");
            let (conn, service, menu) = (conn.clone(), service.to_string(), menu.clone());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                let r = conn.call_method(
                    Some(service.as_str()),
                    menu.as_str(),
                    Some("com.canonical.dbusmenu"),
                    "Event",
                    &(id, "clicked", zbus::zvariant::Value::from(0i32), 0u32),
                );
                println!("tray: clicked {label}: {}", r.is_ok());
            });
        }
    }
    if let Some(ms) = std::env::var("FAKE_KDE_ACTIVATE").ok().and_then(|v| v.parse::<u64>().ok()) {
        std::thread::sleep(Duration::from_millis(ms));
        let r = conn.call_method(Some(service), "/StatusNotifierItem", Some("org.kde.StatusNotifierItem"), "Activate", &(0i32, 0i32));
        println!("tray: activated: {}", r.is_ok());
    }
}

fn main() {
    let conn = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.kde.kglobalaccel")
        .unwrap()
        .name("org.kde.StatusNotifierWatcher")
        .unwrap()
        .serve_at("/kglobalaccel", Accel { keys: Mutex::new(HashMap::new()) })
        .unwrap()
        .serve_at("/StatusNotifierWatcher", Watcher)
        .unwrap()
        .build()
        .unwrap();
    println!("fake KDE ready");
    if let Ok(spec) = std::env::var("FAKE_KDE_PRESS") {
        for (action, ms) in spec.split(',').filter_map(|s| s.split_once('@')) {
            let (conn, action, ms) = (conn.clone(), action.to_string(), ms.parse::<u64>().unwrap());
            std::thread::spawn(move || {
                std::thread::sleep(Duration::from_millis(ms));
                let r = conn.emit_signal(
                    None::<()>,
                    "/component/flatshot",
                    "org.kde.kglobalaccel.Component",
                    "globalShortcutPressed",
                    &("flatshot", action.as_str(), 0i64),
                );
                println!("kglobalaccel: pressed {action}: {}", r.is_ok());
            });
        }
    }
    loop {
        std::thread::park();
    }
}
