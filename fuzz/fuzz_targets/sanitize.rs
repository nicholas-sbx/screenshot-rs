//! Text from other programs made safe: for a file name (no slashes, no
//! control or direction characters, not hidden, not too long), to print
//! or show (no control or direction characters), as notification markup
//! (no tags), and Tesseract's text tidied (once is enough).
#![no_main]

use libfuzzer_sys::fuzz_target;

fn direction(c: char) -> bool {
    matches!(c, '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}' | '\u{200E}' | '\u{200F}' | '\u{061C}')
}

fuzz_target!(|text: &str| {
    let name = flatshot::template::clean(text);
    assert!(!name.contains(['/', '\\']) && !name.chars().any(|c| c.is_control() || direction(c)), "{name:?}");
    assert!(!name.starts_with(['.', ' ', '_']) && !name.ends_with(['.', ' ', '_']), "{name:?}");
    assert!(name.chars().count() <= 80);
    assert_eq!(flatshot::template::clean(&name), name, "cleaned twice");

    let shown = flatshot::util::displayable(text);
    assert!(!shown.chars().any(|c| c.is_control() || direction(c)), "{shown:?}");
    assert_eq!(shown.chars().count(), text.chars().count());

    let markup = flatshot::desktop::notify::escape_markup(text);
    assert!(!markup.contains(['<', '>', '"', '\'']), "{markup:?}");

    let tidy = flatshot::ocr::tidy(text);
    assert_eq!(flatshot::ocr::tidy(&tidy), tidy, "tidied twice");
    assert!(!tidy.starts_with('\n') && !tidy.ends_with('\n') && !tidy.contains("\n\n\n"), "{tidy:?}");
});
