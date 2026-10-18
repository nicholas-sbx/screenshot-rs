//! The Recent captures window (ui::recent): opening it, its input and
//! drawing, the pictures of its cards (made from the files on a thread of
//! their own, only while it's open), and what it asks of the desktop.

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::{Instant, SystemTime};

use super::{App, Input};
use crate::clipboard::Slot;
use crate::deliver;
use crate::geom::{Damage, IRect, Point};
use crate::history::{self, Entry};
use crate::image::{Canvas, Image};
use crate::platform::{Owner, Painted};
use crate::ui::recent::{Recent, Request, THUMB_MAX};
use crate::ui::session::{Button, Waker};

const TITLE: &str = "Flatshot RS Recent Captures";
const SIZE: (u32, u32) = (900, 640);
const MIN_SIZE: (u32, u32) = (480, 400);
/// The biggest file read to copy a picture.
const MAX_COPY: u64 = 256 << 20;

/// The window and what it waits for.
pub struct RecentWin {
    pub r: Recent,
    /// to the thread making the cards' pictures (it ends with the window)
    jobs: mpsc::Sender<Entry>,
    made: mpsc::Receiver<(PathBuf, Option<Image>)>,
    /// when the list was read (to read it again when it changes)
    read_at: Option<SystemTime>,
}

struct RecentPaint<'a>(&'a mut Recent);

impl Painted for RecentPaint<'_> {
    fn damage(&mut self, _: usize) -> Damage {
        self.0.damage()
    }
    fn paint(&self, _: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        self.0.paint(target, origin, region);
    }
    fn buffer_size(&self, _: usize) -> (u32, u32) {
        self.0.device_size()
    }
}

/// Make the cards' pictures, one after another, until the window's gone.
fn thumbnailer(jobs: mpsc::Receiver<Entry>, made: mpsc::Sender<(PathBuf, Option<Image>)>, waker: Waker) {
    while let Ok(e) = jobs.recv() {
        let picture = thumbnail(&e);
        if made.send((e.path, picture)).is_err() {
            return;
        }
        waker();
    }
}

/// A capture's picture, at most THUMB_MAX: the file itself, or a
/// recording's first frame (kept when it was saved).
fn thumbnail(e: &Entry) -> Option<Image> {
    let file = if e.is_video() { e.thumbnail.as_deref()? } else { e.path.as_path() };
    let img = crate::codec::decode_file(file).ok()?;
    let (w, h) = img.size();
    let k = (THUMB_MAX.0 as f64 / w as f64).min(THUMB_MAX.1 as f64 / h as f64);
    if k >= 1.0 {
        return Some(img);
    }
    img.scaled_area(((w as f64 * k).round() as u32).max(1), ((h as f64 * k).round() as u32).max(1)).ok()
}

/// A picture's PNG, for the clipboard: a PNG file as it is, else converted.
fn png_of(path: &Path) -> Result<Vec<u8>, String> {
    let bytes = crate::util::read_limited(path, MAX_COPY).map_err(|e| e.to_string())?;
    if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        return Ok(bytes);
    }
    crate::codec::encode_png(&crate::codec::decode_file(path)?, true)
}

impl App {
    /// Open the Recent captures window (or bring the open one up).
    pub(super) fn show_recent(&mut self) {
        let token = self.tray.tray.as_ref().and_then(|t| t.take_activation_token());
        if self.recent.is_some() {
            self.dpy.activate(Owner::Recent, token);
            self.dpy.flush();
            return;
        }
        if let Err(e) = self.dpy.add_window(Owner::Recent, TITLE, SIZE, true) {
            self.report_failure("Can't show the recent captures", &e);
            return;
        }
        self.dpy.set_min_size(Owner::Recent, MIN_SIZE);
        if token.is_some() {
            self.dpy.activate(Owner::Recent, token);
        }
        let (jobs, rx) = mpsc::channel();
        let (tx, made) = mpsc::channel();
        let waker = self.waker.clone();
        let _ = std::thread::Builder::new().name("flatshot-thumbnails".into()).spawn(move || thumbnailer(rx, tx, waker));
        let read_at = history::changed();
        self.recent = Some(RecentWin { r: Recent::new(history::load()), jobs, made, read_at });
        self.serve_recent();
        self.dpy.flush();
    }

    pub(super) fn has_recent(&self) -> bool {
        self.recent.is_some()
    }

    /// The list changed (a capture saved here): the window shows it.
    pub(super) fn recent_changed(&mut self) {
        if let Some(w) = &mut self.recent {
            w.read_at = history::changed();
            w.r.set_entries(history::load());
        }
        self.serve_recent();
    }

