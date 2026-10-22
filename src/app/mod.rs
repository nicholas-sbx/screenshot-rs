//! The running program on Wayland: one capture from the command line, or
//! (later) the tray app. Ties the compositor's events to the session and
//! the session's outcome to delivery.

mod capture;
mod caret;
mod countdown;
mod editor;
mod finish;
mod ocr;
mod pins;
mod recent;
mod recording;
mod settings;
mod tray;

pub use capture::{Job, Mode, last_region};
pub use recent::run_recent;
pub use settings::run_settings;
pub use tray::{replace_older_tray, run_tray};

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use calloop::{EventLoop, LoopHandle};
use calloop_wayland_source::WaylandSource;

use crate::cli::Args;
use crate::config::{self, Config};
use crate::geom::{Damage, IRect, Point, Rect};
use crate::image::Canvas;
use crate::keys::KeyPress;
use crate::platform::{Display, Monitor, Output, Owner, Painted, TRAY_MENU};
use crate::render::theme;
use crate::timing::{self, Clock};
use crate::ui::session::{Button, CursorShape, Effect, Session, Waker};
use crate::ui::state::Desktop;
use crate::wayland::Wayland;

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

pub struct App {
    pub dpy: Display,
    pub loop_handle: LoopHandle<'static, App>,
    pub cfg: Config,
    pub session: Option<Session>,
    /// the session's monitors, as its overlays are numbered
    monitors: Vec<Monitor>,
    pending: Option<capture::Pending>,
    /// an instant capture's pictures, waiting for the desktop's description
    taken: Option<capture::Taken>,
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
    /// the tray app's services
    tray: tray::TrayState,
    /// to the event loop from the tray app's threads
    events: Option<calloop::channel::Sender<tray::Event>>,
    /// captures pinned to the screen
    pins: Vec<pins::Pin>,
    next_pin: u32,
    /// annotation editor windows
    editors: Vec<editor::EditorWin>,
    next_editor: u32,
    /// "Annotate an image…": the file dialog's answer
    open_dialog: Option<mpsc::Receiver<Result<Option<std::path::PathBuf>, String>>>,
    /// the settings window
    settings: Option<settings::SettingsWin>,
    /// the screen recording in progress
    recording: Option<recording::RecordingRun>,
    /// the tray app quits once the recording is saved
    quit_after_recording: bool,
    /// the tray app starts again once it has quit
    restarting: bool,
    /// the text caret's blink
    blink: caret::Blink,
    /// areas whose text is being read
    reading: Vec<ocr::Reading>,
    /// the Recent captures window
    recent: Option<recent::RecentWin>,
    /// a delayed capture counting down
    countdown: Option<countdown::Countdown>,
    /// "Annotate clipboard image": the clipboard's picture being read
    /// (whether by wl-paste, the second try, and the file -o named for it)
    clipboard_image: Option<(crate::clipboard::Pasted, bool, Option<std::path::PathBuf>)>,
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

impl App {
    /// The Wayland connection's state (only Wayland's events ask for it).
    pub fn wl(&mut self) -> &mut Wayland {
        match &mut self.dpy {
            Display::Wayland(w) => w,
            Display::X11(_) => unreachable!("a Wayland event on X11"),
        }
    }

