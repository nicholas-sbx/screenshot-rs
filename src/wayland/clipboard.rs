//! The clipboard on Wayland. With a data-control protocol (KDE, wlroots
//! compositors; ext- or wlr-) Flatshot can own the clipboard without a
//! focused window, so a capture is copied after the overlay is gone. Else
//! the core protocol is used while the overlay still has the keyboard.
//!
//! What's copied stays in memory, served to whoever pastes, until another
//! program takes the clipboard. Pasting (into a text being typed) reads the
//! current selection's text.

use std::io::{Read, Write};
use std::os::fd::OwnedFd;
use std::sync::{Arc, Condvar, Mutex, mpsc};
use std::time::Duration;

use smithay_client_toolkit::data_device_manager::data_device::DataDevice;
use smithay_client_toolkit::data_device_manager::data_source::CopyPasteSource;
use smithay_client_toolkit::data_device_manager::DataDeviceManagerState;
use smithay_client_toolkit::dispatch2::Dispatch2;
use wayland_client::backend::ObjectData;
use wayland_client::protocol::wl_seat::WlSeat;
use wayland_client::{Connection, Proxy, QueueHandle};
use wayland_protocols::ext::data_control::v1::client::{
    ext_data_control_device_v1 as ext_dev, ext_data_control_manager_v1 as ext_mgr, ext_data_control_offer_v1 as ext_offer,
    ext_data_control_source_v1 as ext_src,
};
use wayland_protocols_wlr::data_control::v1::client::{
    zwlr_data_control_device_v1 as wlr_dev, zwlr_data_control_manager_v1 as wlr_mgr, zwlr_data_control_offer_v1 as wlr_offer,
    zwlr_data_control_source_v1 as wlr_src,
};

use super::protocols::NoEvents;
use crate::app::App;

/// The text types offered and asked for, best first.
pub const TEXT_MIMES: &[&str] = &["text/plain;charset=utf-8", "text/plain", "UTF8_STRING", "STRING", "TEXT"];
/// The most text taken from the clipboard.
const MAX_PASTE: u64 = 1 << 20;

/// An offer of the current selection: its types, as they come.
#[derive(Default)]
pub struct OfferData(pub Mutex<Vec<String>>);

/// How long a paste waits for data still being made.
const WAIT_FOR_DATA: Duration = Duration::from_secs(30);

/// What's offered, which may still be on its way (a picture being encoded
/// after the overlay that had the keyboard has gone).
#[derive(Default)]
pub struct Slot {
    state: Mutex<SlotState>,
    changed: Condvar,
}

#[derive(Default)]
enum SlotState {
    #[default]
    Coming,
    Ready(Arc<Vec<u8>>),
    Gone,
}

impl Slot {
    pub fn ready(data: Arc<Vec<u8>>) -> Arc<Slot> {
        Arc::new(Slot { state: Mutex::new(SlotState::Ready(data)), changed: Condvar::new() })
    }

    pub fn fill(&self, data: Arc<Vec<u8>>) {
        self.set(SlotState::Ready(data));
    }

    /// Nothing will come (whoever is waiting gets nothing).
    pub fn abandon(&self) {
        self.set(SlotState::Gone);
    }

    fn set(&self, state: SlotState) {
        if let Ok(mut s) = self.state.lock() {
            *s = state;
            self.changed.notify_all();
        }
    }

    /// The data, once it's there.
    fn wait(&self) -> Option<Arc<Vec<u8>>> {
        let guard = self.state.lock().ok()?;
        let (guard, _) = self.changed.wait_timeout_while(guard, WAIT_FOR_DATA, |s| matches!(s, SlotState::Coming)).ok()?;
        match &*guard {
            SlotState::Ready(d) => Some(d.clone()),
            _ => None,
        }
    }

    /// The data, if it's there now.
    pub fn get(&self) -> Option<Arc<Vec<u8>>> {
        match &*self.state.lock().ok()? {
            SlotState::Ready(d) => Some(d.clone()),
            _ => None,
        }
    }
}

/// What Flatshot offers: the data and its types.
pub struct Offered {
    pub mimes: Vec<String>,
    pub slot: Arc<Slot>,
    source: Source,
}

