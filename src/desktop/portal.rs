//! The desktop portals (org.freedesktop.portal.*): the desktop's own
//! dialogs and services, asked over D-Bus. Each call answers later, on a
//! request object named after us and a token we choose.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::task::{Context, Poll, Waker};
use std::time::{Duration, Instant};

use zbus::{OwnedVariant, Variant};

use crate::util;

pub const DESKTOP: &str = "org.freedesktop.portal.Desktop";
pub const PATH: &str = "/org/freedesktop/portal/desktop";
/// How often a request that can be cancelled looks whether it has been.
const CANCEL_POLL: Duration = Duration::from_millis(100);

/// How a request ended.
#[derive(Debug)]
pub enum Answer {
    Done(HashMap<String, OwnedVariant>),
    /// the user said no (closed the dialog)
    Cancelled,
}

/// Call `method` of the portal interface `iface` and wait (up to `wait`)
/// for its answer. `body` makes the call's arguments from the options,
/// which have the request's token in them.
pub fn request<B>(
    iface: &str,
    method: &str,
    options: HashMap<&'static str, Variant<'static>>,
    body: impl FnOnce(HashMap<&'static str, Variant<'static>>) -> B,
    wait: Duration,
) -> Result<Answer, String>
where
    B: serde::Serialize + zbus::DynamicType,
{
    request_on(crate::dbus::session()?, iface, method, options, body, wait, None)
}

/// [`request`] on a connection of the caller's (a screen cast lives as
/// long as the connection that asked for it). Once `cancelled` is set (if
/// there is one), the request is closed (its dialog goes) and answers
/// Cancelled.
pub fn request_on<B>(
    conn: &crate::dbus::Bus,
    iface: &str,
    method: &str,
    mut options: HashMap<&'static str, Variant<'static>>,
    body: impl FnOnce(HashMap<&'static str, Variant<'static>>) -> B,
    wait: Duration,
    cancelled: Option<&AtomicBool>,
) -> Result<Answer, String>
where
    B: serde::Serialize + zbus::DynamicType,
{
    let unwanted = || cancelled.is_some_and(|c| c.load(Ordering::Relaxed));
    if unwanted() {
        return Ok(Answer::Cancelled);
    }
    let token = format!("flatshot{}", util::random_hex(6));
    // The request object is named after us and the token: listen on it
    // before asking, so the answer can't come first.
    let sender = conn.unique_name().ok_or("no name on the session bus")?.trim_start_matches(':').replace('.', "_");
    let path = format!("{PATH}/request/{sender}/{token}");
    let mut signals = zbus::block_on(async {
        let proxy = zbus::Proxy::new(conn.inner(), DESKTOP, path.as_str(), "org.freedesktop.portal.Request").await?;
        proxy.receive_signal("Response").await
    })
    .map_err(|e| format!("portal: {e}"))?;
    options.insert("handle_token", Variant::from(token));
    conn.call_method(Some(DESKTOP), PATH, Some(iface), method, &body(options)).map_err(|e| format!("{iface}: {e}"))?;
    // Wait for the answer, but not for ever, nor once it's not wanted.
    let waker = Waker::from(Arc::new(Unpark(std::thread::current())));
    let mut cx = Context::from_waker(&waker);
    let deadline = Instant::now() + wait;
    let msg = loop {
        if let Poll::Ready(msg) = futures_lite::StreamExt::poll_next(&mut signals, &mut cx) {
            break msg.ok_or("the portal went away")?;
        }
        if unwanted() {
            let _ = conn.call_method(Some(DESKTOP), path.as_str(), Some("org.freedesktop.portal.Request"), "Close", &());
            return Ok(Answer::Cancelled);
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return Err("the desktop didn't answer".into());
        }
        std::thread::park_timeout(if cancelled.is_some() { left.min(CANCEL_POLL) } else { left });
    };
    let (response, results): (u32, HashMap<String, OwnedVariant>) = msg.body().deserialize().map_err(|e| format!("portal reply: {e}"))?;
    match response {
        0 => Ok(Answer::Done(results)),
        1 => Ok(Answer::Cancelled),
        _ => Err("the desktop refused".into()),
    }
}

/// Wakes the thread waiting for a request's answer.
struct Unpark(std::thread::Thread);

impl std::task::Wake for Unpark {
    fn wake(self: Arc<Self>) {
        self.0.unpark();
    }
}

