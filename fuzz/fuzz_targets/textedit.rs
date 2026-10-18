//! Typing, moving the caret, selecting, undoing and pasting in a text: the
//! caret and the selection's other end stay on the text's character
//! boundaries, and the limits (one line, a longest length) hold.
#![no_main]

use arbitrary::Arbitrary;
use flatshot::keys::KeyPress;
use flatshot::keyseq::{ALT, CTRL, SHIFT, key};
use flatshot::ui::textedit::TextEdit;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
enum Op {
    Insert(String),
    Paste(String),
    Place(u16, bool),
    Key { which: u8, mods: u8, text: String },
    Undo,
    Redo,
    SelectAll,
}

#[derive(Arbitrary, Debug)]
struct Input {
    start: String,
    single_line: bool,
    max_chars: u8,
    ops: Vec<Op>,
}

const KEYS: [i32; 17] = [
    key::ESCAPE,
    key::RETURN,
    key::ENTER,
    key::BACKSPACE,
    key::DELETE,
    key::LEFT,
    key::RIGHT,
    key::HOME,
    key::END,
    key::UP,
    key::DOWN,
    key::TAB,
    key::A,
    key::C,
    key::X,
    key::V,
    key::Z,
];

fn width(s: &str) -> f32 {
    s.chars().count() as f32 * 7.0
}

fuzz_target!(|input: Input| {
    let mut t = if input.single_line {
        TextEdit::single_line(&input.start.replace(['\n', '\r'], " "), 0)
    } else {
        TextEdit::new(&input.start)
    };
    // (the limit only binds what's typed after it's set)
    let start_chars = t.text.chars().count();
    t.max_chars = input.max_chars as usize;
    for op in input.ops.iter().take(64) {
        match op {
            Op::Insert(s) => t.insert(s),
            Op::Paste(s) => t.paste(s),
            Op::Place(i, select) => t.place(*i as usize, *select),
            Op::Key { which, mods, text } => {
                let k = KEYS.get(*which as usize).copied().unwrap_or(0x41 + (*which as i32 % 26));
                let mods = [0, SHIFT, CTRL, CTRL | SHIFT, ALT, ALT | SHIFT, CTRL | ALT, CTRL | ALT | SHIFT][*mods as usize % 8];
                let _ = t.key(&KeyPress { key: k, mods, text: text.clone(), ..Default::default() }, &width);
            }
            Op::Undo => {
                t.undo_edit(false);
            }
            Op::Redo => {
                t.undo_edit(true);
            }
            Op::SelectAll => t.select_all(),
        }
        assert!(t.cursor <= t.text.len() && t.text.is_char_boundary(t.cursor), "caret {} in {:?}", t.cursor, t.text);
        assert!(t.anchor <= t.text.len() && t.text.is_char_boundary(t.anchor), "anchor {} in {:?}", t.anchor, t.text);
        let _ = t.selected();
        let (row, col) = t.row_col(t.cursor);
        assert_eq!(t.index(row, col), t.cursor, "row {row} col {col} in {:?}", t.text);
        if input.single_line {
            assert!(!t.text.contains('\n'), "{:?}", t.text);
        }
        if t.max_chars > 0 && start_chars <= t.max_chars {
            assert!(t.text.chars().count() <= t.max_chars, "{} > {}", t.text.chars().count(), t.max_chars);
        }
    }
});
