//! Taking the screen on X11: the server copies each monitor's pixels
//! straight into Flatshot's memory (MIT-SHM, given a memfd of ours), on a
//! connection of the capture's own; the pointer is drawn in from XFixes'
//! picture of it when it's wanted.

use x11rb::connection::Connection;
use x11rb::protocol::shm::ConnectionExt as _;
use x11rb::protocol::xfixes::ConnectionExt as _;
use x11rb::protocol::xproto::{self, ConnectionExt as _, ImageFormat, ImageOrder};
use x11rb::rust_connection::RustConnection;

use crate::capture::{Grabbed, Plan};
use crate::geom::IRect;
use crate::image::Image;

/// Take `plan`'s monitors. Blocks: run it off the event loop.
pub fn grab(plan: &Plan) -> Result<Vec<Grabbed>, String> {
    let (conn, screen) = x11rb::connect(None).map_err(|e| format!("can't reach the X server: {e}"))?;
    check_format(&conn, screen)?;
    let root = conn.setup().roots[screen].root;
    let scale = super::scale_of(&conn) as i32;
    let shm = shm_usable(&conn);
    let cursor = (plan.cursor || plan.both).then(|| pointer(&conn)).flatten();
    let mut out = Vec::new();
    for (m, (_, r)) in plan.monitors.iter().enumerate() {
        let device = IRect::new(r.x * scale, r.y * scale, r.w * scale, r.h * scale);
        let mut picture = if shm { shm_image(&conn, root, device) } else { plain_image(&conn, root, device) }?;
        picture.force_opaque();
        if plan.both {
            let mut with = picture.clone();
            if let Some(c) = &cursor {
                c.draw(&mut with, device);
            }
            out.push(Grabbed { monitor: m, cursor: false, picture });
            out.push(Grabbed { monitor: m, cursor: true, picture: with });
        } else {
            if plan.cursor
                && let Some(c) = &cursor
            {
                c.draw(&mut picture, device);
            }
            out.push(Grabbed { monitor: m, cursor: plan.cursor, picture });
        }
    }
    Ok(out)
}

/// Are the server's pixels ours as they are (32-bit BGRX, little-endian)?
pub fn check_format(conn: &RustConnection, screen: usize) -> Result<(), String> {
    let setup = conn.setup();
    let s = &setup.roots[screen];
    let bpp = setup.pixmap_formats.iter().find(|f| f.depth == s.root_depth).map(|f| f.bits_per_pixel);
    let visual = s.allowed_depths.iter().flat_map(|d| &d.visuals).find(|v| v.visual_id == s.root_visual);
    let masks = visual.map(|v| (v.red_mask, v.green_mask, v.blue_mask));
    if setup.image_byte_order != ImageOrder::LSB_FIRST || bpp != Some(32) || masks != Some((0xff0000, 0xff00, 0xff)) {
        return Err(format!("the X server's pixels aren't 32-bit BGRX (depth {}, {:?} bits per pixel)", s.root_depth, bpp));
    }
    Ok(())
}

/// Can the server write into our memory (MIT-SHM 1.2, with descriptors)?
pub fn shm_usable(conn: &RustConnection) -> bool {
    conn.shm_query_version().ok().and_then(|c| c.reply().ok()).is_some_and(|v| (v.major_version, v.minor_version) >= (1, 2))
}

/// `r` (device pixels) of the root window, through shared memory.
fn shm_image(conn: &RustConnection, root: xproto::Window, r: IRect) -> Result<Image, String> {
    let img = Image::shared_populated(r.w as u32, r.h as u32)?;
    let fd = img.fd().ok_or("not shared memory")?.try_clone_to_owned().map_err(|e| e.to_string())?;
    let seg = conn.generate_id().map_err(|e| e.to_string())?;
    conn.shm_attach_fd(seg, fd, false).map_err(|e| e.to_string())?;
    let got = conn
        .shm_get_image(root, r.x as i16, r.y as i16, r.w as u16, r.h as u16, !0, ImageFormat::Z_PIXMAP.into(), seg, 0)
        .map_err(|e| e.to_string())
        .and_then(|c| c.reply().map_err(|e| format!("the X server didn't copy the screen: {e}")));
    let _ = conn.shm_detach(seg);
    let _ = conn.flush();
    got.map(|_| img)
}

