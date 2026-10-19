//! The captures saved lately, for the Recent captures window: each file,
//! and what's known about it (what was captured, from which window, how
//! big, what went on the clipboard...). Only this list is kept, in the
//! state folder, never another copy of a capture. Whatever saves a capture
//! (the tray app, a one-off run, the editor) adds a line to it, under a
//! lock, so they can't lose each other's.

use std::fs::{self, File};
use std::io::{Read, Write};
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value};

use crate::config;
use crate::geom::IRect;

/// The most kept: older ones go.
pub const MAX: usize = 100;
/// The most read (a line is a few hundred bytes).
const MAX_FILE: u64 = 1 << 20;

/// A capture saved, and what's known about it.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Entry {
    pub path: PathBuf,
    /// when it was saved, in ms since 1970
    pub time: u64,
    /// region | window | monitor | desktop | recording | edited
    pub kind: String,
    pub width: u32,
    pub height: u32,
    /// the file's size
    pub bytes: u64,
    /// the window it was taken from (else the active one), when known
    pub app: String,
    pub title: String,
    /// the monitor, when it was one monitor's
    pub monitor: String,
    /// where on the desktop it was (global logical pixels)
    pub area: Option<IRect>,
    /// what went on the clipboard: "image", "path" or ""
    pub copied: String,
    /// QR codes and barcodes in it
    pub codes: usize,
    /// drawings made on it
    pub drawings: usize,
    pub beautified: bool,
    /// changed in the editor since
    pub edited: bool,
    /// a recording's length, in seconds
    pub seconds: Option<f64>,
    /// a picture of it (a recording's first frame)
    pub thumbnail: Option<PathBuf>,
}

impl Entry {
    pub fn is_video(&self) -> bool {
        self.kind == "recording"
    }

    fn to_json(&self) -> Value {
        let mut m = Map::new();
        let mut put = |k: &str, v: Value| {
            m.insert(k.to_string(), v);
        };
        put("path", self.path.to_string_lossy().into_owned().into());
        put("time", self.time.into());
        put("kind", self.kind.clone().into());
        put("width", self.width.into());
        put("height", self.height.into());
        put("bytes", self.bytes.into());
        for (k, v) in [("app", &self.app), ("title", &self.title), ("monitor", &self.monitor), ("copied", &self.copied)] {
            if !v.is_empty() {
                put(k, v.clone().into());
            }
        }
        if let Some(a) = self.area {
            put("area", Value::from(vec![a.x, a.y, a.w, a.h]));
        }
        if self.codes > 0 {
            put("codes", self.codes.into());
        }
        if self.drawings > 0 {
            put("drawings", self.drawings.into());
        }
        if self.beautified {
            put("beautified", true.into());
        }
        if self.edited {
            put("edited", true.into());
        }
        if let Some(s) = self.seconds {
            put("seconds", s.into());
        }
        if let Some(t) = &self.thumbnail {
            put("thumbnail", t.to_string_lossy().into_owned().into());
        }
        Value::Object(m)
    }

    /// An entry from a line of the list (None if it isn't one).
    fn from_json(v: &Value) -> Option<Entry> {
        let m = v.as_object()?;
        let text = |k: &str| m.get(k).and_then(Value::as_str).map(|s| s.chars().take(500).collect::<String>()).unwrap_or_default();
        let int = |k: &str| m.get(k).and_then(Value::as_u64).unwrap_or(0);
        let path = PathBuf::from(m.get("path")?.as_str()?);
        if !path.is_absolute() {
            return None;
        }
        let area = m.get("area").and_then(Value::as_array).and_then(|a| {
            let n: Vec<i32> = a.iter().filter_map(|v| v.as_i64()).map(|v| v.clamp(-1 << 20, 1 << 20) as i32).collect();
            (n.len() == 4).then(|| IRect::new(n[0], n[1], n[2], n[3]))
        });
        let thumbnail = m.get("thumbnail").and_then(Value::as_str).map(PathBuf::from).filter(|p| p.is_absolute());
        Some(Entry {
            path,
            time: int("time"),
            kind: text("kind"),
            width: int("width").min(1 << 20) as u32,
            height: int("height").min(1 << 20) as u32,
            bytes: int("bytes"),
            app: text("app"),
            title: text("title"),
            monitor: text("monitor"),
            area,
            copied: text("copied"),
            codes: int("codes").min(1000) as usize,
            drawings: int("drawings").min(100_000) as usize,
            beautified: m.get("beautified").and_then(Value::as_bool).unwrap_or(false),
            edited: m.get("edited").and_then(Value::as_bool).unwrap_or(false),
            seconds: m.get("seconds").and_then(Value::as_f64).filter(|s| s.is_finite() && *s >= 0.0),
            thumbnail,
        })
    }
}

