//! Annotation editor windows (ui::editor): opening one for a picture,
//! their input and drawing, and doing what they ask (the clipboard, the
//! desktop's "Save as" dialog, closing).

use std::path::{Path, PathBuf};
use std::sync::{Arc, mpsc};
use std::time::Instant;

use super::{App, Input};
use crate::clipboard::{Slot, TEXT_MIMES};
use crate::desktop::portal;
use crate::geom::{Damage, IRect, Point};
use crate::image::Canvas;
use crate::platform::{Owner, Painted};
use crate::ui::editor::{Editor, Request, initial_size};
use crate::ui::session::Button;

/// An editor and its window.
pub struct EditorWin {
    pub id: u32,
    pub ed: Editor,
    title: String,
    /// the "Save as" dialog's answer, when one is open
    dialog: Option<mpsc::Receiver<Result<Option<PathBuf>, String>>>,
    /// the clipboard's text, being read for it
    paste: Option<mpsc::Receiver<Option<String>>>,
    /// the window's least size, as last set
    min: (u32, u32),
    /// a picture for the background being chosen
    image_dialog: Option<mpsc::Receiver<Result<Option<PathBuf>, String>>>,
}

struct EditorPaint<'a>(&'a mut Editor);

impl Painted for EditorPaint<'_> {
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

/// The picture formats the file dialogs offer.
fn image_filters() -> Vec<(String, Vec<String>)> {
    let mut all: Vec<String> = ["png", "jpg", "jpeg", "webp", "gif", "bmp"].iter().map(|e| format!("*.{e}")).collect();
    all.extend(["avif", "jxl"].iter().map(|e| format!("*.{e}")));
    vec![("Images".to_string(), all)]
}

/// The formats "Save as" can write, with the file's own first.
fn save_filters(current: &Path) -> (Vec<(String, Vec<String>)>, usize) {
    let formats = crate::codec::writable_formats();
    let list: Vec<(String, Vec<String>)> = formats
        .iter()
        .map(|(key, label, _)| {
            let mut globs = vec![format!("*.{key}")];
            if *key == "jpg" {
                globs.push("*.jpeg".into());
            }
            (label.to_string(), globs)
        })
        .collect();
    let ext = current.extension().map(|e| e.to_string_lossy().to_lowercase()).unwrap_or_default();
    let ext = if ext == "jpeg" { "jpg".to_string() } else { ext };
    let at = formats.iter().position(|f| f.0 == ext).unwrap_or(0);
    (list, at)
}

impl App {
    /// Open `path` in a new editor window. Save writes back to it (or to
    /// `save_to`). False if it isn't a picture that can be read.
    pub(super) fn open_editor(&mut self, path: &Path, save_to: Option<PathBuf>) -> bool {
        let image = match crate::codec::decode_file(path) {
            Ok(img) => img,
            Err(e) => {
                self.report_failure("Can't annotate that", &format!("{}: {e}", path.display()));
                return false;
            }
        };
        let monitors = self.dpy.monitors();
        let (mon, dpr) = monitors.first().map(|m| ((m.rect.w as f32, m.rect.h as f32), m.scale)).unwrap_or(((1920.0, 1080.0), 1.0));
        let target = save_to.unwrap_or_else(|| path.to_path_buf());
        let ed = Editor::new(image, dpr, target, crate::config::load());
        let min = ed.min_size();
        let size = initial_size((ed.image.width() as f32 / dpr, ed.image.height() as f32 / dpr), mon, min.0 as f32);
        self.next_editor += 1;
        let id = self.next_editor;
        let title = ed.title();
        if let Err(e) = self.dpy.add_window(Owner::Editor(id), &title, size, true) {
            crate::warn!("can't open the editor: {e}");
            return false;
        }
        // (never so small its bars overlap)
        self.dpy.set_min_size(Owner::Editor(id), min);
        self.editors.push(EditorWin { id, ed, title, dialog: None, paste: None, min, image_dialog: None });
        self.dpy.flush();
        true
    }

    pub(super) fn has_editors(&self) -> bool {
        !self.editors.is_empty()
    }

