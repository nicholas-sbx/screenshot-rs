//! Key names, as typed in Settings or found in the settings file: a key's
//! label reads back as the same key, and any set of keys makes a keymap
//! that answers any key press.
#![no_main]

use std::collections::BTreeMap;

use arbitrary::Arbitrary;
use flatshot::keys::{BINDINGS, KeyPress, Keymap, Where};
use flatshot::keyseq;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    text: String,
    changed: Vec<(u8, String)>,
    presses: Vec<(i32, i32, u32)>,
}

fuzz_target!(|input: Input| {
    let seq = keyseq::parse(&input.text);
    let label = keyseq::label(&input.text);
    assert_eq!(keyseq::parse(&label), seq, "{:?} -> {label:?}", input.text);
    assert_eq!(keyseq::label(&label), label);
    let overrides: BTreeMap<String, String> =
        input.changed.iter().take(16).map(|(i, k)| (BINDINGS[*i as usize % BINDINGS.len()].0.to_string(), k.clone())).collect();
    let km = Keymap::new(&overrides);
    for (k, mods, sym) in input.presses.iter().take(16) {
        let press = KeyPress { key: *k, mods: *mods, keysym: *sym, ..Default::default() };
        for place in [Where::Capture, Where::Editor] {
            if let Some(action) = km.action(&press, place) {
                assert!(flatshot::keys::binding(action).is_some());
            }
        }
        let _ = keyseq::keysym_to_qt(*sym, char::from_u32(*sym));
    }
    for (action, ..) in BINDINGS {
        let _ = km.hint("x", action);
    }
});
