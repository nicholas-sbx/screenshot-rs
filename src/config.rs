//! User settings, read from `$XDG_CONFIG_HOME/flatshot-rs/config.json`, and the
//! small bits remembered between runs in `$XDG_STATE_HOME/flatshot-rs/state.json`.
//!
//! Every key is optional and missing keys take the defaults below. The file
//! is laid out as the Python Flatshot's, whose settings are taken over the
//! first time (they're then kept apart, as both can be installed). A value of
//! the wrong type is ignored on its own (the default is used for that key),
//! never the whole file.

use std::collections::BTreeMap;
use std::fs;
use std::io::Write;
use std::os::unix::fs::{DirBuilderExt, OpenOptionsExt, PermissionsExt};
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

pub const FORMAT_EXTENSIONS: &[&str] = &["png", "jpg", "jpeg", "webp", "avif", "jxl"];
pub const CLIPBOARD: &[&str] = &["image", "path", "none"];
pub const LOUPE_SIZES: (i64, i64) = (80, 320);
pub const OPEN_AFTER: &[&str] = &["none", "image", "folder"];
/// Screen capture helpers. "qt" is the old name for grabbing X11 directly.
pub const BACKENDS: &[&str] = &["auto", "kwin", "screencopy", "portal", "spectacle", "grim", "gnome-screenshot", "qt"];
pub const RECORD_FORMATS: &[&str] = &["mp4", "webm", "gif"];
pub const RECORD_FPS: &[i64] = &[24, 30, 60];
pub const REGION_POINTER: &[&str] = &["toggle", "hidden", "shown"];
/// The tray icon's looks (desktop::tray::STYLES).
pub const TRAY_ICONS: &[&str] = &["color", "theme", "white", "black", "auto"];
pub const SCREENSHOT_METHODS: &[&str] = &["desktop", "screens"];

#[derive(Clone, Debug, PartialEq)]
pub struct Config {
    // After capture
    pub save_to_disk: bool,
    pub save_dir: String,
    /// strftime pattern plus {tokens}; the extension follows `format`.
    pub filename: String,
    /// png | jpg | webp | avif | jxl
    pub format: String,
    /// 1-100, for lossy formats
    pub quality: i64,
    /// image | path | none
    pub clipboard: String,
    pub notify: bool,
    /// none | image | folder
    pub open_after: String,
    /// shell command; {path} stands for the file (see deliver::command_line)
    pub run_command: String,
    pub sound: bool,
    pub sound_file: String,
    /// put every capture on a background (the style last used)
    pub beautify: bool,
    /// beautify::Style, in its config form ({}: the defaults)
    pub beautify_style: Map<String, Value>,
    /// name -> a style, for a house style
    pub beautify_presets: Map<String, Value>,
    pub theme: String,
    /// color | theme | white | black | auto (white or black, like the desktop)
    pub tray_icon: String,
    // Capture
    /// screen shading in %, 0 turns it off
    pub dim_opacity: i64,
    pub show_loupe: bool,
    pub loupe_size: i64,
    pub show_crosshair: bool,
    pub show_hint: bool,
    pub rainbow: bool,
    pub snap_edges: bool,
    pub snap_distance: i64,
    pub snap_sensitivity: i64,
    pub span_monitors: bool,
    pub detect_windows: bool,
    /// ... panels, notifications, popups and the like too, not just app windows
    pub detect_all_windows: bool,
    /// The active window (instant capture) on KDE is KWin's picture of it:
    /// with its title bar and borders, and its shadow (transparent round it).
    pub window_frame: bool,
    pub window_shadow: bool,
    /// QR codes and barcodes are looked for while shown (Q)
    pub show_codes: bool,
    pub toolbar_follows_mouse: bool,
    pub backend: String,
    /// "desktop": one picture of the whole desktop; "screens": each screen
    /// on its own, at its own scale, all at once.
    pub screenshot_method: String,
    pub include_pointer: bool,
    /// The pointer when picking a region: hidden | shown | toggle
    pub region_pointer: String,
    pub default_tool: String,
    /// 0-6 the toolbar's colours, 7 your own (custom_color)
    pub default_color: i64,
    /// #RRGGBB
    pub custom_color: String,
    /// keys in Flatshot's windows you changed: action -> key
    pub keys: BTreeMap<String, String>,
    pub default_size: i64,
    // Screen recording
    pub record_dir: String,
    pub record_filename: String,
    pub record_format: String,
    pub record_fps: i64,
    pub record_mic: bool,
    pub record_system_audio: bool,
    pub record_cursor: bool,
    pub record_countdown: i64,
}

