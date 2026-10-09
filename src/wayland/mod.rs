//! Wayland: the connection, the monitors, the seat, and the surfaces
//! Flatshot shows. Everything is drawn by Flatshot itself into shared
//! memory buffers (no GPU context to set up, nothing to wait for), and only
//! the parts that changed are redrawn, once per frame of the display.

pub mod clipboard;
pub mod handlers;
pub mod protocols;
pub mod screencopy;

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use smithay_client_toolkit::compositor::{CompositorState, FrameCallbackData};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::seat::pointer::ThemedPointer;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface};
use smithay_client_toolkit::shell::xdg::XdgShell;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowDecorations};
use smithay_client_toolkit::shm::Shm;
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_keyboard, wl_output, wl_shm, wl_surface, wl_touch};
use wayland_client::{Connection, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

use crate::app::App;
use crate::geom::{Damage, IRect};
use crate::image::{Canvas, Image};
use protocols::{BufferData, FractionalData, NoEvents, TextInputData};

/// A monitor, as the compositor describes it.
#[derive(Clone, Debug)]
pub struct Monitor {
    pub output: wl_output::WlOutput,
    /// connector name: DP-1, HDMI-A-1, ...
    pub name: String,
    /// where it is in the desktop, in logical pixels
    pub rect: IRect,
    /// its integer scale (the real one may be fractional)
    pub scale: i32,
}

/// Whose a surface is.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Owner {
    /// the capture overlay on the session's monitor `.0`
    Overlay(usize),
}