/// The list's file.
pub fn file() -> PathBuf {
    config::state_home().join(crate::APP_ID).join("recent.jsonl")
}

/// Add a capture saved just now.
pub fn add(entry: &Entry) {
    if let Err(e) = change(|lines| lines.push(entry.to_json().to_string())) {
        crate::warn!("could not add to the recent captures: {e}");
    }
}

/// The picture at `path` was saved from the editor (now `size`).
pub fn edited(path: &Path, size: (u32, u32)) {
    let bytes = fs::metadata(path).map(|m| m.len()).unwrap_or(0);
    let e = Entry {
        path: path.to_path_buf(),
        time: crate::util::now_ms(),
        kind: "edited".into(),
        width: size.0,
        height: size.1,
        bytes,
        edited: true,
        ..Default::default()
    };
    add(&e);
}

/// Take these off the list (their files stay).
pub fn remove(paths: &[PathBuf]) {
    let result = change(|lines| {
        let entries = merge(lines);
        lines.clear();
        lines.extend(entries.iter().rev().filter(|e| !paths.contains(&e.path)).map(|e| e.to_json().to_string()));
    });
    if let Err(e) = result {
        crate::warn!("could not change the recent captures: {e}");
    }
}

/// Empty the list (the files stay).
pub fn clear() {
    if let Err(e) = change(Vec::clear) {
        crate::warn!("could not clear the recent captures: {e}");
    }
}

/// The captures, newest first, at most MAX.
pub fn load() -> Vec<Entry> {
    read(&file()).map(|text| parse(&text)).unwrap_or_default()
}

/// The list's text (a line per change, oldest first) as entries, newest
/// first; lines that aren't entries are left out.
pub fn parse(text: &str) -> Vec<Entry> {
    merge(&text.lines().map(str::to_string).collect::<Vec<_>>())
}

/// When the list last changed (to know whether to read it again).
pub fn changed() -> Option<std::time::SystemTime> {
    fs::metadata(file()).and_then(|m| m.modified()).ok()
}

/// The list's lines (oldest first) merged into entries, newest first: a
/// file saved again from the editor is one entry, edited, as of then.
fn merge(lines: &[String]) -> Vec<Entry> {
    let mut entries: Vec<Entry> = Vec::new();
    for e in lines.iter().filter_map(|l| serde_json::from_str::<Value>(l).ok()).filter_map(|v| Entry::from_json(&v)) {
        match entries.iter().position(|x| x.path == e.path) {
            Some(i) if e.kind == "edited" => {
                let mut was = entries.remove(i);
                (was.time, was.width, was.height, was.bytes, was.edited) = (e.time, e.width, e.height, e.bytes, true);
                entries.push(was);
            }
            Some(i) => {
                entries.remove(i);
                entries.push(e);
            }
            None => entries.push(e),
        }
    }
    entries.reverse();
    entries.truncate(MAX);
    entries
}

/// Change the list's lines (oldest first) under the lock, keeping at most
/// twice MAX of them on disk.
fn change(f: impl FnOnce(&mut Vec<String>)) -> std::io::Result<()> {
    let path = file();
    let dir = path.parent().ok_or_else(|| std::io::Error::other("no folder"))?;
    config::private_dir(dir)?;
    let _lock = lock(&dir.join("recent.lock"))?;
    let text = read(&path).unwrap_or_default();
    let mut lines: Vec<String> = text.lines().map(str::to_string).collect();
    let before = lines.clone();
    f(&mut lines);
    if lines.len() > 2 * MAX {
        let kept: Vec<String> = merge(&lines).iter().rev().map(|e| e.to_json().to_string()).collect();
        lines = kept;
    }
    if lines.len() >= before.len() && lines[..before.len()] == before[..] {
        // Only added to: appended.
        let mut out = fs::OpenOptions::new().append(true).create(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&path)?;
        let mut added = lines[before.len()..].join("\n");
        if !added.is_empty() {
            added.push('\n');
        }
        return out.write_all(added.as_bytes());
    }
    let tmp = dir.join(format!(".recent-{}.tmp", crate::util::random_hex(8)));
    let result = (|| {
        let mut out = fs::OpenOptions::new().write(true).create_new(true).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(&tmp)?;
        let mut all = lines.join("\n");
        if !all.is_empty() {
            all.push('\n');
        }
        out.write_all(all.as_bytes())?;
        fs::rename(&tmp, &path)
    })();
    if result.is_err() {
        let _ = fs::remove_file(&tmp);
    }
    result
}