impl Default for Config {
    fn default() -> Self {
        Config::from_map(&Map::new())
    }
}

/// The value for `key` in `data` when it's usable as a bool.
fn get_bool(data: &Map<String, Value>, key: &str) -> Option<bool> {
    match data.get(key)? {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => n.as_f64().map(|f| f != 0.0),
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "1" | "yes" | "on" => Some(true),
            "false" | "0" | "no" | "off" | "" => Some(false),
            _ => None,
        },
        Value::Null => None,
        _ => None,
    }
}

/// As Python's `int()`: whole numbers, floats (truncated) and numeric strings.
fn get_int(data: &Map<String, Value>, key: &str) -> Option<i64> {
    match data.get(key)? {
        Value::Number(n) => n.as_i64().or_else(|| n.as_f64().filter(|f| f.is_finite()).map(|f| f.trunc() as i64)),
        Value::Bool(b) => Some(*b as i64),
        Value::String(s) => s.trim().parse::<i64>().ok(),
        _ => None,
    }
}

fn get_str(data: &Map<String, Value>, key: &str) -> Option<String> {
    match data.get(key)? {
        Value::String(s) => Some(s.clone()),
        _ => None,
    }
}

fn one_of(value: Option<String>, allowed: &[&str], default: &str) -> String {
    match value {
        Some(v) if allowed.contains(&v.as_str()) => v,
        _ => default.to_string(),
    }
}

