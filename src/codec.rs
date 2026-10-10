//! Reading and writing picture files. Decoders are pure Rust (a file to
//! annotate can come from anywhere), with Qt's allocation limit: nothing
//! over 256 MiB of pixels is decoded. AVIF, JPEG XL and lossy WebP go
//! through their reference tools when those are installed.

use std::io::{Cursor, Write};
use std::path::Path;

use crate::image::{Image, MAX_SIDE};

/// Qt's default QImageReader allocation limit.
const MAX_BYTES: usize = 256 << 20;
/// Files larger than this aren't read at all.
const MAX_FILE: u64 = 512 << 20;

fn check_size(w: u32, h: u32) -> Result<(), String> {
    if w == 0 || h == 0 || w > MAX_SIDE || h > MAX_SIDE || w as usize * h as usize * 4 > MAX_BYTES {
        return Err(format!("a {w} x {h} picture is too large to open"));
    }
    Ok(())
}

/// The kinds of file recognised by their first bytes.
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Kind {
    Png,
    Jpeg,
    Webp,
    Gif,
    Bmp,
    Avif,
    Jxl,
}

pub fn sniff(head: &[u8]) -> Option<Kind> {
    if head.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some(Kind::Png)
    } else if head.starts_with(&[0xFF, 0xD8, 0xFF]) {
        Some(Kind::Jpeg)
    } else if head.len() >= 12 && &head[0..4] == b"RIFF" && &head[8..12] == b"WEBP" {
        Some(Kind::Webp)
    } else if head.starts_with(b"GIF87a") || head.starts_with(b"GIF89a") {
        Some(Kind::Gif)
    } else if head.starts_with(b"BM") {
        Some(Kind::Bmp)
    } else if head.len() >= 12 && &head[4..8] == b"ftyp" && (&head[8..12] == b"avif" || &head[8..12] == b"avis") {
        Some(Kind::Avif)
    } else if head.starts_with(&[0xFF, 0x0A]) || head.starts_with(b"\0\0\0\x0cJXL \r\n\x87\n") {
        Some(Kind::Jxl)
    } else {
        None
    }
}

/// Open a picture file (any of the kinds above).
pub fn decode_file(path: &Path) -> Result<Image, String> {
    let bytes = crate::util::read_limited(path, MAX_FILE).map_err(|e| format!("cannot read {}: {e}", path.display()))?;
    match sniff(&bytes) {
        Some(Kind::Png) => decode_png(&bytes),
        Some(Kind::Jpeg) => decode_jpeg(&bytes),
        Some(Kind::Webp) => decode_webp(&bytes),
        Some(Kind::Gif) => decode_gif(&bytes),
        Some(Kind::Bmp) => decode_bmp(&bytes),
        Some(Kind::Avif) => decode_with_tool(path, &[("avifdec", &[])]),
        Some(Kind::Jxl) => decode_with_tool(path, &[("djxl", &[])]),
        None => Err(format!("{} is not a picture Flatshot RS can open", path.display())),
    }
}

pub fn decode_png(bytes: &[u8]) -> Result<Image, String> {
    let mut dec = png::Decoder::new_with_limits(Cursor::new(bytes), png::Limits { bytes: MAX_BYTES });
    dec.set_transformations(png::Transformations::EXPAND | png::Transformations::STRIP_16);
    let mut reader = dec.read_info().map_err(|e| format!("PNG: {e}"))?;
    let (w, h) = (reader.info().width, reader.info().height);
    check_size(w, h)?;
    let size = reader.output_buffer_size().ok_or("PNG: too large")?;
    let mut buf = vec![0; size];
    let info = reader.next_frame(&mut buf).map_err(|e| format!("PNG: {e}"))?;
    let channels = match info.color_type {
        png::ColorType::Grayscale => 1,
        png::ColorType::GrayscaleAlpha => 2,
        png::ColorType::Rgb => 3,
        png::ColorType::Rgba => 4,
        png::ColorType::Indexed => return Err("PNG: palette not expanded".into()),
    };
    from_channels(w, h, &buf, channels, info.line_size)
}

/// Pixels with 1-4 channels (grey, grey+alpha, RGB, RGBA) to a picture.
fn from_channels(w: u32, h: u32, buf: &[u8], channels: usize, line: usize) -> Result<Image, String> {
    let mut img = Image::new_populated(w, h)?;
    for y in 0..h {
        let src = &buf[y as usize * line..y as usize * line + w as usize * channels];
        let dst = img.row_mut(y);
        for (d, s) in dst.as_chunks_mut::<4>().0.iter_mut().zip(src.chunks_exact(channels)) {
            let (r, g, b, a) = match channels {
                1 => (s[0], s[0], s[0], 255),
                2 => (s[0], s[0], s[0], s[1]),
                3 => (s[0], s[1], s[2], 255),
                _ => (s[0], s[1], s[2], s[3]),
            };
            if a == 255 {
                d.copy_from_slice(&[b, g, r, 255]);
            } else {
                let m = |c: u8| ((c as u32 * a as u32 + 127) / 255) as u8;
                d.copy_from_slice(&[m(b), m(g), m(r), a]);
            }
        }
    }
    Ok(img)
}

