//! Folder and file name templates: strftime codes plus {tokens}.
//!
//! Token values come from other programs (a window's title, its app), so
//! they are cleaned before use: no slashes, no control characters and no
//! leading dots, so a title can never point outside the folder.

use std::path::{Component, Path, PathBuf};

use serde_json::Value;

use crate::config::{self, Config};

/// What was captured; fills the {tokens} in folder and file name templates.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct Shot {
    /// region | window | monitor | desktop
    pub mode: String,
    /// the captured window's app, else the active one (when known)
    pub app: String,
    pub title: String,
    /// connector name (DP-1, ...) when the capture is one monitor
    pub monitor: String,
    /// (for the Recent captures list) QR codes and barcodes in it, and
    /// the drawings made on it
    pub codes: usize,
    pub drawings: usize,
}

impl Shot {
    pub fn new(mode: &str) -> Shot {
        Shot { mode: mode.to_string(), ..Default::default() }
    }
}

const LBRACE: char = '\u{E000}';
const RBRACE: char = '\u{E001}';

/// A token value made safe inside one path component.
pub fn clean(value: &str) -> String {
    let mut s = String::with_capacity(value.len());
    let mut in_unsafe = false;
    for c in value.chars() {
        // (controls, C1 ones too, and the invisible characters that turn
        // text around or hide in it: "photo\u{202E}gnp.exe" isn't a PNG)
        let invisible = matches!(c, '\u{200B}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}' | '\u{061C}');
        let unsafe_char = c.is_control() || invisible || c == '/' || c == '\\' || c == LBRACE || c == RBRACE;
        if unsafe_char {
            if !in_unsafe {
                s.push('_');
            }
            in_unsafe = true;
        } else {
            s.push(c);
            in_unsafe = false;
        }
    }
    let joined = s.split_whitespace().collect::<Vec<_>>().join(" ");
    let trimmed = joined.trim_matches(|c| c == ' ' || c == '.' || c == '_');
    let cut: String = trimmed.chars().take(80).collect();
    cut.trim_end_matches([' ', '.', '_']).to_string()
}

/// `fmt` through the C library's strftime for the local time `t`, with
/// Python's own handling of %f, %z and %Z for a naive local datetime.
pub fn strftime(fmt: &str, t: libc::time_t, micros: u32) -> String {
    // Python's datetime.strftime fills %f itself and, for a time without a
    // zone, makes %z and %Z empty, before the C library sees the rest.
    let mut pre = String::with_capacity(fmt.len());
    let mut chars = fmt.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '%' {
            pre.push(c);
            continue;
        }
        match chars.next() {
            Some('f') => pre.push_str(&format!("{micros:06}")),
            Some('z') | Some('Z') => {}
            Some(':') if chars.peek() == Some(&'z') => {
                chars.next();
            }
            Some(other) => {
                pre.push('%');
                pre.push(other);
            }
            None => pre.push('%'),
        }
    }
    // SAFETY: tm is plain data; all zeroes is a valid value.
    let mut tm: libc::tm = unsafe { std::mem::zeroed() };
    // SAFETY: localtime_r fills `tm` from `t`; both are valid locals.
    unsafe { libc::localtime_r(&t, &mut tm) };
    // strftime can't tell "empty result" from "buffer too small": try
    // growing buffers, and give up on absurdly long expansions.
    let Ok(cfmt) = std::ffi::CString::new(pre.replace('\0', "")) else {
        return String::new();
    };
    if pre.is_empty() {
        return String::new();
    }
    let mut size = 256usize.max(pre.len() * 4);
    while size <= 1 << 16 {
        let mut buf = vec![0u8; size];
        // SAFETY: buf has `size` bytes; strftime writes at most that many.
        let n = unsafe { libc::strftime(buf.as_mut_ptr().cast(), size, cfmt.as_ptr(), &tm) };
        if n > 0 {
            buf.truncate(n);
            return String::from_utf8_lossy(&buf).into_owned();
        }
        size *= 4;
    }
    String::new()
}

/// Matches `{app}` ... `{n:4}` at the start of `s`: (token, width, length).
fn token_at(s: &str) -> Option<(&str, Option<usize>, usize)> {
    let rest = s.strip_prefix('{')?;
    let end = rest.find('}')?;
    let inner = &rest[..end];
    let (name, width) = match inner.split_once(':') {
        Some((n, w)) => {
            if w.is_empty() || w.len() > 2 || !w.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            (n, Some(w.parse().ok()?))
        }
        None => (inner, None),
    };
    if !matches!(name, "app" | "title" | "mode" | "monitor" | "w" | "h" | "n") {
        return None;
    }
    Some((name, width, end + 2))
}

/// The counter for {n}: the next number, used up unless `preview`.
fn counter(preview: bool) -> u64 {
    let state = config::load_state();
    let n = state.get("counter").and_then(Value::as_u64).unwrap_or(0) + 1;
    if !preview {
        config::update_state(vec![("counter", Value::from(n))]);
    }
    n
}

struct Filler<'a> {
    shot: &'a Shot,
    size: (u32, u32),
    preview: bool,
    number: Option<u64>,
    now: libc::time_t,
    micros: u32,
}

