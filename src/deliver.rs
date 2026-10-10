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
    let Some(mut cmd) = util::command("wl-copy") else { return false };
    let child = cmd.args(["--type", mime]).stdin(std::process::Stdio::piped()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn();
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

/// Run the user's after-capture command for `path` (in $FLATSHOT_PATH).
pub fn run_command(template: &str, path: &Path) -> bool {
    let command = command_line(template);
    util::spawn_detached(&["/bin/sh".as_ref(), "-c".as_ref(), command.as_ref()], &[("FLATSHOT_PATH", path.as_os_str())])
}

/// The user's command with each `{path}` standing for the file as a
/// reference to $FLATSHOT_PATH, quoted for where it stands: bare, in
/// double quotes, in single quotes, in `$(...)` or backticks. The path is
/// never part of the command's text, so nothing in it (a window title in
/// the file name, say) is read by the shell; quoting the command doesn't
/// foresee can at worst break it.
pub fn command_line(template: &str) -> String {
    #[derive(Clone, Copy, PartialEq)]
    enum Q {
        Bare,
        Single,
        Double,
        /// inside `$(...)`
        Sub,
        /// inside backticks
        Tick,
    }
    let mut out = String::with_capacity(template.len() + 32);
    let mut stack = vec![Q::Bare];
    let mut rest = template;
    while let Some(c) = rest.chars().next() {
        let q = stack.last().copied().unwrap_or(Q::Bare);
        if let Some(after) = rest.strip_prefix("{path}") {
            out.push_str(match q {
                Q::Single => r#"'"$FLATSHOT_PATH"'"#,
                Q::Double => "${FLATSHOT_PATH}",
                Q::Bare | Q::Sub | Q::Tick => r#""$FLATSHOT_PATH""#,
            });
            rest = after;
            continue;
        }
        out.push(c);
        rest = &rest[c.len_utf8()..];
        match (q, c) {
            (Q::Single, '\'') | (Q::Double, '"') | (Q::Sub, ')') | (Q::Tick, '`') => {
                stack.pop();
            }
            (Q::Single, _) => {}
            (_, '\\') => {
                // (the next character is taken as it is)
                if let Some(n) = rest.chars().next() {
                    out.push(n);
                    rest = &rest[n.len_utf8()..];
                }
            }
            (Q::Double, '$') | (Q::Bare | Q::Sub | Q::Tick, '$') if rest.starts_with('(') => {
                out.push('(');
                rest = &rest[1..];
                stack.push(Q::Sub);
            }
            (Q::Double, '`') | (Q::Bare | Q::Sub, '`') => stack.push(Q::Tick),
            (Q::Bare | Q::Sub | Q::Tick, '\'') => stack.push(Q::Single),
            (Q::Bare | Q::Sub | Q::Tick, '"') => stack.push(Q::Double),
            _ => {}
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_path_stays_out_of_the_command() {
        assert_eq!(command_line("cmd {path}"), r#"cmd "$FLATSHOT_PATH""#);
        assert_eq!(command_line(r#"cmd "{path}""#), r#"cmd "${FLATSHOT_PATH}""#);
        assert_eq!(command_line("cmd '{path}'"), r#"cmd ''"$FLATSHOT_PATH"''"#);
        assert_eq!(command_line("curl -F file=@{path} https://x"), r#"curl -F file=@"$FLATSHOT_PATH" https://x"#);
        assert_eq!(command_line(r#"n "$(basename {path})""#), r#"n "$(basename "$FLATSHOT_PATH")""#);
        assert_eq!(command_line(r#"echo \"{path}"#), r#"echo \""$FLATSHOT_PATH""#);
        assert_eq!(command_line("no path"), "no path");
        // Whatever the file is called, and however the command quotes it,
        // the shell gets the name and runs nothing in it.
        let dir = std::env::temp_dir().join(format!("flatshot-cmd-{}", util::random_hex(4)));
        std::fs::create_dir_all(&dir).unwrap();
        let evil = format!("{}/a $(touch pwned1) `touch pwned2` '; touch pwned3; ' \" ; touch pwned4 ; \" \\ $HOME.png", dir.display());
        for template in [
            "printf %s {path}",
            r#"printf %s "{path}""#,
            "printf %s '{path}'",
            r#"printf %s "x$(printf %s {path})y""#,
            r#"x=`printf %s {path}`; printf %s "$x""#,
        ] {
            let out =
                std::process::Command::new("/bin/sh").args(["-c", &command_line(template)]).env("FLATSHOT_PATH", &evil).current_dir(&dir).output().unwrap();
            let got = String::from_utf8_lossy(&out.stdout);
            let want = if template.contains("x$(") { format!("x{evil}y") } else { evil.clone() };
            assert_eq!(got, want, "{template}");
        }
        let made: Vec<_> = std::fs::read_dir(&dir).unwrap().flatten().map(|e| e.file_name()).collect();
        assert!(made.is_empty(), "the shell ran something: {made:?}");
        let _ = std::fs::remove_dir_all(&dir);
    }

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
