//! Pointer and keyboard input for an X server, through XTEST, for tests on
//! a headless one (Xvfb): `xinject move 500 400 drag 100 100 300 200 key
//! ctrl+z type hello sleep 200 click 10 10 rclick 5 5 scroll 1`.

use std::time::Duration;

use x11rb::connection::Connection;
use x11rb::protocol::xproto::ConnectionExt as _;
use x11rb::protocol::xtest::ConnectionExt as _;
use x11rb::wrapper::ConnectionExt as _;
use xkbcommon::xkb;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let (conn, screen) = x11rb::connect(None).expect("X server");
    let root = conn.setup().roots[screen].root;
    let (min, max) = (conn.setup().min_keycode, conn.setup().max_keycode);
    let map = conn.get_keyboard_mapping(min, max - min + 1).unwrap().reply().unwrap();
    let per = map.keysyms_per_keycode as usize;
    // keysym -> (keycode, needs shift)
    let find = |sym: u32| -> Option<(u8, bool)> {
        map.keysyms.chunks(per).enumerate().find_map(|(i, syms)| {
            let level = syms.iter().take(2).position(|s| *s == sym)?;
            Some((min + i as u8, level == 1))
        })
    };
    let sync = || {
        conn.sync().unwrap();
        std::thread::sleep(Duration::from_millis(20));
    };
    let fake = |kind: u8, detail: u8, x: i16, y: i16| {
        conn.xtest_fake_input(kind, detail, 0, root, x, y, 0).unwrap();
    };
    let motion = |x: f64, y: f64| fake(6, 0, x.round() as i16, y.round() as i16);
    let button = |b: u8, down: bool| fake(if down { 4 } else { 5 }, b, 0, 0);
    let key = |code: u8, down: bool| fake(if down { 2 } else { 3 }, code, 0, 0);
    let mod_code = |name: &str| find(xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw()).map(|k| k.0).expect("modifier key");
    let (shift, ctrl, alt) = (mod_code("Shift_L"), mod_code("Control_L"), mod_code("Alt_L"));
    let mut i = 0;
    let num = |i: usize| args[i].parse::<f64>().expect("number");
    while i < args.len() {
        match args[i].as_str() {
            "move" => {
                motion(num(i + 1), num(i + 2));
                sync();
                i += 3;
            }
            "click" | "rclick" => {
                motion(num(i + 1), num(i + 2));
                sync();
                let b = if args[i] == "click" { 1 } else { 3 };
                button(b, true);
                sync();
                button(b, false);
                sync();
                i += 3;
            }
            "drag" => {
                let (x1, y1, x2, y2) = (num(i + 1), num(i + 2), num(i + 3), num(i + 4));
                motion(x1, y1);
                sync();
                button(1, true);
                sync();
                for k in 1..=8 {
                    let f = k as f64 / 8.0;
                    motion(x1 + (x2 - x1) * f, y1 + (y2 - y1) * f);
                    sync();
                }
                button(1, false);
                sync();
                i += 5;
            }
            "down" | "up" => {
                button(1, args[i] == "down");
                sync();
                i += 1;
            }
            "scroll" => {
                let b = if num(i + 1) > 0.0 { 4 } else { 5 };
                button(b, true);
                button(b, false);
                sync();
                i += 2;
            }
            "key" => {
                let mut held = Vec::new();
                let mut name = args[i + 1].as_str();
                while let Some((m, rest)) = name.split_once('+') {
                    held.push(match m {
                        "ctrl" => ctrl,
                        "shift" => shift,
                        "alt" => alt,
                        _ => panic!("modifier {m}"),
                    });
                    name = rest;
                }
                let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS).raw();
                let (code, needs_shift) = find(sym).unwrap_or_else(|| panic!("no key {name}"));
                if needs_shift && !held.contains(&shift) {
                    held.push(shift);
                }
                for m in &held {
                    key(*m, true);
                }
                key(code, true);
                key(code, false);
                for m in held.iter().rev() {
                    key(*m, false);
                }
                sync();
                i += 2;
            }
            "type" => {
                for ch in args[i + 1].chars() {
                    let sym = xkb::utf32_to_keysym(ch as u32).raw();
                    let (code, needs_shift) = find(sym).unwrap_or_else(|| panic!("no key for {ch:?}"));
                    if needs_shift {
                        key(shift, true);
                    }
                    key(code, true);
                    key(code, false);
                    if needs_shift {
                        key(shift, false);
                    }
                    sync();
                }
                i += 2;
            }
            "sleep" => {
                std::thread::sleep(Duration::from_millis(num(i + 1) as u64));
                i += 2;
            }
            other => panic!("unknown command {other}"),
        }
    }
}
