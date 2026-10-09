//! The running program on Wayland: one capture from the command line, or
//! (later) the tray app. Ties the compositor's events to the session and
//! the session's outcome to delivery.

mod finish;

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::seat::pointer::CursorIcon;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_output;

use crate::capture::{self, Grabbed, Plan};
use crate::cli::Args;
use crate::config::{self, Config};
use crate::geom::{Damage, IRect, Point};
use crate::image::{Canvas, Image};
use crate::keys::KeyPress;
use crate::render::theme;
use crate::timing::{self, Clock};
use crate::ui::overlay::Output;
use crate::ui::session::{Button, CursorShape, Effect, RecordSupport, Request, Screen, Session, Setup, Waker};
use crate::ui::state::Desktop;
use crate::wayland::{Monitor, Owner, Painted, Wayland};

/// Input for a surface, as the platform reports it.
pub enum Input {
    Enter((f64, f64)),
    Leave,
    /// where, and whether a finger (not a mouse) moved
    Motion((f64, f64), bool),
    /// a Linux button code, pressed or not, where
    Button(u32, bool, (f64, f64)),
    /// wheel notches, up positive
    Scroll(f32),
    Key(KeyPress),
    KeyUp(KeyPress),
    Modifiers(i32),
}

/// Where a capture's pictures will come from.
enum Source {
    /// the Wayland connection (wlr-screencopy)
    Screencopy,
    /// a worker thread (KWin, the portal, a program), with which backend
    Worker(mpsc::Receiver<Result<Vec<Grabbed>, String>>),
}

/// A capture being taken.
struct Pending {
    request: Request,
    monitors: Vec<Monitor>,
    plan: Plan,
    backend: &'static str,
    source: Source,
    /// the backends left to try if this one fails, and why the others did
    rest: Vec<&'static str>,
    errors: Vec<String>,
}

pub struct App {
    pub wl: Wayland,
    pub loop_handle: LoopHandle<'static, App>,
    pub cfg: Config,
    pub session: Option<Session>,
    /// the session's monitors, as its overlays are numbered
    monitors: Vec<Monitor>,
    pending: Option<Pending>,
    /// the compositor's description of the desktop, asked at capture time
    desktop_rx: Option<mpsc::Receiver<Option<Desktop>>>,
    desktop: Option<Desktop>,
    pub exit: Option<i32>,
    waker: Waker,
    clock: Clock,
    first_frame: bool,
    output_file: Option<String>,
    /// one capture from the command line (not the tray app)
    one_shot: bool,
    /// a one-off capture's exit code, once its session is over
    finished: Option<i32>,
    /// until when it stays to serve the clipboard
    hold_until: Option<Instant>,
    /// the clipboard's text, being read for the session
    paste_rx: Option<mpsc::Receiver<Option<String>>>,
}

impl Painted for Session {
    fn damage(&mut self, part: usize) -> Damage {
        Session::damage(self, part)
    }
    fn paint(&self, part: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect) {
        Session::paint(self, part, target, origin, region);
    }
    fn buffer_size(&self, part: usize) -> (u32, u32) {
        self.overlays.get(part).map(|o| o.device_size()).unwrap_or((0, 0))
    }
}

/// Until recording is in: why it can't be used.
struct NoRecorder;

impl RecordSupport for NoRecorder {
    fn problem(&mut self, _: &str) -> Option<String> {
        Some("Recording isn't available in this build yet.".into())
    }
    fn can_span(&self) -> bool {
        false
    }
}

