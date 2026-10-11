//! The settings window (ui::settings): opening it, its input and drawing,
//! and what it asks of the desktop: file dialogs, KDE's shortcut service
//! (on a thread of its own, in order) and the clipboard.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use super::{App, Input};
use crate::clipboard::{Slot, TEXT_MIMES};
use crate::desktop::{portal, shortcuts, sound};
use crate::geom::{Damage, IRect, Point};
use crate::image::Canvas;
use crate::platform::{Owner, Painted};
use crate::ui::session::{Button, Waker};
use crate::ui::settings::{Request, Settings};

const TITLE: &str = "Flatshot RS Settings";
const SIZE: (u32, u32) = (760, 560);
const MIN_SIZE: (u32, u32) = (640, 440);

/// What the shortcut service is asked, in order.
pub(super) enum KdeJob {
    /// register our actions (when no tray app has), then load their keys
    Start,
    /// load the actions' keys
    Load,
    Block(bool),
    Assign(&'static str, String),
}

enum KdeAnswer {
    /// the actions' keys (None: no shortcut service)
    Keys(Option<BTreeMap<String, String>>),
    /// they were changed (in System Settings, say)
    Changed,
    /// an action's keys now, and why not the ones asked for
    Assigned(&'static str, String, Option<String>),
}

type DialogAnswer = (&'static str, Result<Option<PathBuf>, String>);

/// The settings window and what it waits for.
pub struct SettingsWin {
    pub s: Settings,
    dialog: Option<mpsc::Receiver<DialogAnswer>>,
    paste: Option<mpsc::Receiver<Option<String>>>,
    /// what recording lacks here, being found out
    recorder: Option<mpsc::Receiver<Option<String>>>,
    kde: mpsc::Sender<KdeJob>,
    answers: mpsc::Receiver<KdeAnswer>,
}

struct SettingsPaint<'a>(&'a mut Settings);

impl Painted for SettingsPaint<'_> {
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

/// The actions' keys, as the shortcut service has them.
fn global_keys() -> BTreeMap<String, String> {
    shortcuts::ACTIONS.iter().map(|(action, _, _)| (action.to_string(), shortcuts::get(action))).collect()
}

/// Ask the shortcut service, one job after another, until the window's gone.
fn kde_worker(jobs: mpsc::Receiver<KdeJob>, answers: mpsc::Sender<KdeAnswer>, waker: Waker) {
    let mut started = false;
    while let Ok(job) = jobs.recv() {
        let answer = match job {
            KdeJob::Start => {
                // On our own (no tray app): register, and hear changes made elsewhere.
                let (tx, wake) = (answers.clone(), waker.clone());
                let ok = !started
                    && shortcuts::supported()
                    && shortcuts::start(move |e| {
                        if e == shortcuts::ShortcutEvent::Changed && tx.send(KdeAnswer::Changed).is_ok() {
                            wake();
                        }
                    })
                    .map_err(|e| crate::warn!("global shortcuts unavailable ({e})"))
                    .is_ok();
                started |= ok;
                Some(KdeAnswer::Keys(ok.then(global_keys)))
            }
            KdeJob::Load => Some(KdeAnswer::Keys(Some(global_keys()))),
            KdeJob::Block(on) => {
                shortcuts::block(on);
                None
            }
            KdeJob::Assign(action, seq) => {
                let owner = if seq.is_empty() { String::new() } else { shortcuts::owner_of(&seq, action) };
                if !owner.is_empty() {
                    Some(KdeAnswer::Assigned(action, shortcuts::get(action), Some(format!("{} is already used by {owner}.", crate::keyseq::label(&seq)))))
                } else {
                    let (ok, now) = shortcuts::assign(action, &seq);
                    let what = if seq.is_empty() { "clearing the shortcut".to_string() } else { crate::keyseq::label(&seq) };
                    if ok {
                        Some(KdeAnswer::Assigned(action, now, None))
                    } else {
                        Some(KdeAnswer::Assigned(action, shortcuts::get(action), Some(format!("KDE rejected {what}."))))
                    }
                }
            }
        };
        if let Some(a) = answer {
            if answers.send(a).is_err() {
                return;
            }
            waker();
        }
    }
}

/// Where a folder chooser starts: the folder of a save-folder template, up
/// to its first code.
fn start_folder(template: &str) -> PathBuf {
    crate::template::base_folder(template)
}

/// Where the sound chooser starts.
fn sound_folder(current: &str) -> PathBuf {
    let current = crate::config::expand_user(current);
    let parent = Path::new(&current).parent().filter(|p| !current.is_empty() && p.is_dir()).map(Path::to_path_buf);
    parent.unwrap_or_else(|| PathBuf::from("/usr/share/sounds"))
}

impl App {
    /// Open the settings window (or keep the one open).
    pub(super) fn show_settings(&mut self) {
        if self.settings.is_some() {
            return; // (Wayland has no raising a window without the desktop's token)
        }
        if let Err(e) = self.dpy.add_window(Owner::Settings, TITLE, SIZE, true) {
            self.report_failure("Can't show the settings", &e);
            return;
        }
        self.dpy.set_min_size(Owner::Settings, MIN_SIZE);
        let (kde, jobs) = mpsc::channel();
        let (answers_tx, answers) = mpsc::channel();
        let waker = self.waker.clone();
        let _ = std::thread::Builder::new().name("flatshot-kde".into()).spawn(move || kde_worker(jobs, answers_tx, waker));
        let mut s = Settings::new(crate::config::load());
        // The tray app registered the shortcuts already (or is still at it).
        match self.tray.shortcuts {
            _ if self.one_shot => {
                let _ = kde.send(KdeJob::Start);
            }
            Some(true) => {
                let _ = kde.send(KdeJob::Load);
            }
            Some(false) => s.set_global(None),
            None => {} // (when it's done: shortcuts_ready)
        }
        let format = crate::config::load().record_format;
        let recorder = Some(check_recorder(format, self.waker.clone()));
        self.settings = Some(SettingsWin { s, dialog: None, paste: None, recorder, kde, answers });
        self.dpy.flush();
    }

