//! Key sequences as Qt writes them ("Ctrl+Shift+Z", "Meta+Print") and the
//! integer codes behind them. KDE's global shortcut service stores and
//! sends shortcuts as Qt key codes, and Flatshot's own key settings use
//! Qt's text form, so both are kept exactly as Qt has them.

pub const SHIFT: i32 = 0x0200_0000;
pub const CTRL: i32 = 0x0400_0000;
pub const ALT: i32 = 0x0800_0000;
pub const META: i32 = 0x1000_0000;
pub const KEYPAD: i32 = 0x2000_0000;
pub const MODIFIER_MASK: i32 = 0xFE00_0000u32 as i32;
/// The modifiers shortcuts are made of (`keys._MODS`).
pub const MODS: i32 = CTRL | ALT | META | SHIFT;

pub mod key {
    pub const ESCAPE: i32 = 0x0100_0000;
    pub const TAB: i32 = 0x0100_0001;
    pub const BACKTAB: i32 = 0x0100_0002;
    pub const BACKSPACE: i32 = 0x0100_0003;
    pub const RETURN: i32 = 0x0100_0004;
    pub const ENTER: i32 = 0x0100_0005;
    pub const INSERT: i32 = 0x0100_0006;
    pub const DELETE: i32 = 0x0100_0007;
    pub const PAUSE: i32 = 0x0100_0008;
    pub const PRINT: i32 = 0x0100_0009;
    pub const SYSREQ: i32 = 0x0100_000a;
    pub const CLEAR: i32 = 0x0100_000b;
    pub const HOME: i32 = 0x0100_0010;
    pub const END: i32 = 0x0100_0011;
    pub const LEFT: i32 = 0x0100_0012;
    pub const UP: i32 = 0x0100_0013;
    pub const RIGHT: i32 = 0x0100_0014;
    pub const DOWN: i32 = 0x0100_0015;
    pub const PAGE_UP: i32 = 0x0100_0016;
    pub const PAGE_DOWN: i32 = 0x0100_0017;
    pub const SHIFT: i32 = 0x0100_0020;
    pub const CONTROL: i32 = 0x0100_0021;
    pub const META: i32 = 0x0100_0022;
    pub const ALT: i32 = 0x0100_0023;
    pub const CAPS_LOCK: i32 = 0x0100_0024;
    pub const NUM_LOCK: i32 = 0x0100_0025;
    pub const SCROLL_LOCK: i32 = 0x0100_0026;
    pub const F1: i32 = 0x0100_0030;
    pub const F35: i32 = 0x0100_0052;
    pub const SUPER_L: i32 = 0x0100_0053;
    pub const SUPER_R: i32 = 0x0100_0054;
    pub const MENU: i32 = 0x0100_0055;
    pub const HYPER_L: i32 = 0x0100_0056;
    pub const HYPER_R: i32 = 0x0100_0057;
    pub const HELP: i32 = 0x0100_0058;
    pub const ALT_GR: i32 = 0x0100_1103;
    pub const MULTI_KEY: i32 = 0x0100_1120;
    pub const SPACE: i32 = 0x20;
    pub const PLUS: i32 = 0x2b;
    pub const MINUS: i32 = 0x2d;
    pub const EQUAL: i32 = 0x3d;
    pub const A: i32 = 0x41;
    pub const C: i32 = 0x43;
    pub const D: i32 = 0x44;
    pub const S: i32 = 0x53;
    pub const V: i32 = 0x56;
    pub const W: i32 = 0x57;
    pub const X: i32 = 0x58;
    pub const Y: i32 = 0x59;
    pub const Z: i32 = 0x5a;
    pub const UNKNOWN: i32 = 0x01ff_ffff;
}

