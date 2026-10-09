//! What the capture backends share: the pictures of each monitor, and the
//! mouse pointer kept as just the pixels it covers.

use std::sync::Arc;

use crate::geom::{IPoint, IRect};
use crate::image::Image;

/// The pixels that differ between the picture without the pointer and the
/// one with it: where (in `plain`'s pixels) and those pixels of `with`.
/// None when they're the same (the pointer is on another monitor).
pub fn pointer_patch(plain: &Image, with: &Image) -> Option<(IPoint, Arc<Image>)> {
    if plain.size() != with.size() {
        return None;
    }
    let (w, h) = plain.size();
    let (mut x0, mut y0, mut x1, mut y1) = (u32::MAX, u32::MAX, 0u32, 0u32);
    for y in 0..h {
        let (a, b) = (plain.row(y), with.row(y));
        if a == b {
            continue;
        }
        // The first and last differing pixels in this row.
        let first = a.chunks_exact(4).zip(b.chunks_exact(4)).position(|(p, q)| p != q);
        let last = a.chunks_exact(4).zip(b.chunks_exact(4)).rposition(|(p, q)| p != q);
        if let (Some(f), Some(l)) = (first, last) {
            x0 = x0.min(f as u32);
            x1 = x1.max(l as u32 + 1);
            y0 = y0.min(y);
            y1 = y1.max(y + 1);
        }
    }
    if x0 >= x1 || y0 >= y1 || x1 > w || y1 > h {
        return None;
    }
    let r = IRect::new(x0 as i32, y0 as i32, (x1 - x0) as i32, (y1 - y0) as i32);
    let patch = with.crop(r).ok()?;
    Some((IPoint::new(r.x, r.y), Arc::new(patch)))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn patch_is_just_the_pointer() {
        let plain = Image::filled(300, 200, 0xFF203040).unwrap();
        let mut with = plain.clone();
        for y in 50..74 {
            for x in 100..116 {
                with.set_pixel(x, y, 0xFFFFFFFF);
            }
        }
        let (at, patch) = pointer_patch(&plain, &with).unwrap();
        assert_eq!((at, patch.size()), (IPoint::new(100, 50), (16, 24)));
        assert!(pointer_patch(&plain, &plain).is_none());
    }
}
