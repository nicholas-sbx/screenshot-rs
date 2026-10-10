//! The screenshot portal (org.freedesktop.portal.Screenshot): how GNOME
//! and sandboxed setups hand out the screen. The portal writes a PNG and
//! answers with its URI on a request object.

use std::collections::HashMap;
use std::time::Duration;

use zbus::zvariant::Value;

use crate::codec;
use crate::desktop::portal::{self, Answer};
use crate::image::Image;
use crate::util;

/// The whole desktop, without asking the user (where the portal allows it).
pub fn screenshot() -> Result<Image, String> {
    let mut options: HashMap<&'static str, Value<'static>> = HashMap::new();
    options.insert("interactive", Value::Bool(false));
    let results = match portal::request("org.freedesktop.portal.Screenshot", "Screenshot", options, |o| ("", o), Duration::from_secs(60))? {
        Answer::Done(r) => r,
        Answer::Cancelled => return Err("the screenshot was cancelled".into()),
    };
    let uri = results.get("uri").and_then(|v| <&str>::try_from(&**v).ok()).ok_or("the portal gave no picture")?.to_string();
    let path = util::uri_to_path(&uri).ok_or_else(|| format!("unexpected picture location {uri}"))?;
    let img = codec::decode_file(&path)?;
    // The portal's file is a copy made for us: gone once read, unless it's
    // somewhere the user keeps pictures.
    let temp = std::env::temp_dir();
    let runtime = std::env::var_os("XDG_RUNTIME_DIR").map(std::path::PathBuf::from);
    // (a plain path: "/tmp/../home/x" is not in the temp folder)
    let plain = path.components().all(|c| matches!(c, std::path::Component::RootDir | std::path::Component::Normal(_)));
    if plain && (path.starts_with(&temp) || runtime.is_some_and(|r| path.starts_with(r))) {
        let _ = std::fs::remove_file(&path);
    }
    Ok(img)
}
