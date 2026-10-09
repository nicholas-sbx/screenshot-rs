//! A short sound after a capture, played by whatever audio player the
//! desktop already has. Fire-and-forget: it never delays the capture, and
//! nothing goes through a shell.

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

use crate::util;

/// The desktop's own screenshot sound, if a sound theme has one.
const THEME_SOUNDS: &[&str] = &[
    "/usr/share/sounds/ocean/stereo/screen-capture.oga", // KDE Plasma 6
    "/usr/share/sounds/freedesktop/stereo/screen-capture.oga",
    "/usr/share/sounds/freedesktop/stereo/camera-shutter.oga",
];

/// Players that take a file and exit when it's played.
const PLAYERS: &[&[&str]] = &[
    &["pw-play"], // PipeWire
    &["paplay"],  // PulseAudio (and pipewire-pulse)
    &["ffplay", "-nodisp", "-autoexit", "-loglevel", "quiet"],
];

pub const SOUND_EXTENSIONS: &[&str] = &["oga", "ogg", "wav", "flac", "mp3", "opus"];

pub fn default_sound() -> Option<&'static str> {
    THEME_SOUNDS.iter().copied().find(|p| Path::new(p).is_file())
}

/// Play `path`, or the desktop's screenshot sound when it's empty. False
/// when there's no file, or nothing to play it with.
pub fn play(path: &str) -> bool {
    if path.is_empty() && default_sound().is_none() && util::have("canberra-gtk-play") {
        // No theme file we know of: let libcanberra find the theme's sound.
        return util::spawn(&["canberra-gtk-play", "-i", "screen-capture", "-d", "Flatshot"]);
    }
    let target = if path.is_empty() { PathBuf::from(default_sound().unwrap_or_default()) } else { PathBuf::from(crate::config::expand_user(path)) };
    // Absolute, so it can't be taken for one of the player's options.
    let Ok(target) = target.canonicalize() else { return false };
    if !target.is_file() {
        return false;
    }
    let wav = target.extension().is_some_and(|e| e.eq_ignore_ascii_case("wav"));
    let aplay: &[&str] = &["aplay", "-q"];
    for argv in PLAYERS.iter().copied().chain(wav.then_some(aplay)) {
        if util::have(argv[0]) {
            let mut full: Vec<&OsStr> = argv.iter().map(OsStr::new).collect();
            full.push(target.as_os_str());
            return util::spawn_detached(&full, &[]);
        }
    }
    false
}

/// A sound as KDE's notification settings name it: a file, a file:// URL,
/// or a name in the sound themes (/usr/share/sounds).
pub fn find(name: &str) -> Option<PathBuf> {
    let name = name.strip_prefix("file://").unwrap_or(name);
    if name.is_empty() {
        return None;
    }
    if name.starts_with('/') {
        return Some(PathBuf::from(name));
    }
    if name.contains("..") {
        return None;
    }
    let wanted = Path::new(name);
    let has_ext = wanted.extension().is_some();
    for dir in crate::config::data_dirs() {
        let mut budget = 5000;
        if let Some(p) = search(&dir.join("sounds"), wanted, has_ext, 0, &mut budget) {
            return Some(p);
        }
    }
    None
}

/// A depth-first look for `wanted` under `dir`, a few levels deep at most.
fn search(dir: &Path, wanted: &Path, has_ext: bool, depth: u32, budget: &mut u32) -> Option<PathBuf> {
    if depth > 4 {
        return None;
    }
    let mut dirs = Vec::new();
    for entry in std::fs::read_dir(dir).ok()?.flatten() {
        *budget = budget.checked_sub(1)?;
        let path = entry.path();
        let Ok(kind) = entry.file_type() else { continue };
        if kind.is_dir() {
            dirs.push(path);
            continue;
        }
        let ext_ok = path.extension().and_then(|e| e.to_str()).is_some_and(|e| SOUND_EXTENSIONS.contains(&e.to_ascii_lowercase().as_str()));
        if !ext_ok {
            continue;
        }
        let matches = if has_ext { path.file_name() == wanted.file_name() } else { path.file_stem() == wanted.file_name() };
        if matches {
            return Some(path);
        }
    }
    dirs.sort();
    dirs.iter().find_map(|d| search(d, wanted, has_ext, depth + 1, budget))
}