impl Config {
    /// Settings from a parsed config file, with the same clean-up the Python
    /// version did on load.
    pub fn from_map(data: &Map<String, Value>) -> Config {
        let b = |k: &str, d: bool| get_bool(data, k).unwrap_or(d);
        let i = |k: &str, d: i64| get_int(data, k).unwrap_or(d);
        let s = |k: &str, d: &str| get_str(data, k).unwrap_or_else(|| d.to_string());

        let save_dir = s("save_dir", "");
        let record_dir = s("record_dir", "");
        let mut clipboard = one_of(get_str(data, "clipboard"), CLIPBOARD, "image");
        // Pre-0.2 key, still honoured: false means clipboard = "none".
        if get_bool(data, "copy_to_clipboard") == Some(false) && data.get("copy_to_clipboard").is_some_and(|v| !v.is_null()) {
            clipboard = "none".into();
        }
        let mut format = s("format", "png").to_lowercase().trim_start_matches('.').to_string();
        if format == "jpeg" {
            format = "jpg".into();
        }
        let mut filename = s("filename", "Screenshot_%Y-%m-%d_%H-%M-%S");
        // Older configs kept the extension in the pattern; it now follows `format`.
        if let Some((stem, ext)) = filename.rsplit_once('.')
            && !stem.is_empty()
            && FORMAT_EXTENSIONS.contains(&ext.to_lowercase().as_str())
        {
            filename = stem.to_string();
        }
        let custom_color = match get_str(data, "custom_color") {
            Some(c) if is_hex_color(&c) => c.to_uppercase(),
            _ => "#FF4FA3".into(),
        };
        let keys = match data.get("keys") {
            Some(Value::Object(m)) => m.iter().filter_map(|(k, v)| v.as_str().map(|v| (k.clone(), v.to_string()))).collect(),
            _ => BTreeMap::new(),
        };
        let record_fps = i("record_fps", 30);
        let object = |k: &str| match data.get(k) {
            Some(Value::Object(m)) => m.clone(),
            _ => Map::new(),
        };
        let mut beautify_presets = object("beautify_presets");
        beautify_presets.retain(|_, v| v.is_object());
        Config {
            beautify: b("beautify", false),
            beautify_style: object("beautify_style"),
            beautify_presets,
            save_to_disk: b("save_to_disk", true),
            save_dir: if save_dir.is_empty() { default_dir() } else { expand_user(&save_dir) },
            filename,
            format,
            quality: i("quality", 90).clamp(1, 100),
            clipboard,
            notify: b("notify", true),
            open_after: one_of(get_str(data, "open_after"), OPEN_AFTER, "none"),
            run_command: s("run_command", ""),
            sound: b("sound", false),
            sound_file: s("sound_file", ""),
            theme: s("theme", "ember"),
            tray_icon: one_of(get_str(data, "tray_icon"), TRAY_ICONS, "color"),
            dim_opacity: i("dim_opacity", 60).clamp(0, 90),
            show_loupe: b("show_loupe", true),
            loupe_size: i("loupe_size", 120).clamp(LOUPE_SIZES.0, LOUPE_SIZES.1),
            show_crosshair: b("show_crosshair", true),
            show_hint: b("show_hint", true),
            rainbow: b("rainbow", false),
            snap_edges: b("snap_edges", false),
            snap_distance: i("snap_distance", 10).clamp(2, 40),
            snap_sensitivity: i("snap_sensitivity", 5).clamp(1, 10),
            span_monitors: b("span_monitors", true),
            detect_windows: b("detect_windows", true),
            detect_all_windows: b("detect_all_windows", true),
            window_frame: b("window_frame", true),
            window_shadow: b("window_shadow", true),
            show_codes: b("show_codes", true),
            toolbar_follows_mouse: b("toolbar_follows_mouse", true),
            backend: one_of(get_str(data, "backend"), BACKENDS, "auto"),
            screenshot_method: one_of(get_str(data, "screenshot_method"), SCREENSHOT_METHODS, "screens"),
            include_pointer: b("include_pointer", false),
            region_pointer: one_of(get_str(data, "region_pointer"), REGION_POINTER, "hidden"),
            default_tool: s("default_tool", "region"),
            default_color: i("default_color", 0),
            custom_color,
            keys,
            default_size: i("default_size", 1),
            record_dir: if record_dir.is_empty() { default_record_dir() } else { expand_user(&record_dir) },
            record_filename: s("record_filename", "Recording_%Y-%m-%d_%H-%M-%S"),
            record_format: one_of(get_str(data, "record_format"), RECORD_FORMATS, "mp4"),
            record_fps: if RECORD_FPS.contains(&record_fps) { record_fps } else { 30 },
            record_mic: b("record_mic", false),
            record_system_audio: b("record_system_audio", false),
            record_cursor: b("record_cursor", true),
            record_countdown: i("record_countdown", 3).clamp(0, 10),
        }
    }

    /// Everything as JSON, in the order the Python version wrote it.
    pub fn to_map(&self) -> Map<String, Value> {
        let mut m = Map::new();
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        put("save_to_disk", self.save_to_disk.into());
        put("save_dir", self.save_dir.clone().into());
        put("filename", self.filename.clone().into());
        put("format", self.format.clone().into());
        put("quality", self.quality.into());
        put("clipboard", self.clipboard.clone().into());
        put("notify", self.notify.into());
        put("open_after", self.open_after.clone().into());
        put("run_command", self.run_command.clone().into());
        put("sound", self.sound.into());
        put("sound_file", self.sound_file.clone().into());
        put("beautify", self.beautify.into());
        put("beautify_style", Value::Object(self.beautify_style.clone()));
        put("beautify_presets", Value::Object(self.beautify_presets.clone()));
        put("theme", self.theme.clone().into());
        put("tray_icon", self.tray_icon.clone().into());
        put("dim_opacity", self.dim_opacity.into());
        put("show_loupe", self.show_loupe.into());
        put("loupe_size", self.loupe_size.into());
        put("show_crosshair", self.show_crosshair.into());
        put("show_hint", self.show_hint.into());
        put("rainbow", self.rainbow.into());
        put("snap_edges", self.snap_edges.into());
        put("snap_distance", self.snap_distance.into());
        put("snap_sensitivity", self.snap_sensitivity.into());
        put("span_monitors", self.span_monitors.into());
        put("detect_windows", self.detect_windows.into());
        put("detect_all_windows", self.detect_all_windows.into());
        put("window_frame", self.window_frame.into());
        put("window_shadow", self.window_shadow.into());
        put("show_codes", self.show_codes.into());
        put("toolbar_follows_mouse", self.toolbar_follows_mouse.into());
        put("backend", self.backend.clone().into());
        put("screenshot_method", self.screenshot_method.clone().into());
        put("include_pointer", self.include_pointer.into());
        put("region_pointer", self.region_pointer.clone().into());
        put("default_tool", self.default_tool.clone().into());
        put("default_color", self.default_color.into());
        put("custom_color", self.custom_color.clone().into());
        put("keys", Value::Object(self.keys.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect()));
        put("default_size", self.default_size.into());
        put("record_dir", self.record_dir.clone().into());
        put("record_filename", self.record_filename.clone().into());
        put("record_format", self.record_format.clone().into());
        put("record_fps", self.record_fps.into());
        put("record_mic", self.record_mic.into());
        put("record_system_audio", self.record_system_audio.into());
        put("record_cursor", self.record_cursor.into());
        put("record_countdown", self.record_countdown.into());
        m
    }

