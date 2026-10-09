//! Editing a piece of text: a caret that moves (arrows, Home, End, Ctrl
//! for whole words), Shift to select, Ctrl+A, cut, copy and paste, and its
//! own undo while it's being typed. Used by text drawn on a capture and by
//! the text fields in Flatshot's panels.
//!
//! Positions are byte offsets on grapheme boundaries, so the caret never
//! lands inside a character or between a letter and its accent.

use unicode_segmentation::UnicodeSegmentation;

use crate::keys::KeyPress;
use crate::keyseq::{key, sym};

#[derive(Clone, Debug, Default, PartialEq)]
pub struct TextEdit {
    pub text: String,
    /// where the caret is
    pub cursor: usize,
    /// the other end of the selection (== cursor: none)
    pub anchor: usize,
    undo: Vec<(String, usize)>,
    redo: Vec<(String, usize)>,
    /// the last change was typing (undone a word at a time)
    typing: bool,
    /// A single line (a text field): Enter isn't a new line.
    pub single_line: bool,
    /// Longest allowed text, in characters (0: any).
    pub max_chars: usize,
}

/// What a key did to the text.
#[derive(Clone, Debug, PartialEq)]
pub enum KeyResult {
    /// Not for the text (a Ctrl shortcut it has no use for: undo, save...).
    Ignored,
    /// Used (moved the caret, typed, ...).
    Used,
    /// Enter or Esc: finished.
    Commit,
    /// Copy this to the clipboard (Ctrl+C / Ctrl+X); already handled.
    Copy(String),
    /// Paste from the clipboard: the caller fetches it and calls `insert`.
    Paste,
}

/// Measurements the caret needs for moving up and down a line.
pub trait Measure {
    /// The width of `s` (one line).
    fn width(&self, s: &str) -> f32;
}

impl<F: Fn(&str) -> f32> Measure for F {
    fn width(&self, s: &str) -> f32 {
        self(s)
    }
}

impl TextEdit {
    pub fn new(text: &str) -> TextEdit {
        TextEdit { text: text.to_string(), cursor: text.len(), anchor: text.len(), ..Default::default() }
    }

    pub fn single_line(text: &str, max_chars: usize) -> TextEdit {
        TextEdit { single_line: true, max_chars, ..TextEdit::new(text) }
    }

    pub fn lines(&self) -> Vec<&str> {
        self.text.split('\n').collect()
    }

    pub fn selection(&self) -> (usize, usize) {
        (self.cursor.min(self.anchor), self.cursor.max(self.anchor))
    }

    pub fn selected(&self) -> &str {
        let (a, b) = self.selection();
        &self.text[a..b]
    }

    pub fn has_selection(&self) -> bool {
        self.cursor != self.anchor
    }

    fn clamp(&mut self) {
        let n = self.text.len();
        self.cursor = self.boundary(self.cursor.min(n));
        self.anchor = self.boundary(self.anchor.min(n));
    }

    /// The nearest grapheme boundary at or before `i`.
    fn boundary(&self, i: usize) -> usize {
        if i >= self.text.len() {
            return self.text.len();
        }
        let mut best = 0;
        for (b, _) in self.text.grapheme_indices(true) {
            if b > i {
                break;
            }
            best = b;
        }
        best
    }

    fn prev(&self, i: usize) -> usize {
        self.text[..i].grapheme_indices(true).next_back().map(|(b, _)| b).unwrap_or(0)
    }

    fn next(&self, i: usize) -> usize {
        self.text[i..].graphemes(true).next().map(|g| i + g.len()).unwrap_or(self.text.len())
    }

    /// (row, byte column within the row) of a position.
    pub fn row_col(&self, index: usize) -> (usize, usize) {
        let before = &self.text[..index.min(self.text.len())];
        let row = before.matches('\n').count();
        let col = before.len() - before.rfind('\n').map(|p| p + 1).unwrap_or(0);
        (row, col)
    }

    /// The position at (row, byte column), clamped to the text.
    pub fn index(&self, row: usize, col: usize) -> usize {
        let lines = self.lines();
        let row = row.min(lines.len().saturating_sub(1));
        let start: usize = lines[..row].iter().map(|l| l.len() + 1).sum();
        let line = lines.get(row).copied().unwrap_or("");
        self.boundary(start + col.min(line.len()))
    }

    fn snapshot(&mut self, typing: bool) {
        if !(typing && self.typing) {
            self.undo.push((self.text.clone(), self.cursor));
            if self.undo.len() > 200 {
                self.undo.remove(0);
            }
        }
        self.redo.clear();
        self.typing = typing;
    }

