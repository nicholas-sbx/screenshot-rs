//! Taking the screen for a capture: the overlay's pictures, or an instant
//! capture's (all screens, a monitor, the active window, an exact area).

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use super::{App, NoRecorder, layer_namespace, names_window};
use crate::capture::{self, Grabbed, Plan};
use crate::geom::{IPoint, IRect};
use crate::image::Image;
use crate::template::Shot;
use crate::timing::Clock;
use crate::ui::overlay::Output;
use crate::ui::session::{Request, Screen, Session, Setup};
use crate::wayland::{Monitor, Owner};

/// How long an instant capture waits for the compositor to describe the
/// desktop (the active window, where the pointer is).
const DESKTOP_WAIT: Duration = Duration::from_millis(1500);

/// What to capture.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Mode {
    /// the overlay: drag a region, click a window
    Region,
    /// all screens
    Screens,
    /// the monitor under the pointer
    Monitor,
    /// the active window
    Window,
    /// the last region captured (the overlay if there's none yet)
    Last,
    /// an exact area, global logical pixels
    Rect(IRect),
}

impl Mode {
    /// A mode by name (as on the command line and in tray commands).
    pub fn parse(name: &str, geometry: Option<IRect>) -> Option<Mode> {
        Some(match name {
            "region" => Mode::Region,
            "screens" => Mode::Screens,
            "monitor" => Mode::Monitor,
            "window" => Mode::Window,
            "last" => Mode::Last,
            "rect" => match geometry {
                Some(r) => Mode::Rect(r),
                None => Mode::Region,
            },
            _ => return None,
        })
    }

    fn name(&self) -> &'static str {
        match self {
            Mode::Region => "region",
            Mode::Screens => "screens",
            Mode::Monitor => "monitor",
            Mode::Window => "window",
            Mode::Last => "last",
            Mode::Rect(_) => "rect",
        }
    }
}

/// A capture asked for.
#[derive(Clone, Debug)]
pub struct Job {
    pub mode: Mode,
    /// pin the result instead of saving / copying it
    pub pin: bool,
    /// open with the record tool
    pub record: bool,
    /// look for codes
    pub scan: bool,
    /// when it was asked for (the clock starts there)
    pub asked: Instant,
}

impl Job {
    pub fn new(mode: Mode) -> Job {
        Job { mode, pin: false, record: false, scan: true, asked: Instant::now() }
    }
}

/// The region captured last, if any.
pub fn last_region() -> Option<IRect> {
    let state = crate::config::load_state();
    let v: Vec<i64> = state.get("last_region")?.as_array()?.iter().filter_map(|n| n.as_i64()).collect();
    match v[..] {
        [x, y, w, h] if w > 0 && h > 0 && [x, y, w, h].iter().all(|n| n.unsigned_abs() < 1 << 20) => Some(IRect::new(x as i32, y as i32, w as i32, h as i32)),
        _ => None,
    }
}

/// Where a capture's pictures will come from.
pub(super) enum Source {
    /// the Wayland connection (wlr-screencopy)
    Screencopy,
    /// a worker thread (KWin, the portal, a program)
    Worker(mpsc::Receiver<Result<Vec<Grabbed>, String>>),
}

/// A capture being taken.
pub(super) struct Pending {
    job: Job,
    /// (resolved: never Last)
    mode: Mode,
    monitors: Vec<Monitor>,
    plan: Plan,
    backend: &'static str,
    pub(super) source: Source,
    /// the backends left to try if this one fails, and why the others did
    rest: Vec<&'static str>,
    errors: Vec<String>,
}

/// An instant capture's pictures, waiting for the desktop's description.
pub(super) struct Taken {
    job: Job,
    mode: Mode,
    screens: Vec<(Monitor, Image)>,
    pub(super) wait_until: Instant,
}

impl App {
    /// Is a capture on (being taken, or its overlay up)?
    pub(super) fn busy(&self) -> bool {
        self.session.is_some() || self.pending.is_some() || self.taken.is_some()
    }

