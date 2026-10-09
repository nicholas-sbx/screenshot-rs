//! Pictures in memory: 32-bit premultiplied BGRA, rows packed (the layout of
//! Wayland's ARGB8888 and Qt's ARGB32_Premultiplied on little-endian
//! machines), so pixels from KWin or the compositor need no conversion.
//!
//! Large pictures live in their own memory mapping (see `mem`), so the
//! memory goes back to the system as soon as a capture is done.

use crate::geom::IRect;
use crate::mem::Mapping;

/// Below this many bytes a picture lives on the heap.
const MAP_ABOVE: usize = 256 * 1024;
/// The largest picture accepted: 16384 x 16384.
pub const MAX_SIDE: u32 = 16384;

enum Pixels {
    Heap(Vec<u8>),
    Map(Mapping),
}

pub struct Image {
    width: u32,
    height: u32,
    pixels: Pixels,
}

impl std::fmt::Debug for Image {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Image({}x{})", self.width, self.height)
    }
}

impl Clone for Image {
    fn clone(&self) -> Self {
        let mut out = Image::new_uninit(self.width, self.height).expect("clone of a valid picture");
        out.data_mut().copy_from_slice(self.data());
        out
    }
}

fn check(w: u32, h: u32) -> Result<usize, String> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE {
        return Err(format!("a {w} x {h} picture is out of range"));
    }
    Ok(w as usize * h as usize * 4)
}

#[inline]
pub fn pack(b: u8, g: u8, r: u8, a: u8) -> u32 {
    u32::from_le_bytes([b, g, r, a])
}

/// (r, g, b, a) of a packed 0xAARRGGBB value.
#[inline]
pub fn unpack(p: u32) -> (u8, u8, u8, u8) {
    let [b, g, r, a] = p.to_le_bytes();
    (r, g, b, a)
}

impl Image {
    /// A transparent picture.
    pub fn new(width: u32, height: u32) -> Result<Image, String> {
        // (fresh mappings and calloc'd memory are zero already)
        let len = check(width, height)?;
        let pixels = if len >= MAP_ABOVE {
            Pixels::Map(Mapping::anonymous(len, false).map_err(|e| e.to_string())?)
        } else {
            Pixels::Heap(vec![0; len])
        };
        Ok(Image { width, height, pixels })
    }

    /// A picture whose contents are about to be overwritten in full; its
    /// memory is faulted in now when `populate`.
    pub fn new_populated(width: u32, height: u32) -> Result<Image, String> {
        let len = check(width, height)?;
        let pixels = if len >= MAP_ABOVE {
            Pixels::Map(Mapping::anonymous(len, true).map_err(|e| e.to_string())?)
        } else {
            Pixels::Heap(vec![0; len])
        };
        Ok(Image { width, height, pixels })
    }

    fn new_uninit(width: u32, height: u32) -> Result<Image, String> {
        Image::new(width, height)
    }

    pub fn filled(width: u32, height: u32, argb: u32) -> Result<Image, String> {
        let mut img = Image::new(width, height)?;
        img.fill(argb);
        Ok(img)
    }

    /// From packed BGRA bytes (`width * height * 4` of them).
    pub fn from_bgra(width: u32, height: u32, data: Vec<u8>) -> Result<Image, String> {
        let len = check(width, height)?;
        if data.len() != len {
            return Err(format!("{} bytes for a {width} x {height} picture", data.len()));
        }
        if len >= MAP_ABOVE {
            let mut img = Image::new(width, height)?;
            img.data_mut().copy_from_slice(&data);
            return Ok(img);
        }
        Ok(Image { width, height, pixels: Pixels::Heap(data) })
    }

    pub fn width(&self) -> u32 {
        self.width
    }
    pub fn height(&self) -> u32 {
        self.height
    }
    pub fn size(&self) -> (u32, u32) {
        (self.width, self.height)
    }
    pub fn stride(&self) -> usize {
        self.width as usize * 4
    }
    pub fn rect(&self) -> IRect {
        IRect::new(0, 0, self.width as i32, self.height as i32)
    }
    pub fn byte_len(&self) -> usize {
        self.width as usize * self.height as usize * 4
    }

    pub fn data(&self) -> &[u8] {
        match &self.pixels {
            Pixels::Heap(v) => v,
            Pixels::Map(m) => &m.as_slice()[..self.byte_len()],
        }
    }

    pub fn data_mut(&mut self) -> &mut [u8] {
        let len = self.byte_len();
        match &mut self.pixels {
            Pixels::Heap(v) => v,
            Pixels::Map(m) => &mut m.as_mut_slice()[..len],
        }
    }

