//! A stand-in for KWin's org.kde.KWin.ScreenShot2, for testing without
//! KWin: replies with the picture's size like KWin does, then writes raw
//! pixels into the pipe it was given, from another thread.
//!
//! FAKE_KWIN_SCREENS="A:800x600,B:960x720" names the screens and their
//! sizes; screen i's pixel (x, y) is 0xFF000000 | (x % 256) << 16 |
//! (y % 256) << 8 | 0x10 * (i + 1), plus 0x80 in the low byte when the
//! pointer was asked for. CaptureWorkspace answers 1500 x 900 in RGB32 (4),
//! pixel (x, y) = 0xFF000000 | (x % 256) << 16 | (y % 256) << 8 | 0x5A.

use std::collections::HashMap;
use std::io::Write;

use zbus::zvariant::{OwnedFd, OwnedValue, Value};

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

fn results(w: u32, h: u32, format: u32) -> HashMap<String, OwnedValue> {
    let mut m = HashMap::new();
    m.insert("type".into(), Value::from("raw").try_into().unwrap());
    m.insert("width".into(), Value::U32(w).try_into().unwrap());
    m.insert("height".into(), Value::U32(h).try_into().unwrap());
    m.insert("stride".into(), Value::U32(w * 4).try_into().unwrap());
    m.insert("format".into(), Value::U32(format).try_into().unwrap());
    m.insert("scale".into(), Value::F64(1.0).try_into().unwrap());
    m
}

fn cursor(options: &HashMap<String, OwnedValue>) -> bool {
    options.get("include-cursor").and_then(|v| bool::try_from(&**v).ok()).unwrap_or(false)
}

#[zbus::interface(name = "org.kde.KWin.ScreenShot2")]
impl FakeKWin {
    fn capture_screen(&self, name: &str, options: HashMap<String, OwnedValue>, pipe: OwnedFd) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        let Some(i) = self.screens.iter().position(|s| s.0 == name) else {
            return Err(zbus::fdo::Error::InvalidArgs(format!("no screen {name}")));
        };
        let (_, w, h) = self.screens[i];
        let low = 0x10 * (i as u8 + 1) + if cursor(&options) { 0x80 } else { 0 };
        send(pipe, pixels(w, h, low));
        Ok(results(w, h, 5))
    }

    fn capture_workspace(&self, _options: HashMap<String, OwnedValue>, pipe: OwnedFd) -> zbus::fdo::Result<HashMap<String, OwnedValue>> {
        send(pipe, pixels(1500, 900, 0x5A));
        Ok(results(1500, 900, 4))
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
    let _conn = zbus::blocking::connection::Builder::session()
        .unwrap()
        .name("org.kde.KWin")
        .unwrap()
        .serve_at("/org/kde/KWin/ScreenShot2", FakeKWin { screens })
        .unwrap()
        .build()
        .unwrap();
    println!("fake KWin ScreenShot2 ready");
    loop {
        std::thread::park();
    }
}
