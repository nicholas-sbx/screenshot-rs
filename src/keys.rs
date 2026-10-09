//! Keys inside Flatshot's own windows: the capture overlay and the
//! annotation editor. Each can be changed in Settings → Shortcuts (kept in
//! the config as `keys`: action -> key, "" for none). KDE's global
//! shortcuts, which start a capture, are separate (shortcuts.rs). Esc,
//! Enter, and Ctrl+S / Ctrl+C to capture, stay as they are.

use std::collections::BTreeMap;

use crate::keyseq::{self, MODS, SHIFT};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Where {
    Capture,
    Editor,
    Both,
}

/// (action, label, default key, where it works), in the order Settings shows them.
pub const BINDINGS: &[(&str, &str, &str, Where)] = &[
    ("tool.region", "Capture region", "R", Where::Capture),
    ("tool.record", "Record screen", "V", Where::Capture),
    ("tool.pen", "Pen", "P", Where::Both),
    ("tool.line", "Line", "L", Where::Both),
    ("tool.arrow", "Arrow", "A", Where::Both),
    ("tool.rect", "Rectangle", "B", Where::Both),
    ("tool.solid", "Filled rectangle", "F", Where::Both),
    ("tool.ellipse", "Ellipse", "E", Where::Both),
    ("tool.marker", "Highlighter", "H", Where::Both),
    ("tool.text", "Text", "T", Where::Both),
    ("tool.pixelate", "Pixelate", "X", Where::Both),
    ("tool.blur", "Blur", "U", Where::Both),
    ("tool.counter", "Counter", "N", Where::Both),
    ("color.1", "Colour 1: ember", "1", Where::Both),
    ("color.2", "Colour 2: amber", "2", Where::Both),
    ("color.3", "Colour 3: mint", "3", Where::Both),
    ("color.4", "Colour 4: sky", "4", Where::Both),
    ("color.5", "Colour 5: violet", "5", Where::Both),
    ("color.6", "Colour 6: bone", "6", Where::Both),
    ("color.7", "Colour 7: ink", "7", Where::Both),
    ("color.8", "Your own colour", "8", Where::Both),
    ("size.down", "Thinner", "[", Where::Both),
    ("size.up", "Thicker", "]", Where::Both),
    ("undo", "Undo", "Ctrl+Z", Where::Both),
    ("redo", "Redo", "Ctrl+Shift+Z", Where::Both),
    ("codes", "Show or hide QR codes", "Q", Where::Capture),
    ("pin", "Pin instead of saving", "K", Where::Capture),
    ("snap", "Snap to edges", "G", Where::Capture),
    ("pointer", "Show or hide the mouse pointer", "M", Where::Capture),
    ("copy_color", "Copy the colour under the pointer", "I", Where::Capture),
    ("save", "Save", "Ctrl+S", Where::Editor),
    ("save_as", "Save as", "Ctrl+Shift+S", Where::Editor),
    ("copy", "Copy the picture", "Ctrl+C", Where::Editor),
    ("fit", "Fit the picture in the window", "Ctrl+0", Where::Editor),
    ("actual_size", "Actual size", "Ctrl+1", Where::Editor),
    ("zoom_in", "Zoom in", "Ctrl+=", Where::Editor),
    ("zoom_out", "Zoom out", "Ctrl+-", Where::Editor),
    ("close", "Close the editor", "Ctrl+W", Where::Editor),
];

/// The groups Settings shows the keys in.
pub fn groups() -> Vec<(&'static str, Vec<&'static str>)> {
    let actions = |f: &dyn Fn(&str) -> bool| BINDINGS.iter().map(|b| b.0).filter(|a| f(a)).collect::<Vec<_>>();
    vec![
        ("Tools", actions(&|a| a.starts_with("tool."))),
        ("Colours, sizes and undo", actions(&|a| a.starts_with("color.") || a.starts_with("size.") || a == "undo" || a == "redo")),
        ("While capturing", vec!["codes", "pin", "snap", "pointer", "copy_color"]),
        ("Annotation editor", vec!["save", "save_as", "copy", "fit", "actual_size", "zoom_in", "zoom_out", "close"]),
    ]
}

pub fn binding(action: &str) -> Option<&'static (&'static str, &'static str, &'static str, Where)> {
    BINDINGS.iter().find(|b| b.0 == action)
}

/// Do two actions work in the same window (so can't share a key)?
pub fn overlaps(a: &str, b: &str) -> bool {
    match (binding(a), binding(b)) {
        (Some(x), Some(y)) => x.3 == Where::Both || y.3 == Where::Both || x.3 == y.3,
        _ => false,
    }
}

/// A key press, as the windows report it.
#[derive(Clone, Debug, Default)]
pub struct KeyPress {
    /// Qt's key code for it (keyseq::keysym_to_qt).
    pub key: i32,
    /// keyseq modifier bits held (CTRL, SHIFT, ALT, META).
    pub mods: i32,
    /// The text it types, if any.
    pub text: String,
    pub keysym: u32,
    pub repeat: bool,
}