    /// Take the screens for `job`.
    pub(super) fn start_capture(&mut self, job: Job) {
        if self.busy() {
            return; // one capture at a time
        }
        let mode = match job.mode {
            Mode::Last => last_region().map(Mode::Rect).unwrap_or(Mode::Region),
            m => m,
        };
        let instant = mode != Mode::Region;
        let what = if job.record { "recording".to_string() } else { format!("{} capture", mode.name()) };
        self.clock = Clock::since(what, job.asked);
        let monitors = self.wl.monitors();
        if monitors.is_empty() {
            return self.fail("no monitors found");
        }
        // The pointer can be shown or not in the overlay: both are taken
        // (unless the settings say always or never).
        let plan = Plan {
            monitors: monitors.iter().map(|m| (m.name.clone(), m.rect)).collect(),
            each_screen: self.cfg.screenshot_method == "screens",
            cursor: if instant { self.cfg.include_pointer } else { self.cfg.region_pointer == "shown" },
            both: !instant && self.cfg.region_pointer == "toggle",
        };
        let order = capture::backend_order(&self.cfg.backend, self.wl.screencopy.is_some());
        // Ask the compositor about its windows now, while they are as captured.
        self.desktop = None;
        self.desktop_rx = None;
        if matches!(mode, Mode::Monitor | Mode::Window) || names_window(&self.cfg) || (!instant && self.cfg.detect_windows) {
            let (tx, rx) = mpsc::channel();
            let waker = self.waker.clone();
            let _ = std::thread::Builder::new().name("flatshot-windows".into()).spawn(move || {
                let _ = tx.send(crate::desktop::windows::query(DESKTOP_WAIT));
                waker();
            });
            self.desktop_rx = Some(rx);
        }
        if !instant {
            // The overlay's surfaces are made now and configured while the
            // screen is taken: the first frame can go up as the pixels arrive.
            let namespace = layer_namespace();
            for (o, mon) in monitors.iter().enumerate() {
                if let Err(e) = self.wl.add_overlay(Owner::Overlay(o), mon, &namespace) {
                    return self.fail(&e);
                }
            }
            self.monitors = monitors.clone();
        }
        let mut pending = Pending { job, mode, monitors, plan, backend: "none", source: Source::Screencopy, rest: order, errors: Vec::new() };
        if self.next_backend(&mut pending) {
            self.pending = Some(pending);
        }
    }

    /// Start the next backend of `p`; false (and the capture failed) if none is left.
    fn next_backend(&mut self, p: &mut Pending) -> bool {
        while !p.rest.is_empty() {
            let backend = p.rest.remove(0);
            p.backend = backend;
            if backend == "screencopy" {
                match self.wl.start_screencopy(&p.monitors, p.plan.cursor, p.plan.both) {
                    Ok(()) => {
                        p.source = Source::Screencopy;
                        return true;
                    }
                    Err(e) => {
                        p.errors.push(format!("screencopy: {e}"));
                        continue;
                    }
                }
            }
            let (tx, rx) = mpsc::channel();
            let (plan, waker) = (p.plan.clone(), self.waker.clone());
            let started = std::thread::Builder::new().name("flatshot-capture".into()).spawn(move || {
                let _ = tx.send(capture::grab(backend, &plan));
                waker();
            });
            match started {
                Ok(_) => {
                    p.source = Source::Worker(rx);
                    return true;
                }
                Err(e) => p.errors.push(format!("{backend}: {e}")),
            }
        }
        let why = if p.errors.is_empty() { "no way to capture the screen here".to_string() } else { p.errors.join("\n  ") };
        self.fail(&format!("could not capture the screen\n  {why}"));
        false
    }

    /// The capture in progress, if its pictures are in (or it failed).
    pub(super) fn poll_capture(&mut self) {
        let Some(p) = &mut self.pending else { return };
        let result = match &p.source {
            Source::Screencopy if self.wl.screencopy_done() => {
                let mut out = Vec::new();
                let mut err = None;
                for (monitor, cursor, r) in self.wl.take_screencopy() {
                    match r {
                        Ok(picture) => out.push(Grabbed { monitor, cursor, picture }),
                        Err(e) => err = Some(e),
                    }
                }
                match err {
                    Some(e) => Err(e),
                    None => Ok(out),
                }
            }
            Source::Worker(rx) => match rx.try_recv() {
                Ok(r) => r,
                Err(mpsc::TryRecvError::Empty) => return,
                Err(mpsc::TryRecvError::Disconnected) => Err("the capture stopped".into()),
            },
            _ => return,
        };
        let Some(mut p) = self.pending.take() else { return };
        match result {
            Ok(grabbed) => self.captured(p, grabbed),
            Err(e) => {
                p.errors.push(format!("{}: {e}", p.backend));
                if self.next_backend(&mut p) {
                    self.pending = Some(p);
                }
            }
        }
    }

