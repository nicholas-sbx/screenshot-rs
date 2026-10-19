//! A stand-in for KWin's org.kde.KWin.ScreenShot2, for testing without
//! KWin: replies with the picture's size like KWin does, then writes raw
//! pixels into the pipe it was given, from another thread.
//!
//! FAKE_KWIN_SCREENS="A:800x600,B:960x720" names the screens and their
//! sizes; screen i's pixel (x, y) is 0xFF000000 | (x % 256) << 16 |
//! (y % 256) << 8 | 0x10 * (i + 1), plus 0x80 in the low byte when the
//! pointer was asked for. CaptureWorkspace answers 1500 x 900 in RGB32 (4),
//! pixel (x, y) = 0xFF000000 | (x % 256) << 16 | (y % 256) << 8 | 0x5A.
//! CaptureActiveWindow answers 300 x 200 premultiplied ARGB32 (6): opaque
//! blue inside, a clear 20-pixel border (the window's "shadow"); it prints
//! the options it was given.
//!
//! org.kde.kwin.Scripting runs no JavaScript: a script loaded and started
//! has its callDBus target sent a report of one window, 400 x 300 at
//! (200, 150), after FAKE_KWIN_SCRIPT_DELAY_MS (default 2500), from a
//! second connection of this process (KWin's scripts may call from one).

use std::collections::HashMap;
use std::io::Write;

use zbus::{OwnedFd, OwnedVariant, Variant};

struct FakeKWin {
    screens: Vec<(String, u32, u32)>,
}

fn pixels(w: u32, h: u32, low: u8) -> Vec<u8> {
    let mut out = Vec::with_capacity((w * h * 4) as usize);
    for y in 0..h {
        for x in 0..w {
            out.extend_from_slice(&[low, (y % 256) as u8, (x % 256) as u8, 0xFF]);
        }
    }
    out
}

fn send(pipe: OwnedFd, data: Vec<u8>) {
    std::thread::spawn(move || {
        let fd: std::os::fd::OwnedFd = pipe.into();
        let mut f = std::fs::File::from(fd);
        let _ = f.write_all(&data);
    });
}

fn results(w: u32, h: u32, format: u32) -> HashMap<String, OwnedVariant> {
    let mut m = HashMap::new();
    m.insert("type".into(), Variant::from("raw").try_into().unwrap());
    m.insert("width".into(), Variant::U32(w).try_into().unwrap());
    m.insert("height".into(), Variant::U32(h).try_into().unwrap());
    m.insert("stride".into(), Variant::U32(w * 4).try_into().unwrap());
    m.insert("format".into(), Variant::U32(format).try_into().unwrap());
    m.insert("scale".into(), Variant::F64(1.0).try_into().unwrap());
    m
}

/// As KWin does: without hide-caller-windows false, the caller's own
/// windows would be left out of the picture (refused here, to be noticed).
fn shows_caller(options: &HashMap<String, OwnedVariant>) -> zbus::fdo::Result<()> {
    match options.get("hide-caller-windows").and_then(|v| bool::try_from(&**v).ok()) {
        Some(false) => Ok(()),
        _ => Err(zbus::fdo::Error::InvalidArgs("the caller's own windows would be hidden".into())),
    }
}

fn cursor(options: &HashMap<String, OwnedVariant>) -> bool {
    options.get("include-cursor").and_then(|v| bool::try_from(&**v).ok()).unwrap_or(false)
}

#[zbus::interface(name = "org.kde.KWin.ScreenShot2")]
impl FakeKWin {
    fn capture_screen(&self, name: &str, options: HashMap<String, OwnedVariant>, pipe: OwnedFd) -> zbus::fdo::Result<HashMap<String, OwnedVariant>> {
        shows_caller(&options)?;
        let Some(i) = self.screens.iter().position(|s| s.0 == name) else {
            return Err(zbus::fdo::Error::InvalidArgs(format!("no screen {name}")));
        };
        let (_, w, h) = self.screens[i];
        let low = 0x10 * (i as u8 + 1) + if cursor(&options) { 0x80 } else { 0 };
        send(pipe, pixels(w, h, low));
        Ok(results(w, h, 5))
    }