impl Filler<'_> {
    fn value(&mut self, name: &str, width: Option<usize>) -> String {
        let text = match name {
            "n" => {
                // One number per capture, even if used twice.
                let n = *self.number.get_or_insert_with(|| counter(self.preview));
                n.to_string()
            }
            "w" => self.size.0.to_string(),
            "h" => self.size.1.to_string(),
            "app" => self.shot.app.clone(),
            "title" => self.shot.title.clone(),
            "mode" => self.shot.mode.clone(),
            "monitor" => self.shot.monitor.clone(),
            _ => String::new(),
        };
        let text = clean(&text);
        match width {
            Some(w) if !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit()) && text.len() < w => {
                format!("{}{}", "0".repeat(w - text.len()), text)
            }
            _ => text,
        }
    }

    fn fill(&mut self, template: &str) -> String {
        let escaped = template.replace("{{", &LBRACE.to_string()).replace("}}", &RBRACE.to_string());
        let timed = strftime(&escaped, self.now, self.micros);
        let mut out = String::with_capacity(timed.len());
        let mut i = 0;
        while i < timed.len() {
            let rest = &timed[i..];
            if rest.starts_with('{')
                && let Some((name, width, len)) = token_at(rest)
            {
                let v = self.value(name, width);
                out.push_str(&v);
                i += len;
                continue;
            }
            let c = rest.chars().next().unwrap();
            out.push(c);
            i += c.len_utf8();
        }
        out.replace(LBRACE, "{").replace(RBRACE, "}")
    }
}

/// Expand the folder and file name templates and add the extension.
/// `preview`: don't use up a {n}.
pub fn target_path(save_dir: &str, filename: &str, shot: &Shot, size: (u32, u32), ext: &str, preview: bool) -> PathBuf {
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let mut f = Filler { shot, size, preview, number: None, now: now.as_secs() as libc::time_t, micros: now.subsec_micros() };
    let folder = PathBuf::from(config::expand_user(&f.fill(save_dir)));
    // Relative to the folder, even when an empty token leads it ("{app}/...").
    let filled = f.fill(filename);
    let filled = filled.trim().trim_start_matches('/');
    let filled = if filled.is_empty() { "Screenshot" } else { filled };
    let mut name = normalize(Path::new(filled));
    match name.file_name().map(|n| n.to_string_lossy().into_owned()) {
        None => name.push("Screenshot"),
        Some(last) if last.starts_with('.') => {
            name.set_file_name(format!("Screenshot{last}"));
        }
        _ => {}
    }
    let mut file = name.into_os_string();
    file.push(".");
    file.push(ext);
    folder.join(file)
}

/// Where a capture goes under `cfg`, for a picture of `size`.
pub fn screenshot_path(cfg: &Config, shot: &Shot, size: (u32, u32), ext: &str, preview: bool) -> PathBuf {
    target_path(&cfg.save_dir, &cfg.filename, shot, size, ext, preview)
}

/// As pathlib does: drop "." parts and doubled slashes.
fn normalize(p: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for c in p.components() {
        match c {
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// The folder template's fixed part: ~/Pictures/Screenshots for
/// ~/Pictures/Screenshots/%Y/{app}.
pub fn base_folder(save_dir: &str) -> PathBuf {
    let expanded = config::expand_user(save_dir);
    let mut parts = PathBuf::new();
    let mut any = false;
    for c in Path::new(&expanded).components() {
        let part = c.as_os_str().to_string_lossy();
        let plain = part.replace("%%", "").replace("{{", "").replace("}}", "");
        if plain.contains('%') || has_token(&plain) {
            break;
        }
        parts.push(part.replace("%%", "%").replace("{{", "{").replace("}}", "}"));
        any = true;
    }
    if any { parts } else { config::home() }
}

fn has_token(s: &str) -> bool {
    s.char_indices().any(|(i, c)| c == '{' && token_at(&s[i..]).is_some())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cleaning() {
        assert_eq!(clean("../../.bashrc"), "bashrc");
        assert_eq!(clean("a/b\\c"), "a_b_c");
        assert_eq!(clean("  Notes — Kate  "), "Notes — Kate");
        assert_eq!(clean("..."), "");
        assert_eq!(clean("x\n\ty"), "x_y");
        assert_eq!(clean(&"a".repeat(100)).len(), 80);
        assert_eq!(clean("photo\u{202E}gnp.exe"), "photo_gnp.exe");
        assert_eq!(clean("a\u{9b}31mb"), "a_31mb");
        assert_eq!(clean("👨\u{200D}👩"), "👨\u{200D}👩");
    }

    #[test]
    fn templates() {
        let shot = Shot { mode: "region".into(), app: "firefox".into(), title: "../etc/passwd".into(), monitor: "DP-1".into(), ..Default::default() };
        let p = target_path("/shots/{app}", "{title}_{mode}_{w}x{h}", &shot, (800, 600), "png", true);
        assert_eq!(p, PathBuf::from("/shots/firefox/etc_passwd_region_800x600.png"));
        let p = target_path("/shots", "{{literal}}_100%%", &shot, (1, 1), "jpg", true);
        assert_eq!(p, PathBuf::from("/shots/{literal}_100%.jpg"));
        let p = target_path("/shots", "{app}/.hidden", &Shot::default(), (1, 1), "png", true);
        assert_eq!(p, PathBuf::from("/shots/Screenshot.hidden.png"));
        let p = target_path("/shots", "   ", &Shot::default(), (1, 1), "png", true);
        assert_eq!(p, PathBuf::from("/shots/Screenshot.png"));
        let p = target_path("/shots", "{w:5}", &shot, (800, 600), "png", true);
        assert_eq!(p, PathBuf::from("/shots/00800.png"));
    }

    #[test]
    fn dates() {
        let s = strftime("%Y-%m-%d %%Y %f%z", 0, 42);
        assert!(s.ends_with(" %Y 000042"), "{s}");
        assert_eq!(s.len(), "1970-01-01 %Y 000042".len());
    }

    #[test]
    fn base() {
        assert_eq!(base_folder("/a/b/%Y/{app}"), PathBuf::from("/a/b"));
        assert_eq!(base_folder("/a/100%%/c"), PathBuf::from("/a/100%/c"));
    }
}
