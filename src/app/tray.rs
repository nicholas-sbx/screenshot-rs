//! The tray app: it stays running with the compositor connection open and
//! everything loaded, so a shortcut opens the overlay with nothing to start
//! first. KDE's shortcut service, the tray icon, notifications' buttons and
//! `flatshot` invocations (over the IPC socket) all come into one event loop.

use std::collections::HashMap;
use std::path::PathBuf;
use std::time::{Duration, Instant};

use calloop::channel::{self, Channel};
use calloop::generic::Generic;
use calloop::timer::{TimeoutAction, Timer};
use calloop::{Interest, Mode as PollMode, PostAction};

use super::{App, Job, Mode};
use crate::cli::{self, Args};
use crate::config;
use crate::desktop::notify::{self, NoteEvent, Notifier};
use crate::desktop::shortcuts::{self, ShortcutEvent};
use crate::desktop::tray::{MenuItem, Tray, TrayEvent};
use crate::geom::IRect;
use crate::ipc;
use crate::timing;

/// Let the tray's menu (or a notification) fade before the screen is frozen.
const MENU_DELAY: Duration = Duration::from_millis(220);
/// How many notifications' buttons are remembered.
const MAX_TARGETS: usize = 64;

/// What the desktop asks of the tray app, from threads of its own.
pub enum Event {
    Shortcut(ShortcutEvent, Instant),
    Tray(TrayEvent),
    /// the tray icon is up
    TrayShown(Tray),
    /// the menu's entries, with their shortcuts
    Menu(Vec<MenuItem>),
}

/// What a notification's buttons act on.
pub struct NoteTarget {
    pub path: PathBuf,
    /// where it was captured (global logical pixels)
    pub at: Option<IRect>,
    /// the monitors then: a pin opens where it was only while they're the same
    pub layout: Vec<(String, IRect, f64)>,
}

/// The tray app's own state.
#[derive(Default)]
pub struct TrayState {
    pub tray: Option<Tray>,
    pub notifier: Option<Notifier>,
    note_events: Option<std::sync::mpsc::Receiver<NoteEvent>>,
    pub targets: HashMap<u64, NoteTarget>,
    ipc: Option<ipc::Server>,
}

/// The menu: (key, label); shortcuts' keys are filled in when known.
fn menu_items(keys: &HashMap<String, String>) -> Vec<MenuItem> {
    let k = |a: &str| keys.get(a).cloned().unwrap_or_default();
    let mut items: Vec<MenuItem> =
        ["region", "monitor", "screen", "last", "pin", "record"].iter().map(|a| MenuItem::new(a, shortcuts::label(a), &k(a))).collect();
    items.extend([
        MenuItem::separator(),
        MenuItem::new("region-delayed", "Capture region in 3 seconds", ""),
        MenuItem::new("window-delayed", "Capture active window in 3 seconds", ""),
        MenuItem::separator(),
        MenuItem::new("annotate", "Annotate an image…", ""),
        MenuItem::new("folder", "Open screenshots folder", ""),
        MenuItem::new("settings", "Settings…", ""),
        MenuItem::separator(),
        MenuItem::new("quit", "Quit Flatshot", ""),
    ]);
    items
}

/// Each action's keys, from the shortcut service (in the background).
fn shortcut_keys() -> HashMap<String, String> {
    shortcuts::ACTIONS.iter().map(|(a, _, _)| (a.to_string(), shortcuts::get(a))).filter(|(_, k)| !k.is_empty()).collect()
}

/// The "status" reply: which Flatshot this is.
fn status() -> String {
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    serde_json::json!({ "version": cli::VERSION, "exe": exe, "pid": std::process::id() }).to_string()
}

/// After an upgrade the old tray app keeps running (and handling every
/// capture): if the running one isn't this version from this place, ask
/// it to quit so this one takes over. The Python Flatshot's too.
pub fn replace_older_tray() {
    if ipc::quit_legacy_tray() {
        crate::warn!("replaced the running Flatshot (Python) tray app");
    }
    let Some(reply) = ipc::query("status", Duration::from_millis(500)) else { return };
    let info: serde_json::Value = serde_json::from_str(&reply).unwrap_or_default();
    let exe = std::env::current_exe().map(|p| p.display().to_string()).unwrap_or_default();
    if info["version"] == cli::VERSION && info["exe"] == exe.as_str() {
        return; // the same Flatshot: it shows its settings, as before
    }
    let version = info["version"].as_str().unwrap_or("an older version");
    crate::warn!("replacing the running tray app ({version})");
    ipc::send("quit");
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline && ipc::query("status", Duration::from_millis(300)).is_some() {
        std::thread::sleep(Duration::from_millis(100));
    }
}