    pub fn row(&self, y: u32) -> &[u8] {
        let s = self.stride();
        &self.data()[y as usize * s..(y as usize + 1) * s]
    }

    pub fn row_mut(&mut self, y: u32) -> &mut [u8] {
        let s = self.stride();
        &mut self.data_mut()[y as usize * s..(y as usize + 1) * s]
    }

    /// The pixel at (x, y) as 0xAARRGGBB (premultiplied).
    pub fn pixel(&self, x: u32, y: u32) -> u32 {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        let d = self.data();
        u32::from_le_bytes([d[i], d[i + 1], d[i + 2], d[i + 3]])
    }

    /// The pixel nearest to (x, y), for coordinates that may be outside.
    pub fn pixel_clamped(&self, x: i64, y: i64) -> u32 {
        let x = x.clamp(0, self.width as i64 - 1) as u32;
        let y = y.clamp(0, self.height as i64 - 1) as u32;
        self.pixel(x, y)
    }

    pub fn set_pixel(&mut self, x: u32, y: u32, argb: u32) {
        let i = (y as usize * self.width as usize + x as usize) * 4;
        self.data_mut()[i..i + 4].copy_from_slice(&argb.to_le_bytes());
    }

    pub fn fill(&mut self, argb: u32) {
        let px = argb.to_le_bytes();
        if px == [0, 0, 0, 0] {
            self.data_mut().fill(0);
            return;
        }
        for chunk in self.data_mut().chunks_exact_mut(4) {
            chunk.copy_from_slice(&px);
        }
    }

    /// A copy of the part in `r` (clipped to the picture).
    pub fn crop(&self, r: IRect) -> Result<Image, String> {
        let r = r.intersected(&self.rect());
        if r.is_empty() {
            return Err("nothing of the picture is in that area".into());
        }
        let mut out = Image::new_populated(r.w as u32, r.h as u32)?;
        let (sx, w) = (r.x as usize * 4, r.w as usize * 4);
        for y in 0..r.h as u32 {
            let src = &self.row((r.y as u32) + y)[sx..sx + w];
            out.row_mut(y).copy_from_slice(src);
        }
        Ok(out)
    }

    /// Copy `src` into this picture with its top left at (x, y), clipped.
    pub fn blit(&mut self, src: &Image, x: i32, y: i32) {
        let dst = IRect::new(x, y, src.width as i32, src.height as i32).intersected(&self.rect());
        if dst.is_empty() {
            return;
        }
        let (sx, sy) = ((dst.x - x) as usize, (dst.y - y) as u32);
        let w = dst.w as usize * 4;
        for row in 0..dst.h as u32 {
            let s = &src.row(sy + row)[sx * 4..sx * 4 + w];
            let d = &mut self.row_mut(dst.y as u32 + row)[dst.x as usize * 4..dst.x as usize * 4 + w];
            d.copy_from_slice(s);
        }
    }

    /// Does any pixel let something through?
    pub fn has_alpha(&self) -> bool {
        self.data().chunks_exact(4).any(|p| p[3] != 255)
    }

    /// Make every pixel opaque (a screenshot is: whatever its alpha says).
    pub fn force_opaque(&mut self) {
        for p in self.data_mut().chunks_exact_mut(4) {
            p[3] = 255;
        }
    }

