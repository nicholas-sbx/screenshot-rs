//! `-v` / `--verbose`: how long each step of a capture takes, on stderr
//! (`flatshot --tray -v` for every capture the tray app takes).

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Instant;

static ENABLED: AtomicBool = AtomicBool::new(false);

pub fn enable() {
    ENABLED.store(true, Ordering::Relaxed);
}

pub fn enabled() -> bool {
    ENABLED.load(Ordering::Relaxed)
}

pub fn log(message: impl AsRef<str>) {
    if enabled() {
        eprintln!("flatshot: {}", message.as_ref());
    }
}

/// Times the steps of one thing (a capture, a save): each step says how
/// long it took since the one before, and since the start.
#[derive(Clone, Debug)]
pub struct Clock {
    pub what: String,
    pub start: Instant,
    last: Instant,
}

impl Clock {
    pub fn new(what: impl Into<String>) -> Clock {
        Clock::since(what, Instant::now())
    }

    /// A clock that started at `start` (the key press, say).
    pub fn since(what: impl Into<String>, start: Instant) -> Clock {
        Clock { what: what.into(), start, last: start }
    }

    pub fn step(&mut self, name: &str, detail: impl AsRef<str>) {
        let now = Instant::now();
        if enabled() {
            let detail = detail.as_ref();
            log(format!(
                "{}: {} {:.1} ms (at {:.1} ms){}{}",
                self.what,
                name,
                (now - self.last).as_secs_f64() * 1000.0,
                (now - self.start).as_secs_f64() * 1000.0,
                if detail.is_empty() { "" } else { ", " },
                detail
            ));
        }
        self.last = now;
    }

    /// Milliseconds since the start.
    pub fn since_start(&self) -> f64 {
        self.start.elapsed().as_secs_f64() * 1000.0
    }
}