/// Something drawn on surfaces: what changed, and how to draw a part of it.
pub trait Painted {
    /// What changed on part `part` since it was last drawn (device pixels).
    fn damage(&mut self, part: usize) -> Damage;
    /// Draw `region` (device pixels) of part `part` into `target`, whose
    /// top left is at `origin` (device pixels).
    fn paint(&self, part: usize, target: &mut Canvas<'_>, origin: (i32, i32), region: IRect);
    /// The size of part `part`'s buffer, in device pixels.
    fn buffer_size(&self, part: usize) -> (u32, u32);
}

struct Buf {
    img: Image,
    buffer: wl_buffer::WlBuffer,
    busy: Arc<AtomicBool>,
    /// what changed while this buffer was elsewhere (it still shows the old)
    stale: Damage,
    /// when it was last drawn (the newest one holds the latest frame)
    drawn: u64,
}

impl Drop for Buf {
    fn drop(&mut self) {
        self.buffer.destroy();
    }
}

pub struct Surf {
    pub owner: Owner,
    pub wl: wl_surface::WlSurface,
    pub layer: Option<LayerSurface>,
    pub window: Option<Window>,
    viewport: Option<wp_viewport::WpViewport>,
    fractional: Option<wp_fractional_scale_v1::WpFractionalScaleV1>,
    /// the size the compositor gave it, in logical pixels
    pub logical: (u32, u32),
    pub preferred_scale: Option<f32>,
    pub configured: bool,
    frame_pending: bool,
    bufs: Vec<Buf>,
    frames: u64,
    /// where a damaged part is drawn on its own (clipped to it), then copied in
    scratch: Vec<u8>,
    /// the pointer's look last set here
    pub cursor: Option<smithay_client_toolkit::seat::pointer::CursorIcon>,
}

impl Drop for Surf {
    fn drop(&mut self) {
        self.bufs.clear();
        if let Some(v) = self.viewport.take() {
            v.destroy();
        }
        if let Some(f) = self.fractional.take() {
            f.destroy();
        }
        // (the layer surface or window is destroyed with its wrapper, then the surface)
        self.layer.take();
        self.window.take();
        self.wl.destroy();
    }
}

pub struct Wayland {
    pub conn: Connection,
    pub qh: QueueHandle<App>,
    pub registry: RegistryState,
    pub outputs: OutputState,
    pub seats: SeatState,
    pub compositor: CompositorState,
    pub shm: Shm,
    pub layer_shell: Option<LayerShell>,
    pub xdg_shell: Option<XdgShell>,
    pub viewporter: Option<wp_viewporter::WpViewporter>,
    pub fractional: Option<wp_fractional_scale_manager_v1::WpFractionalScaleManagerV1>,
    pub text_input_manager: Option<zwp_text_input_manager_v3::ZwpTextInputManagerV3>,
    pub screencopy: Option<zwlr_screencopy_manager_v1::ZwlrScreencopyManagerV1>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    pub pointer: Option<ThemedPointer>,
    pub touch: Option<wl_touch::WlTouch>,
    pub text_input: Option<zwp_text_input_v3::ZwpTextInputV3>,
    /// keyseq modifier bits held now
    pub mods: i32,
    /// the latest input event's serial (for the clipboard)
    pub serial: u32,
    pub pointer_on: Option<ObjectId>,
    pub keyboard_on: Option<ObjectId>,
    /// fingers down: (surface, where)
    pub touches: HashMap<i32, (ObjectId, (f64, f64))>,
    pub text_focus: Option<ObjectId>,
    pub text_enabled: bool,
    pub pending_text: String,
    pub surfaces: Vec<Surf>,
    pub buffer_released: bool,
    pub captures: Vec<screencopy::Grab>,
    pub clipboard: clipboard::Clipboard,
}

impl Wayland {
    /// Connect to the compositor and bind what Flatshot uses.
    pub fn connect() -> Result<(Wayland, EventQueue<App>), String> {
        let conn = Connection::connect_to_env().map_err(|e| format!("cannot connect to the Wayland compositor: {e}"))?;
        let (globals, queue) = registry_queue_init::<App>(&conn).map_err(|e| format!("Wayland registry: {e}"))?;
        let qh = queue.handle();
        let compositor = CompositorState::bind(&globals, &qh).map_err(|e| format!("wl_compositor: {e}"))?;
        let shm = Shm::bind(&globals, &qh).map_err(|e| format!("wl_shm: {e}"))?;
        let wl = Wayland {
            registry: RegistryState::new(&globals),
            outputs: OutputState::new(&globals, &qh),
            seats: SeatState::new(&globals, &qh),
            compositor,
            shm,
            layer_shell: LayerShell::bind(&globals, &qh).ok(),
            xdg_shell: XdgShell::bind(&globals, &qh).ok(),
            viewporter: globals.bind(&qh, 1..=1, NoEvents).ok(),
            fractional: globals.bind(&qh, 1..=1, NoEvents).ok(),
            text_input_manager: globals.bind(&qh, 1..=1, NoEvents).ok(),
            screencopy: globals.bind(&qh, 1..=3, NoEvents).ok(),
            keyboard: None,
            pointer: None,
            touch: None,
            text_input: None,
            mods: 0,
            serial: 0,
            pointer_on: None,
            keyboard_on: None,
            touches: HashMap::new(),
            text_focus: None,
            text_enabled: false,
            pending_text: String::new(),
            surfaces: Vec::new(),
            buffer_released: false,
            captures: Vec::new(),
            clipboard: clipboard::Clipboard::bind(&globals, &qh),
            conn,
            qh,
        };
        Ok((wl, queue))
    }

    /// The monitors, as the compositor described them (after a roundtrip).
    pub fn monitors(&self) -> Vec<Monitor> {
        let mut out = Vec::new();
        for output in self.outputs.outputs() {
            let Some(info) = self.outputs.info(&output) else { continue };
            let scale = info.scale_factor.max(1);
            let (x, y) = info.logical_position.unwrap_or(info.location);
            let (w, h) = match info.logical_size {
                Some(s) => s,
                None => {
                    // No xdg-output: the mode in pixels, over the scale, turned.
                    let Some(m) = info.modes.iter().find(|m| m.current).or(info.modes.first()) else { continue };
                    let (mw, mh) = (m.dimensions.0 / scale, m.dimensions.1 / scale);
                    use wl_output::Transform as T;
                    match info.transform {
                        T::_90 | T::_270 | T::Flipped90 | T::Flipped270 => (mh, mw),
                        _ => (mw, mh),
                    }
                }
            };
            if w <= 0 || h <= 0 {
                continue;
            }
            let name = info.name.clone().unwrap_or_else(|| format!("{}-{}", info.make, info.id));
            out.push(Monitor { output, name, rect: IRect::new(x, y, w, h), scale });
        }
        out
    }

