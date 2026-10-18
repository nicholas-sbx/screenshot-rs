//! Beautify, in any style the settings file can hold: the whole picture
//! is laid out within the size limit, and drawn without a crash.
#![no_main]

use arbitrary::Arbitrary;
use flatshot::beautify::{self, Style};
use flatshot::image::{Image, MAX_SIDE};
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    background: u8,
    color: String,
    gradient: u8,
    padding: i64,
    padding_percent: bool,
    radius: i64,
    shadow: i64,
    shadow_offset: i64,
    shadow_opacity: i64,
    ratio: u8,
    size: (u16, u16),
    picture: (u8, u8, u32),
    scale: u8,
}

fuzz_target!(|input: Input| {
    let pick = |list: &[(&str, &str)], i: u8| list.get(i as usize).map(|x| x.0.to_string()).unwrap_or_else(|| "nonsense".into());
    let backgrounds: Vec<(&str, &str)> = beautify::BACKGROUNDS.iter().map(|b| (b.0, b.1)).filter(|b| b.0 != "image").collect();
    let gradients: Vec<(&str, &str)> = beautify::GRADIENTS.iter().map(|g| (g.0, g.0)).collect();
    let ratios: Vec<(&str, &str)> = beautify::RATIOS.iter().map(|r| (r.0, r.1)).collect();
    let mut style = Style {
        background: pick(&backgrounds, input.background),
        color: input.color,
        gradient: pick(&gradients, input.gradient),
        image: String::new(),
        padding: input.padding,
        padding_percent: input.padding_percent,
        radius: input.radius,
        shadow: input.shadow,
        shadow_offset: input.shadow_offset,
        shadow_opacity: input.shadow_opacity,
        ratio: pick(&ratios, input.ratio),
    };
    style.clean();
    let scale = 0.5 + input.scale as f32 / 64.0;
    let size = ((input.size.0 as u32).clamp(1, MAX_SIDE), (input.size.1 as u32).clamp(1, MAX_SIDE));
    let ((w, h), at) = beautify::layout(size, &style, scale);
    assert!((1..=MAX_SIDE).contains(&w) && (1..=MAX_SIDE).contains(&h), "{w} x {h}");
    let _ = at;
    // Drawn, on a small picture (big ones only take longer).
    let (pw, ph, colour) = input.picture;
    let Ok(img) = Image::filled(pw as u32 % 48 + 1, ph as u32 % 48 + 1, colour | 0xFF00_0000) else { return };
    if style.padding_percent || style.padding <= 64 {
        let out = beautify::render(&img, &style, scale).expect("drawn");
        assert_eq!(out.size(), beautify::layout(img.size(), &style, scale).0);
    }
});
