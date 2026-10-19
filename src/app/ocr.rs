//! Copying text from the screen: an area's picture is read on a thread of
//! its own (crate::ocr runs Tesseract), then its text is copied, printed
//! and shown in a notification.

use std::sync::{Arc, mpsc};

use super::App;
use crate::clipboard::Slot;
use crate::desktop::notify;
use crate::image::Image;
use crate::util;

/// An area being read.
pub(super) struct Reading {
    rx: mpsc::Receiver<Result<String, String>>,
    /// the clipboard, claimed while the overlay had the keyboard (where
    /// it can't be had later)
    slot: Option<Arc<Slot>>,
}

impl App {
    /// Read the text in `image` (`dpr` pixels per logical pixel).
    pub(super) fn read_text(&mut self, image: Image, dpr: f32, slot: Option<Arc<Slot>>) {
        let (tx, rx) = mpsc::channel();
        let (waker, languages) = (self.waker.clone(), self.cfg.ocr_languages.clone());
        let started = std::thread::Builder::new().name("flatshot-ocr".into()).spawn(move || {
            let _ = tx.send(crate::ocr::read(&image, dpr, &languages));
            waker();
        });
        match started {
            Ok(_) => self.reading.push(Reading { rx, slot }),
            Err(e) => self.text_read(Err(e.to_string()), slot),
        }
    }

    /// Hand on the text of the areas read by now.
    pub(super) fn poll_reading(&mut self) {
        let mut i = 0;
        while i < self.reading.len() {
            let answer = match self.reading[i].rx.try_recv() {
                Ok(answer) => answer,
                Err(mpsc::TryRecvError::Empty) => {
                    i += 1;
                    continue;
                }
                Err(mpsc::TryRecvError::Disconnected) => Err("the text recognition stopped".into()),
            };
            let r = self.reading.remove(i);
            self.text_read(answer, r.slot);
        }
    }

    pub(super) fn is_reading(&self) -> bool {
        !self.reading.is_empty()
    }

    fn text_read(&mut self, answer: Result<String, String>, slot: Option<Arc<Slot>>) {
        let code = match answer {
            Ok(text) if !text.trim().is_empty() => {
                self.clock.step("read", format!("{} characters", text.chars().count()));
                let copied = self.copy_text(&text, slot);
                for line in text.lines() {
                    util::print_line(line);
                }
                if self.cfg.notify {
                    let summary = if copied { "Copied text" } else { "Text" };
                    let body = util::elide(&util::displayable(&text), 300);
                    self.notify(notify::Note { summary: summary.into(), body, event: "copied", ..Default::default() });
                }
                0
            }
            other => {
                // (the clipboard is let go: nothing came for it)
                if let Some(slot) = slot {
                    slot.abandon();
                    self.dpy.withdraw();
                }
                match other {
                    Err(e) => {
                        self.report_failure("Copying text failed", &e);
                        2
                    }
                    Ok(_) => {
                        if self.cfg.notify {
                            let body = "Tesseract found no text there. Text that is small or faint may read better from a larger area.";
                            self.notify(notify::Note { summary: "No text found".into(), body: body.into(), event: "failed", ..Default::default() });
                        }
                        1
                    }
                }
            }
        };
        self.ended(code);
    }
}