    /// Input for `owner`'s surface (the focused one for keys).
    pub fn input(&mut self, owner: Option<Owner>, ev: Input) {
        match owner {
            Some(Owner::Pin(id)) | Some(Owner::PinPart(id, _)) => return self.pin_input(id, ev),
            Some(Owner::Menu(TRAY_MENU)) => return self.tray_menu_input(ev),
            Some(Owner::TrayIcon) => return self.tray_icon_input(ev),
            Some(Owner::Menu(id)) => return self.menu_input(id, ev),
            Some(Owner::Editor(id)) => return self.editor_input(id, ev),
            Some(Owner::Settings) => return self.settings_input(ev),
            Some(Owner::Recent) => return self.recent_input(ev),
            Some(Owner::RecBar) => return self.recbar_input(ev),
            Some(Owner::RecMark(_) | Owner::Countdown(_)) => return,
            _ => {}
        }
        let Some(s) = &mut self.session else { return };
        let o = match owner {
            Some(Owner::Overlay(o)) => o,
            Some(_) => return,
            None => match ev {
                // (keys before the keyboard entered an overlay: the pointer's one)
                Input::Key(_) | Input::KeyUp(_) | Input::Modifiers(_) => s.st.pointer_overlay.unwrap_or(0),
                _ => return,
            },
        };
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        let mods = self.dpy.mods();
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

    /// Ctrl+C, a logout or `kill`: a recording is saved first; anything
    /// else ends now.
    fn signal(&mut self, signal: i32) {
        timing::log(format!("signal {signal}"));
        if self.is_recording() && !self.quit_after_recording {
            self.quit_after_recording = true;
            self.stop_recording();
            return;
        }
        self.exit = Some(crate::signals::exit_code(signal));
    }

    /// Text from an input method.
    pub fn commit_string(&mut self, text: &str) {
        let focus = self.dpy.text_owner().or(self.dpy.keyboard_owner());
        match focus {
            Some(Owner::Editor(id)) => return self.editor_commit_string(id, text),
            Some(Owner::Settings) => return self.settings_commit_string(text),
            _ => {}
        }
        if let Some(s) = &mut self.session {
            s.commit_string(text);
        }
    }

    /// The display server closed one of our surfaces (or asked to).
    pub fn surface_closed(&mut self, owner: Owner) {
        match owner {
            Owner::Overlay(_) => {
                if let Some(s) = &mut self.session {
                    s.cancel();
                }
            }
            Owner::Pin(p) => self.close_pin(p),
            Owner::PinPart(p, n) => self.pin_part_closed(p, n),
            Owner::Menu(TRAY_MENU) => self.close_tray_menu(),
            Owner::Menu(p) => self.close_menu(p),
            Owner::TrayIcon => {}
            Owner::Editor(e) => self.editor_close_requested(e),
            Owner::Settings => self.close_settings(),
            Owner::Recent => self.close_recent(),
            // (the bar's window closed by the desktop: done recording)
            Owner::RecBar => self.stop_recording(),
            Owner::RecMark(_) | Owner::Countdown(_) => {}
        }
    }

    /// The monitors the capture in progress covers.
    pub fn monitors_in_use(&self) -> Vec<Output> {
        self.monitors.iter().map(|m| m.output.clone()).collect()
    }

    /// A monitor went away: a capture on it can't go on.
    pub fn monitor_gone(&mut self, output: &Output) {
        if self.monitors.iter().any(|m| &m.output == output)
            && let Some(s) = &mut self.session
        {
            s.fail("a monitor was disconnected");
        }
    }

    fn fail(&mut self, message: &str) {
        self.close_session();
        self.pending = None;
        self.taken = None;
        self.report_failure("Screenshot failed", message);
        self.ended(2);
    }

    fn close_session(&mut self) {
        self.session = None;
        self.dpy.remove_overlays();
        self.monitors.clear();
        self.dpy.flush();
        crate::render::text::remember_fonts();
        crate::mem::trim();
    }

    // -- the loop ------------------------------------------------------------------------

    /// How long the loop may sleep.
    fn timeout(&self) -> Option<Duration> {
        let session = self.session.as_ref().and_then(|s| s.next_deadline());
        let taken = self.taken.as_ref().map(|t| t.wait_until);
        let settings = self.settings.as_ref().and_then(|w| w.s.next_deadline());
        let deadline = [
            session,
            self.hold_until,
            taken,
            self.editors_deadline(),
            settings,
            self.recent_deadline(),
            self.countdown_deadline(),
            self.recording_deadline(),
            self.dpy.next_deadline(),
            self.blink.deadline(),
        ]
        .into_iter()
        .flatten()
        .min()?;
        Some(deadline.saturating_duration_since(Instant::now()))
    }

    /// After each round of events: finish what's ready, draw what changed.
    pub fn after_dispatch(&mut self) {
        self.dpy.tick();
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
        if self.taken.is_some() {
            self.finish_instant(false);
        }
        self.poll_notifications();
        self.poll_editors();
        self.poll_open_dialog();
        self.poll_clipboard_image();
        self.poll_settings();
        self.poll_recording();
        self.poll_reading();
        self.poll_recent();
        self.poll_countdown();
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
                    Effect::Paste => self.paste_rx = Some(self.dpy.paste_text(self.waker.clone())),
                }
            }
        }
        if let Some(outcome) = self.session.as_mut().and_then(|s| s.take_outcome()) {
            self.finish(outcome);
        }
        self.blink_caret();
        self.redraw();
        self.update_cursor();
        self.update_text_input();
        self.dpy.flush();
        // (events read meanwhile, while waiting for a reply, are no longer
        // on the socket for the loop to hear: the wake handles them)
        if let Display::X11(x) = &mut self.dpy
            && x.has_events()
        {
            (self.waker)();
        }
        if let Some(code) = self.finished
            && self.exit.is_none()
            && self.may_exit()
        {
            self.exit = Some(code);
        }
    }

    fn redraw(&mut self) {
        self.redraw_pins();
        self.redraw_editors();
        self.redraw_settings();
        self.redraw_recent();
        self.redraw_countdown();
        self.redraw_recording();
        self.redraw_tray_icon();
        let Some(s) = &mut self.session else { return };
        let mut drawn = false;
        for owner in self.dpy.owners() {
            if let Owner::Overlay(o) = owner {
                crate::ui::caret::set_shown(self.blink.shown_in(owner));
                drawn |= self.dpy.redraw(owner, o, s);
            }
        }
        if drawn && self.first_frame {
            self.first_frame = false;
            self.clock.step("first frame", "");
        }
    }

    fn update_cursor(&mut self) {
        let Some(owner) = self.dpy.pointer_owner() else { return };
        let shape = match (owner, &self.session) {
            (Owner::Overlay(o), Some(s)) => s.cursor(o),
            (Owner::Pin(_) | Owner::PinPart(..), _) => CursorShape::Move,
            (Owner::Menu(_), _) => CursorShape::Arrow,
            (Owner::Editor(id), _) => match self.editors.iter().find(|e| e.id == id) {
                Some(e) => e.ed.cursor(),
                None => return,
            },
            (Owner::Settings, _) => match &self.settings {
                Some(w) => w.s.cursor(),
                None => return,
            },
            (Owner::Recent, _) => match &self.recent {
                Some(w) => w.r.cursor(),
                None => return,
            },
            (Owner::RecBar | Owner::TrayIcon, _) => CursorShape::Arrow,
            _ => return,
        };
        self.dpy.set_cursor(owner, shape);
    }

    /// The caret of the text being typed: in which window, and where
    /// (window logical).
    fn caret(&self) -> Option<(Owner, Rect)> {
        match self.dpy.keyboard_owner()? {
            owner @ Owner::Editor(id) => {
                self.editors.iter().find(|e| e.id == id).filter(|e| e.ed.wants_text()).and_then(|e| e.ed.text_input()).map(|r| (owner, r))
            }
            Owner::Settings => self.settings.as_ref().filter(|w| w.s.wants_text()).and_then(|w| w.s.text_input()).map(|r| (Owner::Settings, r)),
            Owner::Overlay(_) => {
                let s = self.session.as_ref()?;
                (0..s.overlays.len()).find_map(|o| s.text_input(o).map(|r| (Owner::Overlay(o), r)))
            }
            _ => None,
        }
    }

    fn update_text_input(&mut self) {
        let caret = match (self.caret(), self.dpy.keyboard_owner()) {
            (Some((owner, r)), Some(k)) if owner == k => Some(r),
            _ => None,
        };
        self.dpy.set_text_input(caret);
    }

    /// Blink the caret: repaint it where it went on or off.
    fn blink_caret(&mut self) {
        let focus = self.dpy.keyboard_owner();
        let caret = self.caret();
        for (owner, r) in self.blink.update(Instant::now(), focus, caret) {
            match owner {
                Owner::Editor(id) => {
                    if let Some(e) = self.editors.iter_mut().find(|e| e.id == id) {
                        e.ed.touch(r);
                    }
                }
                Owner::Settings => {
                    if let Some(w) = &mut self.settings {
                        w.s.touch(r);
                    }
                }
                // (the overlays repaint what looks different by themselves)
                _ => {}
            }
        }
    }
}

