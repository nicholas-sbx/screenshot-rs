//! Headless smoke test (`flatshot --self-test`), run by CI on every
//! package. Builds a fake desktop with a QR code and a barcode, runs a
//! capture session on it with synthetic input, draws, renders and scans.
//! Set FLATSHOT_SELFTEST_OUT=dir to keep the frames it paints.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use crate::codec;
use crate::config::{self, Config};
use crate::geom::{IPoint, IRect, Point, Rect};
use crate::image::{self, Image};
use crate::keys::KeyPress;
use crate::keyseq::{self, CTRL, SHIFT, key};
use crate::render::Painter;
use crate::render::paint::{Align, sk};
use crate::render::text::{self, NORMAL};
use crate::render::theme::{self, Color};
use crate::scanner;
use crate::ui::overlay::{Output, Window};
use crate::ui::session::{Button, CursorShape, Effect, Outcome, RecordSupport, Request, Screen, Session, Setup, Waker};
use crate::ui::shapes::Tool;
use crate::ui::state::{CUSTOM, Desktop};
use crate::ui::toolbar::CodeScan;

pub const TEST_URL: &str = "https://github.com/nicholas-sbx/screenshot";
pub const TEST_EAN: &str = "4006381333931";

/// Fails the self-test with a message (and where), like Python's assert.
macro_rules! check {
    ($cond:expr) => {
        {
            let ok: bool = $cond;
            if !ok {
                return Err(format!("{}:{}: {}", file!(), line!(), stringify!($cond)));
            }
        }
    };
    ($cond:expr, $($fmt:tt)+) => {
        {
            let ok: bool = $cond;
            if !ok {
                return Err(format!("{}:{}: {}: {}", file!(), line!(), stringify!($cond), format!($($fmt)+)));
            }
        }
    };
}

type R = Result<(), String>;

fn asset(name: &str) -> Result<Image, String> {
    // Installed packages carry the fixtures inside the binary.
    let bytes: &[u8] = match name {
        "selftest-qr.png" => include_bytes!("../assets/selftest-qr.png"),
        "selftest-ean13.png" => include_bytes!("../assets/selftest-ean13.png"),
        _ => return Err(format!("no asset {name}")),
    };
    codec::decode_png(bytes)
}

/// The fake desktop: a gradient, a light window with text, a QR code and a
/// barcode on white cards (picture pixels).
pub fn fake_desktop(w: u32, h: u32) -> Result<Image, String> {
    let mut img = Image::new(w, h)?;
    {
        let mut p = Painter::new(img.pixmap_mut(), 1.0);
        let stops = vec![tiny_skia::GradientStop::new(0.0, sk(Color::rgb(0x2B3A55))), tiny_skia::GradientStop::new(1.0, sk(Color::rgb(0x5C4B6E)))];
        if let (Some(shader), Some(path)) = (
            tiny_skia::LinearGradient::new(
                tiny_skia::Point::from_xy(0.0, 0.0),
                tiny_skia::Point::from_xy(w as f32, h as f32),
                stops,
                tiny_skia::SpreadMode::Pad,
                tiny_skia::Transform::identity(),
            ),
            crate::render::paint::rounded_rect_path(Rect::new(0.0, 0.0, w as f32, h as f32), 0.0),
        ) {
            p.fill_path_shader(&path, shader);
        }
        p.fill_rounded_rect(Rect::new(120.0, 140.0, 820.0, 560.0), 14.0, Color::rgb(0xF5F3EF));
        p.draw_text_at(Point::new(170.0, 220.0), "Some window with text", text::font(30.0, NORMAL), Color::rgb(0x333333));
        for i in 0..8 {
            p.draw_text_at(
                Point::new(170.0, 280.0 + i as f32 * 34.0),
                &format!("Line {}: lorem ipsum dolor sit amet, consectetur", i + 1),
                text::font(18.0, NORMAL),
                Color::rgb(0x333333),
            );
        }
        p.fill_rect(Rect::new(1060.0, 240.0, 225.0, 225.0), Color::WHITE);
        p.fill_rect(Rect::new(1060.0, 600.0, 379.0, 205.0), Color::WHITE);
        let _ = Align::Left;
    }
    img.blit(&asset("selftest-qr.png")?, 1080, 260);
    img.blit(&asset("selftest-ean13.png")?, 1080, 620);
    img.force_opaque();
    Ok(img)
}

/// A recorder that can do everything (or nothing), for the tests.
pub struct FakeRecorder {
    pub problem: Option<String>,
    pub span: bool,
}

impl RecordSupport for FakeRecorder {
    fn problem(&mut self, format: &str) -> Option<String> {
        if format == "gif" { Some("GIFs need ffmpeg.".into()) } else { self.problem.clone() }
    }
    fn can_span(&self) -> bool {
        self.span
    }
}

fn no_wake() -> Waker {
    Arc::new(|| {})
}

fn press(k: i32, text: &str, mods: i32) -> KeyPress {
    KeyPress { key: k, mods, text: text.to_string(), keysym: 0, repeat: false }
}

/// Key `k` down and up on overlay `o`.
fn tap(s: &mut Session, o: usize, k: i32, text: &str, mods: i32) {
    let kp = press(k, text, mods);
    s.key(o, &kp);
    s.key_release(o, &kp);
}

