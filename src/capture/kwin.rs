//! KWin's screenshot API (org.kde.KWin.ScreenShot2), called directly.
//!
//! KWin only answers programs named, with X-KDE-DBUS-Restricted-Interfaces
//! set to org.kde.KWin.ScreenShot2, in an installed desktop file (by the
//! absolute path of their executable): the packages install one for
//! /usr/bin/flatshot-rs. KWin replies with the picture's size and format, then
//! writes the raw pixels into a pipe of ours from a thread of its own; they
//! go straight from the pipe into the picture's memory.

use std::collections::HashMap;
use std::io;
use std::os::fd::{AsFd, OwnedFd};
use std::time::{Duration, Instant};

use rustix::event::{PollFd, PollFlags};
use zbus::{Fd, OwnedVariant, Variant};

use crate::image::{Image, MAX_SIDE};

const SERVICE: &str = "org.kde.KWin";
const PATH: &str = "/org/kde/KWin/ScreenShot2";
const IFACE: &str = "org.kde.KWin.ScreenShot2";

// QImage::Format values KWin may send.
const RGB32: u32 = 4;
const ARGB32: u32 = 5;
const ARGB32_PREMULTIPLIED: u32 = 6;
const RGBX8888: u32 = 16;
const RGBA8888: u32 = 17;
const RGBA8888_PREMULTIPLIED: u32 = 18;

/// What to ask KWin for: a screen by name (None: the whole workspace),
/// with or without the mouse pointer.
#[derive(Clone, Debug, PartialEq)]
pub struct Want {
    pub screen: Option<String>,
    pub cursor: bool,
}

/// The active window on its own (KWin's CaptureActiveWindow): with its
/// title bar and borders, and its shadow, if asked.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct WindowWant {
    pub cursor: bool,
    pub decoration: bool,
    pub shadow: bool,
}

/// How long KWin took, for --verbose.
#[derive(Clone, Copy, Debug, Default)]
pub struct Times {
    pub replied: Duration,
    pub pixels: Duration,
}

/// Is KWin's screenshot API there?
pub fn available() -> bool {
    crate::dbus::has_owner(SERVICE)
}

/// Each of `wants`, all asked at once (they cost about one frame together)
/// and read side by side.
pub fn capture(wants: &[Want]) -> Result<(Vec<Image>, Times), String> {
    let conn = crate::dbus::session()?;
    let started = Instant::now();
    let results: Vec<Result<(Image, Duration), String>> = std::thread::scope(|s| {
        let handles: Vec<_> = wants.iter().map(|w| s.spawn(move || capture_one(conn, &Ask::Screen(w), started))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("the capture thread failed".into()))).collect()
    });
    let mut pictures = Vec::with_capacity(wants.len());
    let mut times = Times::default();
    for r in results {
        let (img, replied) = r?;
        times.replied = times.replied.max(replied);
        pictures.push(img);
    }
    times.pixels = started.elapsed().saturating_sub(times.replied);
    Ok((pictures, times))
}

/// The active window on its own: nothing covering it shows, parts off
/// screen are kept, and its rounded corners and shadow come out
/// transparent (premultiplied alpha).
pub fn capture_window(want: WindowWant) -> Result<Image, String> {
    let conn = crate::dbus::session()?;
    let started = Instant::now();
    let (img, replied) = capture_one(conn, &Ask::Window(want), started)?;
    crate::timing::log(format!(
        "KWin answered in {:.0} ms, the window's pixels in {:.0} ms",
        replied.as_secs_f64() * 1000.0,
        started.elapsed().saturating_sub(replied).as_secs_f64() * 1000.0
    ));
    Ok(img)
}

enum Ask<'a> {
    Screen(&'a Want),
    Window(WindowWant),
}