pub fn decode_jpeg(bytes: &[u8]) -> Result<Image, String> {
    use zune_core::colorspace::ColorSpace;
    use zune_core::options::DecoderOptions;
    let opts = DecoderOptions::default().jpeg_set_out_colorspace(ColorSpace::RGB).set_max_width(MAX_SIDE as usize).set_max_height(MAX_SIDE as usize);
    let mut dec = zune_jpeg::JpegDecoder::new_with_options(zune_core::bytestream::ZCursor::new(bytes), opts);
    dec.decode_headers().map_err(|e| format!("JPEG: {e:?}"))?;
    let info = dec.info().ok_or("JPEG: no size")?;
    let (w, h) = (info.width as u32, info.height as u32);
    check_size(w, h)?;
    let px = dec.decode().map_err(|e| format!("JPEG: {e:?}"))?;
    let channels = dec.output_colorspace().map(|c| c.num_components()).unwrap_or(3);
    if px.len() < w as usize * h as usize * channels {
        return Err("JPEG: short data".into());
    }
    from_channels(w, h, &px, channels, w as usize * channels)
}

pub fn decode_webp(bytes: &[u8]) -> Result<Image, String> {
    let mut dec = image_webp::WebPDecoder::new(Cursor::new(bytes)).map_err(|e| format!("WebP: {e}"))?;
    dec.set_memory_limit(MAX_BYTES);
    let (w, h) = dec.dimensions();
    check_size(w, h)?;
    let channels = if dec.has_alpha() { 4 } else { 3 };
    let mut buf = vec![0; dec.output_buffer_size().ok_or("WebP: too large")?];
    dec.read_image(&mut buf).map_err(|e| format!("WebP: {e}"))?;
    from_channels(w, h, &buf, channels, w as usize * channels)
}

pub fn decode_gif(bytes: &[u8]) -> Result<Image, String> {
    let mut opts = gif::DecodeOptions::new();
    opts.set_color_output(gif::ColorOutput::RGBA);
    opts.set_memory_limit(gif::MemoryLimit::Bytes(std::num::NonZeroU64::new(MAX_BYTES as u64).unwrap()));
    let mut dec = opts.read_info(Cursor::new(bytes)).map_err(|e| format!("GIF: {e}"))?;
    let (w, h) = (dec.width() as u32, dec.height() as u32);
    check_size(w, h)?;
    let mut canvas = vec![0u8; w as usize * h as usize * 4];
    // The first frame, placed on the logical screen.
    if let Some(frame) = dec.read_next_frame().map_err(|e| format!("GIF: {e}"))? {
        let (fx, fy, fw, fh) = (frame.left as usize, frame.top as usize, frame.width as usize, frame.height as usize);
        for row in 0..fh {
            let y = fy + row;
            if y >= h as usize {
                break;
            }
            for col in 0..fw {
                let x = fx + col;
                if x >= w as usize {
                    break;
                }
                let s = (row * fw + col) * 4;
                let d = (y * w as usize + x) * 4;
                if s + 4 <= frame.buffer.len() {
                    canvas[d..d + 4].copy_from_slice(&frame.buffer[s..s + 4]);
                }
            }
        }
    }
    Image::from_rgba(w, h, &canvas)
}

/// Uncompressed 24- and 32-bit BMP files.
pub fn decode_bmp(bytes: &[u8]) -> Result<Image, String> {
    let u32le = |o: usize| -> Option<u32> { Some(u32::from_le_bytes(bytes.get(o..o + 4)?.try_into().ok()?)) };
    let u16le = |o: usize| -> Option<u16> { Some(u16::from_le_bytes(bytes.get(o..o + 2)?.try_into().ok()?)) };
    let bad = || "BMP: unsupported or damaged file".to_string();
    let offset = u32le(10).ok_or_else(bad)? as usize;
    let w = u32le(18).ok_or_else(bad)? as i32;
    let hraw = u32le(22).ok_or_else(bad)? as i32;
    let bpp = u16le(28).ok_or_else(bad)?;
    let compression = u32le(30).ok_or_else(bad)?;
    if w <= 0 || hraw == 0 || !(bpp == 24 || bpp == 32) || !(compression == 0 || compression == 3) {
        return Err(bad());
    }
    let (h, bottom_up) = (hraw.unsigned_abs(), hraw > 0);
    check_size(w as u32, h)?;
    let bytes_pp = bpp as usize / 8;
    let stride = (w as usize * bytes_pp).div_ceil(4) * 4;
    if bytes.len() < offset + stride * h as usize {
        return Err(bad());
    }
    let mut img = Image::new_populated(w as u32, h)?;
    for y in 0..h {
        let sy = if bottom_up { h - 1 - y } else { y };
        let src = &bytes[offset + sy as usize * stride..];
        let dst = img.row_mut(y);
        for x in 0..w as usize {
            let s = &src[x * bytes_pp..x * bytes_pp + bytes_pp];
            dst[x * 4..x * 4 + 4].copy_from_slice(&[s[0], s[1], s[2], 255]);
        }
    }
    Ok(img)
}