    fn editor_mut(&mut self, id: u32) -> Option<&mut EditorWin> {
        self.editors.iter_mut().find(|e| e.id == id)
    }

    /// Input on editor `id`'s window.
    pub(super) fn editor_input(&mut self, id: u32, ev: Input) {
        let mods = self.dpy.mods();
        let Some(w) = self.editor_mut(id) else { return };
        let ed = &mut w.ed;
        let p = |at: (f64, f64)| Point::new(at.0 as f32, at.1 as f32);
        match ev {
            Input::Enter(at) | Input::Motion(at, _) => ed.pointer_motion(p(at), mods),
            Input::Leave => ed.pointer_leave(),
            Input::Button(b, pressed, at) => {
                let button = match b {
                    0x110 => Button::Left,
                    0x111 => Button::Right,
                    0x112 => Button::Middle,
                    _ => Button::Other,
                };
                ed.button(button, pressed, p(at), mods);
            }
            Input::Scroll(steps) => ed.scroll(steps),
            Input::Key(k) => ed.key(&k),
            Input::KeyUp(k) => ed.key_release(&k),
            Input::Modifiers(m) => ed.modifiers(m),
        }
        self.serve_editor(id);
    }

    /// The window manager asks editor `id` to close.
    pub(super) fn editor_close_requested(&mut self, id: u32) {
        if let Some(w) = self.editor_mut(id) {
            w.ed.close();
        }
        self.serve_editor(id);
    }

    pub(super) fn editor_commit_string(&mut self, id: u32, text: &str) {
        if let Some(w) = self.editor_mut(id) {
            w.ed.commit_string(text);
        }
        self.serve_editor(id);
    }

    /// Do what editor `id` asked for.
    fn serve_editor(&mut self, id: u32) {
        let Some(i) = self.editors.iter().position(|e| e.id == id) else { return };
        for req in self.editors[i].ed.take_requests() {
            match req {
                Request::CopyImage(png) => {
                    if !self.dpy.offer(vec!["image/png".into()], Slot::ready(png)) {
                        crate::warn!("could not copy: no clipboard");
                    }
                }
                Request::CopyText(text) => {
                    let mimes = TEXT_MIMES.iter().map(|m| m.to_string()).collect();
                    self.dpy.offer(mimes, Slot::ready(Arc::new(text.into_bytes())));
                }
                Request::Paste => self.editors[i].paste = Some(self.dpy.paste_text(self.waker.clone())),
                Request::SaveAs => {
                    if self.editors[i].dialog.is_some() {
                        continue; // (one is open already)
                    }
                    let current = self.editors[i].ed.path.clone();
                    let (filters, at) = save_filters(&current);
                    let (tx, rx) = mpsc::channel();
                    let waker = self.waker.clone();
                    let _ = std::thread::Builder::new().name("flatshot-dialog".into()).spawn(move || {
                        let _ = tx.send(portal::save_file("Save as", &current, &filters, at));
                        waker();
                    });
                    self.editors[i].dialog = Some(rx);
                }
                Request::ChooseImage(start) => {
                    if self.editors[i].image_dialog.is_some() {
                        continue;
                    }
                    let (tx, rx) = mpsc::channel();
                    let waker = self.waker.clone();
                    let _ = std::thread::Builder::new().name("flatshot-dialog".into()).spawn(move || {
                        let images = ["png", "jpg", "jpeg", "webp", "bmp", "gif"].iter().map(|e| format!("*.{e}")).collect();
                        let _ = tx.send(portal::open_file("Background picture", &start, &[("Images".to_string(), images)]));
                        waker();
                    });
                    self.editors[i].image_dialog = Some(rx);
                }
                Request::Saved(path, size) => {
                    crate::history::edited(&std::path::absolute(&path).unwrap_or(path), size);
                    self.recent_changed();
                }
                Request::Close => {
                    self.dpy.remove(Owner::Editor(id));
                    self.editors.remove(i);
                    crate::mem::trim();
                    return;
                }
            }
        }
        // (the title follows the unsaved changes)
        let w = &mut self.editors[i];
        let title = w.ed.title();
        if title != w.title {
            self.dpy.set_title(Owner::Editor(id), &title);
            w.title = title;
        }
        // (and its least size the bars: the picture bar comes and goes)
        let min = w.ed.min_size();
        if min != w.min {
            w.min = min;
            self.dpy.set_min_size(Owner::Editor(id), min);
        }
    }