    /// Type `s` over the selection.
    pub fn insert(&mut self, s: &str) {
        let s: String = if self.single_line { s.replace(['\n', '\r'], " ") } else { s.to_string() };
        if s.is_empty() {
            return;
        }
        self.clamp();
        let (a, b) = self.selection();
        let mut s = s;
        if self.max_chars > 0 {
            let room = self.max_chars.saturating_sub(self.text.chars().count() - self.text[a..b].chars().count());
            if s.chars().count() > room {
                s = s.chars().take(room).collect();
                if s.is_empty() {
                    return;
                }
            }
        }
        let typing = s != " " && s != "\n" && s.chars().count() == 1;
        self.snapshot(typing);
        self.text.replace_range(a..b, &s);
        self.cursor = a + s.len();
        self.anchor = self.cursor;
    }

    fn delete(&mut self, a: usize, b: usize) {
        if a == b {
            return;
        }
        self.snapshot(false);
        self.text.replace_range(a..b, "");
        self.cursor = a;
        self.anchor = a;
    }

    /// Move the caret to `index`; with `select`, extend the selection.
    pub fn place(&mut self, index: usize, select: bool) {
        self.clamp();
        self.cursor = self.boundary(index.min(self.text.len()));
        if !select {
            self.anchor = self.cursor;
        }
        self.typing = false;
    }

    pub fn select_all(&mut self) {
        self.anchor = 0;
        self.cursor = self.text.len();
    }

    pub fn set_text(&mut self, text: &str) {
        self.text = text.to_string();
        self.cursor = self.text.len();
        self.anchor = self.cursor;
    }

    fn word_left(&self, mut i: usize) -> usize {
        let t = &self.text;
        while i > 0 && t[..i].chars().next_back().is_some_and(char::is_whitespace) {
            i = self.prev(i);
        }
        while i > 0 && t[..i].chars().next_back().is_some_and(|c| !c.is_whitespace()) {
            i = self.prev(i);
        }
        i
    }

    fn word_right(&self, mut i: usize) -> usize {
        let t = &self.text;
        let n = t.len();
        while i < n && t[i..].chars().next().is_some_and(char::is_whitespace) {
            i = self.next(i);
        }
        while i < n && t[i..].chars().next().is_some_and(|c| !c.is_whitespace()) {
            i = self.next(i);
        }
        i
    }

    /// Undo (or redo) the last change to the text; false if there's none.
    pub fn undo_edit(&mut self, redo: bool) -> bool {
        let (source, target) = if redo { (&mut self.redo, &mut self.undo) } else { (&mut self.undo, &mut self.redo) };
        let Some((text, cursor)) = source.pop() else { return false };
        target.push((self.text.clone(), self.cursor));
        self.text = text;
        self.cursor = cursor.min(self.text.len());
        self.anchor = self.cursor;
        self.typing = false;
        true
    }

    /// The column on `row` nearest to `x` (from the line's left).
    pub fn col_at(&self, row: usize, x: f32, m: &dyn Measure) -> usize {
        let lines = self.lines();
        let line = lines.get(row).copied().unwrap_or("");
        let mut best = 0;
        let mut best_d = x.abs();
        for (b, g) in line.grapheme_indices(true) {
            let end = b + g.len();
            let d = (m.width(&line[..end]) - x).abs();
            if d < best_d {
                best = end;
                best_d = d;
            }
        }
        best
    }

    /// The x of a position on its line.
    pub fn x_of(&self, row: usize, col: usize, m: &dyn Measure) -> f32 {
        let lines = self.lines();
        let line = lines.get(row).copied().unwrap_or("");
        m.width(&line[..col.min(line.len())])
    }