    /// Straight (not premultiplied) RGBA bytes, for encoders.
    pub fn to_rgba(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.byte_len());
        for p in self.data().chunks_exact(4) {
            let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
            if a == 255 || a == 0 {
                out.extend_from_slice(&[r, g, b, a]);
            } else {
                let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                out.extend_from_slice(&[un(r), un(g), un(b), a]);
            }
        }
        out
    }

    /// One row as straight RGB(A) for an encoder, into `out`.
    pub fn row_rgb(&self, y: u32, alpha: bool, out: &mut Vec<u8>) {
        out.clear();
        for p in self.row(y).chunks_exact(4) {
            let (b, g, r, a) = (p[0], p[1], p[2], p[3]);
            if alpha {
                if a == 255 || a == 0 {
                    out.extend_from_slice(&[r, g, b, a]);
                } else {
                    let un = |c: u8| ((c as u32 * 255 + a as u32 / 2) / a as u32).min(255) as u8;
                    out.extend_from_slice(&[un(r), un(g), un(b), a]);
                }
            } else {
                out.extend_from_slice(&[r, g, b]);
            }
        }
    }

    /// From straight RGBA bytes.
    pub fn from_rgba(width: u32, height: u32, rgba: &[u8]) -> Result<Image, String> {
        let mut img = Image::new_populated(width, height)?;
        if rgba.len() < img.byte_len() {
            return Err("short pixel data".into());
        }
        for (d, s) in img.data_mut().chunks_exact_mut(4).zip(rgba.chunks_exact(4)) {
            let a = s[3] as u32;
            let pm = |c: u8| ((c as u32 * a + 127) / 255) as u8;
            if a == 255 {
                d.copy_from_slice(&[s[2], s[1], s[0], 255]);
            } else {
                d.copy_from_slice(&[pm(s[2]), pm(s[1]), pm(s[0]), a as u8]);
            }
        }
        Ok(img)
    }

    /// Grey levels (as Qt's qGray: (11 r + 16 g + 5 b) / 32), one byte a
    /// pixel, for looking for codes and edges.
    pub fn to_luma(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.width as usize * self.height as usize);
        for p in self.data().chunks_exact(4) {
            out.push(((p[2] as u32 * 11 + p[1] as u32 * 16 + p[0] as u32 * 5) >> 5) as u8);
        }
        out
    }

    /// Shrunk (or grown) to `w` x `h`, each output pixel the average of
    /// what it covers: smooth when shrinking (`SmoothTransformation`).
    pub fn scaled_area(&self, w: u32, h: u32) -> Result<Image, String> {
        let (w, h) = (w.max(1), h.max(1));
        if w >= self.width && h >= self.height {
            return self.scaled_bilinear(w, h);
        }
        let mut out = Image::new_populated(w, h)?;
        let sx = self.width as f64 / w as f64;
        let sy = self.height as f64 / h as f64;
        let mut acc = vec![[0f64; 4]; w as usize];
        for oy in 0..h {
            let y0 = oy as f64 * sy;
            let y1 = ((oy + 1) as f64 * sy).min(self.height as f64);
            for a in acc.iter_mut() {
                *a = [0.0; 4];
            }
            let mut y = y0.floor() as u32;
            while (y as f64) < y1 && y < self.height {
                let wy = ((y + 1) as f64).min(y1) - (y as f64).max(y0);
                let row = self.row(y);
                for (ox, a) in acc.iter_mut().enumerate() {
                    let x0 = ox as f64 * sx;
                    let x1 = ((ox + 1) as f64 * sx).min(self.width as f64);
                    let mut x = x0.floor() as u32;
                    while (x as f64) < x1 && x < self.width {
                        let wx = ((x + 1) as f64).min(x1) - (x as f64).max(x0);
                        let k = wx * wy;
                        let p = &row[x as usize * 4..x as usize * 4 + 4];
                        a[0] += p[0] as f64 * k;
                        a[1] += p[1] as f64 * k;
                        a[2] += p[2] as f64 * k;
                        a[3] += p[3] as f64 * k;
                        x += 1;
                    }
                }
                y += 1;
            }
            let area = sx * (y1 - y0);
            let orow = out.row_mut(oy);
            for (ox, a) in acc.iter().enumerate() {
                let x0 = ox as f64 * sx;
                let x1 = ((ox + 1) as f64 * sx).min(self.width as f64);
                let norm = if (x1 - x0) * (y1 - y0) > 0.0 { (x1 - x0) * (y1 - y0) } else { area.max(1e-9) };
                for c in 0..4 {
                    orow[ox * 4 + c] = (a[c] / norm).round().clamp(0.0, 255.0) as u8;
                }
            }
        }
        Ok(out)
    }

    /// Resized with bilinear filtering (pixel centres aligned).
    pub fn scaled_bilinear(&self, w: u32, h: u32) -> Result<Image, String> {
        let (w, h) = (w.max(1), h.max(1));
        let mut out = Image::new_populated(w, h)?;
        let sx = self.width as f32 / w as f32;
        let sy = self.height as f32 / h as f32;
        let maxx = self.width as i64 - 1;
        let maxy = self.height as i64 - 1;
        let xs: Vec<(usize, usize, f32)> = (0..w)
            .map(|ox| {
                let fx = (ox as f32 + 0.5) * sx - 0.5;
                let x0 = fx.floor() as i64;
                let t = fx - x0 as f32;
                (x0.clamp(0, maxx) as usize, (x0 + 1).clamp(0, maxx) as usize, t)
            })
            .collect();
        for oy in 0..h {
            let fy = (oy as f32 + 0.5) * sy - 0.5;
            let y0 = fy.floor() as i64;
            let ty = fy - y0 as f32;
            let r0 = self.row(y0.clamp(0, maxy) as u32);
            let r1 = self.row((y0 + 1).clamp(0, maxy) as u32);
            let orow = out.row_mut(oy);
            for (ox, &(x0, x1, tx)) in xs.iter().enumerate() {
                for c in 0..4 {
                    let a = r0[x0 * 4 + c] as f32 * (1.0 - tx) + r0[x1 * 4 + c] as f32 * tx;
                    let b = r1[x0 * 4 + c] as f32 * (1.0 - tx) + r1[x1 * 4 + c] as f32 * tx;
                    orow[ox * 4 + c] = (a * (1.0 - ty) + b * ty + 0.5) as u8;
                }
            }
        }
        Ok(out)
    }

    /// Fit within `max` x `max`, keeping the shape (for thumbnails).
    pub fn thumbnail(&self, max: u32) -> Result<Image, String> {
        if self.width <= max && self.height <= max {
            return Ok(self.clone());
        }
        let k = max as f64 / self.width.max(self.height) as f64;
        self.scaled_area(((self.width as f64 * k).round() as u32).max(1), ((self.height as f64 * k).round() as u32).max(1))
    }

    /// A tiny-skia view for drawing on this picture. tiny-skia thinks the
    /// bytes are RGBA; they're BGRA, so colours given to it are swapped
    /// (see `render::Paint`). Blending treats red and blue alike, so the
    /// result is right.
    pub fn pixmap_mut(&mut self) -> tiny_skia::PixmapMut<'_> {
        let (w, h) = (self.width, self.height);
        tiny_skia::PixmapMut::from_bytes(self.data_mut(), w, h).expect("picture sizes are valid pixmaps")
    }

    pub fn pixmap_ref(&self) -> tiny_skia::PixmapRef<'_> {
        tiny_skia::PixmapRef::from_bytes(self.data(), self.width, self.height).expect("picture sizes are valid pixmaps")
    }
}

