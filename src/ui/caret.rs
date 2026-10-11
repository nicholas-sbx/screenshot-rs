//! Whether the text caret is drawn in the window being painted: the app
//! blinks it (`app::caret`) and says so before painting each window.

use std::cell::Cell;
use std::time::Duration;

/// How long the caret stays on, and then off.
pub const HALF: Duration = Duration::from_millis(530);

thread_local! {
    static SHOWN: Cell<bool> = const { Cell::new(true) };
}

/// Is the caret drawn in the window being painted?
pub fn shown() -> bool {
    SHOWN.with(|s| s.get())
}

pub fn set_shown(on: bool) {
    SHOWN.with(|s| s.set(on));
}