fn capture_one(conn: &crate::dbus::Bus, ask: &Ask<'_>, started: Instant) -> Result<(Image, Duration), String> {
    let (read_end, write_end) = big_pipe().map_err(|e| format!("pipe: {e}"))?;
    let mut options: HashMap<&str, Variant<'_>> = HashMap::new();
    options.insert("native-resolution", Variant::Bool(true));
    // KWin leaves the caller's windows out unless told not to: Flatshot's
    // own (an editor, the settings, a pin) are on the screen as any other.
    options.insert("hide-caller-windows", Variant::Bool(false));
    let fd = Fd::from(write_end.as_fd());
    let reply = match ask {
        Ask::Screen(want) => {
            options.insert("include-cursor", Variant::Bool(want.cursor));
            match &want.screen {
                Some(name) => conn.call_method(Some(SERVICE), PATH, Some(IFACE), "CaptureScreen", &(name.as_str(), options, fd)),
                None => conn.call_method(Some(SERVICE), PATH, Some(IFACE), "CaptureWorkspace", &(options, fd)),
            }
        }
        Ask::Window(w) => {
            options.insert("include-cursor", Variant::Bool(w.cursor));
            options.insert("include-decoration", Variant::Bool(w.decoration));
            options.insert("include-shadow", Variant::Bool(w.shadow));
            conn.call_method(Some(SERVICE), PATH, Some(IFACE), "CaptureActiveWindow", &(options, fd))
        }
    };
    // Our copy of the write end goes: the end of the pixels is when KWin
    // closes its own.
    drop(write_end);
    let reply = reply.map_err(|e| describe(&e))?;
    let replied = started.elapsed();
    let results: HashMap<String, OwnedVariant> = reply.body().deserialize().map_err(|e| format!("KWin's reply: {e}"))?;
    let get = |k: &str| results.get(k).and_then(number);
    let (Some(w), Some(h), Some(stride), Some(format)) = (get("width"), get("height"), get("stride"), get("format")) else {
        return Err("KWin's reply lacks the picture's size".into());
    };
    if w == 0 || h == 0 || w > MAX_SIDE as u64 || h > MAX_SIDE as u64 || stride < w * 4 || stride > w * 4 + 4096 {
        return Err(format!("unexpected picture from KWin ({w} x {h}, stride {stride})"));
    }
    let format = format as u32;
    if !matches!(format, RGB32 | ARGB32 | ARGB32_PREMULTIPLIED | RGBX8888 | RGBA8888 | RGBA8888_PREMULTIPLIED) {
        return Err(format!("unsupported pixel format {format} from KWin"));
    }
    let (w, h, stride) = (w as u32, h as u32, stride as usize);
    let mut img = Image::new_populated(w, h)?;
    if stride == img.stride() {
        read_exactly(&read_end, img.data_mut())?;
    } else {
        // Rows with padding: read each, keep the pixels.
        let mut row = vec![0u8; stride];
        let len = img.stride();
        for y in 0..h {
            read_exactly(&read_end, &mut row)?;
            img.row_mut(y).copy_from_slice(&row[..len]);
        }
    }
    if matches!(format, RGBX8888 | RGBA8888 | RGBA8888_PREMULTIPLIED) {
        for px in img.data_mut().as_chunks_mut::<4>().0 {
            px.swap(0, 2);
        }
    }
    if matches!(format, ARGB32 | RGBA8888) {
        // Straight alpha (a screen is opaque, so this is rare): premultiply.
        for px in img.data_mut().as_chunks_mut::<4>().0 {
            let a = px[3] as u32;
            if a != 255 {
                for c in &mut px[..3] {
                    *c = ((*c as u32 * a + 127) / 255) as u8;
                }
            }
        }
    }
    if matches!(ask, Ask::Screen(_)) || matches!(format, RGB32 | RGBX8888) {
        img.force_opaque();
    }
    Ok((img, replied))
}

/// A readable reason from a failed call (KWin refuses unknown programs).
fn describe(e: &zbus::Error) -> String {
    let s = e.to_string();
    if s.contains("NoAuthorized") || s.contains("NotAuthorized") || s.contains("AccessDenied") {
        format!("KWin refused the screenshot ({s}): is flatshot-rs's desktop file installed with X-KDE-DBUS-Restricted-Interfaces?")
    } else if s.contains("ServiceUnknown") || s.contains("UnknownObject") || s.contains("UnknownMethod") {
        format!("KWin's screenshot API isn't there: {s}")
    } else {
        format!("KWin: {s}")
    }
}

/// An unsigned number from a variant, whatever integer type KWin used.
fn number(v: &OwnedVariant) -> Option<u64> {
    match &**v {
        Variant::U32(n) => Some(*n as u64),
        Variant::I32(n) => u64::try_from(*n).ok(),
        Variant::U64(n) => Some(*n),
        Variant::I64(n) => u64::try_from(*n).ok(),
        Variant::U16(n) => Some(*n as u64),
        Variant::I16(n) => u64::try_from(*n).ok(),
        Variant::U8(n) => Some(*n as u64),
        _ => None,
    }
}

/// A pipe as big as the system allows (up to 16 MiB), so a picture comes
/// through in a few large writes rather than hundreds of small ones.
pub fn big_pipe() -> io::Result<(OwnedFd, OwnedFd)> {
    let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC)?;
    let max = std::fs::read_to_string("/proc/sys/fs/pipe-max-size").ok().and_then(|s| s.trim().parse::<usize>().ok()).unwrap_or(1 << 20);
    let _ = rustix::pipe::fcntl_setpipe_size(&w, max.min(16 << 20));
    Ok((r, w))
}

/// Fill `into` from `fd`, giving up if nothing comes for a while.
pub fn read_exactly(fd: &OwnedFd, into: &mut [u8]) -> Result<(), String> {
    let mut got = 0;
    while got < into.len() {
        let mut fds = [PollFd::new(fd, PollFlags::IN)];
        let ready =
            rustix::event::poll(&mut fds, Some(&rustix::time::Timespec { tv_sec: 15, tv_nsec: 0 })).map_err(|e| format!("waiting for the picture: {e}"))?;
        if ready == 0 {
            return Err("no picture came from KWin".into());
        }
        match rustix::io::read(fd, &mut into[got..]) {
            Ok(0) => return Err(format!("short picture from KWin ({got} of {} bytes)", into.len())),
            Ok(n) => got += n,
            Err(rustix::io::Errno::INTR) | Err(rustix::io::Errno::AGAIN) => {}
            Err(e) => return Err(format!("reading the picture: {e}")),
        }
    }
    Ok(())
}