    pub(super) fn has_settings(&self) -> bool {
        self.settings.is_some()
    }

    /// The tray app's shortcuts are registered (or can't be).
    pub(super) fn shortcuts_ready(&mut self, ok: bool) {
        if let Some(w) = &mut self.settings {
            if ok {
                let _ = w.kde.send(KdeJob::Load);
            } else {
                w.s.set_global(None);
            }
        }
    }

    /// KDE's shortcuts changed (here or in System Settings).
    pub(super) fn shortcuts_changed(&mut self) {
        if let Some(w) = &self.settings {
            let _ = w.kde.send(KdeJob::Load);
        }
    }

    pub(super) fn settings_input(&mut self, ev: Input) {
        let mods = self.dpy.mods();
        let Some(w) = &mut self.settings else { return };
        let s = &mut w.s;
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        match ev {
            Input::Enter(at) | Input::Motion(at, _) => s.pointer_motion(p(at)),
            Input::Leave => s.pointer_leave(),
            Input::Button(b, pressed, at) => {
                let button = match b {
                    0x110 => Button::Left,
                    0x111 => Button::Right,
                    0x112 => Button::Middle,
                    _ => Button::Other,
                };
                s.button(button, pressed, p(at), mods);
            }
            Input::Scroll(steps) => s.scroll_by(steps),
            Input::Key(k) => s.key(&k),
            Input::KeyUp(_) => {}
            Input::Modifiers(m) => s.modifiers(m),
        }
        self.serve_settings();
    }

    pub(super) fn settings_commit_string(&mut self, text: &str) {
        if let Some(w) = &mut self.settings {
            w.s.commit_string(text);
        }
        self.serve_settings();
    }

    /// The window manager closes the window: what's typed is kept, and
    /// KDE's shortcuts work again.
    pub(super) fn close_settings(&mut self) {
        let Some(w) = &mut self.settings else { return };
        w.s.closing();
        self.serve_settings();
        if self.settings.is_some() {
            self.dpy.remove(Owner::Settings);
            self.settings = None; // (the worker ends with its last job)
            crate::mem::trim();
        }
    }

