//! Reads the X11 clipboard as another program would, for tests: `xpaste
//! targets` lists its types, `xpaste image/png > out.png` writes one (big
//! data comes in pieces, INCR, as ICCCM has it).

use std::io::Write;
use std::time::{Duration, Instant};

use x11rb::connection::Connection;
use x11rb::protocol::Event;
use x11rb::protocol::xproto::{AtomEnum, ConnectionExt as _, CreateWindowAux, EventMask, WindowClass};

fn main() {
    let what = std::env::args().nth(1).unwrap_or_else(|| "targets".into());
    let (conn, screen) = x11rb::connect(None).expect("X server");
    let root = conn.setup().roots[screen].root;
    let atom = |name: &str| conn.intern_atom(false, name.as_bytes()).unwrap().reply().unwrap().atom;
    let (clipboard, property, incr) = (atom("CLIPBOARD"), atom("XPASTE"), atom("INCR"));
    let target = atom(&what);
    let win = conn.generate_id().unwrap();
    let aux = CreateWindowAux::new().event_mask(EventMask::PROPERTY_CHANGE);
    conn.create_window(0, win, root, 0, 0, 1, 1, 0, WindowClass::INPUT_ONLY, 0, &aux).unwrap();
    conn.convert_selection(win, clipboard, target, property, x11rb::CURRENT_TIME).unwrap();
    conn.flush().unwrap();
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut data = Vec::new();
    let mut incremental = false;
    let mut kind = 0;
    while Instant::now() < deadline {
        let Some(ev) = conn.poll_for_event().unwrap() else {
            std::thread::sleep(Duration::from_millis(5));
            continue;
        };
        let read = match ev {
            Event::SelectionNotify(e) if e.property == x11rb::NONE => {
                eprintln!("xpaste: no {what} on the clipboard");
                std::process::exit(1);
            }
            Event::SelectionNotify(_) => true,
            Event::PropertyNotify(e) => incremental && e.atom == property && e.state == x11rb::protocol::xproto::Property::NEW_VALUE,
            _ => false,
        };
        if !read {
            continue;
        }
        let r = conn.get_property(true, win, property, AtomEnum::ANY, 0, u32::MAX / 4).unwrap().reply().unwrap();
        if !incremental && r.type_ == incr {
            incremental = true;
            eprintln!("xpaste: in pieces");
            continue;
        }
        kind = r.type_;
        if incremental && !r.value.is_empty() {
            data.extend_from_slice(&r.value);
            continue;
        }
        data.extend_from_slice(&r.value);
        break;
    }
    if what == "targets" || what == "TARGETS" {
        let atoms = data.chunks(4).map(|c| u32::from_ne_bytes([c[0], c[1], c[2], c[3]]));
        for a in atoms {
            println!("{}", String::from_utf8_lossy(&conn.get_atom_name(a).unwrap().reply().unwrap().name));
        }
        return;
    }
    let kind = String::from_utf8_lossy(&conn.get_atom_name(kind).map(|c| c.reply().map(|r| r.name).unwrap_or_default()).unwrap_or_default()).into_owned();
    eprintln!("xpaste: {} bytes of {kind}", data.len());
    std::io::stdout().write_all(&data).unwrap();
}
