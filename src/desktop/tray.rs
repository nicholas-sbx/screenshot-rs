//! The tray icon: a StatusNotifierItem (KDE's, which most panels show) and
//! its menu over com.canonical.dbusmenu. The panel draws both; clicks come
//! back as [`TrayEvent`]s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Value};

use crate::image::Image;
use crate::keyseq;

const ITEM_PATH: &str = "/StatusNotifierItem";
const ITEM_IFACE: &str = "org.kde.StatusNotifierItem";
const MENU_PATH: &str = "/MenuBar";
const MENU_IFACE: &str = "com.canonical.dbusmenu";
const WATCHER: &str = "org.kde.StatusNotifierWatcher";

/// What the tray icon or its menu was asked to do.
#[derive(Clone, Debug, PartialEq)]
pub enum TrayEvent {
    /// a click on the icon
    Activate,
    /// a middle click
    SecondaryActivate,
    /// a menu entry, by key
    Menu(String),
}

/// A menu entry; a separator has an empty key.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct MenuItem {
    pub key: String,
    pub label: String,
    /// its shortcut, as Qt writes it ("Ctrl+Print")
    pub keys: String,
}

impl MenuItem {
    pub fn new(key: &str, label: &str, keys: &str) -> MenuItem {
        MenuItem { key: key.into(), label: label.into(), keys: keys.into() }
    }

    pub fn separator() -> MenuItem {
        MenuItem::default()
    }
}

type Pixmaps = Vec<(i32, i32, Vec<u8>)>;
/// A dbusmenu entry and its children: (ia{sv}av).
type Layout = (i32, HashMap<String, OwnedValue>, Vec<OwnedValue>);

struct State {
    items: Vec<MenuItem>,
    revision: u32,
    recording: bool,
    tooltip: String,
    icon: Pixmaps,
    stop_icon: Option<Pixmaps>,
    /// from the panel, to bring our windows forward when it's clicked
    activation_token: Option<String>,
}

type Events = Arc<dyn Fn(TrayEvent) + Send + Sync>;

struct Item {
    state: Arc<Mutex<State>>,
    events: Events,
}

