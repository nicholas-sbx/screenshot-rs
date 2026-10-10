//! What a finished session's outcome becomes: a file, the clipboard, a
//! notification, a sound (or a pin).

use std::sync::Arc;
use std::time::{Duration, Instant};

use super::App;
use super::tray::NoteTarget;
use crate::clipboard::{Slot, TEXT_MIMES};
use crate::config;
use crate::deliver;
use crate::desktop::{notify, sound};
use crate::geom::IRect;
use crate::image::Image;
use crate::template::Shot;
use crate::ui::session::Outcome;
use crate::util;

/// How long a one-off capture stays to serve the clipboard, when nothing
/// else can (no wl-copy to hand it to).
const HOLD_CLIPBOARD: Duration = Duration::from_secs(600);

fn text_mimes() -> Vec<String> {
    TEXT_MIMES.iter().map(|m| m.to_string()).collect()
}

impl App {
    /// The session ended: deliver what came of it.
    pub(super) fn finish(&mut self, outcome: Outcome) {
        let from_screen = self.session.as_ref().is_some_and(|s| !s.request.image);
        if let Some(s) = &self.session {
            self.clock = s.clock.clone(); // (with the session's steps)
        }
        // Without data-control, the clipboard can only be had while an
        // overlay has the keyboard: it's claimed now, and what goes in it
        // follows once it's ready.
        let slot = self.claim_clipboard(&outcome);
        self.close_session();
        self.dpy.flush();
        let code = match outcome {
            Outcome::Image { image, shot, at, pin } => self.deliver_image(image, shot, at, from_screen, pin, slot),
            Outcome::Color { hex, rgb: (r, g, b) } => {
                let copied = self.copy_text(&hex, slot);
                println!("{hex}");
                if self.cfg.notify {
                    let summary = if copied { format!("Copied colour {hex}") } else { format!("Colour {hex}") };
                    self.notify(notify::Note { summary, body: format!("rgb({r}, {g}, {b})"), event: "copied", ..Default::default() });
                }
                0
            }
            Outcome::CodeText(text) => {
                let copied = self.copy_text(&text, slot);
                util::print_line(&text);
                if self.cfg.notify {
                    let summary = if copied { "Copied from code" } else { "Code" };
                    let body = util::elide(&util::displayable(&text), 300);
                    self.notify(notify::Note { summary: summary.into(), body, event: "copied", ..Default::default() });
                }
                0
            }
            Outcome::OpenLink(url) => {
                // (only http(s), mailto: and the like are offered as links)
                if !util::spawn(&["xdg-open", &url]) {
                    crate::warn!("could not open {url}");
                }
                0
            }
            Outcome::Record(job) => {
                if self.start_recording(*job) {
                    0
                } else {
                    2
                }
            }
            Outcome::Cancelled => 1,
            Outcome::Failed(e) => {
                self.report_failure("Screenshot failed", &e);
                2
            }
        };
        self.ended(code);
    }

    /// The session is over with `code`: a one-off capture exits once
    /// nothing is left for it to do.
    pub(super) fn ended(&mut self, code: i32) {
        if self.one_shot {
            self.finished = Some(code);
            if self.dpy.holds() {
                self.hold_until = Some(Instant::now() + HOLD_CLIPBOARD);
                self.dpy.hand_off_clipboard();
            }
        }
    }

    /// Is a one-off capture done with everything (the clipboard served
    /// long enough, no pins left)?
    pub(super) fn may_exit(&self) -> bool {
        let holding = self.dpy.holds() && self.hold_until.is_some_and(|t| Instant::now() < t);
        !holding && !self.has_pins() && !self.has_editors() && !self.has_settings() && !self.is_recording()
    }

    /// Will a one-off capture hand what it copies to wl-copy (so it can
    /// end now, with the clipboard still served)?
    fn hand_off(&self) -> bool {
        self.one_shot && self.dpy.copy_helper().is_some()
    }

    /// Own the clipboard for what `outcome` copies, before the overlays go.
    fn claim_clipboard(&mut self, outcome: &Outcome) -> Option<Arc<Slot>> {
        let mimes = match outcome {
            Outcome::Image { pin: false, .. } => match self.cfg.clipboard.as_str() {
                "image" => vec!["image/png".to_string()],
                "path" if self.output_file.as_deref() != Some("-") => text_mimes(),
                _ => return None,
            },
            Outcome::Color { .. } | Outcome::CodeText(_) => text_mimes(),
            _ => return None,
        };
        if self.hand_off() {
            return None;
        }
        let slot = Arc::new(Slot::default());
        self.dpy.offer(mimes, slot.clone()).then_some(slot)
    }

