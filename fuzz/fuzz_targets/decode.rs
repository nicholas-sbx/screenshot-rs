//! Picture files from anywhere (a file to annotate, the clipboard): read or
//! refused, never a crash, and never past the size limit.
#![no_main]

use flatshot::image::MAX_SIDE;
use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let _ = flatshot::codec::sniff(data);
    if let Ok(img) = flatshot::codec::decode_bytes(data) {
        let (w, h) = img.size();
        assert!(w > 0 && h > 0 && w <= MAX_SIDE && h <= MAX_SIDE, "{w} x {h}");
        assert!(w as usize * h as usize * 4 <= 256 << 20);
        assert_eq!(img.data().len(), w as usize * h as usize * 4);
    }
});