    fn capture_active_window(&self, options: HashMap<String, OwnedVariant>, pipe: OwnedFd) -> zbus::fdo::Result<HashMap<String, OwnedVariant>> {
        shows_caller(&options)?;
        let flag = |k: &str| options.get(k).and_then(|v| bool::try_from(&**v).ok()).unwrap_or(false);
        println!("window: cursor {} decoration {} shadow {}", flag("include-cursor"), flag("include-decoration"), flag("include-shadow"));
        let (w, h) = (300u32, 200u32);
        let mut out = Vec::with_capacity((w * h * 4) as usize);
        for y in 0..h {
            for x in 0..w {
                let inside = (20..w - 20).contains(&x) && (20..h - 20).contains(&y);
                out.extend_from_slice(if inside { &[0xFF, 0, 0, 0xFF] } else { &[0, 0, 0, 0] });
            }
        }
        send(pipe, out);
        Ok(results(w, h, 6))
    }

    fn capture_workspace(&self, options: HashMap<String, OwnedVariant>, pipe: OwnedFd) -> zbus::fdo::Result<HashMap<String, OwnedVariant>> {
        shows_caller(&options)?;
        send(pipe, pixels(1500, 900, 0x5A));
        Ok(results(1500, 900, 4))
    }
}

struct FakeScripting {
    scripts: std::sync::Mutex<Vec<String>>,
}

/// The first two quoted strings after `callDBus(`: the service and path.
fn target(script: &str) -> Option<(String, String)> {
    let rest = &script[script.find("callDBus(")? + 9..];
    let mut parts = rest.split('"').skip(1).step_by(2);
    Some((parts.next()?.to_string(), parts.next()?.to_string()))
}

#[zbus::interface(name = "org.kde.kwin.Scripting")]
impl FakeScripting {
    #[zbus(name = "loadScript")]
    fn load_script(&self, path: &str, _name: &str) -> i32 {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        self.scripts.lock().unwrap().push(text);
        1
    }

    #[zbus(name = "unloadScript")]
    fn unload_script(&self, _name: &str) -> bool {
        true
    }

    #[zbus(name = "start")]
    fn start(&self) {
        let delay: u64 = std::env::var("FAKE_KWIN_SCRIPT_DELAY_MS").ok().and_then(|v| v.parse().ok()).unwrap_or(2500);
        for script in self.scripts.lock().unwrap().drain(..) {
            let Some((service, path)) = target(&script) else { continue };
            std::thread::spawn(move || {
                std::thread::sleep(std::time::Duration::from_millis(delay));
                let conn = zbus::block_on(zbus::Connection::session()).unwrap();
                let report = r#"{"windows": [[200, 150, 400, 300, "Fake window", "fake"]], "active": 0, "cursor": [400, 300]}"#;
                let r = zbus::block_on(conn.call_method(Some(service.as_str()), path.as_str(), Some("org.flatshot.Windows"), "report", &(report,)));
                println!("script reported: {}", r.map(|_| "ok".to_string()).unwrap_or_else(|e| e.to_string()));
            });
        }
    }
}

fn main() {
    let screens = std::env::var("FAKE_KWIN_SCREENS")
        .unwrap_or_default()
        .split(',')
        .filter(|s| !s.is_empty())
        .map(|spec| {
            let (name, size) = spec.split_once(':').expect("NAME:WxH");
            let (w, h) = size.split_once('x').expect("WxH");
            (name.to_string(), w.parse().unwrap(), h.parse().unwrap())
        })
        .collect();
    let _conn = zbus::block_on(
        zbus::connection::Builder::session()
            .name("org.kde.KWin")
            .serve_at("/org/kde/KWin/ScreenShot2", FakeKWin { screens })
            .serve_at("/Scripting", FakeScripting { scripts: Default::default() })
            .build(),
    )
    .unwrap();
    println!("fake KWin ScreenShot2 ready");
    loop {
        std::thread::park();
    }
}