/// Decode with an external tool into a PNG in our private folder.
fn decode_with_tool(path: &Path, tools: &[(&str, &[&str])]) -> Result<Image, String> {
    for (tool, extra) in tools {
        let Some(mut cmd) = crate::util::command(tool) else { continue };
        let tmp = private_temp("png")?;
        let status = cmd
            .args(*extra)
            .arg(path)
            .arg(&tmp)
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status();
        let result = match status {
            Ok(s) if s.success() => crate::util::read_limited(&tmp, MAX_FILE).map_err(|e| e.to_string()).and_then(|b| decode_png(&b)),
            Ok(s) => Err(format!("{tool} failed ({s})")),
            Err(e) => Err(format!("{tool}: {e}")),
        };
        let _ = std::fs::remove_file(&tmp);
        return result;
    }
    Err(format!("opening {} needs {}", path.display(), tools.iter().map(|t| t.0).collect::<Vec<_>>().join(" or ")))
}

/// A new file name in our private runtime folder.
pub fn private_temp(ext: &str) -> Result<std::path::PathBuf, String> {
    let dir = crate::ipc::runtime_dir().map_err(|e| e.to_string())?;
    Ok(dir.join(format!("tmp-{}.{ext}", crate::util::random_hex(8))))
}

// -- writing --------------------------------------------------------------------

/// PNG, fast to write and compact for screen content. `alpha` keeps the
/// alpha channel (else RGB).
pub fn encode_png(img: &Image, alpha: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::with_capacity(img.byte_len() / 4);
    {
        let mut enc = png::Encoder::new(&mut out, img.width(), img.height());
        enc.set_color(if alpha { png::ColorType::Rgba } else { png::ColorType::Rgb });
        enc.set_depth(png::BitDepth::Eight);
        enc.set_compression(png::Compression::Fast);
        let mut writer = enc.write_header().map_err(|e| e.to_string())?;
        let mut stream = writer.stream_writer_with_size(1 << 16).map_err(|e| e.to_string())?;
        let mut row = Vec::with_capacity(img.width() as usize * 4);
        for y in 0..img.height() {
            img.row_rgb(y, alpha, &mut row);
            stream.write_all(&row).map_err(|e| e.to_string())?;
        }
        stream.finish().map_err(|e| e.to_string())?;
    }
    Ok(out)
}

pub fn encode_jpeg(img: &Image, quality: u8) -> Result<Vec<u8>, String> {
    if img.width() > 65535 || img.height() > 65535 {
        return Err("too large for JPEG".into());
    }
    let mut out = Vec::new();
    let enc = jpeg_encoder::Encoder::new(&mut out, quality.clamp(1, 100));
    // Opaque pictures are BGRA with alpha 255: the encoder drops it.
    enc.encode(img.data(), img.width() as u16, img.height() as u16, jpeg_encoder::ColorType::Bgra).map_err(|e| e.to_string())?;
    Ok(out)
}

pub fn encode_webp_lossless(img: &Image, alpha: bool) -> Result<Vec<u8>, String> {
    let mut out = Vec::new();
    let enc = image_webp::WebPEncoder::new(&mut out);
    if alpha {
        enc.encode(&img.to_rgba(), img.width(), img.height(), image_webp::ColorType::Rgba8)
    } else {
        let mut rgb = Vec::with_capacity(img.width() as usize * img.height() as usize * 3);
        let mut row = Vec::new();
        for y in 0..img.height() {
            img.row_rgb(y, false, &mut row);
            rgb.extend_from_slice(&row);
        }
        enc.encode(&rgb, img.width(), img.height(), image_webp::ColorType::Rgb8)
    }
    .map_err(|e| e.to_string())?;
    Ok(out)
}

