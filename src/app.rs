//! The running program on Wayland: one capture from the command line, or
//! (later) the tray app. Ties the compositor's events to the session and
//! the session's outcome to delivery.

use std::sync::Arc;
use std::time::{Duration, Instant};

use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;
use smithay_client_toolkit::seat::pointer::CursorIcon;
use wayland_client::backend::ObjectId;
use wayland_client::protocol::wl_output;

use crate::capture;
use crate::cli::Args;
use crate::config::{self, Config};
use crate::geom::{Damage, IRect, Point};
use crate::image::{Canvas, Image};
use crate::keys::KeyPress;
use crate::render::theme;
use crate::template::{self, Shot};
use crate::timing::{self, Clock};
use crate::ui::overlay::Output;
use crate::ui::session::{Button, CursorShape, Outcome, RecordSupport, Request, Screen, Session, Setup, Waker};
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

/// A capture being taken.
struct Pending {
    request: Request,
    monitors: Vec<Monitor>,
    /// pictures with and without the pointer are taken
    both: bool,
}

pub struct App {
    pub wl: Wayland,
    pub loop_handle: LoopHandle<'static, App>,
    pub cfg: Config,
    pub session: Option<Session>,
    /// the session's monitors, as its overlays are numbered
    monitors: Vec<Monitor>,
    pending: Option<Pending>,
    pub exit: Option<i32>,
    waker: Waker,
    clock: Clock,
    first_frame: bool,
    output_file: Option<String>,
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
        self.clock = Clock::new("region capture");
        let monitors = self.wl.monitors();
        if monitors.is_empty() {
            return self.fail("no monitors found");
        }
        // The pointer can be shown or not in the overlay: take both (unless
        // the settings say always or never).
        let both = self.cfg.region_pointer == "toggle" && !request.image;
        let cursor = self.cfg.region_pointer == "shown";
        if let Err(e) = self.wl.start_screencopy(&monitors, cursor, both) {
            return self.fail(&e);
        }
        self.pending = Some(Pending { request, monitors, both });
    }

    /// The screens are in: show the overlays.
    fn captured(&mut self) {
        let Some(p) = self.pending.take() else { return };
        let results = self.wl.take_screencopy();
        let n = p.monitors.len();
        let mut plain: Vec<Option<Image>> = (0..n).map(|_| None).collect();
        let mut with: Vec<Option<Image>> = (0..n).map(|_| None).collect();
        for (m, cursor, r) in results {
            match r {
                Ok(img) if cursor => with[m] = Some(img),
                Ok(img) => plain[m] = Some(img),
                Err(e) => return self.fail(&format!("could not capture {}: {e}", p.monitors[m].name)),
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
            screens.push(Screen {
                output: Output { name: mon.name.clone(), rect: mon.rect, scale: mon.scale as f32 },
                picture: Arc::new(picture),
                pointer,
            });
        }
        let detail = screens.iter().map(|s| format!("{} × {}", s.picture.width(), s.picture.height())).collect::<Vec<_>>().join(", ");
        self.clock.step("screenshot", format!("{detail} via screencopy"));
        let setup = Setup {
            cfg: self.cfg.clone(),
            request: p.request,
            screens,
            pointer_taken: p.both,
            cursor: None,
            primary: 0,
            can_record: true,
        };
        let session = Session::new(setup, Box::new(NoRecorder), self.waker.clone());
        let namespace = layer_namespace();
        for (o, mon) in p.monitors.iter().enumerate() {
            if let Err(e) = self.wl.add_overlay(Owner::Overlay(o), mon, &namespace) {
                return self.fail(&e);
            }
        }
        self.monitors = p.monitors;
        self.session = Some(session);
        self.first_frame = true;
        self.clock.step("overlays made", "");
    }

    fn fail(&mut self, message: &str) {
        crate::warn!("{message}");
        self.close_session();
        self.exit = Some(2);
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
        let deadline = self.session.as_ref().and_then(|s| s.next_deadline())?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// After each round of events: finish what's ready, draw what changed.
    pub fn after_dispatch(&mut self) {
        if self.pending.is_some() && self.wl.screencopy_done() {
            self.captured();
        }
        if let Some(s) = &mut self.session {
            s.poll();
            if s.next_deadline().is_some_and(|d| d <= Instant::now()) {
                s.on_timer(Instant::now());
            }
            let _ = s.take_effects(); // (the clipboard comes with delivery)
            if let Some(outcome) = s.take_outcome() {
                self.finish(outcome);
            }
        }
        self.redraw();
        self.update_cursor();
        self.update_text_input();
        let _ = self.wl.conn.flush();
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

    /// The session ended: deliver what came of it.
    fn finish(&mut self, outcome: Outcome) {
        self.close_session();
        let _ = self.wl.conn.flush();
        self.exit = Some(match outcome {
            Outcome::Image { image, shot, .. } => match self.save(&image, &shot) {
                Ok(path) => {
                    println!("{path}");
                    0
                }
                Err(e) => {
                    crate::warn!("{e}");
                    2
                }
            },
            Outcome::Color { hex, .. } => {
                println!("{hex}");
                0
            }
            Outcome::CodeText(text) | Outcome::OpenLink(text) => {
                println!("{text}");
                0
            }
            Outcome::Record(_) => 0,
            Outcome::Cancelled => 1,
            Outcome::Failed(e) => {
                crate::warn!("{e}");
                2
            }
        });
    }

    /// (Until delivery is in: save the picture where the settings say.)
    fn save(&mut self, image: &Image, shot: &Shot) -> Result<String, String> {
        let ext = match self.cfg.format.as_str() {
            "jpg" | "jpeg" => "jpg",
            "webp" => "webp",
            _ => "png",
        };
        let path = match &self.output_file {
            Some(p) => std::path::PathBuf::from(p),
            None => template::screenshot_path(&self.cfg, shot, image.size(), ext, false),
        };
        let bytes = crate::codec::encode(image, ext, self.cfg.quality.clamp(1, 100) as u8)?;
        if let Some(dir) = path.parent() {
            std::fs::create_dir_all(dir).map_err(|e| format!("{}: {e}", dir.display()))?;
        }
        std::fs::write(&path, bytes).map_err(|e| format!("{}: {e}", path.display()))?;
        self.clock.step("saved", path.display().to_string());
        Ok(path.display().to_string())
    }
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
        exit: None,
        waker: Arc::new(move || ping.ping()),
        clock: Clock::since("region capture", started),
        first_frame: false,
        output_file: args.output.clone(),
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
    app.start_capture(request);
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
