//! The list of recent captures, as found on disk (damaged, or written by
//! another version): whatever it holds, it's read without a crash, as at
//! most the newest 100 entries, each with an absolute path.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|text: &str| {
    let entries = flatshot::history::parse(text);
    assert!(entries.len() <= flatshot::history::MAX);
    for e in &entries {
        assert!(e.path.is_absolute(), "{:?}", e.path);
        assert!(e.thumbnail.as_ref().is_none_or(|t| t.is_absolute()));
        assert!(e.title.chars().count() <= 500 && e.app.chars().count() <= 500);
        let _ = e.is_video();
    }
});