impl App {
    /// Start the tray app's services: IPC, shortcuts, the icon,
    /// notifications. False if another tray app runs.
    pub(super) fn start_tray(&mut self, events: channel::Sender<Event>, channel: Channel<Event>) -> bool {
        let server = match ipc::Server::bind() {
            Ok(Some(s)) => s,
            Ok(None) => {
                crate::warn!("already running");
                return false;
            }
            Err(e) => {
                crate::warn!("can't listen for commands: {e}");
                return false;
            }
        };
        let fd = match server.listener.try_clone() {
            Ok(fd) => fd,
            Err(e) => {
                crate::warn!("can't listen for commands: {e}");
                return false;
            }
        };
        let source = Generic::new(fd, Interest::READ, PollMode::Level);
        let inserted = self.loop_handle.insert_source(source, |_, _, app: &mut App| {
            let commands = app.tray.ipc.as_ref().map(|s| s.accept(&status)).unwrap_or_default();
            for c in commands {
                app.command(&c);
            }
            Ok(PostAction::Continue)
        });
        if inserted.is_err() {
            return false;
        }
        self.tray.ipc = Some(server);
        let _ = self.loop_handle.insert_source(channel, |event, _, app: &mut App| {
            if let channel::Event::Msg(e) = event {
                app.tray_event(e);
            }
        });
        let (notifier, note_events) = Notifier::start(self.waker.clone());
        self.tray.notifier = Some(notifier);
        self.tray.note_events = Some(note_events);
        // The rest talks to the desktop, which can be slow to answer (at
        // login): in the background, each reporting back when done.
        let tx = events.clone();
        let _ = std::thread::Builder::new().name("flatshot-start".into()).spawn(move || {
            let shown = {
                let tx = tx.clone();
                Tray::show(menu_items(&HashMap::new()), move |e| {
                    let _ = tx.send(Event::Tray(e));
                })
            };
            match shown {
                Ok(t) => {
                    let _ = tx.send(Event::TrayShown(t));
                }
                Err(e) => crate::warn!("{e}; running in the background anyway"),
            }
            let to_main = tx.clone();
            match shortcuts::start(move |e| {
                let _ = to_main.send(Event::Shortcut(e, Instant::now()));
            }) {
                Ok(()) => {
                    let _ = tx.send(Event::Menu(menu_items(&shortcut_keys())));
                }
                Err(e) => crate::warn!("global shortcuts unavailable ({e})"),
            }
            notify::install_events();
        });
        true
    }

    fn tray_event(&mut self, event: Event) {
        match event {
            Event::Shortcut(ShortcutEvent::Pressed(action), at) => {
                timing::log(format!("shortcut {action} pressed"));
                self.shortcut(&action, Duration::ZERO, at);
            }
            Event::Shortcut(ShortcutEvent::Changed, _) => {
                if self.tray.tray.is_some() {
                    let tx = self.events.clone();
                    let _ = std::thread::Builder::new().name("flatshot-keys".into()).spawn(move || {
                        if let Some(tx) = tx {
                            let _ = tx.send(Event::Menu(menu_items(&shortcut_keys())));
                        }
                    });
                }
            }
            Event::TrayShown(t) => self.tray.tray = Some(t),
            Event::Menu(items) => {
                if let Some(t) = &self.tray.tray {
                    t.set_items(items);
                }
            }
            Event::Tray(TrayEvent::Activate) => self.capture_later(Job::new(Mode::Region), MENU_DELAY),
            Event::Tray(TrayEvent::SecondaryActivate) => self.capture_later(Job::new(Mode::Screens), MENU_DELAY),
            Event::Tray(TrayEvent::Menu(key)) => match key.as_str() {
                "region-delayed" => self.capture_later(Job::new(Mode::Region), Duration::from_secs(3)),
                "window-delayed" => self.capture_later(Job::new(Mode::Window), Duration::from_secs(3)),
                "annotate" => self.choose_image(),
                "folder" => {
                    let folder = crate::template::base_folder(&config::load().save_dir);
                    let _ = std::fs::create_dir_all(&folder);
                    crate::deliver::open_file(&folder);
                }
                "settings" => self.show_settings(),
                "quit" => self.quit(),
                action => self.shortcut(action, MENU_DELAY, Instant::now()),
            },
        }
    }

    /// A shortcut's action (or the same from the menu, after `delay`).
    fn shortcut(&mut self, action: &str, delay: Duration, at: Instant) {
        let (mode, pin) = match action {
            "region" => (Mode::Region, false),
            "window" => (Mode::Window, false),
            "monitor" => (Mode::Monitor, false),
            "screen" => (Mode::Screens, false),
            "last" => (Mode::Last, false),
            "pin" => (Mode::Region, true),
            "record" => return self.record(delay),
            _ => return,
        };
        let job = Job { pin, asked: at, ..Job::new(mode) };
        self.capture_later(job, delay);
    }

    /// Choose an area to record (or stop the recording in progress).
    fn record(&mut self, delay: Duration) {
        self.capture_later(Job { record: true, ..Job::new(Mode::Region) }, delay);
    }

    /// Capture after `delay` (now, without one).
    pub(super) fn capture_later(&mut self, mut job: Job, delay: Duration) {
        if delay.is_zero() {
            self.cfg = config::load(); // (settings changed since)
            crate::render::theme::use_theme(&self.cfg.theme);
            return self.start_capture(job);
        }
        let _ = self.loop_handle.insert_source(Timer::from_duration(delay), move |_, _, app: &mut App| {
            job.asked = Instant::now();
            app.capture_later(job.clone(), Duration::ZERO);
            TimeoutAction::Drop
        });
    }

