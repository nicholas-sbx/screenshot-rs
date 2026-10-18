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

use smithay_client_toolkit::activation::{ActivationState, RequestData};
use smithay_client_toolkit::compositor::{CompositorState, FrameCallbackData};
use smithay_client_toolkit::output::OutputState;
use smithay_client_toolkit::registry::RegistryState;
use smithay_client_toolkit::seat::SeatState;
use smithay_client_toolkit::seat::pointer::ThemedPointer;
use smithay_client_toolkit::shell::WaylandSurface;
use smithay_client_toolkit::shell::wlr_layer::{Anchor, KeyboardInteractivity, Layer, LayerShell, LayerSurface};
use smithay_client_toolkit::shell::xdg::popup::Popup;
use smithay_client_toolkit::shell::xdg::window::{Window, WindowDecorations};
use smithay_client_toolkit::shell::xdg::{XdgPositioner, XdgShell, XdgSurface};
use smithay_client_toolkit::shm::Shm;
use wayland_client::backend::ObjectId;
use wayland_client::globals::registry_queue_init;
use wayland_client::protocol::{wl_buffer, wl_keyboard, wl_output, wl_shm, wl_surface, wl_touch};
use wayland_client::{Connection, EventQueue, Proxy, QueueHandle};
use wayland_protocols::wp::fractional_scale::v1::client::{wp_fractional_scale_manager_v1, wp_fractional_scale_v1};
use wayland_protocols::wp::relative_pointer::zv1::client::{zwp_relative_pointer_manager_v1, zwp_relative_pointer_v1};
use wayland_protocols::wp::text_input::zv3::client::{zwp_text_input_manager_v3, zwp_text_input_v3};
use wayland_protocols::wp::viewporter::client::{wp_viewport, wp_viewporter};
use wayland_protocols_wlr::screencopy::v1::client::zwlr_screencopy_manager_v1;

use crate::app::App;
use crate::geom::{Damage, IRect};
use crate::image::Image;
pub use crate::platform::{Monitor, Output, Owner, Painted};
use protocols::{BufferData, FractionalData, NoEvents, RelativeData, TextInputData};

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
    pub popup: Option<Popup>,
    /// see-through where nothing is drawn (rounded corners): ARGB buffers
    pub transparent: bool,
    /// the scale of the outputs it's on, as the compositor last said
    pub int_scale: i32,
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

impl Surf {
    fn new(owner: Owner, wl: wl_surface::WlSurface, logical: (u32, u32)) -> Surf {
        Surf {
            owner,
            wl,
            layer: None,
            window: None,
            popup: None,
            transparent: false,
            int_scale: 1,
            viewport: None,
            fractional: None,
            logical,
            preferred_scale: None,
            configured: false,
            frame_pending: false,
            bufs: Vec::new(),
            frames: 0,
            scratch: Vec::new(),
            cursor: None,
        }
    }