/// Qt's portable names for keys that aren't a plain character.
const NAMES: &[(i32, &str)] = &[
    (key::SPACE, "Space"),
    (key::ESCAPE, "Esc"),
    (key::TAB, "Tab"),
    (key::BACKTAB, "Backtab"),
    (key::BACKSPACE, "Backspace"),
    (key::RETURN, "Return"),
    (key::ENTER, "Enter"),
    (key::INSERT, "Ins"),
    (key::DELETE, "Del"),
    (key::PAUSE, "Pause"),
    (key::PRINT, "Print"),
    (key::SYSREQ, "SysReq"),
    (key::HOME, "Home"),
    (key::END, "End"),
    (key::LEFT, "Left"),
    (key::UP, "Up"),
    (key::RIGHT, "Right"),
    (key::DOWN, "Down"),
    (key::PAGE_UP, "PgUp"),
    (key::PAGE_DOWN, "PgDown"),
    (key::CAPS_LOCK, "CapsLock"),
    (key::NUM_LOCK, "NumLock"),
    (key::SCROLL_LOCK, "ScrollLock"),
    (key::MENU, "Menu"),
    (key::HELP, "Help"),
    (0x0100_0061, "Back"),
    (0x0100_0062, "Forward"),
    (0x0100_0063, "Stop"),
    (0x0100_0064, "Refresh"),
    (0x0100_0070, "Volume Down"),
    (0x0100_0071, "Volume Mute"),
    (0x0100_0072, "Volume Up"),
    (0x0100_0080, "Media Play"),
    (0x0100_0081, "Media Stop"),
    (0x0100_0082, "Media Previous"),
    (0x0100_0083, "Media Next"),
    (0x0100_0084, "Media Record"),
    (0x0100_0085, "Media Pause"),
    (0x0100_0086, "Toggle Media Play/Pause"),
    (0x0100_0090, "Home Page"),
    (0x0100_0091, "Favorites"),
    (0x0100_0092, "Search"),
    (0x0100_0093, "Standby"),
    (0x0100_0094, "Open URL"),
    (0x0100_00a0, "Launch Mail"),
    (0x0100_00a1, "Launch Media"),
    (0x0100_00b2, "Monitor Brightness Up"),
    (0x0100_00b3, "Monitor Brightness Down"),
    (0x0100_00cb, "Calculator"),
    (0x0100_00f6, "Zoom In"),
    (0x0100_00f7, "Zoom Out"),
    (0x0100_0113, "Microphone Mute"),
    (key::ALT_GR, "AltGr"),
    (key::MULTI_KEY, "Multi key"),
    (key::CLEAR, "Clear"),
    (key::SHIFT, "Shift"),
    (key::CONTROL, "Control"),
    (key::META, "Meta"),
    (key::ALT, "Alt"),
    (key::SUPER_L, "Super L"),
    (key::SUPER_R, "Super R"),
    (key::HYPER_L, "Hyper L"),
    (key::HYPER_R, "Hyper R"),
];

/// The name of one key (no modifiers), as Qt's portable text.
pub fn key_name(k: i32) -> String {
    if (key::F1..=key::F35).contains(&k) {
        return format!("F{}", k - key::F1 + 1);
    }
    if let Some((_, name)) = NAMES.iter().find(|(code, _)| *code == k) {
        return (*name).to_string();
    }
    if (0x21..0x0100_0000).contains(&k)
        && let Some(c) = char::from_u32(k as u32)
    {
        return c.to_uppercase().collect();
    }
    String::new()
}

/// One key with its modifiers, in Qt's portable text ("Meta+Ctrl+Alt+Shift+X").
pub fn to_text(combo: i32) -> String {
    let k = combo & !MODIFIER_MASK;
    if combo == -1 || k == key::UNKNOWN || k == 0 {
        return String::new();
    }
    let mut parts: Vec<String> = Vec::new();
    // On Linux the order is Meta, Control, Alt, Shift.
    if combo & META != 0 {
        parts.push("Meta".into());
    }
    if combo & CTRL != 0 {
        parts.push("Ctrl".into());
    }
    if combo & ALT != 0 {
        parts.push("Alt".into());
    }
    if combo & SHIFT != 0 {
        parts.push("Shift".into());
    }
    if combo & KEYPAD != 0 {
        parts.push("Num".into());
    }
    let name = key_name(k);
    if name.is_empty() {
        return String::new();
    }
    parts.push(name);
    parts.join("+")
}

