//! Pixelate and blur over any part of a picture, at any scale a screen
//! has: a cover exactly the size of the part that's on the picture (none
//! when nothing is), and never a crash.
#![no_main]

use arbitrary::Arbitrary;
use flatshot::geom::IRect;
use flatshot::image::Image;
use flatshot::ui::shapes;
use libfuzzer_sys::fuzz_target;

#[derive(Arbitrary, Debug)]
struct Input {
    size: (u8, u8),
    pixels: Vec<u32>,
    rect: (i16, i16, i16, i16),
    scale: u8,
    stroke: u8,
    seed: Option<u64>,
}

fuzz_target!(|input: Input| {
    let (w, h) = (input.size.0 as u32 % 96 + 1, input.size.1 as u32 % 96 + 1);
    let Ok(mut base) = Image::new(w, h) else { return };
    if !input.pixels.is_empty() {
        for y in 0..h {
            for x in 0..w {
                base.set_pixel(x, y, input.pixels[((y * w + x) as usize) % input.pixels.len()] | 0xFF00_0000);
            }
        }
    }
    let r = IRect::new(input.rect.0 as i32, input.rect.1 as i32, input.rect.2 as i32, input.rect.3 as i32);
    let dpr = 0.25 + input.scale as f32 / 32.0;
    let size = input.stroke as usize % 4;
    let on = r.intersected(&base.rect());
    for cover in [shapes::pixelate(&base, r, dpr, size, input.seed), shapes::blur(&base, r, dpr, size, input.seed)] {
        match cover {
            Some(c) => assert_eq!(c.size(), (on.w as u32, on.h as u32), "{r:?} on {w} x {h}"),
            None => assert!(on.is_empty(), "{r:?} on {w} x {h}: no cover"),
        }
    }
});
