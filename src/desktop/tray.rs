//! The tray icon: a StatusNotifierItem (KDE's, which most panels show) and
//! its menu over com.canonical.dbusmenu. The panel draws both; clicks come
//! back as [`TrayEvent`]s.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use zbus::object_server::SignalEmitter;
use zbus::{OwnedObjectPath, OwnedVariant, Structure, Variant};

use crate::geom::Rect;
use crate::image::Image;
use crate::keyseq;
use crate::render::paint::{self, Painter, Pen};
use crate::render::theme::{self, Color};

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
type Layout = (i32, HashMap<String, OwnedVariant>, Vec<OwnedVariant>);

struct State {
    items: Vec<MenuItem>,
    revision: u32,
    recording: bool,
    tooltip: String,
    /// the icon's look (STYLES), "auto" made white or black
    style: &'static str,
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
        crate::APP_ID
    }
    #[zbus(property)]
    fn title(&self) -> &str {
        crate::APP_NAME
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
        // (the installed icon only for its own colours: else our pixmaps)
        if self.state.lock().is_ok_and(|s| !s.recording && s.style == "color") { crate::APP_ID } else { "" }
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
fn properties(item: &MenuItem, recording: bool) -> HashMap<String, OwnedVariant> {
    let mut p: HashMap<String, OwnedVariant> = HashMap::new();
    let mut put = |k: &str, v: Variant<'_>| {
        if let Ok(v) = OwnedVariant::try_from(v) {
            p.insert(k.to_string(), v);
        }
    };
    if item.key.is_empty() {
        put("type", Variant::from("separator"));
        return p;
    }
    let label = if item.key == "record" && recording { "Stop recording" } else { item.label.as_str() };
    // (an underscore marks a mnemonic: a real one is doubled)
    put("label", Variant::from(label.replace('_', "__")));
    if !item.keys.is_empty() {
        put("shortcut", Variant::from(vec![shortcut_names(&item.keys)]));
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
fn layout_value(id: i32, props: HashMap<String, OwnedVariant>, children: Vec<OwnedVariant>) -> OwnedVariant {
    let built = Structure::builder().add_field(id).add_field(props).add_field(children).build();
    built.ok().and_then(|s| OwnedVariant::try_from(Variant::Structure(s)).ok()).unwrap_or_else(|| OwnedVariant::from(0i32))
}

fn root_properties() -> HashMap<String, OwnedVariant> {
    let mut p = HashMap::new();
    if let Ok(v) = OwnedVariant::try_from(Variant::from("submenu")) {
        p.insert("children-display".to_string(), v);
    }
    p
}

impl Menu {
    fn item_props(&self, id: i32) -> Option<HashMap<String, OwnedVariant>> {
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
        let children = |s: &State| -> Vec<OwnedVariant> {
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

    fn get_group_properties(&self, ids: Vec<i32>, _property_names: Vec<String>) -> Vec<(i32, HashMap<String, OwnedVariant>)> {
        ids.into_iter().take(256).filter_map(|id| self.item_props(id).map(|p| (id, p))).collect()
    }

    fn get_property(&self, id: i32, name: String) -> zbus::fdo::Result<OwnedVariant> {
        self.item_props(id).and_then(|mut p| p.remove(&name)).ok_or_else(|| zbus::fdo::Error::InvalidArgs("no such property".into()))
    }

    fn event(&self, id: i32, event_id: String, _data: OwnedVariant, _timestamp: u32) {
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

    fn event_group(&self, events: Vec<(i32, String, OwnedVariant, u32)>) -> Vec<i32> {
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
#[derive(Clone)]
pub struct Tray {
    state: Arc<Mutex<State>>,
}

impl Tray {
    /// Show the icon with `items` in its menu. Err when there's no tray
    /// (no StatusNotifierWatcher) or no session bus.
    pub fn show(items: Vec<MenuItem>, style: &str, events: impl Fn(TrayEvent) + Send + Sync + 'static) -> Result<Tray, String> {
        let conn = crate::dbus::session()?;
        let style = resolve_style(style);
        let state = Arc::new(Mutex::new(State {
            items,
            revision: 1,
            recording: false,
            tooltip: crate::APP_NAME.into(),
            style,
            icon: app_icon(style),
            stop_icon: None,
            activation_token: None,
        }));
        let events: Events = Arc::new(events);
        zbus::block_on(async {
            let server = conn.inner().object_server();
            server.at(ITEM_PATH, Item { state: state.clone(), events: events.clone() }).await?;
            server.at(MENU_PATH, Menu { state: state.clone(), events }).await
        })
        .map_err(|e| e.to_string())?;
        let name = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        conn.request_name(name.as_str()).map_err(|e| e.to_string())?;
        register(conn, &name)?;
        // Again whenever the panel restarts.
        let changes = zbus::block_on(async {
            let proxy = zbus::fdo::DBusProxy::new(conn.inner()).await?;
            proxy.receive_name_owner_changed_with_args(&[(0, WATCHER)]).await
        });
        if let Ok(mut changes) = changes {
            // (not zbus::block_on: `register` blocks on a call of its own)
            let _ = std::thread::Builder::new().name("flatshot-tray".into()).spawn(move || {
                while let Some(change) = futures_lite::future::block_on(futures_lite::StreamExt::next(&mut changes)) {
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

    /// Show the icon in `style` (STYLES; "auto" is looked at again, so
    /// this follows the desktop's scheme too).
    pub fn set_style(&self, style: &str) {
        let style = resolve_style(style);
        {
            let Ok(mut s) = self.state.lock() else { return };
            // (the theme's colours may have changed: always drawn again)
            if s.style == style && style != "theme" {
                return;
            }
            s.style = style;
            s.icon = app_icon(style);
        }
        emit(ITEM_PATH, ITEM_IFACE, "NewIcon", &());
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
                s.tooltip = crate::APP_NAME.into();
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

fn register(conn: &crate::dbus::Bus, name: &str) -> Result<(), String> {
    conn.call_method(Some(WATCHER), "/StatusNotifierWatcher", Some(WATCHER), "RegisterStatusNotifierItem", &(name,))
        .map(|_| ())
        .map_err(|e| format!("no system tray: {e}"))
}

fn emit<B: serde::Serialize + zbus::DynamicType>(path: &str, iface: &str, member: &str, body: &B) {
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

/// The tray icon's looks: the app's own colours, the theme's, plain white
/// or black outlines (for panels with symbolic icons), or white or black
/// to suit the desktop's dark or light scheme.
pub const STYLES: &[&str] = crate::config::TRAY_ICONS;

/// `style` as drawn: "auto" becomes white or black, anything unknown the colour one.
pub fn resolve_style(style: &str) -> &'static str {
    match style {
        "auto" if desktop_is_dark() => "white",
        "auto" => "black",
        s => STYLES.iter().find(|x| **x == s).copied().unwrap_or("color"),
    }
}

/// Whether the desktop uses a dark colour scheme: the portal's appearance
/// setting, else KDE's window colour; unknown counts as dark (the usual
/// panel).
pub fn desktop_is_dark() -> bool {
    if let Some(scheme) = portal_color_scheme() {
        match scheme {
            1 => return true,
            2 => return false,
            _ => {}
        }
    }
    let text = crate::util::read_limited(&crate::config::config_home().join("kdeglobals"), 1 << 20).unwrap_or_default();
    kde_window_is_dark(&String::from_utf8_lossy(&text)).unwrap_or(true)
}

/// org.freedesktop.appearance color-scheme: 0 no preference, 1 dark, 2 light.
fn portal_color_scheme() -> Option<u32> {
    let conn = crate::dbus::session().ok()?;
    let args = ("org.freedesktop.appearance", "color-scheme");
    let call = |method: &str| conn.call_method(Some(PORTAL), "/org/freedesktop/portal/desktop", Some("org.freedesktop.portal.Settings"), method, &args).ok();
    let reply = call("ReadOne").or_else(|| call("Read"))?;
    let mut value: OwnedVariant = reply.body().deserialize().ok()?;
    // (Read wraps it once more)
    loop {
        match &*value {
            Variant::U32(v) => return Some(*v),
            Variant::Variant(inner) => value = OwnedVariant::try_from(&**inner).ok()?,
            _ => return None,
        }
    }
}

const PORTAL: &str = "org.freedesktop.portal.Desktop";

/// From kdeglobals: is [Colors:Window] BackgroundNormal dark?
fn kde_window_is_dark(kdeglobals: &str) -> Option<bool> {
    let mut in_window = false;
    for line in kdeglobals.lines().map(str::trim) {
        if line.starts_with('[') {
            in_window = line == "[Colors:Window]";
        } else if in_window && let Some(v) = line.strip_prefix("BackgroundNormal=") {
            let c: Vec<f32> = v.split(',').take(3).filter_map(|x| x.trim().parse().ok()).collect();
            let [r, g, b] = c[..] else { return None };
            return Some(0.2126 * r + 0.7152 * g + 0.0722 * b < 128.0);
        }
    }
    None
}

/// Call `changed` (on a thread of its own) whenever the desktop's colour
/// scheme changes, for the "auto" icon.
pub fn watch_color_scheme(mut changed: impl FnMut() + Send + 'static) -> Result<(), String> {
    let rule = zbus::MatchRule::builder()
        .msg_type(zbus::message::Type::Signal)
        .sender(PORTAL)
        .interface("org.freedesktop.portal.Settings")
        .member("SettingChanged")
        .add_arg("org.freedesktop.appearance")
        .build()
        .map_err(|e| e.to_string())?;
    crate::dbus::listen(PORTAL, rule, "flatshot-scheme", move |msg| {
        if msg.body().deserialize::<(String, String, OwnedVariant)>().is_ok_and(|(_, key, _)| key == "color-scheme") {
            changed();
        }
        true
    })
}

/// The Flatshot logo (assets/flatshot-rs.svg, a 256 grid) at `size` pixels:
/// on `back`, or (without one) marks filling the icon in thin lines with a
/// margin round them, as the desktop's own symbolic tray icons are drawn.
fn draw_logo(size: u32, back: Option<Color>, frame: Color, dot: Color) -> Option<Image> {
    let mut img = Image::new(size, size).ok()?;
    let mut p = Painter::new(img.pixmap_mut(), 1.0);
    let f = size as f32;
    let brackets = [
        [(60.0, 104.0), (60.0, 60.0), (104.0, 60.0)],
        [(152.0, 60.0), (196.0, 60.0), (196.0, 104.0)],
        [(196.0, 152.0), (196.0, 196.0), (152.0, 196.0)],
        [(104.0, 196.0), (60.0, 196.0), (60.0, 152.0)],
    ];
    let segments: Vec<&[(f32, f32)]> = brackets.iter().map(|b| &b[..]).collect();
    let path = paint::polylines(&segments)?;
    match back {
        Some(back) => {
            p.scale_by(f / 256.0, f / 256.0);
            p.fill_rounded_rect(Rect::new(8.0, 8.0, 240.0, 240.0), 56.0, back);
            p.stroke_path(&path, Pen::round(frame, 18.0));
            p.fill_rounded_rect(Rect::new(104.0, 104.0, 48.0, 48.0), 12.0, dot);
        }
        None => {
            let (lo, hi) = (52.0, 204.0); // the marks, with room for their stroke
            let fill = 0.7; // of the icon
            let k = fill * f / (hi - lo);
            p.translate(f * (1.0 - fill) / 2.0, f * (1.0 - fill) / 2.0);
            p.scale_by(k, k);
            p.translate(-lo, -lo);
            // (about 1.2 px at 22 px: Breeze's line)
            p.stroke_path(&path, Pen::round(frame, (f / 18.0).max(1.2) / k));
            // (a smaller dot, in step with the thin lines)
            p.fill_rounded_rect(Rect::new(112.0, 112.0, 32.0, 32.0), 7.0, dot);
        }
    }
    drop(p);
    Some(img)
}

/// The tray icon in `style` (as resolve_style gives it) at `size` pixels.
fn logo(size: u32, style: &str) -> Option<Image> {
    match style {
        "theme" => {
            let c = theme::c();
            draw_logo(size, Some(c.base), c.accent, c.code)
        }
        "white" => draw_logo(size, None, Color::rgb(0xFFFFFF), Color::rgb(0xFFFFFF)),
        "black" => draw_logo(size, None, Color::rgb(0x000000), Color::rgb(0x000000)),
        _ => crate::codec::decode_png(include_bytes!("../../assets/flatshot-rs.png")).ok()?.scaled_area(size, size).ok(),
    }
}

/// The tray icon as a picture of `size` pixels square: Flatshot's (in
/// `style`), or a stop button while recording.
pub fn icon_image(size: u32, recording: bool, style: &str) -> Option<Image> {
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
    logo(size, resolve_style(style))
}

fn app_icon(style: &str) -> Pixmaps {
    if style == "color" {
        let Ok(full) = crate::codec::decode_png(include_bytes!("../../assets/flatshot-rs.png")) else { return Vec::new() };
        return [16u32, 22, 32, 48, 64].iter().filter_map(|&s| full.scaled_area(s, s).ok()).map(|i| pixmap(&i)).collect();
    }
    [16u32, 22, 24, 32, 48, 64, 128].iter().filter_map(|&s| logo(s, style)).map(|i| pixmap(&i)).collect()
}

/// The icon while recording: the recording bar's red stop button.
fn stop_icon() -> Pixmaps {
    [16u32, 22, 32, 48, 64].iter().filter_map(|&size| icon_image(size, true, "color")).map(|i| pixmap(&i)).collect()
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
    fn icon_styles() {
        // White and black: thin outlines on nothing, filling the icon with a margin.
        for (style, ink) in [("white", 255u8), ("black", 0)] {
            let img = logo(22, style).unwrap();
            let px = |x: u32, y: u32| img.row(y)[x as usize * 4..x as usize * 4 + 4].to_vec();
            assert_eq!(px(0, 0)[3], 0, "{style}: the corner shows");
            assert_eq!(px(11, 11), vec![ink, ink, ink, 255], "{style}: the dot in the middle");
            let inked = (0..22).flat_map(|y| (0..22).map(move |x| (x, y))).filter(|&(x, y)| px(x, y)[3] > 0).count();
            assert!(inked > 30 && inked < 22 * 22 / 2, "{style}: {inked} pixels inked");
        }
        // Theme: on the theme's base, as the colour icon.
        let img = logo(32, "theme").unwrap();
        let c = theme::c().base;
        assert_eq!(img.row(16)[6 * 4 + 3], 255);
        assert_eq!(img.row(16)[6 * 4 + 2], c.r);
        assert_eq!(app_icon("white").len(), 7);
        assert_eq!(resolve_style("nonsense"), "color");
        assert!(["white", "black"].contains(&resolve_style("auto")));
    }

    #[test]
    fn kde_scheme() {
        let dark = "[General]\nColorScheme=BreezeDark\n\n[Colors:Window]\nBackgroundAlternate=0,0,0\nBackgroundNormal=32,35,38\n";
        assert_eq!(kde_window_is_dark(dark), Some(true));
        assert_eq!(kde_window_is_dark("[Colors:Window]\nBackgroundNormal=239,240,241\n"), Some(false));
        assert_eq!(kde_window_is_dark("[Colors:View]\nBackgroundNormal=0,0,0\n"), None);
    }

    #[test]
    fn icons() {
        let icon = app_icon("color");
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