/// Several keys (a chord sequence) as text: "Ctrl+X, Ctrl+Y".
pub fn seq_to_text(combos: &[i32]) -> String {
    combos.iter().map(|&c| to_text(c)).filter(|s| !s.is_empty()).collect::<Vec<_>>().join(", ")
}

fn modifier_bit(name: &str) -> Option<i32> {
    match name.to_ascii_lowercase().as_str() {
        "ctrl" | "control" => Some(CTRL),
        "shift" => Some(SHIFT),
        "alt" => Some(ALT),
        "meta" | "super" | "win" => Some(META),
        "num" => Some(KEYPAD),
        _ => None,
    }
}

fn key_from_name(name: &str) -> Option<i32> {
    if name.is_empty() {
        return None;
    }
    let lower = name.to_ascii_lowercase();
    if let Some(n) = lower.strip_prefix('f').and_then(|d| d.parse::<i32>().ok())
        && (1..=35).contains(&n)
    {
        return Some(key::F1 + n - 1);
    }
    if let Some((code, _)) = NAMES.iter().find(|(_, n)| n.eq_ignore_ascii_case(name)) {
        return Some(*code);
    }
    // A few spellings Qt also accepts.
    let alias = match lower.as_str() {
        "escape" => Some(key::ESCAPE),
        "insert" => Some(key::INSERT),
        "delete" => Some(key::DELETE),
        "pageup" | "page up" => Some(key::PAGE_UP),
        "pagedown" | "page down" => Some(key::PAGE_DOWN),
        "printscreen" | "print screen" => Some(key::PRINT),
        _ => None,
    };
    if alias.is_some() {
        return alias;
    }
    let mut chars = name.chars();
    let c = chars.next()?;
    if chars.next().is_some() {
        return None;
    }
    Some(c.to_uppercase().next().unwrap_or(c) as i32)
}

/// One key with modifiers from text ("Ctrl+Shift+Z", "Ctrl++", "]").
pub fn parse_one(text: &str) -> Option<i32> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    // Split on '+', where a '+' at the end (or doubled) is the key itself.
    let mut parts: Vec<&str> = Vec::new();
    let bytes = text.as_bytes();
    let mut start = 0;
    let mut i = 0;
    while i < bytes.len() {
        if bytes[i] == b'+' && i > start {
            parts.push(&text[start..i]);
            start = i + 1;
        }
        i += 1;
    }
    parts.push(&text[start..]);
    let (key_part, mods) = parts.split_last()?;
    let mut combo = 0;
    for m in mods {
        combo |= modifier_bit(m.trim())?;
    }
    let k = key_from_name(key_part.trim())?;
    Some(combo | k)
}

/// A whole sequence, up to four chords separated by ", ".
pub fn parse(text: &str) -> Vec<i32> {
    let text = text.trim();
    if text.is_empty() {
        return Vec::new();
    }
    let mut out = Vec::new();
    for chord in split_chords(text) {
        match parse_one(&chord) {
            Some(c) => out.push(c),
            None => return Vec::new(),
        }
        if out.len() == 4 {
            break;
        }
    }
    out
}

