//! Renders the icons and some text to a PNG, to look at.
use flatshot::geom::{Point, Rect};
use flatshot::image::Image;
use flatshot::render::{self, Align, Painter, Pen, icons, text};

fn main() {
    let out = std::env::args().nth(1).unwrap_or("/tmp/render_check.png".into());
    let scale = 2.0;
    let (w, h) = (900u32, 420u32);
    let mut img = Image::filled((w as f32 * scale) as u32, (h as f32 * scale) as u32, 0xFF1A1A22).unwrap();
    {
        let mut p = Painter::new(img.pixmap_mut(), scale);
        let c = render::c();
        for (i, name) in icons::NAMES.iter().enumerate() {
            let x = 10.0 + (i % 18) as f32 * 48.0;
            let y = 10.0 + (i / 18) as f32 * 48.0;
            p.fill_rounded_rect(Rect::new(x, y, 36.0, 36.0), 9.0, c.raised);
            let inset = 36.0 * 0.22;
            icons::paint(&mut p, name, Rect::new(x + inset, y + inset, 36.0 - 2.0 * inset, 36.0 - 2.0 * inset), c.soft);
        }
        let mut y = 130.0;
        for (label, f) in [
            ("Drag to capture  ·  Click a window to capture it  ·  Esc to cancel", render::font(12.0, render::MEDIUM)),
            ("Snap selections to edges in the picture (DemiBold 13)", render::font(13.0, render::DEMIBOLD)),
            ("300, 600   #F5F3EF  (mono)", render::mono(12.0, render::MEDIUM)),
            ("Unicode: 漢字かな 한국어 — مرحبا بالعالم — שלום — Ελληνικά — Ünïcödé 😀🎉", render::font(14.0, render::MEDIUM)),
            ("Bold 26 text annotation", render::font(26.0, render::BOLD)),
        ] {
            p.draw_text(Rect::new(10.0, y, 880.0, 30.0), Align::Left, label, f, c.text);
            y += 40.0;
        }
        p.draw_rounded_rect(Rect::new(300.0, 340.0, 300.0, 28.0), 8.0, Some(c.base), Some(Pen::new(c.line, 1.0)));
        p.draw_text(Rect::new(300.0, 340.0, 300.0, 28.0), Align::Center, "Centered pill text", render::font(12.0, render::MEDIUM), c.soft);
        p.draw_rounded_rect(Rect::new(620.0, 340.0, 120.0, 28.0), 8.0, Some(c.accent), None);
        p.draw_text(Rect::new(620.0, 340.0, 120.0, 28.0), Align::Center, "Pin · 820 × 560", render::font(12.0, render::DEMIBOLD), c.on_accent);
        let line = text::shape("flatshot", render::font(38.0, render::BOLD));
        let (path, _) = text::outline_path(&line, Point::new(10.0, 400.0));
        if let Some(path) = path {
            p.fill_path(&path, render::Color::rgb(0xFF6A3D));
        }
    }
    std::fs::write(&out, flatshot::codec::encode_png(&img, false).unwrap()).unwrap();
    println!("wrote {out}");
}