/// A drag with the left button from `a` to `b` (logical), in a few steps.
fn drag(s: &mut Session, o: usize, a: Point, b: Point, mods: i32) {
    s.pointer_motion(o, a, mods, false);
    s.button(o, Button::Left, true, a, mods);
    for i in 1..=4 {
        let t = i as f32 / 4.0;
        s.pointer_motion(o, a + (b - a) * t, mods, false);
    }
    s.button(o, Button::Left, false, b, mods);
}

fn click(s: &mut Session, o: usize, at: Point, mods: i32) {
    s.pointer_motion(o, at, mods, false);
    s.button(o, Button::Left, true, at, mods);
    s.button(o, Button::Left, false, at, mods);
}

/// The whole frame of overlay `o`, as painted.
fn frame(s: &mut Session, o: usize) -> Result<Image, String> {
    let _ = s.damage(o);
    let (w, h) = s.overlays[o].device_size();
    let mut img = Image::new(w, h)?;
    s.paint(o, &mut img.canvas(), (0, 0), IRect::new(0, 0, w as i32, h as i32));
    Ok(img)
}

fn save_png(img: &Image, dir: &Option<PathBuf>, name: &str) -> R {
    if let Some(dir) = dir {
        let bytes = codec::encode_png(img, false)?;
        std::fs::write(dir.join(name), bytes).map_err(|e| format!("{name}: {e}"))?;
    }
    Ok(())
}

/// The centre of the toolbar control `id` on overlay `o` (logical).
fn toolbar_item(s: &Session, o: usize, id: &str) -> Result<Point, String> {
    let tb = s.overlays[o].toolbar.as_ref().ok_or("no toolbar")?;
    let it = tb.items.iter().find(|it| it.id == id).ok_or_else(|| format!("no toolbar item {id}"))?;
    Ok(it.rect.translated(tb.pos).center())
}

fn session(cfg: Config, request: Request, screens: Vec<Screen>, rec: FakeRecorder) -> Session {
    Session::new(Setup { cfg, request, screens, pointer_taken: false, cursor: None, primary: 0, can_record: true }, Box::new(rec), no_wake())
}

fn image_screen(img: Arc<Image>, logical: (i32, i32)) -> Screen {
    Screen { output: Output { name: "TEST-1".into(), rect: IRect::new(0, 0, logical.0, logical.1), scale: 1.0 }, picture: img, pointer: None }
}

fn test_config(tmp: &Path) -> Config {
    Config { save_dir: tmp.join("shots").to_string_lossy().into_owned(), notify: false, clipboard: "none".into(), dim_opacity: 45, ..Default::default() }
}

pub fn run() -> i32 {
    let tmp = std::env::temp_dir().join(format!("flatshot-selftest-{}", crate::util::random_hex(8)));
    if let Err(e) = std::fs::create_dir_all(&tmp) {
        eprintln!("self-test: {e}");
        return 1;
    }
    // Never touch the real settings.
    // SAFETY: the self-test sets these before starting any thread.
    unsafe {
        std::env::set_var("XDG_CONFIG_HOME", tmp.join("config"));
        std::env::set_var("XDG_STATE_HOME", tmp.join("state"));
    }
    let out = std::env::var_os("FLATSHOT_SELFTEST_OUT").map(PathBuf::from);
    if let Some(d) = &out {
        let _ = std::fs::create_dir_all(d);
    }
    let started = Instant::now();
    let result = all(&tmp, &out);
    let _ = std::fs::remove_dir_all(&tmp);
    match result {
        Ok(()) => {
            println!("self-test ok ({:.1} s)", started.elapsed().as_secs_f64());
            0
        }
        Err(e) => {
            eprintln!("self-test FAILED: {e}");
            1
        }
    }
}

fn all(tmp: &Path, out: &Option<PathBuf>) -> R {
    let desktop = Arc::new(fake_desktop(1600, 1000)?);
    overlay_basics(tmp, out, desktop.clone())?;
    codes_on_demand(tmp, desktop.clone())?;
    keys_and_text(tmp, out, desktop.clone())?;
    pointer_and_panels(tmp, out, desktop.clone())?;
    snapping(tmp, desktop.clone())?;
    recording(tmp, out, desktop.clone())?;
    multi_monitor(tmp, out, desktop.clone())?;
    hidpi(tmp, out, desktop)?;
    Ok(())
}