/// The list's lock, held until the file is dropped.
fn lock(path: &Path) -> std::io::Result<File> {
    let f = fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).mode(0o600).custom_flags(libc::O_NOFOLLOW).open(path)?;
    rustix::fs::flock(&f, rustix::fs::FlockOperation::LockExclusive)?;
    Ok(f)
}

fn read(path: &Path) -> std::io::Result<String> {
    let f = fs::OpenOptions::new().read(true).custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK).open(path)?;
    if !f.metadata()?.is_file() {
        return Err(std::io::Error::other("not a file"));
    }
    let mut text = String::new();
    f.take(MAX_FILE).read_to_string(&mut text)?;
    Ok(text)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(path: &str, time: u64) -> Entry {
        Entry {
            path: PathBuf::from(path),
            time,
            kind: "region".into(),
            width: 640,
            height: 480,
            app: "firefox".into(),
            area: Some(IRect::new(1, 2, 3, 4)),
            ..Default::default()
        }
    }

    #[test]
    fn entries_go_to_json_and_back() {
        let mut e = entry("/a/b.png", 5);
        e.seconds = Some(2.5);
        e.codes = 2;
        e.thumbnail = Some(PathBuf::from("/t.png"));
        assert_eq!(Entry::from_json(&e.to_json()), Some(e));
        // Relative paths and junk are no entries.
        assert_eq!(Entry::from_json(&serde_json::json!({"path": "b.png"})), None);
        assert_eq!(Entry::from_json(&serde_json::json!([1, 2])), None);
    }

    #[test]
    fn lines_merge_newest_first_and_edits_join_their_capture() {
        let line = |e: &Entry| e.to_json().to_string();
        let edit = Entry { path: PathBuf::from("/a.png"), time: 30, kind: "edited".into(), width: 700, height: 500, edited: true, ..Default::default() };
        let lines = vec![line(&entry("/a.png", 10)), "not json".into(), line(&entry("/b.png", 20)), line(&edit)];
        let got = merge(&lines);
        assert_eq!(got.iter().map(|e| e.path.to_str().unwrap()).collect::<Vec<_>>(), ["/a.png", "/b.png"]);
        assert!(got[0].edited && got[0].width == 700 && got[0].time == 30 && got[0].app == "firefox");
        let many: Vec<String> = (0..MAX + 20).map(|i| line(&entry(&format!("/{i}.png"), i as u64))).collect();
        let got = merge(&many);
        assert_eq!(got.len(), MAX);
        assert_eq!(got[0].path, PathBuf::from(format!("/{}.png", MAX + 19)));
    }

    #[test]
    fn added_removed_and_cleared_on_disk() {
        // (config::state_home is a folder of the test run's own)
        clear();
        add(&entry("/x/1.png", 1));
        add(&entry("/x/2.png", 2));
        edited(Path::new("/x/1.png"), (10, 20));
        let got = load();
        assert_eq!(got.len(), 2);
        assert_eq!((got[0].path.to_str(), got[0].width, got[0].edited), (Some("/x/1.png"), 10, true));
        remove(&[PathBuf::from("/x/1.png")]);
        assert_eq!(load().iter().map(|e| e.path.clone()).collect::<Vec<_>>(), [PathBuf::from("/x/2.png")]);
        for i in 0..(2 * MAX + 5) {
            add(&entry(&format!("/y/{i}.png"), i as u64));
        }
        let text = read(&file()).unwrap();
        assert!(text.lines().count() <= 2 * MAX, "{} lines", text.lines().count());
        assert_eq!(load().len(), MAX);
        clear();
        assert!(load().is_empty());
    }
}