/// Is there a portal with `iface`?
pub fn has(iface: &str) -> bool {
    let Ok(conn) = crate::dbus::session() else { return false };
    let reply = conn.call_method(Some(DESKTOP), PATH, Some("org.freedesktop.DBus.Properties"), "Get", &(iface, "version"));
    reply.is_ok()
}

/// A file dialog's filters: (label, ["*.png", ...]) as a(sa(us)).
type Filters = Vec<(String, Vec<(u32, String)>)>;

fn filters(list: &[(String, Vec<String>)]) -> Filters {
    list.iter().map(|(label, globs)| (label.clone(), globs.iter().map(|g| (0u32, g.clone())).collect())).collect()
}

/// A folder as the file chooser takes it: its bytes, NUL-terminated.
fn folder_bytes(dir: &Path) -> Vec<u8> {
    use std::os::unix::ffi::OsStrExt;
    let mut b = dir.as_os_str().as_bytes().to_vec();
    b.push(0);
    b
}

fn first_path(results: &HashMap<String, OwnedVariant>) -> Option<PathBuf> {
    let uris = results.get("uris")?;
    let uris: Vec<String> = uris.try_clone().ok()?.try_into().ok()?;
    uris.first().and_then(|u| util::uri_to_path(u))
}

/// The desktop's "Save as" dialog, starting at `current` (its folder and
/// name), filter `current_filter` chosen. None if it was cancelled.
pub fn save_file(title: &str, current: &Path, list: &[(String, Vec<String>)], current_filter: usize) -> Result<Option<PathBuf>, String> {
    let mut options: HashMap<&'static str, Variant<'static>> = HashMap::new();
    options.insert("modal", Variant::Bool(true));
    options.insert("accept_label", Variant::from("Save"));
    if let Some(name) = current.file_name() {
        options.insert("current_name", Variant::from(name.to_string_lossy().into_owned()));
    }
    if let Some(dir) = current.parent().filter(|d| d.is_dir()) {
        options.insert("current_folder", Variant::from(folder_bytes(dir)));
    }
    let f = filters(list);
    if let Some(cur) = f.get(current_filter).cloned() {
        options.insert("current_filter", Variant::from(cur));
    }
    if !f.is_empty() {
        options.insert("filters", Variant::from(f));
    }
    let title = title.to_string();
    match request("org.freedesktop.portal.FileChooser", "SaveFile", options, |o| ("", title, o), Duration::from_secs(3600))? {
        Answer::Done(r) => Ok(first_path(&r)),
        Answer::Cancelled => Ok(None),
    }
}

/// The desktop's "Open" dialog, starting in `folder`. None if cancelled.
pub fn open_file(title: &str, folder: &Path, list: &[(String, Vec<String>)]) -> Result<Option<PathBuf>, String> {
    open(title, folder, list, false)
}

/// The desktop's folder chooser, starting in `folder`. None if cancelled.
pub fn choose_folder(title: &str, folder: &Path) -> Result<Option<PathBuf>, String> {
    open(title, folder, &[], true)
}

fn open(title: &str, folder: &Path, list: &[(String, Vec<String>)], directory: bool) -> Result<Option<PathBuf>, String> {
    let mut options: HashMap<&'static str, Variant<'static>> = HashMap::new();
    options.insert("modal", Variant::Bool(true));
    options.insert("multiple", Variant::Bool(false));
    if directory {
        options.insert("directory", Variant::Bool(true));
    }
    if folder.is_dir() {
        options.insert("current_folder", Variant::from(folder_bytes(folder)));
    }
    let f = filters(list);
    if !f.is_empty() {
        options.insert("filters", Variant::from(f));
    }
    let title = title.to_string();
    match request("org.freedesktop.portal.FileChooser", "OpenFile", options, |o| ("", title, o), Duration::from_secs(3600))? {
        Answer::Done(r) => Ok(first_path(&r)),
        Answer::Cancelled => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dialog_options() {
        assert_eq!(folder_bytes(Path::new("/tmp/a")), b"/tmp/a\0");
        let f = filters(&[("PNG".into(), vec!["*.png".into()])]);
        assert_eq!(f, vec![("PNG".to_string(), vec![(0, "*.png".to_string())])]);
        let v = Variant::from(f);
        assert_eq!(v.value_signature().to_string(), "a(sa(us))");
    }
}
