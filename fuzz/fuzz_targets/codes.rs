//! QR codes and barcodes are looked for in whatever is on screen, a web
//! page's pictures too: no picture makes the scan crash.
#![no_main]

use libfuzzer_sys::fuzz_target;

fuzz_target!(|data: &[u8]| {
    let [w, h, rest @ ..] = data else { return };
    let (w, h) = (*w as u32 + 1, *h as u32 + 1);
    if rest.is_empty() {
        return;
    }
    // (the rest, over and over, as the picture's brightness)
    let luma: Vec<u8> = rest.iter().copied().cycle().take((w * h) as usize).collect();
    for code in flatshot::scanner::scan_luma(&luma, w, h) {
        assert!(code.corners.iter().all(|p| p.x.is_finite() && p.y.is_finite()));
        let _ = code.is_link();
    }
});
