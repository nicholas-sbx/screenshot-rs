//! How long painting an overlay frame takes (cargo run --release --example paintbench).
use std::sync::Arc;
use std::time::Instant;

use flatshot::geom::IRect;
use flatshot::image::Image;
use flatshot::selftest::{FakeRecorder, fake_desktop};
use flatshot::ui::overlay::Output;
use flatshot::ui::session::{Request, Screen, Session, Setup};

fn main() {
    let (w, h) = (3840u32, 2160u32);
    let desktop = Arc::new(fake_desktop(w, h).unwrap());
    let t = Instant::now();
    flatshot::render::text::warm_up();
    println!("fonts: {:.1} ms", t.elapsed().as_secs_f64() * 1000.0);
    let screen = Screen { output: Output { name: "X".into(), rect: IRect::new(0, 0, w as i32, h as i32), scale: 1.0 }, picture: desktop, pointer: None };
    let mut s = Session::new(
        Setup { cfg: Default::default(), request: Request::default(), screens: vec![screen], pointer_taken: false, cursor: None, primary: 0, can_record: true },
        Box::new(FakeRecorder { problem: None, span: false }),
        Arc::new(|| {}),
    );
    let mut img = Image::shared(w, h).unwrap();
    for i in 0..5 {
        let _ = s.damage(0);
        let t = Instant::now();
        s.paint(0, &mut img.canvas(), (0, 0), IRect::new(0, 0, w as i32, h as i32));
        println!("full frame {i}: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    }
    // Just the toolbar's area.
    let tb = s.overlays[0].toolbar.as_ref().unwrap().rect();
    let r = tb.aligned();
    let t = Instant::now();
    let mut scratch = vec![0u8; r.w as usize * r.h as usize * 4];
    for _ in 0..10 {
        let mut c = flatshot::image::Canvas::new(&mut scratch, r.w as u32, r.h as u32).unwrap();
        s.paint(0, &mut c, (r.x, r.y), r);
    }
    println!("toolbar region: {:.2} ms each", t.elapsed().as_secs_f64() * 100.0);
    let pointer = flatshot::geom::Point::new(1000.0, 800.0);
    s.pointer_enter(0, pointer);
    let t = Instant::now();
    let mut n = 0;
    for i in 0..50 {
        s.pointer_motion(0, flatshot::geom::Point::new(1000.0 + i as f32 * 7.0, 800.0 + i as f32 * 3.0), 0, false);
        let d = s.damage(0);
        for r in d.rects_in(IRect::new(0, 0, w as i32, h as i32)) {
            let mut scratch = vec![0u8; r.w as usize * r.h as usize * 4];
            let mut c = flatshot::image::Canvas::new(&mut scratch, r.w as u32, r.h as u32).unwrap();
            s.paint(0, &mut c, (r.x, r.y), r);
            n += 1;
        }
    }
    println!("pointer move frame (crosshair + loupe): {:.2} ms each, {} rects", t.elapsed().as_secs_f64() * 1000.0 / 50.0, n);
    for i in 0..3 {
        let t = Instant::now();
        let out = s.overlays[0].render(None).unwrap();
        println!("render whole screen {i}: {:.2} ms ({} x {})", t.elapsed().as_secs_f64() * 1000.0, out.width(), out.height());
    }
    let t = Instant::now();
    let c = s.overlays[0].base.crop(IRect::new(0, 0, w as i32, h as i32)).unwrap();
    println!("crop: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let mut c2 = c.clone();
    println!("clone: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    c2.force_opaque();
    println!("force_opaque: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
    let t = Instant::now();
    let _ = Image::new_populated(w, h).unwrap();
    println!("new_populated: {:.2} ms", t.elapsed().as_secs_f64() * 1000.0);
}

#[allow(dead_code)]
fn unused() {}