    /// A fullscreen surface over `monitor` for the capture overlay: a
    /// layer-shell overlay (shown at once, above panels, no animations),
    /// else a fullscreen window.
    pub fn add_overlay(&mut self, owner: Owner, monitor: &Monitor, namespace: &str) -> Result<(), String> {
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        let mut layer = None;
        let mut window = None;
        if let Some(ls) = &self.layer_shell {
            let l = ls.create_layer_surface(qh, surface.clone(), Layer::Overlay, Some(namespace), Some(&monitor.output));
            l.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            l.set_exclusive_zone(-1);
            l.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            l.set_size(0, 0);
            layer = Some(l);
        } else if let Some(xdg) = &self.xdg_shell {
            let w = xdg.create_window(surface.clone(), WindowDecorations::None, qh);
            w.set_title("Flatshot");
            w.set_app_id("flatshot");
            w.set_fullscreen(Some(&monitor.output));
            window = Some(w);
        } else {
            surface.destroy();
            return Err("the compositor offers neither layer-shell nor xdg-shell".into());
        }
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(&surface, qh, NoEvents));
        let fractional = self.fractional.as_ref().map(|f| f.get_fractional_scale(&surface, qh, FractionalData(surface.id())));
        if let Some(l) = &layer {
            l.commit();
        }
        if let Some(w) = &window {
            w.commit();
        }
        self.surfaces.push(Surf {
            owner,
            wl: surface,
            layer,
            window,
            viewport,
            fractional,
            logical: (monitor.rect.w as u32, monitor.rect.h as u32),
            preferred_scale: None,
            configured: false,
            frame_pending: false,
            bufs: Vec::new(),
            frames: 0,
            scratch: Vec::new(),
            cursor: None,
        });
        Ok(())
    }

    pub fn surface_index(&self, id: &ObjectId) -> Option<usize> {
        self.surfaces.iter().position(|s| &s.wl.id() == id)
    }

    pub fn owner_of(&self, id: &ObjectId) -> Option<Owner> {
        self.surfaces.iter().find(|s| &s.wl.id() == id).map(|s| s.owner)
    }

    /// Take down every surface of the capture overlay.
    pub fn remove_overlays(&mut self) {
        self.surfaces.retain(|s| !matches!(s.owner, Owner::Overlay(_)));
        if let Some(t) = &self.text_input
            && self.text_enabled
        {
            t.disable();
            t.commit();
            self.text_enabled = false;
        }
        let _ = self.conn.flush();
    }

