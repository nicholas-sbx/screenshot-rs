//! Drive a (headless) wlroots compositor's seat for tests: a virtual
//! pointer and keyboard.
//!
//! inject move X Y | click X Y | rclick X Y | drag X1 Y1 X2 Y2 | down | up |
//!        key NAME (Escape, Return, BackSpace, a, ctrl+z, shift+Return ...) |
//!        type TEXT | scroll N | sleep MS ...

use std::io::Write;
use std::os::fd::AsFd;
use std::time::{Duration, Instant};

use wayland_client::globals::{GlobalListContents, registry_queue_init};
use wayland_client::protocol::{wl_output, wl_pointer, wl_registry, wl_seat};
use wayland_client::{Connection, Dispatch, QueueHandle, delegate_noop};
use wayland_protocols_misc::zwp_virtual_keyboard_v1::client::{zwp_virtual_keyboard_manager_v1, zwp_virtual_keyboard_v1};
use wayland_protocols_wlr::virtual_pointer::v1::client::{zwlr_virtual_pointer_manager_v1, zwlr_virtual_pointer_v1};
use xkbcommon::xkb;

struct State {
    extent: (u32, u32),
}

impl Dispatch<wl_registry::WlRegistry, GlobalListContents> for State {
    fn event(_: &mut Self, _: &wl_registry::WlRegistry, _: wl_registry::Event, _: &GlobalListContents, _: &Connection, _: &QueueHandle<Self>) {}
}

impl Dispatch<wl_output::WlOutput, ()> for State {
    fn event(state: &mut Self, _: &wl_output::WlOutput, event: wl_output::Event, _: &(), _: &Connection, _: &QueueHandle<Self>) {
        if let wl_output::Event::Mode { width, height, .. } = event {
            state.extent = (state.extent.0.max(width as u32), state.extent.1.max(height as u32));
        }
    }
}

delegate_noop!(State: ignore wl_seat::WlSeat);
delegate_noop!(State: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1);
delegate_noop!(State: zwlr_virtual_pointer_v1::ZwlrVirtualPointerV1);
delegate_noop!(State: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1);
delegate_noop!(State: zwp_virtual_keyboard_v1::ZwpVirtualKeyboardV1);