    /// Put `data` (as `mimes`) on the clipboard: into the claimed `slot`,
    /// else handed to wl-copy, else owned now (with data-control).
    fn fill_clipboard(&mut self, mimes: Vec<String>, data: Arc<Vec<u8>>, slot: Option<Arc<Slot>>) -> bool {
        if let Some(slot) = slot {
            slot.fill(data);
            return true;
        }
        if self.hand_off()
            && let Some(helper) = self.dpy.copy_helper()
        {
            return deliver::hand_to(helper, &mimes[0], &data);
        }
        self.dpy.offer(mimes, Slot::ready(data))
    }

    fn copy_text(&mut self, text: &str, slot: Option<Arc<Slot>>) -> bool {
        self.fill_clipboard(text_mimes(), Arc::new(text.as_bytes().to_vec()), slot)
    }

    /// Text copied while a session goes on (a code's text from the picker...).
    pub(super) fn copy_now(&mut self, text: &str) {
        let slot = Slot::ready(Arc::new(text.as_bytes().to_vec()));
        if !self.dpy.offer(text_mimes(), slot) {
            crate::warn!("could not copy: no clipboard");
        }
    }

    /// Save, copy, notify... `at`: where the picture was on the desktop
    /// (global logical pixels), remembered as the last region if `remember`.
    pub(super) fn deliver_image(&mut self, image: Image, shot: Shot, at: Option<IRect>, remember: bool, pin: bool, slot: Option<Arc<Slot>>) -> i32 {
        if let Some(a) = at.filter(|_| remember) {
            config::update_state(vec![("last_region", serde_json::json!([a.x, a.y, a.w, a.h]))]);
        }
        if pin {
            self.pin(Arc::new(image), at, None);
            self.clock.step("pinned", "");
            if self.cfg.sound {
                sound::play(&self.cfg.sound_file);
            }
            return 0;
        }
        let result = match deliver::deliver(&image, &self.cfg, self.output_file.as_deref(), &shot, &mut self.clock) {
            Ok(r) => r,
            Err(e) => {
                if slot.is_some() {
                    self.dpy.withdraw();
                }
                self.report_failure("Screenshot failed", &e);
                return 2;
            }
        };
        drop(image);
        if result.saved
            && let Some(p) = &result.path
        {
            util::print_line(&p.to_string_lossy());
        }
        let copied = match result.clip.clone() {
            Some((mimes, data)) => {
                let ok = self.fill_clipboard(mimes, data, slot);
                self.clock.step(if ok { "copied" } else { "not copied" }, "");
                ok
            }
            None => {
                if slot.is_some() {
                    self.dpy.withdraw();
                }
                false
            }
        };
        self.dpy.flush();
        if self.cfg.sound {
            sound::play(&self.cfg.sound_file);
        }
        if self.cfg.notify {
            let (w, h) = result.size;
            let mut bits = vec![format!("{w} × {h}")];
            if copied {
                bits.push(if self.cfg.clipboard == "path" { "path copied" } else { "copied to clipboard" }.to_string());
            }
            let summary = match &result.path {
                Some(p) if result.saved => {
                    bits.insert(0, util::displayable(&notify::file_name(p)));
                    "Screenshot saved"
                }
                _ if copied => "Screenshot copied",
                _ => "Screenshot taken",
            };
            let mut note =
                notify::Note { summary: summary.into(), body: bits.join("  ·  "), image: result.path.clone(), event: "screenshot", ..Default::default() };
            let mut target = None;
            if let Some(path) = result.path.clone().filter(|_| !self.one_shot) {
                // (the tray app stays to hear the buttons)
                let buttons = [("default", "Open"), ("open", "Open"), ("folder", "Show in folder"), ("annotate", "Annotate"), ("pin", "Pin")];
                note.actions = buttons.iter().filter(|(k, _)| result.saved || *k != "folder").map(|(k, l)| (k.to_string(), l.to_string())).collect();
                let layout = self.dpy.monitors().into_iter().map(|m| (m.name, m.rect, m.scale as f64)).collect();
                target = Some(NoteTarget { path, at, layout });
            }
            self.notify_with(note, target);
            self.clock.step("notified", "");
        }
        0
    }

    /// Pop up `note` (or do what its event is set to do instead).
    pub(super) fn notify(&mut self, note: notify::Note) {
        self.notify_with(note, None);
    }

    /// ... with buttons acting on `target`, in the tray app.
    pub(super) fn notify_with(&mut self, note: notify::Note, target: Option<NoteTarget>) {
        let Some(n) = self.tray.notifier.as_mut() else {
            notify::send_now(&note, false);
            return;
        };
        if let (Some(token), Some(t)) = (n.send(note), target) {
            self.remember_target(token, t);
        }
    }

    /// Say what went wrong: on stderr, and in a notification.
    pub(super) fn report_failure(&mut self, summary: &str, message: &str) {
        crate::warn!("{message}");
        self.notify(notify::Note { summary: summary.into(), body: message.into(), urgent: true, event: "failed", ..Default::default() });
    }
}