/// `r` (device pixels) of the root window, the slow way (no MIT-SHM: a
/// remote X server, say).
fn plain_image(conn: &RustConnection, root: xproto::Window, r: IRect) -> Result<Image, String> {
    let reply = conn
        .get_image(ImageFormat::Z_PIXMAP, root, r.x as i16, r.y as i16, r.w as u16, r.h as u16, !0)
        .map_err(|e| e.to_string())?
        .reply()
        .map_err(|e| format!("the X server didn't give the screen: {e}"))?;
    let want = r.w as usize * r.h as usize * 4;
    if reply.data.len() < want {
        return Err("the X server gave too few pixels".into());
    }
    let mut data = reply.data;
    data.truncate(want);
    Image::from_bgra(r.w as u32, r.h as u32, data)
}

/// The pointer as XFixes shows it: where its top left is (device pixels,
/// in the root window) and its premultiplied ARGB pixels.
struct Pointer {
    x: i32,
    y: i32,
    w: u32,
    h: u32,
    pixels: Vec<u32>,
}

fn pointer(conn: &RustConnection) -> Option<Pointer> {
    conn.xfixes_query_version(4, 0).ok()?.reply().ok()?;
    let c = conn.xfixes_get_cursor_image().ok()?.reply().ok()?;
    let (w, h) = (c.width as u32, c.height as u32);
    if w == 0 || h == 0 || w > 1024 || h > 1024 || c.cursor_image.len() < (w * h) as usize {
        return None;
    }
    Some(Pointer { x: c.x as i32 - c.xhot as i32, y: c.y as i32 - c.yhot as i32, w, h, pixels: c.cursor_image })
}

impl Pointer {
    /// Draw it over `img`, the picture of `area` (device pixels).
    fn draw(&self, img: &mut Image, area: IRect) {
        let (iw, ih) = (img.width() as i32, img.height() as i32);
        for row in 0..self.h as i32 {
            let y = self.y - area.y + row;
            if !(0..ih).contains(&y) {
                continue;
            }
            for col in 0..self.w as i32 {
                let x = self.x - area.x + col;
                if !(0..iw).contains(&x) {
                    continue;
                }
                let s = self.pixels[(row * self.w as i32 + col) as usize];
                let sa = s >> 24;
                if sa == 0 {
                    continue;
                }
                let d = img.pixel(x as u32, y as u32);
                // (both premultiplied: source over)
                let over = |shift: u32| -> u32 {
                    let (sc, dc) = ((s >> shift) & 0xff, (d >> shift) & 0xff);
                    (sc + dc * (255 - sa) / 255).min(255) << shift
                };
                img.set_pixel(x as u32, y as u32, over(24) | over(16) | over(8) | over(0));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pointer_over_a_picture() {
        let mut img = Image::filled(4, 4, 0xFF000000).unwrap();
        // a 2x2 pointer at (1, 1): white, and half see-through red
        let p = Pointer { x: 101, y: 51, w: 2, h: 2, pixels: vec![0xFFFFFFFF, 0x80800000, 0, 0xFFFFFFFF] };
        p.draw(&mut img, IRect::new(100, 50, 4, 4));
        assert_eq!(img.pixel(1, 1), 0xFFFFFFFF);
        assert_eq!(img.pixel(2, 1), 0xFF800000);
        assert_eq!(img.pixel(1, 2), 0xFF000000, "see-through");
        assert_eq!(img.pixel(0, 0), 0xFF000000, "outside");
    }
}
