//! Keys on X11. The server's keymap is named by its XKB rules, model,
//! layouts, variants and options (the root window's _XKB_RULES_NAMES, as
//! setxkbmap and the desktop leave them); libxkbcommon compiles it as on
//! Wayland, and each key event's modifiers and layout group pick the symbol.

use x11rb::connection::Connection;
use x11rb::protocol::xproto::{self, ConnectionExt as _};
use xkbcommon::xkb;

use crate::keys::KeyPress;
use crate::keyseq;

/// rules, model, layouts, variants, options
pub type Names = [String; 5];

pub struct Keyboard {
    names: Names,
    /// compiled at the first key: it takes a few milliseconds, which a
    /// capture's first frame needn't wait for
    state: Option<xkb::State>,
    /// keys held, by keycode (a held key's repeats come as more presses)
    down: [u64; 4],
}

impl Keyboard {
    pub fn new(names: Names) -> Keyboard {
        Keyboard { names, state: None, down: [0; 4] }
    }

    /// The keymap's names on the root window (the defaults if there are none).
    pub fn read_names(conn: &impl Connection, root: xproto::Window, property: xproto::Atom) -> Names {
        let mut names: Names = Default::default();
        let reply = conn.get_property(false, root, property, xproto::AtomEnum::STRING, 0, 1024).ok().and_then(|c| c.reply().ok());
        if let Some(r) = reply {
            for (slot, part) in names.iter_mut().zip(r.value.split(|&b| b == 0)) {
                *slot = String::from_utf8_lossy(part).into_owned();
            }
        }
        names
    }

    /// The keymap's names now: compiled again at the next key if they
    /// changed (setxkbmap, the desktop's settings).
    pub fn reset_if_changed(&mut self, names: Names) {
        if names != self.names {
            self.names = names;
            self.state = None;
        }
    }

    fn state(&mut self) -> Option<&mut xkb::State> {
        if self.state.is_none() {
            let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
            let [rules, model, layout, variant, options] = &self.names;
            let options = (!options.is_empty()).then(|| options.clone());
            let keymap = xkb::Keymap::new_from_names(&ctx, rules, model, layout, variant, options, xkb::KEYMAP_COMPILE_NO_FLAGS)
                .or_else(|| xkb::Keymap::new_from_names(&ctx, "", "", "", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS))?;
            self.state = Some(xkb::State::new(&keymap));
        }
        self.state.as_mut()
    }

    /// Key `keycode` pressed or let go, with the modifiers and layout group
    /// of `state` (an X event's).
    pub fn key(&mut self, keycode: u8, state: u16, pressed: bool) -> KeyPress {
        let (word, bit) = (keycode as usize / 64, 1u64 << (keycode % 64));
        let repeat = pressed && self.down[word] & bit != 0;
        if pressed {
            self.down[word] |= bit;
        } else {
            self.down[word] &= !bit;
        }
        let (keysym, text) = match self.state() {
            Some(st) => {
                st.update_mask((state & 0xff) as u32, 0, 0, 0, 0, ((state >> 13) & 3) as u32);
                let code = xkb::Keycode::new(keycode as u32);
                (st.key_get_one_sym(code).raw(), if pressed { st.key_get_utf8(code) } else { String::new() })
            }
            None => (0, String::new()),
        };
        let keypad = if keyseq::is_keypad(keysym) { keyseq::KEYPAD } else { 0 };
        let key = keyseq::keysym_to_qt(keysym, text.chars().next());
        KeyPress { key, mods: mods_of(state) | keypad, text, keysym, repeat }
    }
}

/// keyseq modifier bits of an X event's state (Mod1 is Alt and Mod4 is
/// Super, as in nearly every keymap).
pub fn mods_of(state: u16) -> i32 {
    let mut mods = 0;
    if state & 0x1 != 0 {
        mods |= keyseq::SHIFT;
    }
    if state & 0x4 != 0 {
        mods |= keyseq::CTRL;
    }
    if state & 0x8 != 0 {
        mods |= keyseq::ALT;
    }
    if state & 0x40 != 0 {
        mods |= keyseq::META;
    }
    mods
}

/// The modifiers once `keysym` is pressed or let go (an X event carries
/// those from before it).
pub fn mods_after(mods: i32, keysym: u32, pressed: bool) -> i32 {
    let bit = match keysym {
        0xffe1 | 0xffe2 => keyseq::SHIFT,
        0xffe3 | 0xffe4 => keyseq::CTRL,
        0xffe7..=0xffea => keyseq::ALT,
        0xffeb | 0xffec => keyseq::META,
        _ => return mods,
    };
    if pressed { mods | bit } else { mods & !bit }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_through_the_keymap() {
        let mut kb = Keyboard::new(["evdev".into(), "pc105".into(), "us".into(), String::new(), String::new()]);
        // (keycode 38 is A, 10 is 1, 9 Escape)
        let a = kb.key(38, 0, true);
        assert_eq!((a.keysym, a.text.as_str(), a.key), (0x61, "a", 0x41));
        assert!(!a.repeat);
        assert!(kb.key(38, 0, true).repeat, "held: a repeat");
        let up = kb.key(38, 0, false);
        assert!(up.text.is_empty() && !up.repeat);
        let shifted = kb.key(38, 0x1, true);
        assert_eq!((shifted.text.as_str(), shifted.mods), ("A", keyseq::SHIFT));
        assert_eq!(kb.key(10, 0x1, true).text, "!");
        assert_eq!(kb.key(9, 0x4, true).mods, keyseq::CTRL);
        assert_eq!(mods_after(0, 0xffe3, true), keyseq::CTRL);
        assert_eq!(mods_after(keyseq::CTRL | keyseq::SHIFT, 0xffe1, false), keyseq::CTRL);
    }
}
