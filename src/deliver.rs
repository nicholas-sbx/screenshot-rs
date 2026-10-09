//! Where a finished capture goes: a file, the clipboard (handed to the
//! platform), the viewer or file manager, the user's command.

use std::io::Write;
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::codec;
use crate::config::{self, Config};
use crate::image::Image;
use crate::template::{self, Shot};
use crate::timing::Clock;
use crate::util;

/// What came of delivering a capture.
#[derive(Debug, Default)]
pub struct Delivery {
    pub path: Option<PathBuf>,
    /// the file is in the screenshot folder (not a temporary one)
    pub saved: bool,
    pub size: (u32, u32),
    /// for the clipboard: (types, data)
    pub clip: Option<(Vec<String>, Arc<Vec<u8>>)>,
}

/// Run the configured after-capture steps (but notifying and the clipboard,
/// which the caller does). `explicit` is `-o`'s file ("-": PNG to stdout).
pub fn deliver(image: &Image, cfg: &Config, explicit: Option<&str>, shot: &Shot, clock: &mut Clock) -> Result<Delivery, String> {
    let mut result = Delivery { size: image.size(), ..Default::default() };
    if explicit == Some("-") {
        let png = codec::encode_png(image, false)?;
        let mut out = std::io::stdout().lock();
        out.write_all(&png).and_then(|()| out.flush()).map_err(|e| format!("stdout: {e}"))?;
        if cfg.clipboard == "image" {
            result.clip = Some((vec!["image/png".into()], Arc::new(png)));
        }
        return Ok(result);
    }
    let mut png: Option<Arc<Vec<u8>>> = None;
    if explicit.is_some() || cfg.save_to_disk {
        let (path, bytes, format) = save(image, cfg, explicit, shot)?;
        clock.step("saved", format!("{}, {}", format.to_uppercase(), human_size(bytes.len())));
        if format == "png" {
            png = Some(Arc::new(bytes));
        }
        result.path = Some(path);
        result.saved = true;
    } else if cfg.needs_file() {
        // The path on the clipboard, opening it or a command need a file
        // even when none is kept.
        let bytes = codec::encode_png(image, false)?;
        let path = write_unique(&temp_path()?, &bytes)?;
        clock.step("written to a temporary file", "");
        png = Some(Arc::new(bytes));
        result.path = Some(path);
    }
    match cfg.clipboard.as_str() {
        "image" => {
            let data = match png {
                Some(p) => p,
                None => Arc::new(codec::encode_png(image, false)?),
            };
            result.clip = Some((vec!["image/png".into()], data));
        }
        "path" => {
            if let Some(p) = &result.path {
                let text = p.to_string_lossy().into_owned().into_bytes();
                let mimes = crate::wayland::clipboard::TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                result.clip = Some((mimes, Arc::new(text)));
            }
        }
        _ => {}
    }
    if let Some(path) = &result.path {
        match cfg.open_after.as_str() {
            "image" => {
                open_file(path);
            }
            "folder" => {
                show_in_folder(path);
            }
            _ => {}
        }
        if !cfg.run_command.trim().is_empty() {
            run_command(&cfg.run_command, path);
        }
        if cfg.open_after != "none" || !cfg.run_command.trim().is_empty() {
            clock.step("opened / command started", "");
        }
    }
    Ok(result)
}

/// The format to write: `wanted` if it can be written here, else PNG.
fn writable(wanted: &str) -> &'static str {
    let wanted = if wanted == "jpeg" { "jpg" } else { wanted };
    match codec::writable_formats().iter().find(|f| f.0 == wanted) {
        Some(f) => f.0,
        None => {
            if wanted != "png" && !wanted.is_empty() {
                crate::warn!("can't write {wanted} here, saving PNG");
            }
            "png"
        }
    }
}

/// Encode and write the picture where the settings (or `explicit`) say.
/// Returns where, the bytes written and their format.
pub fn save(image: &Image, cfg: &Config, explicit: Option<&str>, shot: &Shot) -> Result<(PathBuf, Vec<u8>, &'static str), String> {
    let quality = cfg.quality.clamp(1, 100) as u8;
    if let Some(e) = explicit {
        let path = PathBuf::from(config::expand_user(e));
        let ext = path.extension().map(|x| x.to_string_lossy().to_lowercase()).unwrap_or_default();
        let format = if ext.is_empty() { "png" } else { writable(&ext) };
        let bytes = codec::encode(image, format, quality)?;
        if let Some(dir) = path.parent().filter(|d| !d.as_os_str().is_empty()) {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, &bytes).map_err(|e| format!("could not write {}: {e}", path.display()))?;
        return Ok((path, bytes, format));
    }
    let format = writable(&cfg.format);
    let bytes = codec::encode(image, format, quality)?;
    let path = template::screenshot_path(cfg, shot, image.size(), format, false);
    let path = write_unique(&path, &bytes)?;
    Ok((path, bytes, format))
}