    /// Clipboard-path, open or a command need a file even when not keeping one.
    pub fn needs_file(&self) -> bool {
        self.save_to_disk || self.clipboard == "path" || self.open_after != "none" || !self.run_command.is_empty()
    }

    pub fn save(&self) -> std::io::Result<()> {
        write_json(&config_path(), &self.to_map())
    }

    /// The pretty-printed settings in effect (`--print-config`).
    pub fn dump(&self) -> String {
        serde_json::to_string_pretty(&Value::Object(self.to_map())).unwrap_or_default()
    }
}

pub fn is_hex_color(s: &str) -> bool {
    s.len() == 7 && s.starts_with('#') && s[1..].bytes().all(|c| c.is_ascii_hexdigit())
}

/// Load the settings. A missing or unreadable file gives the defaults.
pub fn load() -> Config {
    adopt_python_settings();
    Config::from_map(&read_json(&config_path()))
}

/// Change some settings on disk without disturbing the others: read what's
/// there now, apply `change`, write it back.
pub fn update(change: impl FnOnce(&mut Config)) {
    let mut saved = load();
    change(&mut saved);
    if let Err(e) = saved.save() {
        crate::warn!("could not save the settings: {e}");
    }
}

// -- paths ---------------------------------------------------------------------

pub fn home() -> PathBuf {
    if let Some(h) = std::env::var_os("HOME").filter(|h| !h.is_empty()) {
        return PathBuf::from(h);
    }
    // No $HOME: ask the password database.
    let uid = rustix::process::getuid().as_raw();
    // SAFETY: passwd is plain data; all zeroes is a valid (empty) value.
    let mut pwd: libc::passwd = unsafe { std::mem::zeroed() };
    let mut buf = vec![0 as libc::c_char; 4096];
    let mut result: *mut libc::passwd = std::ptr::null_mut();
    // SAFETY: getpwuid_r writes into `pwd` and `buf`, which outlive the call;
    // `result` is only read when the call reports success.
    let rc = unsafe { libc::getpwuid_r(uid, &mut pwd, buf.as_mut_ptr(), buf.len(), &mut result) };
    if rc == 0 && !result.is_null() && !pwd.pw_dir.is_null() {
        // SAFETY: pw_dir points into `buf`, a NUL-terminated string.
        let dir = unsafe { std::ffi::CStr::from_ptr(pwd.pw_dir) };
        use std::os::unix::ffi::OsStrExt;
        return PathBuf::from(std::ffi::OsStr::from_bytes(dir.to_bytes()));
    }
    PathBuf::from("/")
}

fn xdg_dir(var: &str, fallback: &str) -> PathBuf {
    // (unit tests never touch the real settings)
    #[cfg(test)]
    if var != "XDG_DATA_HOME" {
        return test_home().join(fallback);
    }
    match std::env::var_os(var) {
        // The spec says relative values are invalid and must be ignored.
        Some(v) if Path::new(&v).is_absolute() => PathBuf::from(v),
        _ => home().join(fallback),
    }
}

