//! What the compositor tells Flatshot: surfaces configured and shown,
//! monitors, the keyboard, the pointer and touch.

use smithay_client_toolkit::compositor::CompositorHandler;
use smithay_client_toolkit::data_device_manager::WritePipe;
use smithay_client_toolkit::data_device_manager::data_device::DataDeviceHandler;
use smithay_client_toolkit::data_device_manager::data_offer::{DataOfferHandler, DragOffer};
use smithay_client_toolkit::data_device_manager::data_source::DataSourceHandler;
use smithay_client_toolkit::output::{OutputHandler, OutputState};
use smithay_client_toolkit::registry::{ProvidesRegistryState, RegistryState};
use smithay_client_toolkit::seat::keyboard::{KeyEvent, KeyboardHandler, Keysym, Modifiers, RawModifiers};
use smithay_client_toolkit::seat::pointer::{PointerEvent, PointerEventKind, PointerHandler, ThemeSpec};
use smithay_client_toolkit::seat::touch::TouchHandler;
use smithay_client_toolkit::seat::{Capability, SeatHandler, SeatState};
use smithay_client_toolkit::shell::wlr_layer::{LayerShellHandler, LayerSurface, LayerSurfaceConfigure};
use smithay_client_toolkit::shell::xdg::popup::{Popup, PopupConfigure, PopupHandler};
use smithay_client_toolkit::shell::xdg::window::{Window, WindowConfigure, WindowHandler};
use smithay_client_toolkit::shm::{Shm, ShmHandler};
use smithay_client_toolkit::{delegate_dispatch2, delegate_registry, registry_handlers};
use wayland_client::protocol::wl_data_device::WlDataDevice;
use wayland_client::protocol::wl_data_device_manager::DndAction;
use wayland_client::protocol::wl_data_source::WlDataSource;
use wayland_client::protocol::{wl_keyboard, wl_output, wl_pointer, wl_seat, wl_surface, wl_touch};
use wayland_client::{Connection, Proxy, QueueHandle};

use crate::app::{App, Input};
use crate::keys::KeyPress;
use crate::keyseq;

impl CompositorHandler for App {
    fn scale_factor_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, factor: i32) {
        if let Some(s) = self.wl.surfaces.iter_mut().find(|s| &s.wl == surface) {
            s.int_scale = factor.max(1);
        }
    }

    fn transform_changed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: wl_output::Transform) {}

    fn frame(&mut self, _: &Connection, _: &QueueHandle<Self>, surface: &wl_surface::WlSurface, _: u32) {
        self.wl.frame_done(surface);
    }

    fn surface_enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}

    fn surface_leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_surface::WlSurface, _: &wl_output::WlOutput) {}
}

impl OutputHandler for App {
    fn output_state(&mut self) -> &mut OutputState {
        &mut self.wl.outputs
    }

    fn new_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn update_output(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_output::WlOutput) {}

    fn output_destroyed(&mut self, _: &Connection, _: &QueueHandle<Self>, output: wl_output::WlOutput) {
        self.monitor_gone(&output);
    }
}

impl LayerShellHandler for App {
    fn closed(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface) {
        let id = layer.wl_surface().id();
        self.surface_closed(&id);
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, layer: &LayerSurface, configure: LayerSurfaceConfigure, _: u32) {
        let id = layer.wl_surface().id();
        if let Some(s) = self.wl.surfaces.iter_mut().find(|s| s.wl.id() == id) {
            let (w, h) = configure.new_size;
            if w > 0 && h > 0 {
                s.logical = (w, h);
            }
            if !s.configured {
                crate::timing::log(format!(
                    "overlay configured at {w} × {h} ({:?})",
                    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_micros() % 1_000_000).unwrap_or(0)
                ));
            }
            s.configured = true;
        }
    }
}

use smithay_client_toolkit::shell::WaylandSurface;

impl WindowHandler for App {
    fn request_close(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window) {
        let id = window.wl_surface().id();
        self.surface_closed(&id);
    }

    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, window: &Window, configure: WindowConfigure, _: u32) {
        let id = window.wl_surface().id();
        if let Some(s) = self.wl.surfaces.iter_mut().find(|s| s.wl.id() == id) {
            if let (Some(w), Some(h)) = configure.new_size {
                s.logical = (w.get(), h.get());
            }
            s.configured = true;
        }
    }
}

impl PopupHandler for App {
    fn configure(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup, config: PopupConfigure) {
        let id = popup.wl_surface().id();
        if let Some(s) = self.wl.surfaces.iter_mut().find(|s| s.wl.id() == id) {
            if config.width > 0 && config.height > 0 {
                s.logical = (config.width as u32, config.height as u32);
            }
            s.configured = true;
        }
    }

    fn done(&mut self, _: &Connection, _: &QueueHandle<Self>, popup: &Popup) {
        let id = popup.wl_surface().id();
        self.surface_closed(&id);
    }
}