#[zbus::interface(name = "org.kde.StatusNotifierItem")]
impl Item {
    #[zbus(property)]
    fn category(&self) -> &str {
        "ApplicationStatus"
    }
    #[zbus(property)]
    fn id(&self) -> &str {
        "flatshot"
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        "Flatshot"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "Active"
    }
    #[zbus(property)]
    fn window_id(&self) -> i32 {
        0
    }
    #[zbus(property)]
    fn icon_name(&self) -> &str {
        if self.state.lock().is_ok_and(|s| s.recording) { "" } else { "flatshot" }
    }
    #[zbus(property)]
    fn icon_pixmap(&self) -> Pixmaps {
        let Ok(mut s) = self.state.lock() else { return Vec::new() };
        if s.recording { s.stop_icon.get_or_insert_with(stop_icon).clone() } else { s.icon.clone() }
    }
    #[zbus(property)]
    fn overlay_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn overlay_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }
    #[zbus(property)]
    fn attention_icon_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn attention_icon_pixmap(&self) -> Pixmaps {
        Vec::new()
    }
    #[zbus(property)]
    fn attention_movie_name(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> &str {
        ""
    }
    #[zbus(property)]
    fn tool_tip(&self) -> (String, Pixmaps, String, String) {
        let title = self.state.lock().map(|s| s.tooltip.clone()).unwrap_or_default();
        (String::new(), Vec::new(), title, String::new())
    }
    #[zbus(property)]
    fn item_is_menu(&self) -> bool {
        false
    }
    #[zbus(property)]
    fn menu(&self) -> OwnedObjectPath {
        OwnedObjectPath::try_from(MENU_PATH).unwrap_or_default()
    }

    fn context_menu(&self, _x: i32, _y: i32) {}

    fn activate(&self, _x: i32, _y: i32) {
        (self.events)(TrayEvent::Activate);
    }

    fn secondary_activate(&self, _x: i32, _y: i32) {
        (self.events)(TrayEvent::SecondaryActivate);
    }

    fn scroll(&self, _delta: i32, _orientation: &str) {}

    fn provide_xdg_activation_token(&self, token: String) {
        if let Ok(mut s) = self.state.lock()
            && token.len() < 512
        {
            s.activation_token = Some(token);
        }
    }

    #[zbus(signal)]
    async fn new_icon(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
    #[zbus(signal)]
    async fn new_tool_tip(emitter: &SignalEmitter<'_>) -> zbus::Result<()>;
}

struct Menu {
    state: Arc<Mutex<State>>,
    events: Events,
}

/// A menu entry's properties, as dbusmenu has them.
fn properties(item: &MenuItem, recording: bool) -> HashMap<String, OwnedValue> {
    let mut p: HashMap<String, OwnedValue> = HashMap::new();
    let mut put = |k: &str, v: Value<'_>| {
        if let Ok(v) = OwnedValue::try_from(v) {
            p.insert(k.to_string(), v);
        }
    };
    if item.key.is_empty() {
        put("type", Value::from("separator"));
        return p;
    }
    let label = if item.key == "record" && recording { "Stop recording" } else { item.label.as_str() };
    // (an underscore marks a mnemonic: a real one is doubled)
    put("label", Value::from(label.replace('_', "__")));
    if !item.keys.is_empty() {
        put("shortcut", Value::from(vec![shortcut_names(&item.keys)]));
    }
    p
}

/// "Ctrl+Alt+Print" as dbusmenu's key names: ["Control", "Alt", "Print"].
fn shortcut_names(keys: &str) -> Vec<String> {
    let Some(first) = keyseq::parse(keys).first().copied() else { return Vec::new() };
    let mut names = Vec::new();
    for (bit, name) in [(keyseq::CTRL, "Control"), (keyseq::ALT, "Alt"), (keyseq::SHIFT, "Shift"), (keyseq::META, "Super")] {
        if first & bit != 0 {
            names.push(name.to_string());
        }
    }
    names.push(keyseq::key_name(first & !keyseq::MODIFIER_MASK));
    names
}

/// One entry and its children, as dbusmenu's (ia{sv}av).
fn layout_value(id: i32, props: HashMap<String, OwnedValue>, children: Vec<OwnedValue>) -> OwnedValue {
    let built = StructureBuilder::new().add_field(id).add_field(props).add_field(children).build();
    built.ok().and_then(|s| OwnedValue::try_from(Value::Structure(s)).ok()).unwrap_or_else(|| OwnedValue::from(0i32))
}

fn root_properties() -> HashMap<String, OwnedValue> {
    let mut p = HashMap::new();
    if let Ok(v) = OwnedValue::try_from(Value::from("submenu")) {
        p.insert("children-display".to_string(), v);
    }
    p
}

impl Menu {
    fn item_props(&self, id: i32) -> Option<HashMap<String, OwnedValue>> {
        let s = self.state.lock().ok()?;
        if id == 0 {
            return Some(root_properties());
        }
        let item = s.items.get(usize::try_from(id.checked_sub(1)?).ok()?)?;
        Some(properties(item, s.recording))
    }
}

#[zbus::interface(name = "com.canonical.dbusmenu")]
impl Menu {
    #[zbus(property)]
    fn version(&self) -> u32 {
        3
    }
    #[zbus(property)]
    fn text_direction(&self) -> &str {
        "ltr"
    }
    #[zbus(property)]
    fn status(&self) -> &str {
        "normal"
    }
    #[zbus(property)]
    fn icon_theme_path(&self) -> Vec<String> {
        Vec::new()
    }

    fn get_layout(&self, parent_id: i32, recursion_depth: i32, _property_names: Vec<String>) -> zbus::fdo::Result<(u32, Layout)> {
        let Ok(s) = self.state.lock() else { return Err(zbus::fdo::Error::Failed("busy".into())) };
        let children = |s: &State| -> Vec<OwnedValue> {
            s.items.iter().enumerate().map(|(i, item)| layout_value(i as i32 + 1, properties(item, s.recording), Vec::new())).collect()
        };
        if parent_id == 0 {
            let kids = if recursion_depth == 0 { Vec::new() } else { children(&s) };
            return Ok((s.revision, (0, root_properties(), kids)));
        }
        let item = parent_id
            .checked_sub(1)
            .and_then(|i| usize::try_from(i).ok())
            .and_then(|i| s.items.get(i))
            .ok_or_else(|| zbus::fdo::Error::InvalidArgs("no such item".into()))?;
        Ok((s.revision, (parent_id, properties(item, s.recording), Vec::new())))
    }

    fn get_group_properties(&self, ids: Vec<i32>, _property_names: Vec<String>) -> Vec<(i32, HashMap<String, OwnedValue>)> {
        ids.into_iter().take(256).filter_map(|id| self.item_props(id).map(|p| (id, p))).collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedValue> {
        self.item_props(id).and_then(|mut p| p.remove(&name)).ok_or_else(|| zbus::fdo::Error::InvalidArgs("no such property".into()))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedValue, _timestamp: u32) {
        if event_id != "clicked" {
            return;
        }
        let key = self
            .state
            .lock()
            .ok()
            .and_then(|s| id.checked_sub(1).and_then(|i| usize::try_from(i).ok()).and_then(|i| s.items.get(i)).map(|item| item.key.clone()));
        if let Some(key) = key.filter(|k| !k.is_empty()) {
            (self.events)(TrayEvent::Menu(key));
        }
    }

    fn event_group(&self, events: Vec<(i32, String, OwnedValue, u32)>) -> Vec<i32> {
        for (id, event_id, data, timestamp) in events.into_iter().take(64) {
            self.event(id, event_id, data, timestamp);
        }
        Vec::new()
    }

    fn about_to_show(&self, _id: i32) -> bool {
        false
    }

    fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
        (Vec::new(), Vec::new())
    }

    #[zbus(signal)]
    async fn layout_updated(emitter: &SignalEmitter<'_>, revision: u32, parent: i32) -> zbus::Result<()>;
}

/// Our tray icon, once shown.
pub struct Tray {
    state: Arc<Mutex<State>>,
}

impl Tray {
    /// Show the icon with `items` in its menu. Err when there's no tray
    /// (no StatusNotifierWatcher) or no session bus.
    pub fn show(items: Vec<MenuItem>, events: impl Fn(TrayEvent) + Send + Sync + 'static) -> Result<Tray, String> {
        let conn = crate::dbus::session()?;
        let state = Arc::new(Mutex::new(State {
            items,
            revision: 1,
            recording: false,
            tooltip: "Flatshot".into(),
            icon: app_icon(),
            stop_icon: None,
            activation_token: None,
        }));
        let events: Events = Arc::new(events);
        let server = conn.object_server();
        server.at(ITEM_PATH, Item { state: state.clone(), events: events.clone() }).map_err(|e| e.to_string())?;
        server.at(MENU_PATH, Menu { state: state.clone(), events }).map_err(|e| e.to_string())?;
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        conn.request_name(name.as_str()).map_err(|e| e.to_string())?;
        register(conn, &name)?;
        // Again whenever the panel restarts.
        let proxy = zbus::blocking::fdo::DBusProxy::new(conn).map_err(|e| e.to_string())?;
        if let Ok(changes) = proxy.receive_name_owner_changed_with_args(&[(0, WATCHER)]) {
            let _ = std::thread::Builder::new().name("flatshot-tray".into()).spawn(move || {
                for change in changes {
                    if change.args().is_ok_and(|a| a.new_owner().is_some()) {
                        let _ = register(conn, &name);
                    }
                }
            });
        }
        Ok(Tray { state })
    }

    /// Show `items` in the menu (when the shortcuts change).
    pub fn set_items(&self, items: Vec<MenuItem>) {
        let revision = {
            let Ok(mut s) = self.state.lock() else { return };
            if s.items == items {
                return;
            }
            s.items = items;
            s.revision += 1;
            s.revision
        };
        emit(MENU_PATH, MENU_IFACE, "LayoutUpdated", &(revision, 0i32));
    }

    /// Recording or not: the icon becomes a stop button while it is.
    pub fn set_recording(&self, recording: bool) {
        let revision = {
            let Ok(mut s) = self.state.lock() else { return };
            if s.recording == recording {
                return;
            }
            s.recording = recording;
            s.revision += 1;
            if !recording {
                s.tooltip = "Flatshot".into();
            }
            s.revision
        };
        emit(ITEM_PATH, ITEM_IFACE, "NewIcon", &());
        emit(ITEM_PATH, ITEM_IFACE, "NewToolTip", &());
        emit(MENU_PATH, MENU_IFACE, "LayoutUpdated", &(revision, 0i32));
    }

    pub fn set_tooltip(&self, text: &str) {
        if let Ok(mut s) = self.state.lock() {
            if s.tooltip == text {
                return;
            }
            s.tooltip = text.to_string();
        }
        emit(ITEM_PATH, ITEM_IFACE, "NewToolTip", &());
    }

    /// The activation token the panel gave with its last click, if any.
    pub fn take_activation_token(&self) -> Option<String> {
        self.state.lock().ok().and_then(|mut s| s.activation_token.take())
    }
}

fn register(conn: &zbus::blocking::Connection, name: &str) -> Result<(), String> {
    conn.call_method(Some(WATCHER), "/StatusNotifierWatcher", Some(WATCHER), "RegisterStatusNotifierItem", &(name,))
        .map(|_| ())
        .map_err(|e| format!("no system tray: {e}"))
}

fn emit<B: serde::Serialize + zbus::zvariant::DynamicType>(path: &str, iface: &str, member: &str, body: &B) {
    if let Ok(conn) = crate::dbus::session() {
        let _ = conn.emit_signal(None::<()>, path, iface, member, body);
    }
}

/// An icon as StatusNotifierItem wants it: ARGB32, straight alpha, in
/// network byte order.
fn pixmap(img: &Image) -> (i32, i32, Vec<u8>) {
    let mut data = Vec::with_capacity(img.byte_len());
    for y in 0..img.height() {
        for px in img.row(y).as_chunks::<4>().0 {
            let (b, g, r, a) = (px[0] as u32, px[1] as u32, px[2] as u32, px[3] as u32);
            let un = |c: u32| (c * 255 + a / 2).checked_div(a).map_or(0, |v| v.min(255) as u8);
            data.extend_from_slice(&[a as u8, un(r), un(g), un(b)]);
        }
    }
    (img.width() as i32, img.height() as i32, data)
}

/// Flatshot's icon at the sizes panels use.
/// The tray icon as a picture of `size` pixels square: Flatshot's, or a
/// stop button while recording.
pub fn icon_image(size: u32, recording: bool) -> Option<Image> {
    if recording {
        let mut img = Image::new(size, size).ok()?;
        let f = size as f32;
        let mut pm = img.pixmap_mut();
        let mut paint = tiny_skia::Paint { anti_alias: true, ..Default::default() };
        paint.set_color_rgba8(0xE5, 0x48, 0x4D, 0xFF);
        let circle = tiny_skia::PathBuilder::from_circle(f / 2.0, f / 2.0, f / 2.0 - 1.0)?;
        pm.fill_path(&circle, &paint, tiny_skia::FillRule::Winding, tiny_skia::Transform::identity(), None);
        paint.set_color_rgba8(0xFF, 0xFF, 0xFF, 0xFF);
        let s = f * 0.32;
        let square = tiny_skia::Rect::from_xywh((f - s) / 2.0, (f - s) / 2.0, s, s)?;
        pm.fill_rect(square, &paint, tiny_skia::Transform::identity(), None);
        return Some(img);
    }
    crate::codec::decode_png(include_bytes!("../../assets/flatshot.png")).ok()?.scaled_area(size, size).ok()
}

fn app_icon() -> Pixmaps {
    let Ok(full) = crate::codec::decode_png(include_bytes!("../../assets/flatshot.png")) else { return Vec::new() };
    [16u32, 22, 32, 48, 64].iter().filter_map(|&s| full.scaled_area(s, s).ok()).map(|i| pixmap(&i)).collect()
}

/// The icon while recording: the recording bar's red stop button.
fn stop_icon() -> Pixmaps {
    [16u32, 22, 32, 48, 64].iter().filter_map(|&size| icon_image(size, true)).map(|i| pixmap(&i)).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn menu_entries() {
        assert_eq!(shortcut_names("Ctrl+Alt+Print"), vec!["Control", "Alt", "Print"]);
        assert_eq!(shortcut_names("Meta+Shift+S"), vec!["Shift", "Super", "S"]);
        let p = properties(&MenuItem::new("record", "Record screen", ""), true);
        assert_eq!(String::try_from(p["label"].clone()).unwrap(), "Stop recording");
        assert!(properties(&MenuItem::separator(), false).contains_key("type"));
        let v = layout_value(3, root_properties(), vec![]);
        assert_eq!(v.value_signature().to_string(), "(ia{sv}av)");
    }

    #[test]
    fn icons() {
        let icon = app_icon();
        assert_eq!(icon.len(), 5);
        assert_eq!(icon[0].2.len(), 16 * 16 * 4);
        let stop = stop_icon();
        // The middle of the stop button is white, the edge transparent.
        let (w, _, data) = &stop[4];
        let mid = ((32 * w + 32) * 4) as usize;
        assert_eq!(&data[mid..mid + 4], &[255, 255, 255, 255]);
        assert_eq!(data[3], 0);
    }
}