#[cfg(test)]
fn test_home() -> PathBuf {
    static DIR: std::sync::OnceLock<PathBuf> = std::sync::OnceLock::new();
    DIR.get_or_init(|| std::env::temp_dir().join(format!("flatshot-test-{}", std::process::id()))).clone()
}

pub fn config_home() -> PathBuf {
    xdg_dir("XDG_CONFIG_HOME", ".config")
}

pub fn state_home() -> PathBuf {
    xdg_dir("XDG_STATE_HOME", ".local/state")
}

pub fn cache_home() -> PathBuf {
    xdg_dir("XDG_CACHE_HOME", ".cache")
}

pub fn data_home() -> PathBuf {
    xdg_dir("XDG_DATA_HOME", ".local/share")
}

/// Where shared data is looked for: the user's own first, then the system's.
pub fn data_dirs() -> Vec<PathBuf> {
    let rest = std::env::var("XDG_DATA_DIRS").ok().filter(|v| !v.is_empty()).unwrap_or_else(|| "/usr/local/share:/usr/share".into());
    let mut dirs = vec![data_home()];
    for d in rest.split(':').map(PathBuf::from).filter(|d| d.is_absolute()) {
        if !dirs.contains(&d) {
            dirs.push(d);
        }
    }
    dirs
}

pub fn config_path() -> PathBuf {
    config_home().join(crate::APP_ID).join("config.json")
}

pub fn state_path() -> PathBuf {
    state_home().join(crate::APP_ID).join("state.json")
}

/// With no settings of our own yet, a copy of the Python Flatshot's (and
/// what it remembered), once.
fn adopt_python_settings() {
    static ONCE: std::sync::Once = std::sync::Once::new();
    ONCE.call_once(|| {
        for (ours, theirs) in
            [(config_path(), config_home().join("flatshot").join("config.json")), (state_path(), state_home().join("flatshot").join("state.json"))]
        {
            if fs::symlink_metadata(&ours).is_ok() {
                continue;
            }
            let Ok(text) = crate::util::read_limited(&theirs, 4 << 20) else { continue };
            let Ok(Value::Object(map)) = serde_json::from_slice::<Value>(&text) else { continue };
            if write_json(&ours, &map).is_ok() {
                crate::warn!("took the Python Flatshot's {} as a start", theirs.display());
            }
        }
    });
}

/// "~" and "~/..." to the home folder, as `os.path.expanduser`.
pub fn expand_user(path: &str) -> String {
    if path == "~" {
        return home().to_string_lossy().into_owned();
    }
    if let Some(rest) = path.strip_prefix("~/") {
        return home().join(rest).to_string_lossy().into_owned();
    }
    path.to_string()
}

/// A folder from `user-dirs.dirs` (XDG_PICTURES_DIR, ...), as Qt's
/// QStandardPaths reads it.
pub fn user_dir(name: &str) -> Option<PathBuf> {
    let text = fs::read_to_string(config_home().join("user-dirs.dirs")).ok()?;
    for line in text.lines() {
        let line = line.trim();
        let Some(value) = line.strip_prefix(name).and_then(|r| r.strip_prefix('=')) else {
            continue;
        };
        let value = value.trim().trim_matches('"');
        let path = if let Some(rest) = value.strip_prefix("$HOME") {
            let rest = rest.trim_start_matches('/');
            if rest.is_empty() { home() } else { home().join(rest) }
        } else if value.starts_with('/') {
            PathBuf::from(value)
        } else {
            continue;
        };
        return Some(path);
    }
    None
}

fn default_dir() -> String {
    let pictures = user_dir("XDG_PICTURES_DIR").unwrap_or_else(|| home().join("Pictures"));
    pictures.join("Screenshots").to_string_lossy().into_owned()
}

fn default_record_dir() -> String {
    let videos = user_dir("XDG_VIDEOS_DIR").unwrap_or_else(|| home().join("Videos"));
    videos.join("Screencasts").to_string_lossy().into_owned()
}

// -- files ---------------------------------------------------------------------

/// Create `dir` (and its parents) readable by this user only, if missing.
pub fn private_dir(dir: &Path) -> std::io::Result<()> {
    if dir.is_dir() {
        return Ok(());
    }
    fs::DirBuilder::new().recursive(true).mode(0o700).create(dir)
}