impl SeatHandler for App {
    fn seat_state(&mut self) -> &mut SeatState {
        &mut self.wl.seats
    }

    fn new_seat(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat) {
        self.wl.clipboard.add_seat(&seat, qh);
    }

    fn new_capability(&mut self, _: &Connection, qh: &QueueHandle<Self>, seat: wl_seat::WlSeat, capability: Capability) {
        self.wl.clipboard.add_seat(&seat, qh);
        match capability {
            Capability::Keyboard if self.wl.keyboard.is_none() => {
                let handle = self.loop_handle.clone();
                let keyboard = self.wl.seats.get_keyboard_with_repeat(
                    qh,
                    &seat,
                    None,
                    handle,
                    Box::new(|app: &mut App, _: &wl_keyboard::WlKeyboard, event: KeyEvent| app.key_event(event, true, true)),
                );
                match keyboard {
                    Ok(k) => self.wl.keyboard = Some(k),
                    Err(e) => crate::warn!("no keyboard: {e}"),
                }
                self.wl.create_text_input(&seat);
            }
            Capability::Pointer if self.wl.pointer.is_none() => {
                let surface = self.wl.compositor.create_surface(qh);
                match self.wl.seats.get_pointer_with_theme::<App, ()>(qh, &seat, self.wl.shm.wl_shm(), surface, ThemeSpec::System) {
                    Ok(p) => {
                        self.wl.pointer = Some(p);
                        self.wl.create_relative_pointer();
                    }
                    Err(e) => crate::warn!("no pointer: {e}"),
                }
            }
            Capability::Touch if self.wl.touch.is_none() => {
                if let Ok(t) = self.wl.seats.get_touch(qh, &seat) {
                    self.wl.touch = Some(t);
                }
            }
            _ => {}
        }
    }

    fn remove_capability(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat, capability: Capability) {
        match capability {
            Capability::Keyboard => {
                if let Some(k) = self.wl.keyboard.take() {
                    // Its data stops key repeat in the event loop when it's
                    // dropped, which can't be done from inside the loop's
                    // dispatch (where the compositor's "forget it" comes):
                    // hold on to it until things are quiet.
                    if let Some(data) = k.object_data().cloned() {
                        self.wl.retired.push((std::time::Instant::now(), data));
                    }
                    k.release();
                }
            }
            Capability::Pointer => {
                if let Some(r) = self.wl.relative.take() {
                    r.destroy();
                }
                if let Some(p) = self.wl.pointer.take() {
                    p.pointer().release();
                }
            }
            Capability::Touch => {
                if let Some(t) = self.wl.touch.take() {
                    t.release();
                }
            }
            _ => {}
        }
    }

    fn remove_seat(&mut self, _: &Connection, _: &QueueHandle<Self>, _: wl_seat::WlSeat) {}
}

impl KeyboardHandler for App {
    fn enter(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_keyboard::WlKeyboard,
        surface: &wl_surface::WlSurface,
        serial: u32,
        _: &[u32],
        _: &[Keysym],
    ) {
        self.wl.keyboard_on = Some(surface.id());
        self.wl.serial = serial;
    }

    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, surface: &wl_surface::WlSurface, _: u32) {
        if self.wl.keyboard_on.as_ref() == Some(&surface.id()) {
            self.wl.keyboard_on = None;
        }
    }

    fn press_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, serial: u32, event: KeyEvent) {
        self.wl.serial = serial;
        self.key_event(event, true, false);
    }

    fn repeat_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key_event(event, true, true);
    }

    fn release_key(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, event: KeyEvent) {
        self.key_event(event, false, false);
    }

    fn update_modifiers(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_keyboard::WlKeyboard, _: u32, m: Modifiers, _: RawModifiers, _: u32) {
        let mut mods = 0;
        if m.ctrl {
            mods |= keyseq::CTRL;
        }
        if m.shift {
            mods |= keyseq::SHIFT;
        }
        if m.alt {
            mods |= keyseq::ALT;
        }
        if m.logo {
            mods |= keyseq::META;
        }
        self.wl.mods = mods;
        self.input(None, Input::Modifiers(mods));
    }
}

impl App {
    /// A key, in Qt's terms (as the keymap and the session take them).
    pub fn key_event(&mut self, event: KeyEvent, pressed: bool, repeat: bool) {
        let text = event.utf8.clone().unwrap_or_default();
        let ch = text.chars().next();
        let key = keyseq::keysym_to_qt(event.keysym.raw(), ch);
        // Keypad keys carry their own modifier bit in Qt.
        let keypad = if keyseq::is_keypad(event.keysym.raw()) { keyseq::KEYPAD } else { 0 };
        let press = KeyPress { key, mods: self.wl.mods | keypad, text: if pressed { text } else { String::new() }, keysym: event.keysym.raw(), repeat };
        let on = self.wl.keyboard_on.clone();
        self.input(on, if pressed { Input::Key(press) } else { Input::KeyUp(press) });
    }
}