    /// Do what the window asked for.
    fn serve_settings(&mut self) {
        let Some(w) = &mut self.settings else { return };
        let requests = w.s.take_requests();
        for req in requests {
            let Some(w) = &mut self.settings else { return };
            match req {
                Request::ChooseFolder(id, current) => {
                    let title = if id == "record_dir" { "Recording folder" } else { "Screenshot folder" };
                    let start = start_folder(&current);
                    w.dialog = Some(spawn_dialog(id, self.waker.clone(), move || portal::choose_folder(title, &start)));
                }
                Request::ChooseSound(current) => {
                    let start = sound_folder(&current);
                    let globs = sound::SOUND_EXTENSIONS.iter().map(|e| format!("*.{e}")).collect();
                    w.dialog =
                        Some(spawn_dialog("sound_file", self.waker.clone(), move || portal::open_file("Sound", &start, &[("Sounds".to_string(), globs)])));
                }
                Request::AssignShortcut(action, seq) => {
                    let _ = w.kde.send(KdeJob::Assign(action, seq));
                }
                Request::BlockShortcuts(on) => {
                    let _ = w.kde.send(KdeJob::Block(on));
                }
                Request::ThemeChanged => {
                    for e in &mut self.editors {
                        e.ed.restyle();
                    }
                    self.restyle_pins();
                    self.update_tray_icon(None); // (the theme style's colours)
                }
                Request::TrayIconChanged(style) => self.update_tray_icon(Some(style)),
                Request::CopyText(text) => {
                    let mimes = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                    self.dpy.offer(mimes, Slot::ready(Arc::new(text.into_bytes())));
                }
                Request::Paste => w.paste = Some(self.dpy.paste_text(self.waker.clone())),
                Request::CheckRecorder(format) => w.recorder = Some(check_recorder(format, self.waker.clone())),
                Request::Close => {
                    self.dpy.remove(Owner::Settings);
                    self.settings = None; // (the worker ends with its last job)
                    crate::mem::trim();
                    return;
                }
            }
        }
    }

    /// Answers from dialogs, the clipboard and the shortcut service.
    pub(super) fn poll_settings(&mut self) {
        let Some(w) = &mut self.settings else { return };
        if let Some(rx) = &w.paste
            && let Ok(text) = rx.try_recv()
        {
            w.paste = None;
            if let Some(t) = text {
                w.s.paste(&t);
            }
        }
        if let Some(rx) = &w.recorder
            && let Ok(problem) = rx.try_recv()
        {
            w.recorder = None;
            w.s.set_recorder_problem(problem);
        }
        if let Some(rx) = &w.dialog
            && let Ok((id, answer)) = rx.try_recv()
        {
            w.dialog = None;
            match answer {
                Ok(Some(path)) => w.s.chosen(id, &path.to_string_lossy()),
                Ok(None) => {}
                Err(e) => {
                    crate::timing::log(format!("no file dialog ({e})"));
                    w.s.dialog_failed(id, "The desktop has no file dialog to offer: type it in instead.");
                }
            }
        }
        while let Ok(a) = w.answers.try_recv() {
            match a {
                KdeAnswer::Keys(keys) => w.s.set_global(keys),
                KdeAnswer::Changed => {
                    let _ = w.kde.send(KdeJob::Load);
                }
                KdeAnswer::Assigned(action, now, error) => w.s.shortcut_result(action, now, error),
            }
        }
        self.serve_settings();
    }

    /// Draw the settings window, if it changed.
    pub(super) fn redraw_settings(&mut self) {
        let Some(w) = &mut self.settings else { return };
        let Some((logical, scale)) = self.dpy.surface(Owner::Settings) else { return };
        w.s.set_view(logical.0 as f32, logical.1 as f32);
        w.s.set_scale(scale);
        crate::ui::caret::set_shown(self.blink.shown_in(Owner::Settings));
        self.dpy.redraw(Owner::Settings, 0, &mut SettingsPaint(&mut w.s));
    }
}

/// What recording in `format` lacks here, found out on a thread of its own
/// (it may ask GStreamer about its plugins).
fn check_recorder(format: String, waker: Waker) -> mpsc::Receiver<Option<String>> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("flatshot-recorder".into()).spawn(move || {
        let _ = tx.send(crate::record::problem(crate::record::method(), &format));
        waker();
    });
    rx
}

/// Run a file dialog on a thread of its own; its answer comes for row `id`.
fn spawn_dialog(id: &'static str, waker: Waker, dialog: impl FnOnce() -> Result<Option<PathBuf>, String> + Send + 'static) -> mpsc::Receiver<DialogAnswer> {
    let (tx, rx) = mpsc::channel();
    let _ = std::thread::Builder::new().name("flatshot-dialog".into()).spawn(move || {
        let _ = tx.send((id, dialog()));
        waker();
    });
    rx
}

/// The settings window on its own (`flatshot --settings`, no tray app
/// running): exits when it's closed.
pub fn run_settings() -> i32 {
    let started = Instant::now();
    crate::dbus::connect_early();
    let _ = std::thread::Builder::new().name("flatshot-fonts".into()).spawn(crate::render::text::warm_up);
    let (mut app, mut event_loop) = match super::setup(started, None) {
        Ok(x) => x,
        Err(code) => return code,
    };
    app.show_settings();
    if app.settings.is_none() {
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