    /// Answers from dialogs and the clipboard, and passing messages.
    pub(super) fn poll_editors(&mut self) {
        let now = Instant::now();
        let ids: Vec<u32> = self.editors.iter().map(|e| e.id).collect();
        for id in ids {
            let Some(w) = self.editor_mut(id) else { continue };
            if w.ed.next_deadline().is_some_and(|t| t <= now) {
                w.ed.on_timer(now);
            }
            if let Some(rx) = &w.paste
                && let Ok(text) = rx.try_recv()
            {
                w.paste = None;
                if let Some(t) = text {
                    w.ed.paste(&t);
                }
            }
            if let Some(rx) = &w.image_dialog
                && let Ok(answer) = rx.try_recv()
            {
                w.image_dialog = None;
                match answer {
                    Ok(Some(path)) => w.ed.background_image(&path),
                    Ok(None) => {}
                    Err(e) => crate::warn!("no file dialog: {e}"),
                }
            }
            if let Some(rx) = &w.dialog
                && let Ok(answer) = rx.try_recv()
            {
                w.dialog = None;
                match answer {
                    Ok(Some(path)) => {
                        w.ed.save_to(&path);
                    }
                    Ok(None) => {}
                    Err(e) => {
                        // No file dialog here: beside the picture, under a new name.
                        crate::timing::log(format!("no file dialog ({e})"));
                        let p = &w.ed.path;
                        let stem = p.file_stem().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "picture".into());
                        let ext = p.extension().map(|s| s.to_string_lossy().into_owned()).unwrap_or_else(|| "png".into());
                        let mut target = p.with_file_name(format!("{stem}-annotated.{ext}"));
                        let mut n = 1;
                        while target.exists() && n < 1000 {
                            target = p.with_file_name(format!("{stem}-annotated-{n}.{ext}"));
                            n += 1;
                        }
                        w.ed.save_to(&target);
                    }
                }
            }
            self.serve_editor(id);
        }
    }

    /// The earliest an editor needs a look.
    pub(super) fn editors_deadline(&self) -> Option<Instant> {
        self.editors.iter().filter_map(|e| e.ed.next_deadline()).min()
    }

    /// Draw the editors that changed.
    pub(super) fn redraw_editors(&mut self) {
        for owner in self.dpy.owners() {
            let Owner::Editor(id) = owner else { continue };
            let Some((logical, scale)) = self.dpy.surface(owner) else { continue };
            if let Some(w) = self.editors.iter_mut().find(|e| e.id == id) {
                w.ed.set_view(logical.0 as f32, logical.1 as f32);
                w.ed.set_scale(scale);
                crate::ui::caret::set_shown(self.blink.shown_in(owner));
                self.dpy.redraw(owner, 0, &mut EditorPaint(&mut w.ed));
            }
        }
    }

    /// Pick a picture to annotate with the desktop's file dialog.
    pub(super) fn choose_image(&mut self) {
        let folder = crate::template::base_folder(&crate::config::load().save_dir);
        let (tx, rx) = mpsc::channel();
        let waker = self.waker.clone();
        let _ = std::thread::Builder::new().name("flatshot-dialog".into()).spawn(move || {
            let _ = tx.send(portal::open_file("Annotate an image", &folder, &image_filters()));
            waker();
        });
        self.open_dialog = Some(rx);
    }

    /// The answer of "Annotate an image…".
    pub(super) fn poll_open_dialog(&mut self) {
        let Some(rx) = &self.open_dialog else { return };
        let Ok(answer) = rx.try_recv() else { return };
        self.open_dialog = None;
        match answer {
            Ok(Some(path)) => {
                self.open_editor(&path, None);
            }
            Ok(None) => {}
            Err(e) => self.report_failure("No file dialog", &format!("The desktop has no file dialog to offer ({e}). Use flatshot-rs -i FILE instead.")),
        }
    }
}