impl PointerHandler for App {
    fn pointer_frame(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_pointer::WlPointer, events: &[PointerEvent]) {
        for e in events {
            let id = e.surface.id();
            let at = (e.position.0, e.position.1);
            match e.kind {
                PointerEventKind::Enter { serial } => {
                    self.wl.pointer_on = Some(id.clone());
                    if let Some(s) = self.wl.surfaces.iter_mut().find(|s| s.wl.id() == id) {
                        s.cursor = None; // (set again for this entry)
                    }
                    self.wl.serial = serial;
                    self.input(Some(id), Input::Enter(at));
                }
                PointerEventKind::Leave { .. } => {
                    if self.wl.pointer_on.as_ref() == Some(&id) {
                        self.wl.pointer_on = None;
                    }
                    self.input(Some(id), Input::Leave);
                }
                PointerEventKind::Motion { .. } => self.input(Some(id), Input::Motion(at, false)),
                PointerEventKind::Press { button, serial, .. } => {
                    self.wl.serial = serial;
                    self.input(Some(id), Input::Button(button, true, at));
                }
                PointerEventKind::Release { button, serial, .. } => {
                    self.wl.serial = serial;
                    self.input(Some(id), Input::Button(button, false, at));
                }
                PointerEventKind::Axis { vertical, .. } => {
                    // Notches up are positive, as Qt's angle delta.
                    let steps = if vertical.value120 != 0 {
                        -vertical.value120 as f32 / 120.0
                    } else if vertical.discrete != 0 {
                        -vertical.discrete as f32
                    } else {
                        -(vertical.absolute / 15.0) as f32
                    };
                    if steps != 0.0 {
                        self.input(Some(id), Input::Scroll(steps));
                    }
                }
            }
        }
    }
}

impl TouchHandler for App {
    fn down(
        &mut self,
        _: &Connection,
        _: &QueueHandle<Self>,
        _: &wl_touch::WlTouch,
        serial: u32,
        _: u32,
        surface: wl_surface::WlSurface,
        id: i32,
        position: (f64, f64),
    ) {
        self.wl.serial = serial;
        // The first finger acts as the left button; more are ignored.
        if !self.wl.touches.is_empty() {
            self.wl.touches.insert(id, (surface.id(), position));
            return;
        }
        self.wl.touches.insert(id, (surface.id(), position));
        self.input(Some(surface.id()), Input::Motion(position, true));
        self.input(Some(surface.id()), Input::Button(smithay_client_toolkit::seat::pointer::BTN_LEFT, true, position));
    }

    fn up(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, serial: u32, _: u32, id: i32) {
        self.wl.serial = serial;
        let first = self.wl.touches.len() == 1;
        if let Some((surface, at)) = self.wl.touches.remove(&id)
            && first
        {
            self.input(Some(surface), Input::Button(smithay_client_toolkit::seat::pointer::BTN_LEFT, false, at));
        }
    }

    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, _: u32, id: i32, position: (f64, f64)) {
        let single = self.wl.touches.len() == 1;
        if let Some(t) = self.wl.touches.get_mut(&id) {
            t.1 = position;
            if single {
                let surface = t.0.clone();
                self.input(Some(surface), Input::Motion(position, true));
            }
        }
    }

    fn shape(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, _: i32, _: f64, _: f64) {}

    fn orientation(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch, _: i32, _: f64) {}

    fn cancel(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &wl_touch::WlTouch) {
        let first = self.wl.touches.drain().next();
        if let Some((_, (surface, at))) = first {
            self.input(Some(surface), Input::Button(smithay_client_toolkit::seat::pointer::BTN_LEFT, false, at));
        }
    }
}

impl DataDeviceHandler for App {
    fn enter(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64, _: &wl_surface::WlSurface) {}
    fn leave(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn motion(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice, _: f64, _: f64) {}
    fn selection(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
    fn drop_performed(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataDevice) {}
}

impl DataSourceHandler for App {
    fn accept_mime(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: Option<String>) {}

    fn send_request(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, mime: String, fd: WritePipe) {
        self.wl.clipboard.send(&mime, fd.into());
    }

    fn cancelled(&mut self, _: &Connection, _: &QueueHandle<Self>, source: &WlDataSource) {
        self.wl.clipboard.core_cancelled(source);
    }

    fn dnd_dropped(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn dnd_finished(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource) {}
    fn action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &WlDataSource, _: DndAction) {}
}

impl DataOfferHandler for App {
    fn source_actions(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
    fn selected_action(&mut self, _: &Connection, _: &QueueHandle<Self>, _: &mut DragOffer, _: DndAction) {}
}

impl ShmHandler for App {
    fn shm_state(&mut self) -> &mut Shm {
        &mut self.wl.shm
    }
}

impl ProvidesRegistryState for App {
    fn registry(&mut self) -> &mut RegistryState {
        &mut self.wl.registry
    }
    registry_handlers![OutputState, SeatState];
}

delegate_registry!(App);
delegate_dispatch2!(App);