impl App {
    /// Input for `surface` (the focused one for keys).
    pub fn input(&mut self, surface: Option<ObjectId>, ev: Input) {
        let owner = surface.as_ref().and_then(|id| self.wl.owner_of(id));
        let Some(s) = &mut self.session else { return };
        let o = match owner {
            Some(Owner::Overlay(o)) => o,
            None => match ev {
                // (keys before the keyboard entered an overlay: the pointer's one)
                Input::Key(_) | Input::KeyUp(_) | Input::Modifiers(_) => s.st.pointer_overlay.unwrap_or(0),
                _ => return,
            },
        };
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        let mods = self.wl.mods;
        match ev {
            Input::Enter(at) => s.pointer_enter(o, p(at)),
            Input::Leave => s.pointer_leave(o),
            Input::Motion(at, touch) => s.pointer_motion(o, p(at), mods, touch),
            Input::Button(b, pressed, at) => {
                let button = match b {
                    0x110 => Button::Left,
                    0x111 => Button::Right,
                    0x112 => Button::Middle,
                    _ => Button::Other,
                };
                s.button(o, button, pressed, p(at), mods)
            }
            Input::Scroll(steps) => s.scroll(o, steps),
            Input::Key(k) => s.key(o, &k),
            Input::KeyUp(k) => s.key_release(o, &k),
            Input::Modifiers(m) => s.modifiers(m),
        }
    }

    /// Text from an input method.
    pub fn commit_string(&mut self, text: &str) {
        if let Some(s) = &mut self.session {
            s.commit_string(text);
        }
    }

    /// The compositor closed one of our surfaces (or asked to).
    pub fn surface_closed(&mut self, id: &ObjectId) {
        if let Some(Owner::Overlay(_)) = self.wl.owner_of(id)
            && let Some(s) = &mut self.session
        {
            s.cancel();
        }
    }

    /// A monitor went away: a capture on it can't go on.
    pub fn monitor_gone(&mut self, output: &wl_output::WlOutput) {
        if self.monitors.iter().any(|m| &m.output == output)
            && let Some(s) = &mut self.session
        {
            s.fail("a monitor was disconnected");
        }
    }

    // -- capturing ---------------------------------------------------------------------

