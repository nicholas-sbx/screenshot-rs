//! The command line, and the commands another flatshot-rs hands the tray
//! app over its socket (`rect:WxH+X+Y 2 pin`): any of them parsed or
//! refused, and an exact area always one with room in it.
#![no_main]

use arbitrary::Arbitrary;
use flatshot::app::Mode;
use flatshot::geom::IRect;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    args: Vec<String>,
    geometry: String,
    command: String,
}

fuzz_target!(|input: Input| {
    let argv: Vec<String> = std::iter::once("flatshot-rs".to_string()).chain(input.args.into_iter().take(12)).collect();
    let _ = flatshot::cli::parse(&argv);
    for text in [input.geometry.as_str(), input.command.split_whitespace().next().unwrap_or("").split_once(':').map_or("", |g| g.1)] {
        let Some(r) = flatshot::cli::parse_geometry(text) else { continue };
        assert!(r.w > 0 && r.h > 0, "{text:?} -> {r:?}");
        // As a capture uses it: on the screens.
        let screens = IRect::new(-1920, 0, 3840, 1080);
        let _ = (r.x2(), r.y2(), r.area(), r.center());
        let on = r.intersected(&screens);
        assert!(on.is_empty() || screens.contains_rect(&on));
        let _ = r.united(&screens);
        assert_eq!(Mode::parse("rect", Some(r)), Some(Mode::Rect(r)));
    }
});