impl KeyPress {
    pub fn ctrl(&self) -> bool {
        self.mods & keyseq::CTRL != 0
    }
    pub fn shift(&self) -> bool {
        self.mods & SHIFT != 0
    }
    pub fn alt(&self) -> bool {
        self.mods & keyseq::ALT != 0
    }
}

/// The keys in effect: the defaults with the user's changes.
#[derive(Clone, Debug)]
pub struct Keymap {
    pub keys: BTreeMap<String, String>,
    parsed: Vec<(&'static str, Option<(i32, i32)>)>,
}

fn parse_key(text: &str) -> Option<(i32, i32)> {
    let combo = *keyseq::parse(text).first()?;
    Some((combo & !keyseq::MODIFIER_MASK, combo & MODS))
}

impl Keymap {
    pub fn new(overrides: &BTreeMap<String, String>) -> Keymap {
        let mut keys = BTreeMap::new();
        let mut parsed = Vec::with_capacity(BINDINGS.len());
        for (action, _, default, _) in BINDINGS {
            let value = overrides.get(*action).cloned().unwrap_or_else(|| default.to_string());
            parsed.push((*action, parse_key(&value)));
            keys.insert(action.to_string(), value);
        }
        Keymap { keys, parsed }
    }

    pub fn label(&self, action: &str) -> String {
        self.keys.get(action).map(|k| keyseq::label(k)).unwrap_or_default()
    }

    /// `text` with its key after it, as the toolbar's tips show it.
    pub fn hint(&self, text: &str, action: &str) -> String {
        let key = self.label(action);
        if key.is_empty() { text.to_string() } else { format!("{text}  ·  {key}") }
    }

    /// The action `press` is bound to in `place`.
    pub fn action(&self, press: &KeyPress, place: Where) -> Option<&'static str> {
        let k = press.key;
        let mods = press.mods & MODS;
        let letter = (keyseq::key::A..=keyseq::key::Z).contains(&k);
        for (action, parsed) in &self.parsed {
            let Some((key, want)) = parsed else { continue };
            let here = binding(action).map(|b| b.3);
            if here != Some(place) && here != Some(Where::Both) {
                continue;
            }
            if *key != k || (mods & !SHIFT) != (want & !SHIFT) {
                continue;
            }
            // Shift counts when the key asks for it, and for letters; other
            // keys may need it just to be typed (on some layouts, "]").
            if want & SHIFT != 0 && mods & SHIFT == 0 {
                continue;
            }
            if letter && mods & SHIFT != 0 && want & SHIFT == 0 {
                continue;
            }
            return Some(action);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::keyseq::{CTRL, key};

    fn press(k: i32, mods: i32) -> KeyPress {
        KeyPress { key: k, mods, ..Default::default() }
    }

    #[test]
    fn defaults_and_matching() {
        let km = Keymap::new(&BTreeMap::new());
        assert_eq!(km.action(&press(0x50, 0), Where::Capture), Some("tool.pen"));
        assert_eq!(km.action(&press(key::Z, CTRL), Where::Editor), Some("undo"));
        assert_eq!(km.action(&press(key::Z, CTRL | SHIFT), Where::Editor), Some("redo"));
        // Shift+P is not P (letters).
        assert_eq!(km.action(&press(0x50, SHIFT), Where::Capture), None);
        // "]" typed with Shift on some layouts still counts.
        assert_eq!(km.action(&press(0x5d, SHIFT), Where::Capture), Some("size.up"));
        // Editor-only keys don't work while capturing.
        assert_eq!(km.action(&press(key::S, CTRL), Where::Capture), None);
        assert_eq!(km.action(&press(key::S, CTRL), Where::Editor), Some("save"));
        assert_eq!(km.label("redo"), "Ctrl+Shift+Z");
        assert_eq!(km.hint("Undo", "undo"), "Undo  ·  Ctrl+Z");
    }

    #[test]
    fn overrides() {
        let mut o = BTreeMap::new();
        o.insert("tool.pen".to_string(), "J".to_string());
        o.insert("codes".to_string(), String::new());
        let km = Keymap::new(&o);
        assert_eq!(km.action(&press(0x4a, 0), Where::Capture), Some("tool.pen"));
        assert_eq!(km.action(&press(0x50, 0), Where::Capture), None);
        assert_eq!(km.action(&press(0x51, 0), Where::Capture), None);
        assert_eq!(km.hint("Codes", "codes"), "Codes");
    }

    #[test]
    fn groups_cover_everything() {
        let n: usize = groups().iter().map(|g| g.1.len()).sum();
        assert_eq!(n, BINDINGS.len());
        assert!(overlaps("tool.pen", "save"));
        assert!(!overlaps("codes", "save"));
    }
}