    /// The scale to draw at: the compositor's preferred (fractional) one,
    /// else that of the outputs it's on.
    pub fn scale(&self) -> f32 {
        self.preferred_scale.unwrap_or(self.int_scale.max(1) as f32)
    }
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
        // (the layer surface, window or popup is destroyed with its wrapper, then the surface)
        self.popup.take();
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
    pub relative_manager: Option<zwp_relative_pointer_manager_v1::ZwpRelativePointerManagerV1>,
    /// the pointer's motion as it moves (dragging pins)
    pub relative: Option<zwp_relative_pointer_v1::ZwpRelativePointerV1>,
    pub keyboard: Option<wl_keyboard::WlKeyboard>,
    /// what's left of keyboards gone from the seat, dropped later (see
    /// remove_capability)
    pub retired: Vec<(std::time::Instant, Arc<dyn wayland_client::backend::ObjectData>)>,
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
    /// to bring a window to the front (xdg-activation)
    pub activation: Option<ActivationState>,
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
            relative_manager: globals.bind(&qh, 1..=1, NoEvents).ok(),
            relative: None,
            keyboard: None,
            retired: Vec::new(),
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
            activation: ActivationState::bind(&globals, &qh).ok(),
            conn,
            qh,
        };
        let mut wl = wl;
        // (the clipboard's devices don't wait for the seat's keyboard)
        for seat in wl.seats.seats() {
            wl.clipboard.add_seat(&seat, &wl.qh);
        }
        Ok((wl, queue))
    }

    /// The monitors, as the compositor described them (after a roundtrip).
    pub fn monitors(&self) -> Vec<Monitor> {
        let mut out = Vec::new();
        for output in self.outputs.outputs() {
            let Some(info) = self.outputs.info(&output) else { continue };
            let int_scale = info.scale_factor.max(1);
            let (x, y) = info.logical_position.unwrap_or(info.location);
            // The mode in pixels, turned as the monitor is.
            let mode = info.modes.iter().find(|m| m.current).or(info.modes.first()).map(|m| {
                use wl_output::Transform as T;
                match info.transform {
                    T::_90 | T::_270 | T::Flipped90 | T::Flipped270 => (m.dimensions.1, m.dimensions.0),
                    _ => m.dimensions,
                }
            });
            let (w, h) = match (info.logical_size, mode) {
                (Some(s), _) => s,
                // No xdg-output: the mode over the scale.
                (None, Some((mw, mh))) => (mw / int_scale, mh / int_scale),
                (None, None) => continue,
            };
            if w <= 0 || h <= 0 {
                continue;
            }
            let scale = real_scale(mode, (w, h), int_scale);
            let name = info.name.clone().unwrap_or_else(|| format!("{}-{}", info.make, info.id));
            out.push(Monitor { output: Output::Wayland(output), name, rect: IRect::new(x, y, w, h), scale });
        }
        out
    }

    /// How many times a second `monitor` shows a new picture (its mode's
    /// refresh rate), if it says.
    pub fn refresh_rate(&self, monitor: &Monitor) -> Option<f64> {
        let info = self.outputs.info(monitor.wl_output()?)?;
        let mode = info.modes.iter().find(|m| m.current)?;
        (mode.refresh_rate > 0).then(|| mode.refresh_rate as f64 / 1000.0)
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
            let l = ls.create_layer_surface(qh, surface.clone(), Layer::Overlay, Some(namespace), monitor.wl_output());
            l.set_anchor(Anchor::TOP | Anchor::BOTTOM | Anchor::LEFT | Anchor::RIGHT);
            l.set_exclusive_zone(-1);
            l.set_keyboard_interactivity(KeyboardInteractivity::Exclusive);
            l.set_size(0, 0);
            layer = Some(l);
        } else if let Some(xdg) = &self.xdg_shell {
            let w = xdg.create_window(surface.clone(), WindowDecorations::None, qh);
            w.set_title(crate::APP_NAME);
            w.set_app_id(crate::APP_ID);
            w.set_fullscreen(monitor.wl_output());
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
        let mut surf = Surf::new(owner, surface, (monitor.rect.w as u32, monitor.rect.h as u32));
        surf.layer = layer;
        surf.window = window;
        surf.viewport = viewport;
        surf.fractional = fractional;
        surf.int_scale = monitor.scale.ceil() as i32;
        self.surfaces.push(surf);
        Ok(())
    }

    /// Scaling and fractional-scale hints for a new surface.
    fn scaling(&self, surface: &wl_surface::WlSurface) -> (Option<wp_viewport::WpViewport>, Option<wp_fractional_scale_v1::WpFractionalScaleV1>) {
        let qh = &self.qh;
        let viewport = self.viewporter.as_ref().map(|v| v.get_viewport(surface, qh, NoEvents));
        let fractional = self.fractional.as_ref().map(|f| f.get_fractional_scale(surface, qh, FractionalData(surface.id())));
        (viewport, fractional)
    }

    /// A surface above the windows on `monitor`, its top left at `at`
    /// (logical, from the monitor's), `size` big: a pin. False without
    /// layer-shell.
    pub fn add_layer(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), namespace: &str) -> bool {
        let Some(ls) = &self.layer_shell else { return false };
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        let l = ls.create_layer_surface(qh, surface.clone(), Layer::Top, Some(namespace), monitor.wl_output());
        l.set_anchor(Anchor::TOP | Anchor::LEFT);
        l.set_margin(at.1, 0, 0, at.0);
        l.set_size(size.0.max(1), size.1.max(1));
        l.set_exclusive_zone(0);
        l.set_keyboard_interactivity(KeyboardInteractivity::OnDemand);
        let (viewport, fractional) = self.scaling(&surface);
        l.commit();
        let mut surf = Surf::new(owner, surface, size);
        surf.layer = Some(l);
        surf.viewport = viewport;
        surf.fractional = fractional;
        surf.int_scale = monitor.scale.ceil() as i32;
        self.surfaces.push(surf);
        true
    }

    /// A small see-through surface above everything on `monitor` that
    /// never takes the keyboard, its top left at `at` (logical, from the
    /// monitor's): a recording's controls. `click_through`: the pointer
    /// goes to what's under it. False without layer-shell.
    pub fn add_control(&mut self, owner: Owner, monitor: &Monitor, at: (i32, i32), size: (u32, u32), namespace: &str, click_through: bool) -> bool {
        let Some(ls) = &self.layer_shell else { return false };
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        if click_through && let Ok(region) = smithay_client_toolkit::compositor::Region::new(&self.compositor) {
            surface.set_input_region(Some(region.wl_region()));
        }
        let l = ls.create_layer_surface(qh, surface.clone(), Layer::Overlay, Some(namespace), monitor.wl_output());
        l.set_anchor(Anchor::TOP | Anchor::LEFT);
        l.set_margin(at.1, 0, 0, at.0);
        l.set_size(size.0.max(1), size.1.max(1));
        l.set_exclusive_zone(-1);
        l.set_keyboard_interactivity(KeyboardInteractivity::None);
        let (viewport, fractional) = self.scaling(&surface);
        l.commit();
        let mut surf = Surf::new(owner, surface, size);
        surf.layer = Some(l);
        surf.viewport = viewport;
        surf.fractional = fractional;
        surf.int_scale = monitor.scale.ceil() as i32;
        surf.transparent = true;
        self.surfaces.push(surf);
        true
    }

    /// A window (xdg toplevel) of `size` (logical).
    pub fn add_window(&mut self, owner: Owner, title: &str, size: (u32, u32), decorated: bool) -> Result<(), String> {
        let Some(xdg) = &self.xdg_shell else { return Err("the compositor has no xdg-shell".into()) };
        let qh = &self.qh;
        let surface = self.compositor.create_surface(qh);
        let decorations = if decorated { WindowDecorations::RequestServer } else { WindowDecorations::None };
        let w = xdg.create_window(surface.clone(), decorations, qh);
        w.set_title(title);
        w.set_app_id(crate::APP_ID);
        let (viewport, fractional) = self.scaling(&surface);
        w.commit();
        let mut surf = Surf::new(owner, surface, size);
        surf.window = Some(w);
        surf.viewport = viewport;
        surf.fractional = fractional;
        self.surfaces.push(surf);
        Ok(())
    }

    /// A popup (a menu) of `size` (logical) next to `anchor` (logical, on
    /// `parent`'s surface), taking the keyboard and pointer until it's
    /// dismissed.
    pub fn add_popup(&mut self, owner: Owner, parent: Owner, anchor: IRect, size: (u32, u32)) -> Result<(), String> {
        use wayland_protocols::xdg::shell::client::xdg_positioner::{Anchor as PAnchor, ConstraintAdjustment, Gravity};
        let Some(xdg) = &self.xdg_shell else { return Err("the compositor has no xdg-shell".into()) };
        let Some(p) = self.surfaces.iter().find(|s| s.owner == parent) else { return Err("no parent surface".into()) };
        let qh = &self.qh;
        let positioner = XdgPositioner::new(xdg).map_err(|e| e.to_string())?;
        positioner.set_size(size.0.max(1) as i32, size.1.max(1) as i32);
        positioner.set_anchor_rect(anchor.x, anchor.y, anchor.w.max(1), anchor.h.max(1));
        positioner.set_anchor(PAnchor::BottomRight);
        positioner.set_gravity(Gravity::BottomRight);
        positioner
            .set_constraint_adjustment(ConstraintAdjustment::FlipX | ConstraintAdjustment::FlipY | ConstraintAdjustment::SlideX | ConstraintAdjustment::SlideY);
        let surface = self.compositor.create_surface(qh);
        let parent_xdg = p.window.as_ref().map(|w| w.xdg_surface().clone()).or_else(|| p.popup.as_ref().map(|pp| pp.xdg_surface().clone()));
        let popup = Popup::from_surface(parent_xdg.as_ref(), &positioner, qh, surface.clone(), xdg).map_err(|e| e.to_string())?;
        if parent_xdg.is_none() {
            match &p.layer {
                Some(l) => l.get_popup(popup.xdg_popup()),
                None => return Err("the parent can't have popups".into()),
            }
        }
        if let Some(seat) = self.seats.seats().next() {
            popup.xdg_popup().grab(&seat, self.serial);
        }
        let (viewport, fractional) = self.scaling(&surface);
        let int_scale = p.int_scale;
        surface.commit();
        let mut surf = Surf::new(owner, surface, size);
        surf.popup = Some(popup);
        surf.transparent = true;
        surf.viewport = viewport;
        surf.fractional = fractional;
        surf.int_scale = int_scale;
        self.surfaces.push(surf);
        Ok(())
    }

    /// Take down `owner`'s surfaces.
    pub fn remove(&mut self, owner: Owner) {
        self.surfaces.retain(|s| s.owner != owner);
        let _ = self.conn.flush();
    }

    /// The size `owner`'s surface is shown at (logical), ahead of the
    /// compositor saying so.
    pub fn set_logical_size(&mut self, owner: Owner, size: (u32, u32)) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) {
            s.logical = size;
        }
    }

    pub fn surf(&self, owner: Owner) -> Option<&Surf> {
        self.surfaces.iter().find(|s| s.owner == owner)
    }

    /// Move a layer surface (logical, from its monitor's top left) and size it.
    /// (`at` from `monitor`'s top left: the one the layer surface is on,
    /// which can't change: another monitor's is a surface of its own)
    pub fn place_layer(&mut self, owner: Owner, _monitor: &Monitor, at: (i32, i32), size: (u32, u32)) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner)
            && let Some(l) = &s.layer
        {
            l.set_margin(at.1, 0, 0, at.0);
            l.set_size(size.0.max(1), size.1.max(1));
            l.commit();
        }
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
            None if s.bufs.len() < 2 => match new_buffer(&shm, &qh, w, h, s.transparent) {
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
        let buf = &mut s.bufs[idx];
        let started = std::time::Instant::now();
        let rects = crate::platform::paint_damage(&mut buf.img, &mut s.scratch, &todo, ui, part);
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

    /// Hear the pointer's motion as it moves (for dragging pins).
    pub fn create_relative_pointer(&mut self) {
        if self.relative.is_none()
            && let (Some(m), Some(p)) = (&self.relative_manager, &self.pointer)
        {
            self.relative = Some(m.get_relative_pointer(p.pointer(), &self.qh, RelativeData));
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

impl Wayland {
    // -- what the app asks through platform::Display -----------------------------------

    pub fn surface(&self, owner: Owner) -> Option<((u32, u32), f32)> {
        self.surf(owner).map(|s| (s.logical, s.scale()))
    }

    pub fn owners(&self) -> Vec<Owner> {
        self.surfaces.iter().map(|s| s.owner).collect()
    }

    pub fn redraw_owner(&mut self, owner: Owner, part: usize, ui: &mut dyn Painted) -> bool {
        match self.surfaces.iter().position(|s| s.owner == owner) {
            Some(i) => self.redraw(i, part, ui),
            None => false,
        }
    }

    pub fn set_min_size(&mut self, owner: Owner, size: (u32, u32)) {
        if let Some(w) = self.surf(owner).and_then(|s| s.window.as_ref()) {
            w.set_min_size(Some(size));
        }
    }

    pub fn set_title(&mut self, owner: Owner, title: &str) {
        if let Some(w) = self.surf(owner).and_then(|s| s.window.as_ref()) {
            w.set_title(title.to_string());
        }
    }

    pub fn set_transparent(&mut self, owner: Owner) {
        if let Some(s) = self.surfaces.iter_mut().find(|s| s.owner == owner) {
            s.transparent = true;
        }
    }

    pub fn flush(&self) {
        let _ = self.conn.flush();
    }

    pub fn pointer_owner(&self) -> Option<Owner> {
        self.pointer_on.as_ref().and_then(|id| self.owner_of(id))
    }

    pub fn keyboard_owner(&self) -> Option<Owner> {
        self.keyboard_on.as_ref().and_then(|id| self.owner_of(id))
    }

    pub fn text_owner(&self) -> Option<Owner> {
        self.text_focus.as_ref().and_then(|id| self.owner_of(id))
    }

    pub fn set_cursor(&mut self, owner: Owner, shape: crate::ui::session::CursorShape) {
        use crate::ui::session::CursorShape;
        use smithay_client_toolkit::seat::pointer::CursorIcon;
        let icon = match shape {
            CursorShape::Crosshair => CursorIcon::Crosshair,
            CursorShape::Text => CursorIcon::Text,
            // (the four arrows: "move" is the drag and drop arrow in many themes)
            CursorShape::Move => CursorIcon::AllScroll,
            CursorShape::Hand => CursorIcon::Pointer,
            CursorShape::Arrow => CursorIcon::Default,
            CursorShape::ResizeNS => CursorIcon::NsResize,
            CursorShape::ResizeEW => CursorIcon::EwResize,
            CursorShape::ResizeNWSE => CursorIcon::NwseResize,
            CursorShape::ResizeNESW => CursorIcon::NeswResize,
        };
        let Some(i) = self.surfaces.iter().position(|s| s.owner == owner) else { return };
        if self.surfaces[i].cursor != Some(icon)
            && let Some(p) = &self.pointer
            && p.set_cursor(&self.conn, icon).is_ok()
        {
            self.surfaces[i].cursor = Some(icon);
        }
    }

    /// Bring `owner`'s window to the front, focused: with `token` (from
    /// the desktop, for the click that asked), else one asked for with our
    /// latest input.
    pub fn activate(&mut self, owner: Owner, token: Option<String>) {
        let (Some(a), Some(surface)) = (&self.activation, self.surf(owner).map(|s| s.wl.clone())) else { return };
        match token {
            Some(token) => a.activate::<App>(&surface, token),
            None => {
                let from =
                    self.keyboard_on.as_ref().or(self.pointer_on.as_ref()).and_then(|id| self.surfaces.iter().find(|s| &s.wl.id() == id)).map(|s| s.wl.clone());
                let seat = self.seats.seats().next().map(|s| (s, self.serial));
                a.request_token(&self.qh, RequestData { app_id: Some(crate::APP_ID.to_string()), seat_and_serial: seat, surface: from, udata: owner });
            }
        }
    }

    pub fn start_move(&mut self, owner: Owner) -> bool {
        match (self.surf(owner).and_then(|s| s.window.as_ref()), self.seats.seats().next()) {
            (Some(w), Some(seat)) => {
                w.move_(&seat, self.serial);
                true
            }
            _ => false,
        }
    }

    pub fn has_relative(&self) -> bool {
        self.relative.is_some()
    }

    pub fn can_place(&self) -> bool {
        self.layer_shell.is_some()
    }

    pub fn offer(&mut self, mimes: Vec<String>, slot: Arc<crate::clipboard::Slot>) -> bool {
        self.clipboard.offer(mimes, slot, self.serial, &self.qh)
    }

    pub fn offer_each(&mut self, parts: Vec<(String, Arc<Vec<u8>>)>) -> bool {
        self.clipboard.offer_each(parts, self.serial, &self.qh)
    }

    pub fn holds(&self) -> bool {
        self.clipboard.holds()
    }

    pub fn withdraw(&mut self) {
        self.clipboard.withdraw();
    }

    pub fn without_focus(&self) -> bool {
        self.clipboard.without_focus()
    }

    pub fn paste_text(&mut self, wake: crate::ui::session::Waker) -> std::sync::mpsc::Receiver<Option<String>> {
        self.clipboard.paste_text(wake)
    }

    pub fn paste(&mut self, mimes: &'static [&'static str], max: u64, wake: crate::ui::session::Waker) -> crate::clipboard::Pasted {
        self.clipboard.paste(mimes, max, wake)
    }

    pub fn tick(&mut self) {
        // (long since forgotten by the compositor: safe to drop here)
        self.retired.retain(|(t, _)| t.elapsed() < std::time::Duration::from_secs(5));
    }

    pub fn next_deadline(&self) -> Option<std::time::Instant> {
        None
    }
}

/// A buffer of `w` x `h` in its own shared memory.
/// A monitor's real scale: its pixels (`mode`, turned) over its logical
/// size, where a fractional scale (125%: 1.25) shows, in the steps the
/// compositor scales by (1/120); else its integer scale.
fn real_scale(mode: Option<(i32, i32)>, logical: (i32, i32), int_scale: i32) -> f32 {
    match mode {
        Some((mw, _)) if mw > 0 && logical.0 > 0 => ((mw as f32 / logical.0 as f32) * 120.0).round().max(1.0) / 120.0,
        _ => int_scale as f32,
    }
}

fn new_buffer(shm: &wl_shm::WlShm, qh: &QueueHandle<App>, w: u32, h: u32, transparent: bool) -> Result<Buf, String> {
    let img = Image::shared(w, h)?;
    let fd = img.fd().ok_or("not shared memory")?;
    let len = img.byte_len() as i32;
    let pool = shm.create_pool(fd, len, qh, NoEvents);
    let busy = Arc::new(AtomicBool::new(false));
    // Opaque surfaces (the overlay, pins) say so: the compositor needn't blend them.
    let format = if transparent { wl_shm::Format::Argb8888 } else { wl_shm::Format::Xrgb8888 };
    let buffer = pool.create_buffer(0, w as i32, h as i32, (w * 4) as i32, format, qh, BufferData(busy.clone()));
    pool.destroy(); // (the buffer keeps the memory)
    Ok(Buf { img, buffer, busy, stale: Damage::full(), drawn: 0 })
}

#[cfg(test)]
mod tests {
    use super::real_scale;

    #[test]
    fn fractional_scales() {
        assert_eq!(real_scale(Some((2560, 1600)), (2048, 1280), 2), 1.25);
        assert_eq!(real_scale(Some((1920, 1080)), (1371, 771), 2), 1.4);
        assert_eq!(real_scale(Some((3840, 2160)), (1920, 1080), 2), 2.0);
        assert_eq!(real_scale(Some((1920, 1080)), (1920, 1080), 1), 1.0);
        assert_eq!(real_scale(None, (1920, 1080), 2), 2.0);
    }
}
