//! Folder and file name templates. A window's title and app come from other
//! programs: whatever they are, the file stays in the folder, under a name
//! that isn't hidden. And any template one types in Settings is filled
//! without a crash.
#![no_main]

use std::path::{Component, Path};

use arbitrary::Arbitrary;
use flatshot::template::{Shot, target_path};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    app: String,
    title: String,
    monitor: String,
    mode: String,
    folder: String,
    file: String,
    size: (u16, u16),
}

/// ({n} uses up a number from the state file: not here.)
fn no_counter(template: &str) -> String {
    template.replace("{n", "{N")
}

fuzz_target!(|input: Input| {
    let shot = Shot { mode: input.mode, app: input.app, title: input.title, monitor: input.monitor, codes: 0, drawings: 0 };
    let size = (input.size.0 as u32, input.size.1 as u32);
    for file in ["{app}/{title}", "{title}", "{mode}-{monitor}_%Y", "{app}{title}{mode}{monitor}{w}x{h}"] {
        let path = target_path("/base", file, &shot, size, "png", true);
        let rest = path.strip_prefix("/base").unwrap_or_else(|_| panic!("{path:?} is out of the folder"));
        assert!(rest.components().all(|c| matches!(c, Component::Normal(_))), "{path:?}");
        let name = path.file_name().unwrap().to_string_lossy().into_owned();
        assert!(!name.starts_with('.') && name.ends_with(".png"), "{path:?}");
        assert!(Path::new(&name).components().count() == 1);
    }
    // Templates as typed: anything goes, nothing breaks.
    let _ = target_path(&no_counter(&input.folder), &no_counter(&input.file), &shot, size, "png", true);
    let _ = flatshot::template::base_folder(&no_counter(&input.folder));
});
