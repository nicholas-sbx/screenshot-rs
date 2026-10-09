//! Taking the screen. Wayland doesn't let ordinary programs read it, so
//! each desktop has its own way: KWin's screenshot API on KDE Plasma
//! (asked directly, raw pixels into our memory), wlr-screencopy on sway,
//! Hyprland and other wlroots compositors (in `wayland::screencopy`), the
//! screenshot portal on GNOME, and the desktop's own tools as a last resort.

pub mod kwin;
pub mod portal;
pub mod tools;

use std::sync::Arc;

use crate::geom::{IPoint, IRect};
use crate::image::Image;

/// A picture taken: of which monitor (as numbered in the plan), with the
/// pointer or not.
pub struct Grabbed {
    pub monitor: usize,
    pub cursor: bool,
    pub picture: Image,
}

/// What to take.
#[derive(Clone, Debug)]
pub struct Plan {
    /// (connector name, where in the desktop, logical pixels)
    pub monitors: Vec<(String, IRect)>,
    /// each monitor on its own, at its own scale (else the whole desktop, cut up)
    pub each_screen: bool,
    /// the pointer drawn in
    pub cursor: bool,
    /// both with and without the pointer (the toolbar can switch)
    pub both: bool,
}

impl Plan {
    /// The whole desktop's bounds.
    pub fn desktop(&self) -> IRect {
        self.monitors.iter().fold(IRect::default(), |acc, (_, r)| acc.united(r))
    }

    /// (with the pointer) for each picture wanted of a monitor.
    fn variants(&self) -> Vec<bool> {
        if self.both { vec![false, true] } else { vec![self.cursor] }
    }
}

/// The ways of capturing to try, best first, for `preferred` (a setting;
/// "auto" picks by desktop).
pub fn backend_order(preferred: &str, has_screencopy: bool) -> Vec<&'static str> {
    let named = crate::config::BACKENDS.iter().find(|b| **b == preferred && **b != "auto" && **b != "qt").copied();
    if let Some(b) = named {
        return vec![b];
    }
    let desktop = std::env::var("XDG_CURRENT_DESKTOP").unwrap_or_default().to_lowercase();
    let mut order: Vec<&'static str> = if desktop.contains("kde") {
        vec!["kwin", "screencopy", "portal", "spectacle", "grim"]
    } else if desktop.contains("gnome") {
        vec!["portal", "gnome-screenshot", "grim"]
    } else {
        vec!["screencopy", "grim", "portal", "spectacle", "gnome-screenshot"]
    };
    if !has_screencopy {
        order.retain(|b| *b != "screencopy");
    }
    order
}

/// Take the pictures with `backend` (anything but screencopy, which goes
/// through the Wayland connection). Blocks: run it off the event loop.
pub fn grab(backend: &str, plan: &Plan) -> Result<Vec<Grabbed>, String> {
    match backend {
        "kwin" => grab_kwin(plan),
        "portal" => split_all(plan, |_| portal::screenshot()),
        "grim" | "spectacle" | "gnome-screenshot" => {
            // grim can be asked twice at once; the others once (no switch).
            let plan = if plan.both && backend != "grim" { Plan { both: false, cursor: false, ..plan.clone() } } else { plan.clone() };
            split_all(&plan, |cursor| tools::run(backend, cursor))
        }
        other => Err(format!("unknown capture backend {other}")),
    }
}

fn grab_kwin(plan: &Plan) -> Result<Vec<Grabbed>, String> {
    let variants = plan.variants();
    if plan.each_screen {
        let mut wants = Vec::new();
        let mut index = Vec::new();
        for (m, (name, _)) in plan.monitors.iter().enumerate() {
            for &c in &variants {
                wants.push(kwin::Want { screen: Some(name.clone()), cursor: c });
                index.push((m, c));
            }
        }
        let (pictures, times) = kwin::capture(&wants)?;
        crate::timing::log(format!("KWin answered in {:.0} ms, pixels in {:.0} ms", times.replied.as_secs_f64() * 1000.0, times.pixels.as_secs_f64() * 1000.0));
        return Ok(index.into_iter().zip(pictures).map(|((monitor, cursor), picture)| Grabbed { monitor, cursor, picture }).collect());
    }
    let wants: Vec<kwin::Want> = variants.iter().map(|&c| kwin::Want { screen: None, cursor: c }).collect();
    let (pictures, _) = kwin::capture(&wants)?;
    let mut out = Vec::new();
    for (picture, &cursor) in pictures.into_iter().zip(&variants) {
        out.extend(split(plan, picture, cursor)?);
    }
    Ok(out)
}

/// Each variant from `take` (a whole-desktop picture), cut into monitors.
fn split_all(plan: &Plan, take: impl Fn(bool) -> Result<Image, String> + Sync) -> Result<Vec<Grabbed>, String> {
    let variants = plan.variants();
    let pictures: Vec<Result<Image, String>> = std::thread::scope(|s| {
        let take = &take;
        let handles: Vec<_> = variants.iter().map(|&c| s.spawn(move || take(c))).collect();
        handles.into_iter().map(|h| h.join().unwrap_or_else(|_| Err("the capture failed".into()))).collect()
    });
    let mut out = Vec::new();
    for (picture, &cursor) in pictures.into_iter().zip(&variants) {
        out.extend(split(plan, picture?, cursor)?);
    }
    Ok(out)
}

/// A picture of the whole desktop, cut into each monitor's part (at the
/// picture's scale).
pub fn split(plan: &Plan, desktop: Image, cursor: bool) -> Result<Vec<Grabbed>, String> {
    let virt = plan.desktop();
    if plan.monitors.len() == 1 {
        return Ok(vec![Grabbed { monitor: 0, cursor, picture: desktop }]);
    }
    let scale = desktop.width() as f32 / virt.w.max(1) as f32;
    let mut out = Vec::new();
    for (m, (name, r)) in plan.monitors.iter().enumerate() {
        let part = IRect::new(
            ((r.x - virt.x) as f32 * scale).round() as i32,
            ((r.y - virt.y) as f32 * scale).round() as i32,
            (r.w as f32 * scale).round() as i32,
            (r.h as f32 * scale).round() as i32,
        )
        .intersected(&desktop.rect());
        if part.is_empty() {
            return Err(format!("{name} isn't in the picture of the desktop"));
        }
        out.push(Grabbed { monitor: m, cursor, picture: desktop.crop(part)? });
    }
    Ok(out)
}

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

    #[test]
    fn splitting_the_desktop() {
        let plan = Plan {
            monitors: vec![("A".into(), IRect::new(0, 0, 100, 50)), ("B".into(), IRect::new(100, 0, 50, 50))],
            each_screen: false,
            cursor: false,
            both: false,
        };
        // At 2x: 300 x 100.
        let mut desktop = Image::filled(300, 100, 0xFF000000).unwrap();
        desktop.set_pixel(210, 10, 0xFFFF0000);
        let parts = split(&plan, desktop, false).unwrap();
        assert_eq!(parts[0].picture.size(), (200, 100));
        assert_eq!(parts[1].picture.size(), (100, 100));
        assert_eq!(parts[1].picture.pixel(10, 10), 0xFFFF0000);
    }

    #[test]
    fn backends_by_desktop() {
        assert_eq!(backend_order("grim", true), ["grim"]);
        assert!(!backend_order("auto", false).contains(&"screencopy"));
    }
}