enum Source {
    Ext(ext_src::ExtDataControlSourceV1),
    Wlr(wlr_src::ZwlrDataControlSourceV1),
    /// (held: dropping it gives up the clipboard)
    #[allow(dead_code)]
    Core(CopyPasteSource),
}

impl Drop for Offered {
    fn drop(&mut self) {
        match &self.source {
            Source::Ext(s) => s.destroy(),
            Source::Wlr(s) => s.destroy(),
            Source::Core(_) => {}
        }
    }
}

enum Incoming {
    Ext(ext_offer::ExtDataControlOfferV1),
    Wlr(wlr_offer::ZwlrDataControlOfferV1),
}

pub struct Clipboard {
    ext: Option<ext_mgr::ExtDataControlManagerV1>,
    wlr: Option<wlr_mgr::ZwlrDataControlManagerV1>,
    core: Option<DataDeviceManagerState>,
    ext_device: Option<ext_dev::ExtDataControlDeviceV1>,
    wlr_device: Option<wlr_dev::ZwlrDataControlDeviceV1>,
    core_device: Option<DataDevice>,
    pub offered: Option<Offered>,
    incoming: Option<Incoming>,
}

impl Clipboard {
    pub fn bind(globals: &wayland_client::globals::GlobalList, qh: &QueueHandle<App>) -> Clipboard {
        Clipboard {
            ext: globals.bind(qh, 1..=1, NoEvents).ok(),
            wlr: globals.bind(qh, 1..=2, NoEvents).ok(),
            core: DataDeviceManagerState::bind(globals, qh).ok(),
            ext_device: None,
            wlr_device: None,
            core_device: None,
            offered: None,
            incoming: None,
        }
    }

    /// Devices for the seat (once it's known).
    pub fn add_seat(&mut self, seat: &WlSeat, qh: &QueueHandle<App>) {
        if let Some(m) = &self.ext
            && self.ext_device.is_none()
        {
            self.ext_device = Some(m.get_data_device(seat, qh, DeviceData));
        } else if let Some(m) = &self.wlr
            && self.wlr_device.is_none()
            && self.ext_device.is_none()
        {
            self.wlr_device = Some(m.get_data_device(seat, qh, DeviceData));
        }
        if let Some(m) = &self.core
            && self.core_device.is_none()
        {
            self.core_device = Some(m.get_data_device(qh, seat));
        }
    }

    /// Can the clipboard be set without a focused window?
    pub fn without_focus(&self) -> bool {
        self.ext_device.is_some() || self.wlr_device.is_some()
    }

    /// Own the clipboard with what comes in `slot`, as each of `mimes`.
    /// `serial`: a recent input event's (needed without data-control).
    pub fn offer(&mut self, mimes: Vec<String>, slot: Arc<Slot>, serial: u32, qh: &QueueHandle<App>) -> bool {
        let source = if let (Some(m), Some(d)) = (&self.ext, &self.ext_device) {
            let s = m.create_data_source(qh, NoEventsSource);
            for t in &mimes {
                s.offer(t.clone());
            }
            d.set_selection(Some(&s));
            Source::Ext(s)
        } else if let (Some(m), Some(d)) = (&self.wlr, &self.wlr_device) {
            let s = m.create_data_source(qh, NoEventsSource);
            for t in &mimes {
                s.offer(t.clone());
            }
            d.set_selection(Some(&s));
            Source::Wlr(s)
        } else if let (Some(m), Some(d)) = (&self.core, &self.core_device) {
            let s = m.create_copy_paste_source(qh, mimes.iter().map(|m| m.as_str()));
            s.set_selection(d, serial);
            Source::Core(s)
        } else {
            return false;
        };
        self.offered = Some(Offered { mimes, slot, source });
        true
    }

    /// A program wants what we offer, as `mime`, written to `fd`.
    pub fn send(&self, mime: &str, fd: OwnedFd) {
        let Some(o) = &self.offered else { return };
        if !o.mimes.iter().any(|m| m == mime) {
            return;
        }
        let slot = o.slot.clone();
        // (big pictures fill the pipe, and may still be being made: written
        // by a thread of their own)
        let _ = std::thread::Builder::new().name("flatshot-clipboard".into()).spawn(move || {
            if let Some(data) = slot.wait() {
                let mut f = std::fs::File::from(fd);
                let _ = f.write_all(&data);
            }
        });
    }