const BTN_LEFT: u32 = 0x110;
const BTN_RIGHT: u32 = 0x111;

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let conn = Connection::connect_to_env().expect("wayland");
    let (globals, mut queue) = registry_queue_init::<State>(&conn).expect("registry");
    let qh = queue.handle();
    let mut state = State { extent: (0, 0) };
    let seat: wl_seat::WlSeat = globals.bind(&qh, 1..=7, ()).expect("seat");
    let _output: wl_output::WlOutput = globals.bind(&qh, 1..=4, ()).expect("output");
    let pm: zwlr_virtual_pointer_manager_v1::ZwlrVirtualPointerManagerV1 = globals.bind(&qh, 1..=2, ()).expect("virtual pointer");
    let km: zwp_virtual_keyboard_manager_v1::ZwpVirtualKeyboardManagerV1 = globals.bind(&qh, 1..=1, ()).expect("virtual keyboard");
    queue.roundtrip(&mut state).unwrap();
    let pointer = pm.create_virtual_pointer(Some(&seat), &qh, ());
    let keyboard = km.create_virtual_keyboard(&seat, &qh, ());

    let ctx = xkb::Context::new(xkb::CONTEXT_NO_FLAGS);
    let keymap = xkb::Keymap::new_from_names(&ctx, "", "", "us", "", None, xkb::KEYMAP_COMPILE_NO_FLAGS).expect("keymap");
    let text = keymap.get_as_string(xkb::KEYMAP_FORMAT_TEXT_V1);
    let fd = rustix::fs::memfd_create("keymap", rustix::fs::MemfdFlags::CLOEXEC).unwrap();
    let mut file = std::fs::File::from(fd);
    file.write_all(text.as_bytes()).unwrap();
    file.write_all(&[0]).unwrap();
    keyboard.keymap(1, file.as_fd(), text.len() as u32 + 1);
    let shift = 1u32 << keymap.mod_get_index("Shift");
    let ctrl = 1u32 << keymap.mod_get_index("Control");
    let alt = 1u32 << keymap.mod_get_index("Mod1");
    let start = Instant::now();
    let t = || start.elapsed().as_millis() as u32;
    let (ew, eh) = state.extent;

    // keysym name -> (evdev code, needs shift)
    let find = |sym: xkb::Keysym| -> Option<(u32, bool)> {
        let mut found = None;
        keymap.key_for_each(|km, code| {
            if found.is_some() {
                return;
            }
            for level in 0..2 {
                if km.key_get_syms_by_level(code, 0, level).contains(&sym) {
                    found = Some((code.raw() - 8, level == 1));
                    return;
                }
            }
        });
        found
    };
    let send_key = |code: u32, mods: u32| {
        keyboard.modifiers(mods, 0, 0, 0);
        keyboard.key(t(), code, 1);
        keyboard.key(t(), code, 0);
        keyboard.modifiers(0, 0, 0, 0);
    };
    let motion = |x: f64, y: f64| {
        pointer.motion_absolute(t(), x.round() as u32, y.round() as u32, ew, eh);
        pointer.frame();
    };
    let button = |b: u32, down: bool| {
        pointer.button(t(), b, if down { wl_pointer::ButtonState::Pressed } else { wl_pointer::ButtonState::Released });
        pointer.frame();
    };
    let flush = |queue: &mut wayland_client::EventQueue<State>, state: &mut State| {
        queue.roundtrip(state).unwrap();
        std::thread::sleep(Duration::from_millis(30));
    };

    let mut i = 0;
    let num = |i: usize| args[i].parse::<f64>().expect("number");
    while i < args.len() {
        match args[i].as_str() {
            "move" => {
                motion(num(i + 1), num(i + 2));
                i += 3;
            }
            "click" | "rclick" => {
                motion(num(i + 1), num(i + 2));
                flush(&mut queue, &mut state);
                let b = if args[i] == "click" { BTN_LEFT } else { BTN_RIGHT };
                button(b, true);
                flush(&mut queue, &mut state);
                button(b, false);
                i += 3;
            }
            "drag" => {
                let (x1, y1, x2, y2) = (num(i + 1), num(i + 2), num(i + 3), num(i + 4));
                motion(x1, y1);
                flush(&mut queue, &mut state);
                button(BTN_LEFT, true);
                for k in 1..=8 {
                    let f = k as f64 / 8.0;
                    motion(x1 + (x2 - x1) * f, y1 + (y2 - y1) * f);
                    flush(&mut queue, &mut state);
                }
                button(BTN_LEFT, false);
                i += 5;
            }
            "down" => {
                button(BTN_LEFT, true);
                i += 1;
            }
            "up" => {
                button(BTN_LEFT, false);
                i += 1;
            }
            "scroll" => {
                pointer.axis_discrete(t(), wl_pointer::Axis::VerticalScroll, num(i + 1) * 15.0, num(i + 1) as i32);
                pointer.frame();
                i += 2;
            }
            "key" => {
                let mut mods = 0;
                let mut name = args[i + 1].as_str();
                while let Some((m, rest)) = name.split_once('+') {
                    mods |= match m {
                        "ctrl" => ctrl,
                        "shift" => shift,
                        "alt" => alt,
                        _ => panic!("modifier {m}"),
                    };
                    name = rest;
                }
                let sym = xkb::keysym_from_name(name, xkb::KEYSYM_NO_FLAGS);
                let (code, needs_shift) = find(sym).unwrap_or_else(|| panic!("no key {name}"));
                send_key(code, mods | if needs_shift { shift } else { 0 });
                i += 2;
            }
            "type" => {
                for ch in args[i + 1].chars() {
                    let sym = xkb::utf32_to_keysym(ch as u32);
                    let (code, needs_shift) = find(sym).unwrap_or_else(|| panic!("no key for {ch:?}"));
                    send_key(code, if needs_shift { shift } else { 0 });
                    flush(&mut queue, &mut state);
                }
                i += 2;
            }
            "sleep" => {
                std::thread::sleep(Duration::from_millis(num(i + 1) as u64));
                i += 2;
            }
            other => panic!("unknown command {other}"),
        }
        flush(&mut queue, &mut state);
    }
}