    /// A key while typing.
    pub fn key(&mut self, press: &KeyPress, m: &dyn Measure) -> KeyResult {
        let ctrl = press.ctrl();
        let shift = press.shift();
        let alt = press.alt();
        self.clamp();
        let (a, b) = self.selection();
        let (row, col) = self.row_col(self.cursor);
        let k = press.key;
        if k == key::ESCAPE {
            return KeyResult::Commit;
        }
        if k == key::RETURN || k == key::ENTER {
            if shift && !self.single_line {
                self.insert("\n");
                return KeyResult::Used;
            }
            return KeyResult::Commit;
        }
        if k == key::BACKSPACE {
            let range = if a != b {
                (a, b)
            } else if ctrl {
                (self.word_left(a), a)
            } else {
                (self.prev(a), a)
            };
            self.delete(range.0, range.1);
            return KeyResult::Used;
        }
        if k == key::DELETE {
            let range = if a != b {
                (a, b)
            } else if ctrl {
                (a, self.word_right(a))
            } else {
                (a, self.next(a))
            };
            self.delete(range.0, range.1);
            return KeyResult::Used;
        }
        if k == key::LEFT {
            let to = if ctrl {
                self.word_left(self.cursor)
            } else if a != b && !shift {
                a
            } else {
                self.prev(self.cursor)
            };
            self.place(to, shift);
            return KeyResult::Used;
        }
        if k == key::RIGHT {
            let to = if ctrl {
                self.word_right(self.cursor)
            } else if a != b && !shift {
                b
            } else {
                self.next(self.cursor)
            };
            self.place(to, shift);
            return KeyResult::Used;
        }
        if k == key::HOME {
            let to = if ctrl { 0 } else { self.index(row, 0) };
            self.place(to, shift);
            return KeyResult::Used;
        }
        if k == key::END {
            let to = if ctrl {
                self.text.len()
            } else {
                let len = self.lines().get(row).map_or(0, |l| l.len());
                self.index(row, len)
            };
            self.place(to, shift);
            return KeyResult::Used;
        }
        if k == key::UP || k == key::DOWN {
            let up = k == key::UP;
            let rows = self.lines().len();
            if (up && row > 0) || (!up && row + 1 < rows) {
                let target = if up { row - 1 } else { row + 1 };
                let x = self.x_of(row, col, m);
                let c = self.col_at(target, x, m);
                let to = self.index(target, c);
                self.place(to, shift);
            } else {
                self.place(if up { 0 } else { self.text.len() }, shift);
            }
            return KeyResult::Used;
        }
        if ctrl && !alt {
            match k {
                key::A => {
                    self.select_all();
                    return KeyResult::Used;
                }
                key::C if a != b => return KeyResult::Copy(self.selected().to_string()),
                key::X if a != b => {
                    let s = self.selected().to_string();
                    self.delete(a, b);
                    return KeyResult::Copy(s);
                }
                key::V => return KeyResult::Paste,
                _ => return KeyResult::Ignored,
            }
        }
        if k == key::TAB || press.keysym == sym::TAB {
            return KeyResult::Used; // (not to move the focus away)
        }
        let t = &press.text;
        if !t.is_empty() && t.chars().all(|c| !c.is_control()) {
            self.insert(t);
        }
        KeyResult::Used // (any other key does nothing while typing)
    }

    /// Text from the clipboard, as Python's paste cleaned it.
    pub fn paste(&mut self, clip: &str) {
        let s = clip.replace("\r\n", "\n").replace('\t', "    ");
        self.insert(&s);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyseq::{CTRL, SHIFT};

    fn press(k: i32, mods: i32, text: &str) -> KeyPress {
        KeyPress { key: k, mods, text: text.into(), ..Default::default() }
    }

    fn w(s: &str) -> f32 {
        s.chars().count() as f32 * 10.0
    }

    #[test]
    fn typing_and_undo() {
        let mut t = TextEdit::default();
        for ch in "hi!".chars() {
            t.key(&press(0x41, 0, &ch.to_string()), &w);
        }
        assert_eq!(t.text, "hi!");
        t.key(&press(key::BACKSPACE, 0, ""), &w);
        assert_eq!(t.text, "hi");
        assert_eq!(t.key(&press(key::RETURN, 0, ""), &w), KeyResult::Commit);
        // Typing is undone a word at a time.
        assert!(t.undo_edit(false));
        assert_eq!(t.text, "hi!");
        assert!(t.undo_edit(false));
        assert_eq!(t.text, "");
        assert!(t.undo_edit(true));
        assert_eq!(t.text, "hi!");
    }

    #[test]
    fn words_selection_and_lines() {
        let mut t = TextEdit::new("hello big world");
        t.key(&press(key::LEFT, CTRL, ""), &w);
        assert_eq!(t.cursor, 10);
        t.key(&press(key::LEFT, CTRL | SHIFT, ""), &w);
        assert_eq!(t.selected(), "big ");
        assert_eq!(t.key(&press(key::C, CTRL, ""), &w), KeyResult::Copy("big ".into()));
        t.key(&press(key::RETURN, SHIFT, ""), &w);
        assert_eq!(t.text, "hello \nworld");
        t.place(t.text.len(), false);
        t.key(&press(key::UP, 0, ""), &w);
        assert_eq!(t.row_col(t.cursor), (0, 5));
        t.key(&press(key::A, CTRL, ""), &w);
        assert_eq!(t.selected(), "hello \nworld");
        assert_eq!(t.key(&press(key::Z, CTRL, ""), &w), KeyResult::Ignored);
    }

    #[test]
    fn graphemes_stay_whole() {
        let mut t = TextEdit::new("e\u{301}x");
        t.key(&press(key::LEFT, 0, ""), &w);
        t.key(&press(key::BACKSPACE, 0, ""), &w);
        assert_eq!(t.text, "x");
    }

    #[test]
    fn single_line_limits() {
        let mut t = TextEdit::single_line("#FF", 7);
        t.insert("1234567");
        assert_eq!(t.text, "#FF1234");
        assert_eq!(t.key(&press(key::RETURN, SHIFT, ""), &w), KeyResult::Commit);
    }
}