    /// Someone else took the clipboard: let go of ours.
    pub fn cancelled(&mut self) {
        self.offered = None;
    }

    /// The core protocol's source `source` was replaced.
    pub fn core_cancelled(&mut self, source: &wayland_client::protocol::wl_data_source::WlDataSource) {
        if matches!(&self.offered, Some(Offered { source: Source::Core(s), .. }) if s.inner() == source) {
            self.offered = None;
        }
    }

    /// Give up the clipboard (what was to be offered never came).
    pub fn withdraw(&mut self) {
        if let Some(o) = self.offered.take() {
            o.slot.abandon();
        }
    }

    /// Do we own the clipboard?
    pub fn holds(&self) -> bool {
        self.offered.is_some()
    }

    /// The current selection's text, read in the background; the result
    /// comes on the returned channel (None if there's none).
    pub fn paste_text(&self, wake: crate::ui::session::Waker) -> mpsc::Receiver<Option<String>> {
        let (tx, rx) = mpsc::channel();
        let mime_of = |mimes: &[String]| TEXT_MIMES.iter().find(|t| mimes.iter().any(|m| m == *t)).map(|t| t.to_string());
        let receive: Option<(String, OwnedFd)> = (|| {
            let (r, w) = rustix::pipe::pipe_with(rustix::pipe::PipeFlags::CLOEXEC).ok()?;
            match &self.incoming {
                Some(Incoming::Ext(o)) => {
                    let mime = mime_of(&o.data::<OfferData>()?.0.lock().ok()?)?;
                    o.receive(mime.clone(), std::os::fd::AsFd::as_fd(&w));
                    Some((mime, r))
                }
                Some(Incoming::Wlr(o)) => {
                    let mime = mime_of(&o.data::<OfferData>()?.0.lock().ok()?)?;
                    o.receive(mime.clone(), std::os::fd::AsFd::as_fd(&w));
                    Some((mime, r))
                }
                None => {
                    let offer = self.core_device.as_ref()?.data().selection_offer()?;
                    let mime = offer.with_mime_types(|m| mime_of(m))?;
                    let pipe = offer.receive(mime.clone()).ok()?;
                    Some((mime, OwnedFd::from(pipe)))
                }
            }
        })();
        match receive {
            Some((_, fd)) => {
                let _ = std::thread::Builder::new().name("flatshot-paste".into()).spawn(move || {
                    let mut text = Vec::new();
                    let ok = read_for(std::fs::File::from(fd), &mut text, WAIT_FOR_DATA).is_ok();
                    let _ = tx.send(ok.then(|| String::from_utf8_lossy(&text).into_owned()));
                    wake();
                });
            }
            None => {
                let _ = tx.send(None);
            }
        }
        rx
    }
}