    /// A frame was shown on `surface`: it can be drawn again.
    pub fn frame_done(&mut self, surface: &wl_surface::WlSurface) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| &s.wl == surface) {
            s.frame_pending = false;
        }
    }

    /// Draw surface `i` (part `part` of `ui`) if it can be drawn now: it's
    /// configured, the last frame was shown, and a buffer is free. Only
    /// what changed is drawn. Returns whether something was committed.
    pub fn redraw(&mut self, i: usize, part: usize, ui: &mut dyn Painted) -> bool {
        let qh = self.qh.clone();
        let shm = self.shm.wl_shm().clone();
        let have_viewport = self.viewporter.is_some();
        let s = &mut self.surfaces[i];
        if !s.configured || s.frame_pending {
            return false;
        }
        let (w, h) = ui.buffer_size(part);
        if w == 0 || h == 0 {
            return false;
        }
        // Buffers of another size are of no more use (once let go).
        s.bufs.retain(|b| b.busy.load(Ordering::Acquire) || (b.img.width(), b.img.height()) == (w, h));
        let free = s.bufs.iter().position(|b| !b.busy.load(Ordering::Acquire) && (b.img.width(), b.img.height()) == (w, h));
        let idx = match free {
            Some(idx) => idx,
            None if s.bufs.len() < 2 => match new_buffer(&shm, &qh, w, h) {
                Ok(mut b) => {
                    // A second buffer starts as a copy of the latest frame
                    // (far cheaper than drawing it all again).
                    if let Some(latest) = s.bufs.iter().filter(|o| (o.img.width(), o.img.height()) == (w, h)).max_by_key(|o| o.drawn)
                        && latest.stale.is_empty()
                    {
                        b.img.data_mut().copy_from_slice(latest.img.data());
                        b.stale = Damage::new();
                    }
                    s.bufs.push(b);
                    s.bufs.len() - 1
                }
                Err(e) => {
                    crate::warn!("no buffer for the overlay: {e}");
                    return false;
                }
            },
            None => return false, // (all in use: drawn when one is let go)
        };
        let damage = ui.damage(part);
        let mut todo = std::mem::take(&mut s.bufs[idx].stale);
        todo.add_all(&damage);
        for (j, b) in s.bufs.iter_mut().enumerate() {
            if j != idx {
                b.stale.add_all(&damage);
            }
        }
        if todo.is_empty() {
            return false;
        }
        let full = IRect::new(0, 0, w as i32, h as i32);
        let rects = todo.rects_in(full);
        let buf = &mut s.bufs[idx];
        let started = std::time::Instant::now();
        for r in &rects {
            if *r == full {
                ui.paint(part, &mut buf.img.canvas(), (0, 0), *r);
                continue;
            }
            // A part: drawn on its own (everything outside it clipped
            // away), then copied into place.
            let len = r.w as usize * r.h as usize * 4;
            if s.scratch.len() < len {
                s.scratch.resize(len, 0);
            }
            let Some(mut c) = Canvas::new(&mut s.scratch, r.w as u32, r.h as u32) else { continue };
            ui.paint(part, &mut c, (r.x, r.y), *r);
            let (src, stride) = (c.data(), r.w as usize * 4);
            let dst_stride = buf.img.stride();
            let data = buf.img.data_mut();
            for row in 0..r.h as usize {
                let at = (r.y as usize + row) * dst_stride + r.x as usize * 4;
                data[at..at + stride].copy_from_slice(&src[row * stride..(row + 1) * stride]);
            }
        }
        if crate::timing::enabled() && todo.is_full() {
            crate::timing::log(format!("painted {w} × {h} in {:.1} ms", started.elapsed().as_secs_f64() * 1000.0));
        }
        for r in &rects {
            s.wl.damage_buffer(r.x, r.y, r.w, r.h);
        }
        match &s.viewport {
            Some(vp) => vp.set_destination(s.logical.0 as i32, s.logical.1 as i32),
            None if !have_viewport => {
                // Without a viewport, only whole scales fit a buffer to the screen.
                let scale = (w as f32 / s.logical.0.max(1) as f32).round().max(1.0) as i32;
                s.wl.set_buffer_scale(scale);
            }
            None => {}
        }
        s.wl.frame(&qh, FrameCallbackData(s.wl.clone()));
        s.wl.attach(Some(&buf.buffer), 0, 0);
        buf.busy.store(true, Ordering::Release);
        s.frames += 1;
        buf.drawn = s.frames;
        s.wl.commit();
        s.frame_pending = true;
        true
    }

    /// Turn the input method on for the text being typed (with the caret
    /// at `caret`, logical, on the focused surface), or off.
    pub fn set_text_input(&mut self, caret: Option<crate::geom::Rect>) {
        let Some(t) = &self.text_input else { return };
        match caret {
            Some(r) if self.text_focus.is_some() => {
                if !self.text_enabled {
                    t.enable();
                    t.set_content_type(zwp_text_input_v3::ContentHint::Spellcheck, zwp_text_input_v3::ContentPurpose::Normal);
                    self.text_enabled = true;
                }
                t.set_cursor_rectangle(r.x.round() as i32, r.y.round() as i32, r.w.round().max(1.0) as i32, r.h.round() as i32);
                t.commit();
            }
            None if self.text_enabled => {
                t.disable();
                t.commit();
                self.text_enabled = false;
            }
            _ => {}
        }
    }

    pub fn create_text_input(&mut self, seat: &wayland_client::protocol::wl_seat::WlSeat) {
        if self.text_input.is_none()
            && let Some(m) = &self.text_input_manager
        {
            self.text_input = Some(m.get_text_input(seat, &self.qh, TextInputData));
        }
    }
}

/// A buffer of `w` x `h` in its own shared memory.
fn new_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<App>, w: u32, h: u32) -> Result<Buf, String> {
    let img = Image::shared(w, h)?;
    let fd = img.fd().ok_or("not shared memory")?;
    let len = img.byte_len() as i32;
    let pool = shm.create_pool(fd, len, qh, NoEvents);
    let busy = Arc::new(AtomicBool::new(false));
    // The overlay is opaque: XRGB lets the compositor skip blending it.
    let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, wl_shm::Format::Xrgb8888, qh, BufferData(busy.clone()));
    pool.destroy(); // (the buffer keeps the memory)
    Ok(Buf { img, buffer, busy, stale: Damage::full(), drawn: 0 })
}