/// Codes, windows, drawing, undo, rendering and the capture itself.
fn overlay_basics(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let holders = Arc::strong_count(&desktop);
    let cfg = test_config(tmp);
    let mut s = session(
        cfg,
        Request { image: true, ..Default::default() },
        vec![image_screen(desktop.clone(), (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    check!(s.overlays.len() == 1, "no overlay was created");
    check!(s.st.toolbar_overlay == Some(0) && s.overlays[0].toolbar_visible, "toolbar not on the active monitor");
    check!(s.st.dim.a == (45.0f32 * 2.55).round_ties_even() as u8);
    check!(!s.st.can_record, "annotating a picture can't record");

    // Codes: found, toggled with Q, dismissed with ×; toggling never revives a dismissed one.
    let t = Instant::now();
    let codes = scanner::scan(&desktop);
    let scan_ms = t.elapsed().as_secs_f64() * 1000.0;
    let mut texts: Vec<&str> = codes.iter().map(|c| c.text.as_str()).collect();
    texts.sort();
    check!(texts == [TEST_EAN, TEST_URL], "found {texts:?}");
    s.codes_found(0, codes.clone());
    check!(s.code_count() == 2 && s.overlays[0].chips_shown(&s.st), "no code cards");
    tap(&mut s, 0, 0x51, "q", 0);
    check!(!s.st.codes_visible && !s.overlays[0].chips_shown(&s.st));
    check!(!config::load().show_codes, "hiding the codes isn't remembered");
    tap(&mut s, 0, 0x51, "q", 0);
    check!(s.st.codes_visible && config::load().show_codes);
    println!("self-test: found {} codes (QR + EAN-13) in {scan_ms:.0} ms", codes.len());

    s.pointer_enter(0, Point::new(700.0, 600.0));
    save_png(&frame(&mut s, 0)?, out, "overlay-region.png")?;

    // Window detection: hover highlights the topmost window, a click captures it.
    s.desktop_found(Desktop {
        windows: vec![
            Window { rect: IRect::new(1060, 240, 225, 225), title: "QR card".into(), app: "viewer".into() },
            Window { rect: IRect::new(120, 140, 820, 560), title: "Notes — Kate".into(), app: "kate".into() },
        ],
        active: None,
        cursor: None,
    });
    check!(s.overlays[0].windows.len() == 2);
    s.pointer_motion(0, Point::new(1180.0, 450.0), 0, false);
    let hovered = s.overlays[0].hover_window.map(|h| s.st.windows[h.1].title.clone());
    check!(hovered.as_deref() == Some("QR card"), "{hovered:?}");
    s.pointer_motion(0, Point::new(300.0, 600.0), 0, false);
    let hovered = s.overlays[0].hover_window.map(|h| s.st.windows[h.1].title.clone());
    check!(hovered.as_deref() == Some("Notes — Kate"), "{hovered:?}");
    save_png(&frame(&mut s, 0)?, out, "overlay-window.png")?;
    s.pointer_motion(0, Point::new(1500.0, 900.0), 0, false);
    check!(s.overlays[0].hover_window.is_none());

    // Dismiss the first code with its card's ×.
    let n = s.code_count();
    let chip = &s.overlays[0].chips[0];
    let close = chip.items.iter().find(|it| it.id == "close").ok_or("no dismiss button")?.rect.translated(chip.pos).center();
    click(&mut s, 0, close, 0);
    check!(s.code_count() == n - 1, "the code wasn't dismissed");
    tap(&mut s, 0, 0x51, "q", 0);
    tap(&mut s, 0, 0x51, "q", 0);
    check!(s.code_count() == n - 1, "a dismissed code came back");

    // Drawing with the mouse: each tool from the toolbar.
    for (tool, a, b) in [
        ("arrow", (200.0, 760.0), (420.0, 640.0)),
        ("rect", (150.0, 170.0), (700.0, 240.0)),
        ("pixelate", (160.0, 330.0), (560.0, 400.0)),
        ("marker", (160.0, 460.0), (600.0, 460.0)),
        ("ellipse", (980.0, 200.0), (1340.0, 600.0)),
    ] {
        let at = toolbar_item(&s, 0, tool)?;
        click(&mut s, 0, at, 0);
        check!(s.st.tool == Tool::from_id(tool).unwrap(), "the {tool} button didn't choose it");
        drag(&mut s, 0, Point::new(a.0, a.1), Point::new(b.0, b.1), 0);
    }
    check!(s.overlays[0].annotations.len() == 5, "{} drawn", s.overlays[0].annotations.len());
    let at = toolbar_item(&s, 0, "counter")?;
    click(&mut s, 0, at, 0);
    click(&mut s, 0, Point::new(140.0, 160.0), 0);
    click(&mut s, 0, Point::new(940.0, 160.0), 0);
    check!(s.overlays[0].annotations.len() == 7);
    let at = toolbar_item(&s, 0, "text")?;
    click(&mut s, 0, at, 0);
    click(&mut s, 0, Point::new(460.0, 720.0), 0);
    check!(s.st.text_edit.is_some(), "a click with the text tool doesn't start a text");
    for ch in "flatshot".chars() {
        tap(&mut s, 0, 0x41, &ch.to_string(), 0);
    }
    tap(&mut s, 0, key::RETURN, "", 0);
    check!(s.st.text_edit.is_none() && s.overlays[0].annotations.len() == 8, "{}", s.overlays[0].annotations.len());
    s.undo();
    check!(s.overlays[0].annotations.len() == 7);
    s.redo();
    check!(s.overlays[0].annotations.len() == 8);
    s.pointer_motion(0, Point::new(1450.0, 120.0), 0, false);
    save_png(&frame(&mut s, 0)?, out, "overlay-draw.png")?;

    // The capture: drag out a region with the capture tool.
    let at = toolbar_item(&s, 0, "region")?;
    click(&mut s, 0, at, 0);
    drag(&mut s, 0, Point::new(100.0, 100.0), Point::new(1000.0, 800.0), 0);
    check!(s.done, "the drag didn't capture");
    match s.take_outcome() {
        Some(Outcome::Image { image, shot, at, pin }) => {
            check!(image.size() == (900, 700), "{:?}", image.size());
            check!(shot.mode == "region" && at.is_none() && !pin, "{shot:?} {at:?}");
            check!(image.pixel(899, 699) >> 24 == 0xFF, "the result isn't opaque");
            save_png(&image, out, "result.png")?;
        }
        other => return Err(format!("captured {other:?}")),
    }
    check!(s.overlays.is_empty(), "the overlays outlive the capture");
    check!(Arc::strong_count(&desktop) == holders, "something still holds the picture");
    Ok(())
}

/// Keys: tools, colours, sizes, typing, undo, Enter and Esc.
fn keys_and_text(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let mut s = session(
        test_config(tmp),
        Request { image: true, ..Default::default() },
        vec![image_screen(desktop.clone(), (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    s.pointer_enter(0, Point::new(800.0, 500.0));
    tap(&mut s, 0, 0x54, "t", 0);
    check!(s.st.tool == Tool::Text, "{:?}", s.st.tool);
    click(&mut s, 0, Point::new(1000.0, 760.0), 0);
    for ch in "hi!".chars() {
        tap(&mut s, 0, 0x41, &ch.to_string(), 0); // (the key code doesn't matter while typing)
    }
    tap(&mut s, 0, key::BACKSPACE, "", 0);
    // Shift+Enter: a new line; Enter: done.
    tap(&mut s, 0, key::RETURN, "", SHIFT);
    tap(&mut s, 0, 0x41, "x", 0);
    tap(&mut s, 0, key::RETURN, "", 0);
    let typed = s.overlays[0].annotations.last().and_then(|a| a.as_text()).map(|t| t.edit.text.clone());
    check!(s.st.text_edit.is_none() && typed.as_deref() == Some("hi\nx"), "{typed:?}");
    tap(&mut s, 0, key::Z, "", CTRL);
    check!(s.overlays[0].annotations.is_empty());
    tap(&mut s, 0, key::Z, "", CTRL | SHIFT);
    check!(s.overlays[0].annotations.len() == 1, "Ctrl+Shift+Z doesn't redo");
    tap(&mut s, 0, 0x33, "3", 0);
    check!(s.st.color_index == 2);
    tap(&mut s, 0, 0x50, "p", 0);
    check!(s.st.tool == Tool::Pen);
    tap(&mut s, 0, 0x5D, "]", 0);
    tap(&mut s, 0, 0x5D, "]", 0);
    tap(&mut s, 0, 0x5D, "]", 0);
    check!(s.st.size == 2, "{}", s.st.size);
    tap(&mut s, 0, 0x5B, "[", 0);
    check!(s.st.size == 1);

    // Editing a text again: a click on it edits a copy; undo brings the original back.
    tap(&mut s, 0, 0x54, "t", 0);
    click(&mut s, 0, Point::new(1005.0, 770.0), 0);
    check!(s.st.text_edit.as_ref().is_some_and(|(_, t)| t.as_text().is_some_and(|t| t.replaces.is_some())), "not editing the text again");
    check!(s.overlays[0].annotations[0].hidden, "the original shows under its copy");
    tap(&mut s, 0, 0x41, "y", 0);
    tap(&mut s, 0, key::ESCAPE, "", 0);
    check!(s.st.text_edit.is_none() && !s.done, "Esc while typing should just finish the text");
    check!(s.overlays[0].annotations.len() == 2 && s.overlays[0].annotations[0].hidden);
    s.undo();
    check!(s.overlays[0].annotations.len() == 1 && !s.overlays[0].annotations[0].hidden, "undo didn't bring the original back");

    // Typing in a text: the caret moves, Shift selects, Ctrl+A, cut and paste.
    click(&mut s, 0, Point::new(300.0, 820.0), 0);
    for ch in "hello world".chars() {
        tap(&mut s, 0, 0x41, &ch.to_string(), 0);
    }
    tap(&mut s, 0, key::LEFT, "", CTRL | SHIFT);
    tap(&mut s, 0, key::X, "", CTRL);
    let fx = s.take_effects();
    check!(fx == [Effect::Clipboard("world".into())], "{fx:?}");
    tap(&mut s, 0, key::HOME, "", 0);
    tap(&mut s, 0, key::V, "", CTRL);
    check!(s.take_effects() == [Effect::Paste]);
    s.paste("big ");
    let t = s.st.text_edit.as_ref().and_then(|(_, t)| t.as_text()).map(|t| t.edit.text.clone());
    check!(t.as_deref() == Some("big hello "), "{t:?}");
    save_png(&frame(&mut s, 0)?, out, "overlay-typing.png")?;
    tap(&mut s, 0, key::RETURN, "", 0);

    // Esc acts when released, not pressed (the release would go to the
    // window underneath); a release without a press here does nothing.
    let esc = press(key::ESCAPE, "", 0);
    s.key_release(0, &esc);
    check!(!s.done, "a release alone closed it");
    s.key(0, &esc);
    check!(!s.done, "Esc closed on the way down");
    s.key_release(0, &esc);
    check!(s.done && matches!(s.take_outcome(), Some(Outcome::Cancelled)));
    Ok(())
}

/// The pointer over the panels, the picker, the eyedropper, the hint and
/// the rainbow crosshair.
fn pointer_and_panels(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let mut cfg = test_config(tmp);
    cfg.rainbow = true;
    let mut s = session(
        cfg,
        Request { image: true, ..Default::default() },
        vec![image_screen(desktop.clone(), (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    s.pointer_enter(0, Point::new(800.0, 700.0));
    check!(s.cursor(0) == CursorShape::Crosshair);
    // The crosshair keeps following the pointer over the toolbar's buttons;
    // the button's tip becomes the hint.
    let pen = toolbar_item(&s, 0, "pen")?;
    s.pointer_motion(0, pen, 0, false);
    check!(s.overlays[0].cursor_pos == Some(pen), "the crosshair stopped at the toolbar");
    check!(s.st.hint.as_deref() == Some("Pen  ·  P"), "{:?}", s.st.hint);
    check!(s.cursor(0) == CursorShape::Hand);
    // A finger on a button isn't a pointer: the crosshair stays where it was.
    let rect_button = toolbar_item(&s, 0, "rect")?;
    s.pointer_motion(0, rect_button, 0, true);
    check!(s.overlays[0].cursor_pos == Some(pen));
    s.pointer_motion(0, Point::new(800.0, 700.0), 0, false);
    check!(s.st.hint.is_none(), "the tip outlived the pointer on its button");

    // The hint: in the middle, faded while the pointer is near it.
    let (_, at, opacity) = s.overlays[0].hint(&s.st);
    check!(opacity == 1.0 && (at.x - 800.0).abs() < 400.0 && (at.y - 486.0).abs() < 1.0, "{at:?}");
    s.pointer_motion(0, Point::new(800.0, 500.0), 0, false);
    check!(s.overlays[0].hint(&s.st).2 < 1.0, "the hint doesn't fade under the pointer");
    s.pointer_motion(0, Point::new(800.0, 700.0), 0, false);

    // Rainbow: the crosshair's colour moves on its own.
    let first = frame(&mut s, 0)?;
    s.on_timer(Instant::now() + Duration::from_millis(1500));
    let damage = s.damage(0);
    check!(!damage.is_empty(), "the rainbow didn't repaint the crosshair");
    let second = frame(&mut s, 0)?;
    check!(first.pixel(800, 100) != second.pixel(800, 100), "the crosshair's colour didn't move");

    // Picking a colour from a tool without one goes back to the last drawing tool.
    tap(&mut s, 0, 0x45, "e", 0); // ellipse
    tap(&mut s, 0, 0x58, "x", 0); // pixelate
    tap(&mut s, 0, 0x32, "2", 0);
    check!(s.st.tool == Tool::Ellipse && s.st.color_index == 1, "{:?}", s.st.tool);

    // The magnifier zooms with the wheel, within limits.
    tap(&mut s, 0, 0x52, "r", 0);
    let z = s.st.loupe_zoom;
    s.scroll(0, 1.0);
    check!(s.st.loupe_zoom > z);
    for _ in 0..40 {
        s.scroll(0, 1.0);
    }
    check!(s.st.loupe_zoom == 40.0);

    // I copies the colour under the pointer and finishes.
    s.pointer_motion(0, Point::new(500.0, 650.0), 0, false);
    tap(&mut s, 0, 0x49, "i", 0);
    match s.take_outcome() {
        Some(Outcome::Color { hex, rgb }) => check!(hex == "#F5F3EF" && rgb == (0xF5, 0xF3, 0xEF), "{hex}"),
        other => return Err(format!("copy colour gave {other:?}")),
    }

    // Your own colour: key 8 picks it; the picker changes it (dragging,
    // typing hex), Enter in the hex field doesn't capture, a click
    // elsewhere closes it, the eyedropper takes a pixel, and it's
    // remembered. Esc closes it and cancels, as ever.
    let mut s = session(
        test_config(tmp),
        Request { image: true, ..Default::default() },
        vec![image_screen(desktop.clone(), (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    s.pointer_enter(0, Point::new(800.0, 700.0));
    tap(&mut s, 0, 0x38, "8", 0);
    check!(s.st.color_index == CUSTOM && Some(s.st.custom_color) == Color::parse(&config::load().custom_color), "{:?}", s.st.custom_color);
    s.set_tool(Tool::Region);
    let swatch = toolbar_item(&s, 0, "color.8")?;
    click(&mut s, 0, swatch, 0);
    check!(s.overlays[0].picker_open() && s.st.tool == s.st.colour_tool, "the custom swatch doesn't open the picker");
    let at = s.overlays[0].picker.as_ref().ok_or("no picker")?.pos;
    drag(&mut s, 0, Point::new(at.x + 108.0, at.y + 72.0), Point::new(at.x + 10.0 + 196.0, at.y + 10.0), 0);
    let (_, sat, val) = s.st.custom_color.hsv();
    check!(sat > 0.95 && val > 0.95, "{:?}", s.st.custom_color);
    let p = s.overlays[0].picker.as_ref().ok_or("no picker")?;
    let hex = p.hex.rect.translated(p.pos).center();
    click(&mut s, 0, hex, 0);
    check!(s.cursor(0) == CursorShape::Text);
    tap(&mut s, 0, key::A, "", CTRL);
    for ch in "22aa77".chars() {
        tap(&mut s, 0, 0x30, &ch.to_string(), 0); // (the # is optional)
    }
    check!(s.st.custom_color == Color::rgb(0x22AA77), "{:?}", s.st.custom_color);
    save_png(&frame(&mut s, 0)?, out, "overlay-picker.png")?;
    tap(&mut s, 0, key::RETURN, "", 0);
    check!(!s.done && !s.overlays[0].picker_open(), "Enter in the hex field captured, or left the picker open");
    check!(config::load().custom_color == "#22AA77", "your colour isn't remembered");
    click(&mut s, 0, swatch, 0);
    check!(s.overlays[0].picker_open());
    let pen = toolbar_item(&s, 0, "pen")?;
    click(&mut s, 0, pen, 0);
    check!(!s.overlays[0].picker_open(), "a click on the toolbar left the picker open");
    click(&mut s, 0, swatch, 0);
    let p = s.overlays[0].picker.as_ref().ok_or("no picker")?;
    let inside = Point::new(p.pos.x + 108.0, p.pos.y + 72.0);
    click(&mut s, 0, inside, 0);
    check!(s.overlays[0].picker_open(), "a click in the picker closed it");
    let p = s.overlays[0].picker.as_ref().ok_or("no picker")?;
    let dropper = p.dropper.rect.translated(p.pos).center();
    click(&mut s, 0, dropper, 0);
    check!(s.st.eyedropper && !s.overlays[0].picker_open() && s.cursor(0) == CursorShape::Crosshair);
    click(&mut s, 0, Point::new(500.0, 650.0), 0); // the light window
    check!(!s.st.eyedropper && s.st.custom_color == Color::rgb(0xF5F3EF), "took {:?}", s.st.custom_color);
    check!(s.overlays[0].picker_open(), "the picker didn't come back to adjust the colour");
    // The toolbar, dragged by its grip, stays inside the screen (the press
    // closes the picker, as any press elsewhere does).
    let grip = toolbar_item(&s, 0, "grip")?;
    drag(&mut s, 0, grip, grip + Point::new(-40.0, 60.0), 0);
    let bar = s.overlays[0].toolbar.as_ref().ok_or("no toolbar")?.rect();
    check!(bar.y == 80.0 && !s.overlays[0].picker_open(), "{bar:?}");
    drag(&mut s, 0, grip + Point::new(-40.0, 60.0), Point::new(-500.0, -500.0), 0);
    let bar = s.overlays[0].toolbar.as_ref().ok_or("no toolbar")?.rect();
    check!((bar.x, bar.y) == (8.0, 8.0), "{bar:?}");
    let swatch = toolbar_item(&s, 0, "color.8")?;
    click(&mut s, 0, swatch, 0);
    let (bar, p) = (s.overlays[0].toolbar.as_ref().ok_or("no toolbar")?.rect(), s.overlays[0].picker.as_ref().ok_or("no picker")?);
    check!(p.pos.y == bar.bottom() + 8.0, "the picker isn't under the toolbar: {:?} {bar:?}", p.pos);
    tap(&mut s, 0, key::ESCAPE, "", 0);
    check!(s.done && matches!(s.take_outcome(), Some(Outcome::Cancelled)), "Esc only closed the picker");
    Ok(())
}

/// Snapping: off by default and free; on, a selection's corner jumps to the
/// window's edge (the light window starts at 120, 140).
fn snapping(tmp: &Path, desktop: Arc<Image>) -> R {
    let mut s = session(
        test_config(tmp),
        Request { image: true, ..Default::default() },
        vec![image_screen(desktop.clone(), (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    check!(!s.st.snap_edges, "snapping is on by default");
    s.pointer_enter(0, Point::new(124.0, 145.0));
    tap(&mut s, 0, 0x47, "g", 0);
    check!(s.st.snap_edges && config::load().snap_edges, "G doesn't turn snapping on, or it isn't remembered");
    // (the edge maps are built in the background)
    let deadline = Instant::now() + Duration::from_secs(10);
    while s.overlays[0].edge_maps.is_none() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(5));
        s.poll();
    }
    check!(s.overlays[0].edge_maps.is_some(), "no edge maps");
    s.pointer_motion(0, Point::new(124.0, 145.0), 0, false);
    let snap = s.overlays[0].snap_point;
    check!(snap == Some(Point::new(120.0, 140.0)), "snapped to {snap:?}");
    // Ctrl: free.
    s.pointer_motion(0, Point::new(124.0, 145.0), CTRL, false);
    check!(s.overlays[0].snap_point == Some(Point::new(124.0, 145.0)));
    // A whole drag: both corners land on the window's edges.
    drag(&mut s, 0, Point::new(124.0, 145.0), Point::new(936.0, 696.0), 0);
    match s.take_outcome() {
        Some(Outcome::Image { image, .. }) => check!(image.size() == (820, 560), "{:?}", image.size()),
        other => return Err(format!("snapped capture gave {other:?}")),
    }
    let mut cfg = config::load();
    cfg.snap_edges = false;
    cfg.save().map_err(|e| e.to_string())?;
    Ok(())
}

/// Codes hidden at the start: nothing is looked for until they're shown.
fn codes_on_demand(tmp: &Path, desktop: Arc<Image>) -> R {
    let cfg = Config { show_codes: false, ..test_config(tmp) };
    let mut s = session(
        cfg,
        Request { image: true, scan: true, ..Default::default() },
        vec![image_screen(desktop, (1600, 1000))],
        FakeRecorder { problem: None, span: false },
    );
    check!(s.code_scan() == CodeScan::NotYet, "{:?}", s.code_scan());
    let codes = s.overlays[0].toolbar.as_ref().and_then(|t| t.items.iter().find(|i| i.id == "codes")).map(|i| i.hint.clone()).unwrap_or_default();
    check!(codes.ends_with("Not looked for yet"), "{codes}");
    tap(&mut s, 0, 0x51, "q", 0);
    check!(s.st.codes_visible && s.code_scan() == CodeScan::Scanning, "{:?}", s.code_scan());
    let deadline = Instant::now() + std::time::Duration::from_secs(20);
    while s.code_scan() == CodeScan::Scanning && Instant::now() < deadline {
        std::thread::sleep(std::time::Duration::from_millis(10));
        s.poll();
    }
    check!(s.code_scan() == CodeScan::Found(2), "{:?}", s.code_scan());
    // Hidden and shown again: not looked for twice.
    tap(&mut s, 0, 0x51, "q", 0);
    tap(&mut s, 0, 0x51, "q", 0);
    check!(s.code_scan() == CodeScan::Found(2), "{:?}", s.code_scan());
    s.cancel();
    println!("self-test: hidden codes are looked for once shown");
    Ok(())
}

/// Recording: choose an area, the panel's options, the countdown, the job.
fn recording(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let mut cfg = test_config(tmp);
    cfg.record_countdown = 2;
    let screen = Screen { output: Output { name: "DP-1".into(), rect: IRect::new(0, 0, 1600, 1000), scale: 1.0 }, picture: desktop.clone(), pointer: None };
    let mut s = session(cfg, Request { record: true, ..Default::default() }, vec![screen], FakeRecorder { problem: None, span: false });
    check!(s.st.tool == Tool::Record && s.st.can_record);
    s.pointer_enter(0, Point::new(300.0, 300.0));
    drag(&mut s, 0, Point::new(200.0, 200.0), Point::new(700.0, 500.0), 0);
    check!(!s.done && s.overlays[0].rec_rect == Some(Rect::new(200.0, 200.0, 500.0, 300.0)), "{:?}", s.overlays[0].rec_rect);
    check!(s.st.any_armed && s.overlays[0].panel.is_some(), "no recording panel");
    save_png(&frame(&mut s, 0)?, out, "overlay-record.png")?;
    // The panel's options are remembered; GIF can't be chosen here.
    let panel = s.overlays[0].panel.as_ref().ok_or("no panel")?;
    let mic = panel.items.iter().find(|it| it.id == "mic").ok_or("no mic chip")?.rect.translated(panel.pos).center();
    let was = s.st.cfg.record_mic;
    click(&mut s, 0, mic, 0);
    check!(s.st.cfg.record_mic != was && config::load().record_mic != was, "the mic option isn't remembered");
    let panel = s.overlays[0].panel.as_ref().ok_or("no panel")?;
    let fps = panel.items.iter().find(|it| it.id == "fps").ok_or("no fps chip")?.rect.translated(panel.pos).center();
    click(&mut s, 0, fps, 0);
    check!(s.st.cfg.record_fps == 60, "{}", s.st.cfg.record_fps);
    // Resize by the bottom right handle.
    drag(&mut s, 0, Point::new(700.0, 500.0), Point::new(800.0, 560.0), 0);
    check!(s.overlays[0].rec_rect == Some(Rect::new(200.0, 200.0, 600.0, 360.0)), "{:?}", s.overlays[0].rec_rect);
    // Ctrl resizes about the centre (500, 380): grabbed 2 px inside the right
    // edge, the edge keeps that offset and the left one mirrors it.
    drag(&mut s, 0, Point::new(798.0, 380.0), Point::new(818.0, 380.0), CTRL);
    check!(s.overlays[0].rec_rect == Some(Rect::new(180.0, 200.0, 640.0, 360.0)), "{:?}", s.overlays[0].rec_rect);
    drag(&mut s, 0, Point::new(500.0, 560.0), Point::new(500.0, 540.0), CTRL);
    check!(s.overlays[0].rec_rect == Some(Rect::new(180.0, 220.0, 640.0, 320.0)), "{:?}", s.overlays[0].rec_rect);
    // Esc goes back to choosing; Enter on the whole screen otherwise.
    tap(&mut s, 0, key::ESCAPE, "", 0);
    check!(!s.done && s.overlays[0].rec_rect.is_none() && !s.st.any_armed);
    drag(&mut s, 0, Point::new(200.0, 200.0), Point::new(700.0, 500.0), 0);
    tap(&mut s, 0, key::RETURN, "", 0);
    check!(s.st.countdown == Some(2), "{:?}", s.st.countdown);
    check!(!s.overlays[0].toolbar_visible, "the toolbar shows during the countdown");
    save_png(&frame(&mut s, 0)?, out, "overlay-countdown.png")?;
    s.on_timer(Instant::now() + Duration::from_secs(1));
    check!(s.st.countdown == Some(1));
    s.on_timer(Instant::now() + Duration::from_secs(3));
    match s.take_outcome() {
        Some(Outcome::Record(job)) => {
            check!(job.area == IRect::new(200, 200, 500, 300) && job.fps == 60 && job.shot.mode == "region", "{job:?}");
            check!(job.thumbnail.size() == (500, 300));
        }
        other => return Err(format!("recording gave {other:?}")),
    }
    Ok(())
}

/// Two monitors side by side: the toolbar follows the pointer, a selection
/// crosses from one to the other and is cut from both pictures.
fn multi_monitor(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let left = Arc::new(desktop.crop(IRect::new(0, 0, 800, 1000))?);
    let right = Arc::new(desktop.crop(IRect::new(800, 0, 800, 1000))?);
    let screens = vec![
        Screen { output: Output { name: "DP-1".into(), rect: IRect::new(0, 0, 800, 1000), scale: 1.0 }, picture: left, pointer: None },
        Screen { output: Output { name: "DP-2".into(), rect: IRect::new(800, 0, 800, 1000), scale: 1.0 }, picture: right, pointer: None },
    ];
    let mut s = session(test_config(tmp), Request::default(), screens, FakeRecorder { problem: None, span: true });
    check!(s.st.spans, "a selection can't cross monitors");
    s.pointer_enter(1, Point::new(400.0, 500.0));
    check!(s.st.toolbar_overlay == Some(1) && s.overlays[1].toolbar_visible && !s.overlays[0].toolbar_visible, "the toolbar didn't follow");
    // Draw on the left monitor, then select across both.
    s.pointer_enter(0, Point::new(400.0, 500.0));
    tap(&mut s, 0, 0x42, "b", 0);
    drag(&mut s, 0, Point::new(700.0, 300.0), Point::new(790.0, 400.0), 0);
    tap(&mut s, 0, 0x52, "r", 0);
    s.pointer_motion(0, Point::new(600.0, 200.0), 0, false);
    s.button(0, Button::Left, true, Point::new(600.0, 200.0), 0);
    // The drag goes on past the right edge (the pointer is still grabbed here).
    s.pointer_motion(0, Point::new(1000.0, 600.0), 0, false);
    check!(s.overlays[1].span_rect == Some(Rect::new(-200.0, 200.0, 400.0, 400.0)), "{:?}", s.overlays[1].span_rect);
    let f = frame(&mut s, 1)?;
    save_png(&f, out, "overlay-span.png")?;
    s.button(0, Button::Left, false, Point::new(1000.0, 600.0), 0);
    match s.take_outcome() {
        Some(Outcome::Image { image, at, .. }) => {
            check!(image.size() == (400, 400) && at == Some(IRect::new(600, 200, 400, 400)), "{:?} {at:?}", image.size());
            // Pixels from both pictures, as in the desktop.
            check!(image.pixel(10, 300) == desktop.pixel(610, 500) && image.pixel(390, 10) == desktop.pixel(990, 210));
            save_png(&image, out, "result-span.png")?;
        }
        other => return Err(format!("the selection across monitors gave {other:?}")),
    }
    Ok(())
}

/// A monitor at 2x: picture pixels are twice the logical ones; the drawings
/// and the capture follow.
fn hidpi(tmp: &Path, out: &Option<PathBuf>, desktop: Arc<Image>) -> R {
    let screen = Screen {
        output: Output { name: "eDP-1".into(), rect: IRect::new(0, 0, 800, 500), scale: 2.0 },
        picture: desktop.clone(),
        pointer: Some((IPoint::new(400, 400), Arc::new(Image::filled(24, 36, 0xFFFF0000)?))),
    };
    let mut s = Session::new(
        Setup {
            cfg: test_config(tmp),
            request: Request::default(),
            screens: vec![screen],
            pointer_taken: true,
            cursor: Some(Point::new(300.0, 200.0)),
            primary: 0,
            can_record: true,
        },
        Box::new(FakeRecorder { problem: None, span: false }),
        no_wake(),
    );
    check!(s.overlays[0].dpr == 2.0 && s.overlays[0].device_size() == (1600, 1000));
    check!(s.overlays[0].cursor_pos == Some(Point::new(300.0, 200.0)), "the pointer's place wasn't used");
    // The pointer, shown and hidden.
    check!(s.st.pointer_available);
    tap(&mut s, 0, 0x4D, "m", 0);
    check!(s.st.show_pointer && s.overlays[0].base.pixel(410, 410) == 0xFFFF0000, "the pointer isn't shown");
    tap(&mut s, 0, 0x4D, "m", 0);
    check!(!s.st.show_pointer && s.overlays[0].base.pixel(410, 410) == desktop.pixel(410, 410) && s.overlays[0].with_pointer.is_none());
    tap(&mut s, 0, 0x42, "b", 0);
    drag(&mut s, 0, Point::new(100.0, 100.0), Point::new(300.0, 200.0), 0);
    save_png(&frame(&mut s, 0)?, out, "overlay-hidpi.png")?;
    tap(&mut s, 0, 0x52, "r", 0);
    drag(&mut s, 0, Point::new(50.0, 50.0), Point::new(350.0, 250.0), 0);
    match s.take_outcome() {
        Some(Outcome::Image { image, at, shot, .. }) => {
            check!(image.size() == (600, 400) && at == Some(IRect::new(50, 50, 300, 200)) && shot.monitor == "eDP-1", "{:?} {at:?}", image.size());
            // The box's stroke is in picture pixels: at 2x, where 100, 100 is.
            let (r, g, b, _) = image::unpack(image.pixel(100, 100));
            let ember = theme::SWATCHES[0];
            check!((r, g, b) == (ember.r, ember.g, ember.b), "{:?}", (r, g, b));
            save_png(&image, out, "result-hidpi.png")?;
        }
        other => return Err(format!("the 2x capture gave {other:?}")),
    }
    let _ = keyseq::MODS;
    Ok(())
}