/// Read `from` to its end (at most MAX_PASTE), giving up after `limit`:
/// whoever owns the clipboard may never write.
fn read_for(from: std::fs::File, into: &mut Vec<u8>, limit: Duration) -> std::io::Result<()> {
    use rustix::event::{PollFd, PollFlags};
    let deadline = std::time::Instant::now() + limit;
    let mut buf = [0u8; 16384];
    loop {
        let left = deadline.saturating_duration_since(std::time::Instant::now());
        if left.is_zero() {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        let mut fds = [PollFd::new(&from, PollFlags::IN)];
        let ts = rustix::time::Timespec { tv_sec: left.as_secs() as i64, tv_nsec: left.subsec_nanos() as i64 };
        if rustix::event::poll(&mut fds, Some(&ts))? == 0 {
            return Err(std::io::ErrorKind::TimedOut.into());
        }
        match (&from).read(&mut buf) {
            Ok(0) => return Ok(()),
            Ok(n) => {
                if into.len() as u64 + n as u64 > MAX_PASTE {
                    return Err(std::io::ErrorKind::FileTooLarge.into());
                }
                into.extend_from_slice(&buf[..n]);
            }
            Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
            Err(e) => return Err(e),
        }
    }
}

/// User data for data-control devices.
pub struct DeviceData;
/// User data for our own data-control sources.
pub struct NoEventsSource;

impl Dispatch2<ext_dev::ExtDataControlDeviceV1, App> for DeviceData {
    fn event(&self, app: &mut App, _: &ext_dev::ExtDataControlDeviceV1, event: ext_dev::Event, _: &Connection, _: &QueueHandle<App>) {
        if let ext_dev::Event::Selection { id } = event {
            if let Some(Incoming::Ext(old)) = app.wl.clipboard.incoming.take() {
                old.destroy();
            }
            app.wl.clipboard.incoming = id.map(Incoming::Ext);
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<App>) -> Arc<dyn ObjectData> {
        match opcode {
            ext_dev::EVT_DATA_OFFER_OPCODE => qh.make_data::<ext_offer::ExtDataControlOfferV1, OfferData>(OfferData::default()),
            _ => panic!("unexpected child of a data-control device: {opcode}"),
        }
    }
}

impl Dispatch2<wlr_dev::ZwlrDataControlDeviceV1, App> for DeviceData {
    fn event(&self, app: &mut App, _: &wlr_dev::ZwlrDataControlDeviceV1, event: wlr_dev::Event, _: &Connection, _: &QueueHandle<App>) {
        if let wlr_dev::Event::Selection { id } = event {
            if let Some(Incoming::Wlr(old)) = app.wl.clipboard.incoming.take() {
                old.destroy();
            }
            app.wl.clipboard.incoming = id.map(Incoming::Wlr);
        }
    }

    fn event_created_child(opcode: u16, qh: &QueueHandle<App>) -> Arc<dyn ObjectData> {
        match opcode {
            wlr_dev::EVT_DATA_OFFER_OPCODE => qh.make_data::<wlr_offer::ZwlrDataControlOfferV1, OfferData>(OfferData::default()),
            _ => panic!("unexpected child of a data-control device: {opcode}"),
        }
    }
}

impl Dispatch2<ext_offer::ExtDataControlOfferV1, App> for OfferData {
    fn event(&self, _: &mut App, _: &ext_offer::ExtDataControlOfferV1, event: ext_offer::Event, _: &Connection, _: &QueueHandle<App>) {
        if let ext_offer::Event::Offer { mime_type } = event
            && let Ok(mut m) = self.0.lock()
            && m.len() < 64
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch2<wlr_offer::ZwlrDataControlOfferV1, App> for OfferData {
    fn event(&self, _: &mut App, _: &wlr_offer::ZwlrDataControlOfferV1, event: wlr_offer::Event, _: &Connection, _: &QueueHandle<App>) {
        if let wlr_offer::Event::Offer { mime_type } = event
            && let Ok(mut m) = self.0.lock()
            && m.len() < 64
        {
            m.push(mime_type);
        }
    }
}

impl Dispatch2<ext_src::ExtDataControlSourceV1, App> for NoEventsSource {
    fn event(&self, app: &mut App, source: &ext_src::ExtDataControlSourceV1, event: ext_src::Event, _: &Connection, _: &QueueHandle<App>) {
        let ours = matches!(&app.wl.clipboard.offered, Some(Offered { source: Source::Ext(s), .. }) if s == source);
        match event {
            ext_src::Event::Send { mime_type, fd } if ours => app.wl.clipboard.send(&mime_type, fd),
            ext_src::Event::Cancelled if ours => app.wl.clipboard.cancelled(),
            ext_src::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

impl Dispatch2<wlr_src::ZwlrDataControlSourceV1, App> for NoEventsSource {
    fn event(&self, app: &mut App, source: &wlr_src::ZwlrDataControlSourceV1, event: wlr_src::Event, _: &Connection, _: &QueueHandle<App>) {
        let ours = matches!(&app.wl.clipboard.offered, Some(Offered { source: Source::Wlr(s), .. }) if s == source);
        match event {
            wlr_src::Event::Send { mime_type, fd } if ours => app.wl.clipboard.send(&mime_type, fd),
            wlr_src::Event::Cancelled if ours => app.wl.clipboard.cancelled(),
            wlr_src::Event::Cancelled => source.destroy(),
            _ => {}
        }
    }
}

macro_rules! no_events_data_control {
    ($($i:ty),*) => {
        $(
            impl Dispatch2<$i, App> for NoEvents {
                fn event(&self, _: &mut App, _: &$i, _: <$i as Proxy>::Event, _: &Connection, _: &QueueHandle<App>) {}
            }
        )*
    };
}

no_events_data_control!(ext_mgr::ExtDataControlManagerV1, wlr_mgr::ZwlrDataControlManagerV1);
