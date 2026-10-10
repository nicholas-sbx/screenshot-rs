//! `flatshot --diagnose`: what this Flatshot and the running tray app use,
//! and what's missing.

use std::path::{Path, PathBuf};
use std::time::Duration;

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::wl_registry;
use wayland_client::{Connection, Dispatch, QueueHandle};

use crate::{capture, cli, config, ipc, record};

struct Registry;

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for Registry {
    fn event(_: &mut Registry, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Registry>) {}
}

/// The compositor's interfaces, by name.
fn wayland_globals() -> Result<Vec<String>, String> {
    let conn = Connection::connect_to_env().map_err(|e| e.to_string())?;
    let (globals, _queue) = registry_queue_init::<Registry>(&conn).map_err(|e| e.to_string())?;
    Ok(globals.contents().clone_list().into_iter().map(|g| g.interface).collect())
}

/// The installed desktop file that lets `exe` use KWin's screenshot API.
fn kwin_desktop_file(exe: &Path) -> Option<PathBuf> {
    let mut dirs = vec![config::data_home()];
    dirs.extend(config::data_dirs());
    for dir in dirs {
        let Ok(entries) = std::fs::read_dir(dir.join("applications")) else { continue };
        for e in entries.flatten() {
            let path = e.path();
            if path.extension().is_none_or(|x| x != "desktop") {
                continue;
            }
            let Ok(text) = crate::util::read_limited(&path, 64 << 10) else { continue };
            let text = String::from_utf8_lossy(&text);
            let restricted = text.lines().any(|l| l.starts_with("X-KDE-DBUS-Restricted-Interfaces=") && l.contains("org.kde.KWin.ScreenShot2"));
            let names_exe =
                text.lines().filter_map(|l| l.strip_prefix("Exec=")).any(|exec| exec.split_whitespace().next().map(|p| p.trim_matches('"')) == exe.to_str());
            if restricted && names_exe {
                return Some(path);
            }
        }
    }
    None
}

fn rows(out: &mut Vec<String>, data: &[(&str, String)]) {
    let width = data.iter().map(|(k, _)| k.len()).max().unwrap_or(0);
    for (k, v) in data {
        out.push(format!("  {:<w$} {v}", format!("{k}:"), w = width + 1));
    }
}

pub fn report() -> String {
    let cfg = config::load();
    let exe = std::env::current_exe().ok();
    let exe_text = exe.as_ref().map(|p| p.display().to_string()).unwrap_or_else(|| "unknown".into());
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default();
    let mut data: Vec<(&str, String)> = vec![
        ("version", cli::VERSION.to_string()),
        ("pid", std::process::id().to_string()),
        ("executable", exe_text),
        ("desktop", if desktop.is_empty() { "unknown".into() } else { desktop.clone() }),
    ];
    let globals = match std::env::var_os("WAYLAND_DISPLAY") {
        Some(d) if !crate::x11::chosen() => {
            data.push(("session", format!("Wayland ({})", d.to_string_lossy())));
            wayland_globals()
        }
        _ if crate::x11::chosen() => {
            data.push(("session", format!("X11 ({})", std::env::var("DISPLAY").unwrap_or_default())));
            data.extend(crate::x11::diagnose());
            Err("an X11 session".into())
        }
        _ => {
            data.push(("session", "none found (neither WAYLAND_DISPLAY nor DISPLAY is set)".into()));
            Err("no session".into())
        }
    };
    let has = |name: &str| globals.as_ref().is_ok_and(|g| g.iter().any(|i| i == name));
    match &globals {
        Ok(_) => {
            let yes = |b: bool| if b { "yes" } else { "no" };
            data.push((
                "overlay",
                if has("zwlr_layer_shell_v1") {
                    format!("a layer-shell surface (scope {})", crate::app::layer_namespace())
                } else {
                    "a fullscreen window (the compositor has no layer-shell)".into()
                },
            ));
            data.push(("screen copy", yes(has("zwlr_screencopy_manager_v1")).into()));
            data.push(("clipboard without focus", yes(has("ext_data_control_manager_v1") || has("zwlr_data_control_manager_v1")).into()));
            data.push(("fractional scaling", yes(has("wp_fractional_scale_manager_v1") && has("wp_viewporter")).into()));
        }
        Err(e) if !crate::x11::chosen() => data.push(("compositor", format!("not reached: {e}"))),
        Err(_) => {}
    }
    let order = capture::backend_order(&cfg.backend, has("zwlr_screencopy_manager_v1"));
    data.push(("capture, in order", order.join(", ")));
    if order.contains(&"kwin") {
        let kwin = if !capture::kwin::available() {
            "KWin isn't running".to_string()
        } else {
            match exe.as_deref().and_then(kwin_desktop_file) {
                Some(f) => format!("allowed, by {}", f.display()),
                None => "not allowed: no installed desktop file names this executable with X-KDE-DBUS-Restricted-Interfaces (use /usr/bin/flatshot from the package)".into(),
            }
        };
        data.push(("KWin screenshot API", kwin));
    }
    let how = record::method();
    let recorder = match record::problem(how, &cfg.record_format) {
        Some(p) => format!("{} (not usable: {p})", how.label()),
        None => how.label().to_string(),
    };
    data.push(("screen recorder", recorder));
    data.push(("settings", config::config_path().display().to_string()));
    let mut out = vec!["This flatshot:".to_string()];
    rows(&mut out, &data);
    match ipc::query("status", Duration::from_millis(800)) {
        None => out.push("Tray app: not running.".into()),
        Some(reply) => match serde_json::from_str::<serde_json::Value>(&reply) {
            Ok(serde_json::Value::Object(m)) => {
                out.push("Tray app (handles shortcuts and `flatshot` captures):".into());
                let tray: Vec<(&str, String)> = m.iter().map(|(k, v)| (k.as_str(), v.as_str().map(str::to_string).unwrap_or_else(|| v.to_string()))).collect();
                rows(&mut out, &tray);
            }
            _ => out.push("Tray app: running, but an older version: restart it (flatshot --quit, then start Flatshot again).".into()),
        },
    }
    if std::os::unix::net::UnixStream::connect(ipc::legacy_socket()).is_ok() {
        out.push("A Python Flatshot tray app is running: starting this one's tray app replaces it.".into());
    }
    out.join("\n") + "\n"
}