    /// A command from another `flatshot`: `<mode>[:WxH+X+Y] [delay] [pin]`,
    /// `record [delay]`, `edit <file>`, `settings` or `quit`.
    fn command(&mut self, command: &str) {
        timing::log(format!("command: {command}"));
        let mut words = command.split_whitespace();
        let Some(name) = words.next() else { return };
        let rest: Vec<&str> = words.collect();
        let delay = rest.first().and_then(|d| d.parse::<f64>().ok()).filter(|d| d.is_finite()).unwrap_or(0.0).clamp(0.0, 3600.0);
        let delay = Duration::from_secs_f64(delay);
        match name {
            "edit" => {
                let path = command.split_once(' ').map(|x| x.1.trim()).unwrap_or("");
                self.edit(path);
            }
            "settings" => self.show_settings(),
            "quit" => self.quit(),
            "record" => self.record(delay),
            _ => {
                let name = match name {
                    "capture" => "region", // (older invocations)
                    "full" => "screens",
                    n => n,
                };
                let (mode, geometry) = name.split_once(':').map(|(m, g)| (m, Some(g))).unwrap_or((name, None));
                let geometry = geometry.and_then(cli::parse_geometry);
                let Some(mode) = Mode::parse(mode, geometry) else { return };
                let job = Job { pin: rest.contains(&"pin"), ..Job::new(mode) };
                self.capture_later(job, delay);
            }
        }
    }

    /// Clicks on our notifications' buttons.
    pub(super) fn poll_notifications(&mut self) {
        let events: Vec<NoteEvent> = match &self.tray.note_events {
            Some(rx) => rx.try_iter().collect(),
            None => return,
        };
        for e in events {
            match e {
                NoteEvent::Action(token, key) => {
                    let Some(target) = self.tray.targets.get(&token) else { continue };
                    let path = target.path.clone();
                    match key.as_str() {
                        "default" | "open" => {
                            crate::deliver::open_file(&path);
                        }
                        "folder" => {
                            crate::deliver::show_in_folder(&path);
                        }
                        "annotate" if path.exists() => self.edit(&path.to_string_lossy()),
                        "pin" => self.pin_file(token),
                        _ => {}
                    }
                }
                NoteEvent::Closed(token) => {
                    self.tray.targets.remove(&token);
                }
            }
        }
    }

    /// Remember what a notification's buttons act on.
    pub(super) fn remember_target(&mut self, token: u64, target: NoteTarget) {
        if self.tray.targets.len() >= MAX_TARGETS {
            let live = self.tray.notifier.as_ref().map(|n| n.live_tokens()).unwrap_or_default();
            self.tray.targets.retain(|t, _| live.contains(t));
            while self.tray.targets.len() >= MAX_TARGETS {
                let Some(oldest) = self.tray.targets.keys().min().copied() else { break };
                self.tray.targets.remove(&oldest);
            }
        }
        self.tray.targets.insert(token, target);
    }

    /// Open `path` in the annotation editor.
    fn edit(&mut self, path: &str) {
        crate::warn!("the annotation editor isn't in this build yet ({path})");
    }

    /// Pin the screenshot a notification was about: where it was taken,
    /// unless the monitors changed since (then the desktop's middle).
    fn pin_file(&mut self, token: u64) {
        let Some(t) = self.tray.targets.get(&token) else { return };
        let (path, at, layout) = (t.path.clone(), t.at, t.layout.clone());
        let image = match crate::codec::decode_file(&path) {
            Ok(img) => img,
            Err(e) => return crate::warn!("can't pin {}: {e}", path.display()),
        };
        let now: Vec<(String, IRect, f64)> = self.wl.monitors().into_iter().map(|m| (m.name, m.rect, m.scale as f64)).collect();
        let same = at.is_some() && now == layout;
        self.pin(std::sync::Arc::new(image), at.filter(|_| same), at.map(|a| (a.w as u32, a.h as u32)));
    }

    fn show_settings(&mut self) {
        crate::warn!("the settings window isn't in this build yet");
    }

    /// Pick an image to annotate (the desktop's file dialog).
    fn choose_image(&mut self) {
        crate::warn!("the annotation editor isn't in this build yet");
    }

    fn quit(&mut self) {
        self.exit = Some(0);
    }
}

/// The tray app (`flatshot --tray`).
pub fn run_tray(_args: &Args) -> i32 {
    let started = Instant::now();
    let _ = std::thread::Builder::new().name("flatshot-fonts".into()).spawn(crate::render::text::warm_up);
    crate::dbus::connect_early();
    let (mut app, mut event_loop) = match super::setup(started, None) {
        Ok(x) => x,
        Err(code) => return code,
    };
    app.one_shot = false;
    let (tx, rx) = channel::channel::<Event>();
    app.events = Some(tx.clone());
    if !app.start_tray(tx, rx) {
        return 1;
    }
    timing::log(format!("tray app ready in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
    // (nothing big is kept between captures)
    crate::mem::trim();
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