    /// Take the screens for a capture with `request`.
    fn start_capture(&mut self, request: Request) {
        let monitors = self.wl.monitors();
        if monitors.is_empty() {
            return self.fail("no monitors found");
        }
        // The pointer can be shown or not in the overlay: take both (unless
        // the settings say always or never).
        let plan = Plan {
            monitors: monitors.iter().map(|m| (m.name.clone(), m.rect)).collect(),
            each_screen: self.cfg.screenshot_method == "screens",
            cursor: self.cfg.region_pointer == "shown",
            both: self.cfg.region_pointer == "toggle" && !request.image,
        };
        let order = capture::backend_order(&self.cfg.backend, self.wl.screencopy.is_some());
        // Ask the compositor about its windows now, while they are as captured.
        self.desktop = None;
        if self.cfg.detect_windows || names_window(&self.cfg) {
            let (tx, rx) = mpsc::channel();
            let waker = self.waker.clone();
            let _ = std::thread::Builder::new().name("flatshot-windows".into()).spawn(move || {
                let _ = tx.send(crate::desktop::windows::query(Duration::from_millis(1500)));
                waker();
            });
            self.desktop_rx = Some(rx);
        }
        // The overlay's surfaces are made now and configured while the
        // screen is taken: the first frame can go up as the pixels arrive.
        let namespace = layer_namespace();
        for (o, mon) in monitors.iter().enumerate() {
            if let Err(e) = self.wl.add_overlay(Owner::Overlay(o), mon, &namespace) {
                return self.fail(&e);
            }
        }
        self.monitors = monitors.clone();
        let mut pending = Pending { request, monitors, plan, backend: "none", source: Source::Screencopy, rest: order, errors: Vec::new() };
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
    fn poll_capture(&mut self) {
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

    /// The screens are in: show the overlays.
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
        let mut screens = Vec::with_capacity(n);
        for (m, mon) in p.monitors.iter().enumerate() {
            let (picture, pointer) = match (plain[m].take(), with[m].take()) {
                (Some(a), Some(b)) => {
                    let patch = capture::pointer_patch(&a, &b);
                    (a, patch)
                }
                (Some(a), None) | (None, Some(a)) => (a, None),
                (None, None) => return self.fail(&format!("nothing captured on {}", mon.name)),
            };
            screens.push(Screen { output: Output { name: mon.name.clone(), rect: mon.rect, scale: mon.scale as f32 }, picture: Arc::new(picture), pointer });
        }
        let detail = screens.iter().map(|s| format!("{} × {}", s.picture.width(), s.picture.height())).collect::<Vec<_>>().join(", ");
        self.clock.step("screenshot", format!("{detail} via {}", p.backend));
        let setup = Setup { cfg: self.cfg.clone(), request: p.request, screens, pointer_taken: both, cursor: None, primary: 0, can_record: true };
        let mut session = Session::new(setup, Box::new(NoRecorder), self.waker.clone());
        session.clock = self.clock.clone();
        if let Some(d) = &self.desktop {
            session.desktop_found(d.clone());
        }
        self.session = Some(session);
        self.first_frame = true;
    }

    fn fail(&mut self, message: &str) {
        self.close_session();
        self.pending = None;
        self.report_failure("Screenshot failed", message);
        self.ended(2);
    }

    fn close_session(&mut self) {
        self.session = None;
        self.wl.remove_overlays();
        self.monitors.clear();
        crate::mem::trim();
    }

    // -- the loop ------------------------------------------------------------------------

    /// How long the loop may sleep.
    fn timeout(&self) -> Option<Duration> {
        let session = self.session.as_ref().and_then(|s| s.next_deadline());
        let deadline = [session, self.hold_until].into_iter().flatten().min()?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// After each round of events: finish what's ready, draw what changed.
    pub fn after_dispatch(&mut self) {
        self.poll_capture();
        if let Some(rx) = &self.desktop_rx
            && let Ok(found) = rx.try_recv()
        {
            self.desktop_rx = None;
            if let Some(d) = found {
                if let Some(s) = &mut self.session {
                    s.desktop_found(d.clone());
                }
                self.desktop = Some(d);
            }
        }
        if let Some(rx) = &self.paste_rx
            && let Ok(text) = rx.try_recv()
        {
            self.paste_rx = None;
            if let (Some(s), Some(text)) = (&mut self.session, text) {
                s.paste(&text);
            }
        }
        if let Some(s) = &mut self.session {
            s.poll();
            if s.next_deadline().is_some_and(|d| d <= Instant::now()) {
                s.on_timer(Instant::now());
            }
            for effect in s.take_effects() {
                match effect {
                    Effect::Clipboard(text) => self.copy_now(&text),
                    Effect::Paste => self.paste_rx = Some(self.wl.clipboard.paste_text(self.waker.clone())),
                }
            }
        }
        if let Some(outcome) = self.session.as_mut().and_then(|s| s.take_outcome()) {
            self.finish(outcome);
        }
        self.redraw();
        self.update_cursor();
        self.update_text_input();
        let _ = self.wl.conn.flush();
        if let Some(code) = self.finished
            && self.exit.is_none()
            && self.may_exit()
        {
            self.exit = Some(code);
        }
    }

    fn redraw(&mut self) {
        let Some(s) = &mut self.session else { return };
        let mut drawn = false;
        for i in 0..self.wl.surfaces.len() {
            let Owner::Overlay(o) = self.wl.surfaces[i].owner;
            drawn |= self.wl.redraw(i, o, s);
        }
        if drawn && self.first_frame {
            self.first_frame = false;
            self.clock.step("first frame", "");
        }
    }

    fn update_cursor(&mut self) {
        let (Some(s), Some(on)) = (&self.session, &self.wl.pointer_on) else { return };
        let Some(i) = self.wl.surface_index(on) else { return };
        let Owner::Overlay(o) = self.wl.surfaces[i].owner;
        let icon = match s.cursor(o) {
            CursorShape::Crosshair => CursorIcon::Crosshair,
            CursorShape::Text => CursorIcon::Text,
            CursorShape::Move => CursorIcon::Move,
            CursorShape::Hand => CursorIcon::Pointer,
            CursorShape::Arrow => CursorIcon::Default,
            CursorShape::ResizeNS => CursorIcon::NsResize,
            CursorShape::ResizeEW => CursorIcon::EwResize,
            CursorShape::ResizeNWSE => CursorIcon::NwseResize,
            CursorShape::ResizeNESW => CursorIcon::NeswResize,
        };
        if self.wl.surfaces[i].cursor != Some(icon)
            && let Some(p) = &self.wl.pointer
            && p.set_cursor(&self.wl.conn, icon).is_ok()
        {
            self.wl.surfaces[i].cursor = Some(icon);
        }
    }

    fn update_text_input(&mut self) {
        let caret = match (&self.session, &self.wl.keyboard_on) {
            (Some(s), Some(on)) => match self.wl.owner_of(on) {
                Some(Owner::Overlay(o)) => s.text_input(o),
                None => None,
            },
            _ => None,
        };
        self.wl.set_text_input(caret);
    }
}

/// Do the file name settings use the window's app or title?
fn names_window(cfg: &Config) -> bool {
    let both = format!("{}{}", cfg.save_dir, cfg.filename);
    both.contains("{app") || both.contains("{title")
}

/// The layer-shell namespace: KWin makes it a window type and animates
/// every one it doesn't know but "utility" (which every Plasma 6 knows).
fn layer_namespace() -> String {
    if let Ok(s) = std::env::var("FLATSHOT_LAYER_SCOPE")
        && !s.is_empty()
    {
        return s;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.contains("kde") { "utility".into() } else { "flatshot".into() }
}

/// One capture from the command line: the overlay, then the result.
pub fn run_once(args: &Args) -> i32 {
    let started = Instant::now();
    // Find the fonts while the screen is being captured, so the first frame
    // doesn't wait for them.
    crate::dbus::connect_early();
    let _ = std::thread::Builder::new().name("flatshot-fonts".into()).spawn(move || {
        crate::render::text::warm_up();
        timing::log(format!("fonts ready at {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
    });
    let mut cfg = config::load();
    if args.no_copy {
        cfg.clipboard = "none".into();
    }
    if args.no_save {
        cfg.save_to_disk = false;
    }
    theme::use_theme(&cfg.theme);
    let (wl, mut queue) = match Wayland::connect() {
        Ok(x) => x,
        Err(e) => {
            crate::warn!("{e}");
            return 2;
        }
    };
    let mut event_loop: EventLoop<'static, App> = match EventLoop::try_new() {
        Ok(l) => l,
        Err(e) => {
            crate::warn!("event loop: {e}");
            return 2;
        }
    };
    let (ping, ping_source) = match calloop::ping::make_ping() {
        Ok(p) => p,
        Err(e) => {
            crate::warn!("event loop: {e}");
            return 2;
        }
    };
    let _ = event_loop.handle().insert_source(ping_source, |_, _, _| {});
    let mut app = App {
        wl,
        loop_handle: event_loop.handle(),
        cfg,
        session: None,
        monitors: Vec::new(),
        pending: None,
        desktop_rx: None,
        desktop: None,
        exit: None,
        waker: Arc::new(move || ping.ping()),
        clock: Clock::since("region capture", started),
        first_frame: false,
        output_file: args.output.clone(),
        one_shot: true,
        finished: None,
        hold_until: None,
        paste_rx: None,
    };
    // The monitors (and xdg-output's logical layout) arrive in two rounds.
    for _ in 0..2 {
        if let Err(e) = queue.roundtrip(&mut app) {
            crate::warn!("Wayland: {e}");
            return 2;
        }
    }
    timing::log(format!("connected to Wayland in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
    if let Err(e) = WaylandSource::new(app.wl.conn.clone(), queue).insert(event_loop.handle()) {
        crate::warn!("Wayland source: {e}");
        return 2;
    }
    if args.delay > 0.0 {
        std::thread::sleep(Duration::from_secs_f64(args.delay.min(3600.0)));
    }
    let request = Request { pin: args.pin, scan: !args.no_scan, record: args.record, image: false };
    app.clock = Clock::since("region capture", started);
    app.start_capture(request);
    app.after_dispatch(); // (it may have ended already)
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