/// The colour of a pixel as #RRGGBB.
pub fn hex(argb: u32) -> String {
    let (r, g, b, _) = unpack(argb);
    format!("#{r:02X}{g:02X}{b:02X}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pixels_and_crop() {
        let mut img = Image::new(10, 8).unwrap();
        img.set_pixel(3, 4, 0xFF112233);
        assert_eq!(img.pixel(3, 4), 0xFF112233);
        assert_eq!(unpack(0xFF112233), (0x11, 0x22, 0x33, 0xFF));
        let c = img.crop(IRect::new(2, 3, 4, 4)).unwrap();
        assert_eq!(c.size(), (4, 4));
        assert_eq!(c.pixel(1, 1), 0xFF112233);
        assert_eq!(hex(0xFF112233), "#112233");
    }

    #[test]
    fn big_pictures_are_mapped() {
        let img = Image::filled(1000, 1000, 0xFF000000).unwrap();
        assert!(matches!(img.pixels, Pixels::Map(_)));
        assert_eq!(img.pixel(999, 999), 0xFF000000);
        assert!(Image::new(0, 1).is_err());
        assert!(Image::new(MAX_SIDE + 1, 1).is_err());
    }

    #[test]
    fn scaling_averages() {
        let mut img = Image::new(4, 2).unwrap();
        for x in 0..4 {
            for y in 0..2 {
                img.set_pixel(x, y, if x < 2 { 0xFF000000 } else { 0xFFFFFFFF });
            }
        }
        let s = img.scaled_area(2, 1).unwrap();
        assert_eq!(s.pixel(0, 0), 0xFF000000);
        assert_eq!(s.pixel(1, 0), 0xFFFFFFFF);
        let s = img.scaled_area(1, 1).unwrap();
        let (r, _, _, a) = unpack(s.pixel(0, 0));
        assert!((127..=128).contains(&r) && a == 255);
        let b = img.scaled_bilinear(8, 4).unwrap();
        assert_eq!(b.size(), (8, 4));
    }

    #[test]
    fn rgba_round_trip() {
        let img = Image::from_rgba(2, 1, &[255, 0, 0, 255, 0, 0, 255, 128]).unwrap();
        assert_eq!(img.pixel(0, 0), 0xFFFF0000);
        let back = img.to_rgba();
        assert_eq!(&back[..4], &[255, 0, 0, 255]);
        assert_eq!(back[6], 255);
        assert_eq!(back[7], 128);
    }
}