/// Do the file name settings use the window's app or title?
fn names_window(cfg: &Config) -> bool {
    let both = format!("{}{}", cfg.save_dir, cfg.filename);
    both.contains("{app") || both.contains("{title")
}

/// The layer-shell namespace: KWin makes it a window type and animates
/// every one it doesn't know but "utility" (which every Plasma 6 knows).
pub fn layer_namespace() -> String {
    if let Ok(s) = std::env::var("FLATSHOT_LAYER_SCOPE")
        && !s.is_empty()
    {
        return s;
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    if desktop.contains("kde") { "utility".into() } else { crate::APP_ID.into() }
}

/// The connection to the compositor and the event loop around it.
fn setup(started: Instant, output_file: Option<String>) -> Result<(App, EventLoop<'static, App>), i32> {
    let cfg = config::load();
    theme::use_theme(&cfg.theme);
    let (dpy, queue) = connect().map_err(|e| {
        crate::warn!("{e}");
        2
    })?;
    let event_loop: EventLoop<'static, App> = EventLoop::try_new().map_err(|e| {
        crate::warn!("event loop: {e}");
        2
    })?;
    let (ping, ping_source) = calloop::ping::make_ping().map_err(|e| {
        crate::warn!("event loop: {e}");
        2
    })?;
    // (a wake from a thread, or from after_dispatch for X events read while
    // waiting for a reply: those are no longer on the socket, so the X
    // source won't fire for them, and they're handled here)
    let _ = event_loop.handle().insert_source(ping_source, |_, _, app: &mut App| {
        if matches!(app.dpy, Display::X11(_)) {
            app.x11_dispatch();
        }
    });
    if let Some(fd) = crate::signals::fd() {
        let source = calloop::generic::Generic::new(fd, calloop::Interest::READ, calloop::Mode::Level);
        let _ = event_loop.handle().insert_source(source, |_, fd, app: &mut App| {
            for s in crate::signals::read(std::os::fd::AsFd::as_fd(&**fd)) {
                app.signal(s);
            }
            Ok(calloop::PostAction::Continue)
        });
    }
    let mut app = App {
        dpy,
        loop_handle: event_loop.handle(),
        cfg,
        session: None,
        monitors: Vec::new(),
        pending: None,
        taken: None,
        desktop_rx: None,
        desktop: None,
        exit: None,
        waker: Arc::new(move || ping.ping()),
        clock: Clock::since("region capture", started),
        first_frame: false,
        output_file,
        one_shot: true,
        finished: None,
        hold_until: None,
        paste_rx: None,
        tray: tray::TrayState::default(),
        events: None,
        pins: Vec::new(),
        next_pin: 0,
        editors: Vec::new(),
        next_editor: 0,
        open_dialog: None,
        settings: None,
        recording: None,
        quit_after_recording: false,
        restarting: false,
        blink: caret::Blink::default(),
        reading: Vec::new(),
        recent: None,
        countdown: None,
        clipboard_image: None,
    };
    match queue {
        Some(mut queue) => {
            // The monitors (and xdg-output's logical layout) arrive in two rounds.
            for _ in 0..2 {
                queue.roundtrip(&mut app).map_err(|e| {
                    crate::warn!("Wayland: {e}");
                    2
                })?;
            }
            timing::log(format!("connected to Wayland in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
            let conn = app.wl().conn.clone();
            WaylandSource::new(conn, queue).insert(event_loop.handle()).map_err(|e| {
                crate::warn!("Wayland source: {e}");
                2
            })?;
        }
        None => {
            timing::log(format!("connected to X11 in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
            crate::x11::insert_source(&event_loop.handle(), &app).map_err(|e| {
                crate::warn!("X11: {e}");
                2
            })?;
        }
    }
    Ok((app, event_loop))
}

/// The display server: Wayland when there is one (with its event queue,
/// still to be read), else X11.
fn connect() -> Result<(Display, Option<wayland_client::EventQueue<App>>), String> {
    if crate::x11::chosen() {
        return Ok((Display::X11(crate::x11::X11::connect()?), None));
    }
    let (wl, queue) = Wayland::connect()?;
    Ok((Display::Wayland(wl), Some(queue)))
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
    let (mut app, mut event_loop) = match setup(started, args.output.clone()) {
        Ok(x) => x,
        Err(code) => return code,
    };
    if args.no_copy {
        app.cfg.clipboard = "none".into();
    }
    if args.no_save {
        app.cfg.save_to_disk = false;
    }
    if let Some(b) = &args.backend {
        app.cfg.backend = b.clone();
    }
    let Some(mode) = Mode::parse(args.mode(), args.region.as_deref().and_then(crate::cli::parse_geometry)) else {
        crate::warn!("unknown capture mode");
        return 2;
    };
    let job = Job { mode, pin: args.pin, record: args.record, ocr: args.ocr, scan: !args.no_scan, asked: started };
    if args.delay > 0.0 {
        // (counted down on screen; Ctrl+C still stops it)
        app.count_down(job, Duration::from_secs_f64(args.delay.min(3600.0)), false);
    } else {
        app.start_capture(job);
    }
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

/// The annotation editor on its own (`flatshot -i FILE`): exits when its
/// window is closed.
pub fn run_editor(args: &Args) -> i32 {
    let started = Instant::now();
    crate::dbus::connect_early();
    let _ = std::thread::Builder::new().name("flatshot-fonts".into()).spawn(crate::render::text::warm_up);
    let (mut app, mut event_loop) = match setup(started, None) {
        Ok(x) => x,
        Err(code) => return code,
    };
    if args.from_clipboard {
        app.annotate_clipboard(args.output.as_ref().map(|o| std::path::PathBuf::from(config::expand_user(o))));
    } else {
        let Some(image) = &args.image else { return 2 };
        let path = std::path::PathBuf::from(config::expand_user(image));
        let save_to = args.output.as_ref().map(|o| std::path::PathBuf::from(config::expand_user(o)));
        if !app.open_editor(&path, save_to) {
            return 2;
        }
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