/// Write `bytes` to a new file at `path`, or `name_1`, `name_2`, ... if it
/// exists: never over a file (or through a link) already there.
pub fn write_unique(path: &Path, bytes: &[u8]) -> Result<PathBuf, String> {
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let stem = path.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "Screenshot".into());
    let ext = path.extension().map(|s| s.to_string_lossy().into_owned());
    for n in 0..10_000 {
        let candidate = if n == 0 {
            path.to_path_buf()
        } else {
            let name = match &ext {
                Some(e) => format!("{stem}_{n}.{e}"),
                None => format!("{stem}_{n}"),
            };
            path.with_file_name(name)
        };
        match std::fs::OpenOptions::new().write(true).create_new(true).mode(0o644).custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC).open(&candidate) {
            Ok(mut f) => {
                f.write_all(bytes).map_err(|e| format!("could not write {}: {e}", candidate.display()))?;
                return Ok(candidate);
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => continue,
            Err(e) => return Err(format!("could not write {}: {e}", candidate.display())),
        }
    }
    Err(format!("no free file name next to {}", path.display()))
}

/// A file for a capture that isn't kept, in our private folder.
fn temp_path() -> Result<PathBuf, String> {
    let dir = crate::ipc::runtime_dir().map_err(|e| e.to_string())?;
    let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap_or_default();
    let name = template::strftime("Screenshot_%Y-%m-%d_%H-%M-%S", now.as_secs() as libc::time_t, now.subsec_micros());
    Ok(dir.join(format!("{name}.png")))
}

fn human_size(n: usize) -> String {
    if n < 1 << 20 { format!("{:.0} KiB", n as f64 / 1024.0) } else { format!("{:.1} MiB", n as f64 / (1 << 20) as f64) }
}

/// Hand clipboard data to wl-copy, which stays in the background to serve
/// it: a one-off capture can then end at once.
pub fn hand_to_wl_copy(mime: &str, data: &[u8]) -> bool {
    let Some(program) = util::which("wl-copy") else { return false };
    let child = std::process::Command::new(program)
        .args(["--type", mime])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn();
    let Ok(mut child) = child else { return false };
    let written = child.stdin.take().is_some_and(|mut s| s.write_all(data).is_ok());
    // It reads everything, takes the clipboard, then forks: the one we
    // started leaves at once.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match child.try_wait() {
            Ok(Some(status)) => return written && status.success(),
            Ok(None) if std::time::Instant::now() < deadline => std::thread::sleep(std::time::Duration::from_millis(2)),
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                return false;
            }
        }
    }
}

/// Open a file (or folder) with the desktop's default program.
pub fn open_file(path: &Path) -> bool {
    util::spawn_detached(&["xdg-open".as_ref(), path.as_os_str()], &[])
}

/// Select the file in the file manager (Dolphin, Nautilus, ...), else open its folder.
pub fn show_in_folder(path: &Path) -> bool {
    let uri = util::file_uri(path);
    if let Ok(conn) = crate::dbus::session()
        && conn
            .call_method(
                Some("org.freedesktop.FileManager1"),
                "/org/freedesktop/FileManager1",
                Some("org.freedesktop.FileManager1"),
                "ShowItems",
                &(vec![uri.as_str()], ""),
            )
            .is_ok()
    {
        return true;
    }
    path.parent().is_some_and(open_file)
}

/// The user's after-capture command, `{path}` replaced by the quoted path
/// (and in $FLATSHOT_PATH).
pub fn run_command(template: &str, path: &Path) -> bool {
    let quoted = util::shell_quote(&path.to_string_lossy());
    let command = template.replace("{path}", &quoted);
    util::spawn_detached(&["/bin/sh".as_ref(), "-c".as_ref(), command.as_ref()], &[("FLATSHOT_PATH", path.as_os_str())])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn unique_names_never_overwrite() {
        let dir = std::env::temp_dir().join(format!("flatshot-deliver-{}", util::random_hex(4)));
        let p = dir.join("Shot.png");
        assert_eq!(write_unique(&p, b"a").unwrap(), p);
        assert_eq!(write_unique(&p, b"b").unwrap(), dir.join("Shot_1.png"));
        assert_eq!(std::fs::read(&p).unwrap(), b"a");
        // A link in the way is not followed.
        std::os::unix::fs::symlink("/etc/passwd", dir.join("Shot_2.png")).unwrap();
        assert_eq!(write_unique(&p, b"c").unwrap(), dir.join("Shot_3.png"));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