    pub(super) fn recent_input(&mut self, ev: Input) {
        let mods = self.dpy.mods();
        let Some(w) = &mut self.recent else { return };
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        match ev {
            Input::Enter(at) => {
                // (captures saved elsewhere meanwhile, by a one-off run)
                let now = history::changed();
                if now != w.read_at {
                    w.read_at = now;
                    w.r.set_entries(history::load());
                }
                w.r.pointer_motion(p(at));
            }
            Input::Motion(at, _) => w.r.pointer_motion(p(at)),
            Input::Leave => w.r.pointer_leave(),
            Input::Button(b, pressed, at) => {
                let button = match b {
                    0x110 => Button::Left,
                    0x111 => Button::Right,
                    0x112 => Button::Middle,
                    _ => Button::Other,
                };
                w.r.button(button, pressed, p(at), mods);
            }
            Input::Scroll(steps) => w.r.scroll_by(steps),
            Input::Key(k) => w.r.key(&k),
            Input::KeyUp(_) => {}
            Input::Modifiers(m) => w.r.modifiers(m),
        }
        self.serve_recent();
    }

    pub(super) fn close_recent(&mut self) {
        if self.recent.take().is_some() {
            self.dpy.remove(Owner::Recent);
            crate::mem::trim(); // (its pictures)
        }
    }

    /// Do what the window asked for.
    fn serve_recent(&mut self) {
        let Some(w) = &mut self.recent else { return };
        for req in w.r.take_requests() {
            let Some(w) = &mut self.recent else { return };
            match req {
                Request::Thumbnails(entries) => {
                    for e in entries {
                        let _ = w.jobs.send(e);
                    }
                }
                Request::Open(path) => {
                    deliver::open_file(&path);
                }
                Request::Annotate(path) => {
                    self.open_editor(&path, None);
                }
                Request::ShowInFolder(path) => {
                    // (asking the file manager waits on D-Bus: not here)
                    let _ = std::thread::Builder::new().name("flatshot-folder".into()).spawn(move || deliver::show_in_folder(&path));
                }
                Request::Copy(e) => self.copy_entry(&e),
                Request::Remove(paths) => {
                    history::remove(&paths);
                    self.recent_changed();
                }
                Request::Clear => {
                    history::clear();
                    self.recent_changed();
                }
                Request::OpenFolder => {
                    let folder = crate::template::base_folder(&self.cfg.save_dir);
                    deliver::open_file(&folder);
                }
                Request::Close => {
                    self.close_recent();
                    return;
                }
            }
        }
    }

    /// A capture on the clipboard: a picture as a PNG (read on a thread,
    /// handed over once ready), a recording as its file.
    fn copy_entry(&mut self, e: &Entry) {
        if e.is_video() {
            let uri = crate::util::file_uri(&e.path);
            let parts = vec![
                ("text/uri-list".to_string(), Arc::new(format!("{uri}\r\n").into_bytes())),
                ("x-special/gnome-copied-files".to_string(), Arc::new(format!("copy\n{uri}").into_bytes())),
            ];
            self.dpy.offer_each(parts);
            return;
        }
        let slot = Arc::new(Slot::default());
        if !self.dpy.offer(vec!["image/png".into()], slot.clone()) {
            crate::warn!("could not copy: no clipboard");
            return;
        }
        let path = e.path.clone();
        let _ = std::thread::Builder::new().name("flatshot-copy".into()).spawn(move || match png_of(&path) {
            Ok(png) => slot.fill(Arc::new(png)),
            Err(err) => {
                crate::warn!("could not copy {}: {err}", path.display());
                slot.abandon();
            }
        });
    }

    /// Pictures made, and the window's timers.
    pub(super) fn poll_recent(&mut self) {
        let Some(w) = &mut self.recent else { return };
        while let Ok((path, picture)) = w.made.try_recv() {
            w.r.thumbnail(&path, picture);
        }
        let now = Instant::now();
        if w.r.next_deadline().is_some_and(|t| t <= now) {
            w.r.on_timer(now);
        }
    }

    pub(super) fn recent_deadline(&self) -> Option<Instant> {
        self.recent.as_ref().and_then(|w| w.r.next_deadline())
    }

    pub(super) fn redraw_recent(&mut self) {
        let Some(w) = &mut self.recent else { return };
        let Some((logical, scale)) = self.dpy.surface(Owner::Recent) else { return };
        w.r.set_view(logical.0 as f32, logical.1 as f32);
        w.r.set_scale(scale);
        self.dpy.redraw(Owner::Recent, 0, &mut RecentPaint(&mut w.r));
        // (a new size may want more pictures)
        self.serve_recent();
    }
}

/// The Recent captures window on its own (`flatshot-rs --recent` with no
/// tray app to show it).
pub fn run_recent() -> i32 {
    let started = Instant::now();
    let _ = std::thread::Builder::new().name("flatshot-fonts".into()).spawn(crate::render::text::warm_up);
    let (mut app, mut event_loop) = match super::setup(started, None) {
        Ok(x) => x,
        Err(code) => return code,
    };
    app.show_recent();
    if app.recent.is_none() {
        return 2;
    }
    app.finished = Some(0);
    while app.exit.is_none() {
        let timeout = app.timeout();
        if let Err(e) = event_loop.dispatch(timeout, &mut app) {
            crate::warn!("event loop: {e}");
            return 2;
        }
        app.after_dispatch();
    }
    app.exit.unwrap_or(0)
}