    /// The screens are in: show the overlays (or finish an instant capture).
    fn captured(&mut self, p: Pending, grabbed: Vec<Grabbed>) {
        let n = p.monitors.len();
        let mut plain: Vec<Option<Image>> = (0..n).map(|_| None).collect();
        let mut with: Vec<Option<Image>> = (0..n).map(|_| None).collect();
        let mut both = false;
        for g in grabbed {
            if g.monitor >= n {
                continue;
            }
            if g.cursor && p.plan.both {
                with[g.monitor] = Some(g.picture);
                both = true;
            } else {
                plain[g.monitor] = Some(g.picture);
            }
        }
        let mut pictures = Vec::with_capacity(n);
        for (m, mon) in p.monitors.iter().enumerate() {
            let (picture, pointer) = match (plain[m].take(), with[m].take()) {
                (Some(a), Some(b)) => {
                    let patch = capture::pointer_patch(&a, &b);
                    (a, patch)
                }
                (Some(a), None) | (None, Some(a)) => (a, None),
                (None, None) => return self.fail(&format!("nothing captured on {}", mon.name)),
            };
            pictures.push((picture, pointer));
        }
        let detail = pictures.iter().map(|(p, _)| format!("{} × {}", p.width(), p.height())).collect::<Vec<_>>().join(", ");
        self.clock.step("screenshot", format!("{detail} via {}", p.backend));
        if p.mode != Mode::Region {
            let screens = p.monitors.into_iter().zip(pictures.into_iter().map(|(pic, _)| pic)).collect();
            self.taken = Some(Taken { job: p.job, mode: p.mode, screens, wait_until: Instant::now() + DESKTOP_WAIT });
            return self.finish_instant(false);
        }
        let screens = p
            .monitors
            .iter()
            .zip(pictures)
            .map(|(mon, (picture, pointer))| Screen {
                output: Output { name: mon.name.clone(), rect: mon.rect, scale: mon.scale as f32 },
                picture: Arc::new(picture),
                pointer,
            })
            .collect();
        let request = Request { pin: p.job.pin, scan: p.job.scan, record: p.job.record, image: false };
        let setup = Setup { cfg: self.cfg.clone(), request, screens, pointer_taken: both, cursor: None, primary: 0, can_record: true };
        let mut session = Session::new(setup, Box::new(NoRecorder), self.waker.clone());
        session.clock = self.clock.clone();
        if let Some(d) = &self.desktop {
            session.desktop_found(d.clone());
        }
        self.session = Some(session);
        self.first_frame = true;
    }

    /// Crop an instant capture to its mode's area and deliver it, once the
    /// desktop is known (or `force`: it won't be).
    pub(super) fn finish_instant(&mut self, force: bool) {
        let waiting = self.desktop_rx.is_some() && self.taken.as_ref().is_some_and(|t| Instant::now() < t.wait_until);
        if waiting && !force {
            return;
        }
        let Some(t) = self.taken.take() else { return };
        self.desktop_rx = None;
        let desktop = self.desktop.clone().unwrap_or_default();
        let mut shot = Shot::new("desktop");
        if let Some(a) = &desktop.active {
            shot.app = a.app.clone();
            shot.title = a.title.clone();
        }
        let virt = t.screens.iter().fold(IRect::default(), |acc, (m, _)| acc.united(&m.rect));
        let area = match t.mode {
            Mode::Rect(r) => {
                shot.mode = "region".into();
                r
            }
            Mode::Monitor => {
                let at = desktop.cursor.map(|c| IPoint::new(c.x.round() as i32, c.y.round() as i32));
                let mon = at.and_then(|p| t.screens.iter().find(|(m, _)| m.rect.contains(p))).unwrap_or(&t.screens[0]);
                shot.mode = "monitor".into();
                shot.monitor = mon.0.name.clone();
                mon.0.rect
            }
            Mode::Window => match &desktop.active {
                Some(w) => {
                    shot.mode = "window".into();
                    w.rect
                }
                None => return self.fail("No active window to capture. Active-window capture needs KDE Plasma, Sway or Hyprland."),
            },
            _ => virt,
        };
        let parts: Vec<(IRect, &Image)> = t.screens.iter().map(|(m, img)| (m.rect, img)).collect();
        let image = match capture::compose(&parts, area) {
            Ok(img) => img,
            Err(e) => return self.fail(&e),
        };
        self.clock.step("cropped", format!("{} × {}", image.width(), image.height()));
        drop(parts);
        drop(t.screens);
        let remember = matches!(t.mode, Mode::Rect(_));
        let code = self.deliver_image(image, shot, Some(area), remember, t.job.pin, None);
        crate::mem::trim();
        self.ended(code);
    }
}