/// (config value, label, lossy) for each format Flatshot can write here.
pub fn writable_formats() -> Vec<(&'static str, &'static str, bool)> {
    // (WebP is lossy, with a quality, through cwebp; lossless without it)
    let mut out = vec![("png", "PNG", false), ("jpg", "JPEG", true), ("webp", "WebP", crate::util::have("cwebp"))];
    if crate::util::have("avifenc") {
        out.push(("avif", "AVIF", true));
    }
    if crate::util::have("cjxl") {
        out.push(("jxl", "JPEG XL", true));
    }
    out
}

/// Encode `img` as `format` (png | jpg | webp | avif | jxl).
pub fn encode(img: &Image, format: &str, quality: u8) -> Result<Vec<u8>, String> {
    let alpha = img.has_alpha();
    match format {
        "png" => encode_png(img, alpha),
        "jpg" | "jpeg" => encode_jpeg(img, quality),
        "webp" => {
            if quality < 100 && crate::util::have("cwebp") {
                encode_with_tool(img, "webp", "cwebp", &["-quiet", "-q", &quality.to_string()], true)
            } else {
                encode_webp_lossless(img, alpha)
            }
        }
        "avif" => encode_with_tool(img, "avif", "avifenc", &["-q", &quality.to_string()], false),
        "jxl" => encode_with_tool(img, "jxl", "cjxl", &["-q", &quality.to_string(), "--quiet"], false),
        other => Err(format!("can't write {other}")),
    }
}

/// Write a PNG to a private temp file, run `tool [args] in out`, read out.
fn encode_with_tool(img: &Image, ext: &str, tool: &str, args: &[&str], out_flag: bool) -> Result<Vec<u8>, String> {
    let mut cmd = crate::util::command(tool).ok_or_else(|| format!("{tool} is not installed"))?;
    let input = private_temp("png")?;
    let output = private_temp(ext)?;
    let png = encode_png(img, img.has_alpha())?;
    let cleanup = || {
        let _ = std::fs::remove_file(&input);
        let _ = std::fs::remove_file(&output);
    };
    let write = || -> std::io::Result<()> {
        use std::os::unix::fs::OpenOptionsExt;
        let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&input)?;
        f.write_all(&png)
    };
    if let Err(e) = write() {
        cleanup();
        return Err(e.to_string());
    }
    cmd.args(args).arg(&input);
    if out_flag {
        cmd.arg("-o");
    }
    cmd.arg(&output).stdin(std::process::Stdio::null()).stdout(std::process::Stdio::null()).stderr(std::process::Stdio::piped());
    let result = match cmd.output() {
        Ok(o) if o.status.success() => crate::util::read_limited(&output, MAX_FILE).map_err(|e| e.to_string()),
        Ok(o) => Err(format!("{tool}: {}", String::from_utf8_lossy(&o.stderr).lines().last().unwrap_or("failed"))),
        Err(e) => Err(format!("{tool}: {e}")),
    };
    cleanup();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_round_trip() {
        let mut img = Image::filled(37, 21, 0xFF102030).unwrap();
        img.set_pixel(5, 6, 0xFFFF8000);
        let bytes = encode_png(&img, false).unwrap();
        assert_eq!(sniff(&bytes), Some(Kind::Png));
        let back = decode_png(&bytes).unwrap();
        assert_eq!(back.size(), (37, 21));
        assert_eq!(back.pixel(5, 6), 0xFFFF8000);
        assert_eq!(back.pixel(0, 0), 0xFF102030);
    }

    #[test]
    fn jpeg_and_webp() {
        let img = Image::filled(64, 48, 0xFF336699).unwrap();
        let j = encode_jpeg(&img, 90).unwrap();
        assert_eq!(sniff(&j), Some(Kind::Jpeg));
        let back = decode_jpeg(&j).unwrap();
        assert_eq!(back.size(), (64, 48));
        let (r, g, b, _) = crate::image::unpack(back.pixel(10, 10));
        assert!((r as i32 - 0x33).abs() < 6 && (g as i32 - 0x66).abs() < 6 && (b as i32 - 0x99).abs() < 6);
        let w = encode_webp_lossless(&img, false).unwrap();
        assert_eq!(sniff(&w), Some(Kind::Webp));
        assert_eq!(decode_webp(&w).unwrap().pixel(3, 3), 0xFF336699);
    }

    #[test]
    fn fixtures_decode() {
        let qr = std::fs::read(concat!(env!("CARGO_MANIFEST_DIR"), "/assets/selftest-qr.png")).unwrap();
        let img = decode_png(&qr).unwrap();
        assert!(img.width() > 50);
    }

    #[test]
    fn refuses_garbage() {
        assert!(decode_png(b"\x89PNG\r\n\x1a\nnope").is_err());
        assert!(decode_bmp(b"BMxxxx").is_err());
        assert_eq!(sniff(b"hello"), None);
    }
}