/// Write JSON so a reader never sees half a file: into a new file beside
/// it (mode 0600), then renamed over it.
pub fn write_json(path: &Path, data: &Map<String, Value>) -> std::io::Result<()> {
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no parent folder"))?;
    private_dir(dir)?;
    let mut text = serde_json::to_string_pretty(&Value::Object(data.clone())).map_err(std::io::Error::other)?;
    text.push('\n');
    let tmp = dir.join(format!(".tmp-{}.json", crate::util::random_hex(8)));
    let result = (|| {
        let mut f = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        f.write_all(text.as_bytes())?;
        f.set_permissions(fs::Permissions::from_mode(0o600))?;
        drop(f);
        fs::rename(&tmp, path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The JSON object in `path`; empty when it's missing or not an object.
/// Files over 4 MiB aren't read: settings are a few hundred bytes.
pub fn read_json(path: &Path) -> Map<String, Value> {
    let text = match crate::util::read_limited(path, 4 << 20) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Map::new(),
        Err(e) => {
            crate::warn!("ignoring {}: {e}", path.display());
            return Map::new();
        }
    };
    match serde_json::from_slice::<Value>(&text) {
        Ok(Value::Object(m)) => m,
        Ok(_) => Map::new(),
        Err(e) => {
            crate::warn!("ignoring {}: {e}", path.display());
            Map::new()
        }
    }
}

/// Small bits Flatshot remembers between runs (last region, counter,
/// screen-cast restore tokens).
pub fn load_state() -> Map<String, Value> {
    read_json(&state_path())
}

pub fn update_state(changes: Vec<(&str, Value)>) {
    let mut state = load_state();
    for (k, v) in changes {
        state.insert(k.to_string(), v);
    }
    if let Err(e) = write_json(&state_path(), &state) {
        crate::warn!("could not save {}: {e}", state_path().display());
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn cfg(v: Value) -> Config {
        Config::from_map(v.as_object().unwrap())
    }

    #[test]
    fn defaults_match_the_python_version() {
        let c = Config::from_map(&Map::new());
        assert!(c.save_to_disk && c.notify && c.show_loupe && c.show_codes);
        assert_eq!(c.filename, "Screenshot_%Y-%m-%d_%H-%M-%S");
        assert_eq!((c.quality, c.dim_opacity, c.loupe_size), (90, 60, 120));
        assert_eq!((c.clipboard.as_str(), c.open_after.as_str(), c.backend.as_str()), ("image", "none", "auto"));
        assert_eq!(c.custom_color, "#FF4FA3");
        assert!(c.save_dir.ends_with("Screenshots"));
        assert!(c.record_dir.ends_with("Screencasts"));
    }

    #[test]
    fn values_are_cleaned_up() {
        let c = cfg(json!({"dim_opacity": 150, "quality": "0", "loupe_size": 9999, "format": ".JPEG",
                            "filename": "shot_%H.png", "custom_color": "#abcdef", "record_fps": 25,
                            "snap_distance": 1, "clipboard": "bogus", "copy_to_clipboard": false,
                            "keys": {"tool.pen": "J", "bad": 3}}));
        assert_eq!(c.dim_opacity, 90);
        assert_eq!(c.quality, 1);
        assert_eq!(c.loupe_size, 320);
        assert_eq!(c.format, "jpg");
        assert_eq!(c.filename, "shot_%H");
        assert_eq!(c.custom_color, "#ABCDEF");
        assert_eq!(c.record_fps, 30);
        assert_eq!(c.snap_distance, 2);
        assert_eq!(c.clipboard, "none");
        assert_eq!(c.keys.len(), 1);
    }

    #[test]
    fn one_bad_value_keeps_the_rest() {
        let c = cfg(json!({"notify": [1, 2], "theme": "nord", "dim_opacity": "40"}));
        assert!(c.notify);
        assert_eq!(c.theme, "nord");
        assert_eq!(c.dim_opacity, 40);
    }

    #[test]
    fn round_trip() {
        let c = cfg(json!({"theme": "dusk", "keys": {"tool.pen": "J"}, "save_dir": "/x/y"}));
        let again = Config::from_map(&c.to_map());
        assert_eq!(c, again);
    }
}