fn split_chords(text: &str) -> Vec<String> {
    // "Ctrl+,, X" has a comma key; split on ", " that follows a key name.
    let mut out = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        if chars[i] == ',' && i + 1 < chars.len() && chars[i + 1] == ' ' && !cur.is_empty() && !cur.ends_with('+') {
            out.push(std::mem::take(&mut cur));
            i += 2;
            continue;
        }
        cur.push(chars[i]);
        i += 1;
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// How a key is shown in Flatshot's windows: the same as its text.
pub fn label(text: &str) -> String {
    seq_to_text(&parse(text))
}

// -- keysyms ----------------------------------------------------------------------

/// XKB keysyms the app looks at directly.
pub mod sym {
    pub const BACKSPACE: u32 = 0xff08;
    pub const TAB: u32 = 0xff09;
    pub const RETURN: u32 = 0xff0d;
    pub const ESCAPE: u32 = 0xff1b;
    pub const DELETE: u32 = 0xffff;
    pub const HOME: u32 = 0xff50;
    pub const LEFT: u32 = 0xff51;
    pub const UP: u32 = 0xff52;
    pub const RIGHT: u32 = 0xff53;
    pub const DOWN: u32 = 0xff54;
    pub const END: u32 = 0xff57;
    pub const KP_ENTER: u32 = 0xff8d;
    pub const SPACE: u32 = 0x20;
    pub const ISO_LEFT_TAB: u32 = 0xfe20;
}

/// Special keysyms and the Qt keys they are (Qt's own table, abridged).
const KEYSYMS: &[(u32, i32)] = &[
    (0xff1b, key::ESCAPE),
    (0xff09, key::TAB),
    (0xfe20, key::BACKTAB),
    (0xff08, key::BACKSPACE),
    (0xff0d, key::RETURN),
    (0xff8d, key::ENTER),
    (0xff63, key::INSERT),
    (0xffff, key::DELETE),
    (0xff0b, key::DELETE), // Clear
    (0xff13, key::PAUSE),
    (0xff61, key::PRINT),
    (0xff15, key::SYSREQ),
    (0xff50, key::HOME),
    (0xff57, key::END),
    (0xff51, key::LEFT),
    (0xff52, key::UP),
    (0xff53, key::RIGHT),
    (0xff54, key::DOWN),
    (0xff55, key::PAGE_UP),
    (0xff56, key::PAGE_DOWN),
    (0xffe1, key::SHIFT),
    (0xffe2, key::SHIFT),
    (0xffe6, key::SHIFT), // Shift_Lock
    (0xffe3, key::CONTROL),
    (0xffe4, key::CONTROL),
    (0xffe7, key::META),
    (0xffe8, key::META),
    (0xffe9, key::ALT),
    (0xffea, key::ALT),
    (0xffe5, key::CAPS_LOCK),
    (0xff7f, key::NUM_LOCK),
    (0xff14, key::SCROLL_LOCK),
    (0xffeb, key::SUPER_L),
    (0xffec, key::SUPER_R),
    (0xff67, key::MENU),
    (0xffed, key::HYPER_L),
    (0xffee, key::HYPER_R),
    (0xff6a, key::HELP),
    (0xfe03, key::ALT_GR),
    (0xff20, key::MULTI_KEY),
    // Keypad
    (0xff80, key::SPACE),
    (0xff89, key::TAB),
    (0xff95, key::HOME),
    (0xff96, key::LEFT),
    (0xff97, key::UP),
    (0xff98, key::RIGHT),
    (0xff99, key::DOWN),
    (0xff9a, key::PAGE_UP),
    (0xff9b, key::PAGE_DOWN),
    (0xff9c, key::END),
    (0xff9d, key::CLEAR), // KP_Begin
    (0xff9e, key::INSERT),
    (0xff9f, key::DELETE),
    (0xffbd, key::EQUAL),
    (0xffaa, 0x2a), // KP_Multiply
    (0xffab, key::PLUS),
    (0xffac, 0x2c), // KP_Separator
    (0xffad, key::MINUS),
    (0xffae, 0x2e), // KP_Decimal
    (0xffaf, 0x2f), // KP_Divide
    // Media and launch keys
    (0x1008ff11, 0x0100_0070),
    (0x1008ff12, 0x0100_0071),
    (0x1008ff13, 0x0100_0072),
    (0x1008ff14, 0x0100_0080),
    (0x1008ff15, 0x0100_0081),
    (0x1008ff16, 0x0100_0082),
    (0x1008ff17, 0x0100_0083),
    (0x1008ff18, 0x0100_0090),
    (0x1008ff19, 0x0100_00a0),
    (0x1008ff1b, 0x0100_0092),
    (0x1008ff1c, 0x0100_0084),
    (0x1008ff1d, 0x0100_00cb),
    (0x1008ff26, 0x0100_0061),
    (0x1008ff27, 0x0100_0062),
    (0x1008ff28, 0x0100_0063),
    (0x1008ff29, 0x0100_0064),
    (0x1008ff31, 0x0100_0085),
    (0x1008ff02, 0x0100_00b2),
    (0x1008ff03, 0x0100_00b3),
    (0x1008ff8b, 0x0100_00f6),
    (0x1008ff8c, 0x0100_00f7),
    (0x1008ffb2, 0x0100_0113),
];

/// Is this keysym on the keypad (Qt adds the keypad modifier for those)?
pub fn is_keypad(keysym: u32) -> bool {
    (0xff80..=0xffbd).contains(&keysym)
}

/// The Qt key for an XKB keysym, as Qt's xkbcommon support computes it.
/// `unicode` is the character the keysym stands for, when it has one.
pub fn keysym_to_qt(keysym: u32, unicode: Option<char>) -> i32 {
    if (0xffbe..=0xffe0).contains(&keysym) {
        return key::F1 + (keysym - 0xffbe) as i32;
    }
    if (0xffb0..=0xffb9).contains(&keysym) {
        return 0x30 + (keysym - 0xffb0) as i32;
    }
    if (0x20..=0xff).contains(&keysym) {
        // Latin-1: the upper-case version where that is in Latin-1 too.
        let c = char::from_u32(keysym).unwrap_or(' ');
        let up = c.to_uppercase().next().unwrap_or(c) as u32;
        return if up <= 0xff { up as i32 } else { keysym as i32 };
    }
    if let Some((_, k)) = KEYSYMS.iter().find(|(s, _)| *s == keysym) {
        return *k;
    }
    let c = unicode.or_else(|| if keysym & 0xff00_0000 == 0x0100_0000 { char::from_u32(keysym & 0x00ff_ffff) } else { None });
    match c {
        Some(c) if c.is_ascii_digit() => c as i32,
        Some(c) if !c.is_control() => c.to_uppercase().next().unwrap_or(c) as i32,
        _ => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_codes() {
        assert_eq!(parse_one("Ctrl+Print"), Some(CTRL | key::PRINT));
        assert_eq!(parse_one("Ctrl+Print"), Some(0x0400_0000 | 0x0100_0009));
        assert_eq!(parse_one("Meta+Shift+X"), Some(META | SHIFT | 0x58));
        assert_eq!(parse_one("ctrl+shift+z"), Some(CTRL | SHIFT | key::Z));
        assert_eq!(parse_one("Ctrl++"), Some(CTRL | key::PLUS));
        assert_eq!(parse_one("]"), Some(0x5d));
        assert_eq!(parse_one("8"), Some(0x38));
        assert_eq!(parse_one("F12"), Some(key::F1 + 11));
        assert_eq!(parse_one("Ctrl+Bogus"), None);
    }

    #[test]
    fn text_round_trip_in_qt_order() {
        assert_eq!(to_text(SHIFT | CTRL | META | key::X), "Meta+Ctrl+Shift+X");
        assert_eq!(to_text(CTRL | ALT | key::PRINT), "Ctrl+Alt+Print");
        for s in ["Ctrl+Print", "Ctrl+Alt+Print", "Ctrl+Shift+Print", "Meta+Shift+X", "Ctrl+=", "Ctrl+-", "Ctrl+0", "[", "Esc"] {
            assert_eq!(to_text(parse_one(s).unwrap()), s);
        }
        assert_eq!(seq_to_text(&parse("Ctrl+X, Ctrl+Y")), "Ctrl+X, Ctrl+Y");
    }

    #[test]
    fn keysyms_like_qt() {
        assert_eq!(keysym_to_qt(0x61, Some('a')), key::A); // a
        assert_eq!(keysym_to_qt(0x41, Some('A')), key::A); // A (shifted)
        assert_eq!(keysym_to_qt(0x21, Some('!')), 0x21);
        assert_eq!(keysym_to_qt(0xff61, None), key::PRINT);
        assert_eq!(keysym_to_qt(0xffbe, None), key::F1);
        assert_eq!(keysym_to_qt(0xffb3, None), 0x33); // KP_3
        assert_eq!(keysym_to_qt(0x6c1, Some('ф')), 'Ф' as i32); // Cyrillic
        assert_eq!(keysym_to_qt(0xff8d, None), key::ENTER);
    }
}
