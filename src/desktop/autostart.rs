//! Start at login: an XDG autostart entry that runs the tray app.

use std::path::PathBuf;

pub fn entry_path() -> PathBuf {
    crate::config::config_home().join("autostart").join("flatshot.desktop")
}

pub fn enabled() -> bool {
    entry_path().is_file()
}

/// This same Flatshot, as an Exec= line can name it.
fn launcher() -> String {
    let exe = std::env::current_exe().ok().and_then(|p| p.canonicalize().ok());
    let path = match exe {
        Some(p) => p.to_string_lossy().into_owned(),
        None => "flatshot".into(),
    };
    exec_quote(&path)
}

/// A word for a desktop entry's Exec= (its own quoting rules).
fn exec_quote(word: &str) -> String {
    if word.chars().all(|c| c.is_ascii_alphanumeric() || "/._-+".contains(c)) {
        return word.to_string();
    }
    let mut out = String::from("\"");
    for c in word.chars() {
        if matches!(c, '"' | '`' | '$' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('"');
    // (and the key-file level: a backslash is written twice)
    out.replace('\\', "\\\\")
}

pub fn set_enabled(on: bool) -> Result<(), String> {
    let path = entry_path();
    if !on {
        return match std::fs::remove_file(&path) {
            Ok(()) => Ok(()),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(e) => Err(format!("{}: {e}", path.display())),
        };
    }
    if let Some(dir) = path.parent() {
        std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
    }
    let text = format!(
        "[Desktop Entry]\nType=Application\nName=Flatshot\nComment=Screenshot tool in the system tray\nExec={} --tray\nIcon=flatshot\nTerminal=false\nX-GNOME-Autostart-enabled=true\nX-KDE-autostart-phase=2\n",
        launcher()
    );
    std::fs::write(&path, text).map_err(|e| format!("{}: {e}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exec_words() {
        assert_eq!(exec_quote("/usr/bin/flatshot"), "/usr/bin/flatshot");
        assert_eq!(exec_quote("/home/a b/flatshot"), "\"/home/a b/flatshot\"");
        assert_eq!(exec_quote("/x/$y"), "\"/x/\\\\$y\"");
    }
}
